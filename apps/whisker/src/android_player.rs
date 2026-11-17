//! Plays videos and sound inside Whisker on Android, with the phone's own
//! decoders: Android's media extractor reads the file, MediaCodec decodes
//! it (in hardware where the phone can), the pictures come back through an
//! image reader and are drawn over the preview in the message, like the
//! desktop player, and the sound goes out through AAudio. The same controls
//! work: play and pause, seek, mute, loop and the large view. Videos start
//! muted. Pausing, seeking, muting and the large view restart the decoders
//! at the right place, as on the desktop.
//!
//! The file stays in Whisker's private folder: nothing is handed to other apps.

use crate::{AppWindow, Player};
use ndk::audio::{AudioDirection, AudioFormat, AudioStreamBuilder};
use ndk::media::image_reader::{AcquireResult, Image, ImageFormat, ImageReader};
use ndk::media::media_codec::{DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection};
use ndk::media::media_format::MediaFormat;
use ndk_sys as ffi;
use slint::{ComponentHandle, Image as SlintImage, Rgba8Pixel, SharedPixelBuffer, Weak};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Most frames a second shown (the decoder may make more; they're skipped).
const FPS: f64 = 30.0;
/// Widest and tallest frame drawn, in the message and in the large view.
const MAX_PX: (u32, u32) = (960, 720);
const MAX_LARGE_PX: (u32, u32) = (1600, 900);
/// The codec flag for the last piece of a stream.
const END_OF_STREAM: u32 = 4;

struct Playing {
    id: String,
    path: PathBuf,
    info: Info,
    /// Where in it it is (or was paused), in seconds.
    at: f64,
    muted: bool,
    large: bool,
    /// Meant to be playing (the decoders may still be starting).
    playing: bool,
    /// Bumped on every (re)start, so old decoders stop.
    generation: u64,
}

#[derive(Clone, Copy)]
struct Info {
    duration: f64,
    video: Option<usize>,
    audio: Option<usize>,
    width: u32,
    height: u32,
    rotation: i32,
}

static NOW: Mutex<Option<Playing>> = Mutex::new(None);
/// Videos start over when they end.
static LOOP: AtomicBool = AtomicBool::new(false);

/// Turns playing videos over and over on or off.
pub fn toggle_loop(ui: &Weak<AppWindow>) {
    let on = !LOOP.fetch_xor(true, Ordering::Relaxed);
    let _ = ui.upgrade_in_event_loop(move |u| u.global::<Player>().set_looping(on));
}

/// Phones always have decoders.
pub fn available() -> bool {
    true
}

/// Whether message `id` is the one loaded in the player.
pub fn is_loaded(id: &str) -> bool {
    NOW.lock().unwrap().as_ref().is_some_and(|p| p.id == id)
}

/// Starts playing the downloaded video (or sound) `path` from message `id`.
pub fn play(ui: Weak<AppWindow>, id: String, path: PathBuf) -> bool {
    stop(&ui);
    let Some(info) = probe(&path) else { return false };
    *NOW.lock().unwrap() = Some(Playing { id: id.clone(), path, info, at: 0.0, muted: info.video.is_some(), large: false, playing: false, generation: 0 });
    let muted = info.video.is_some();
    let _ = ui.upgrade_in_event_loop(move |u| {
        let p = u.global::<Player>();
        p.set_id(id.into());
        p.set_frame(SlintImage::default());
        p.set_duration(info.duration as f32);
        p.set_position(0.0);
        p.set_muted(muted);
        p.set_has_subtitles(false);
        p.set_subtitles(false);
        p.set_large(false);
    });
    resume(&ui);
    true
}

/// Pauses or carries on.
pub fn toggle(ui: &Weak<AppWindow>) {
    let running = NOW.lock().unwrap().as_ref().is_some_and(|p| p.playing);
    if running { pause(ui) } else { resume(ui) }
}

/// Jumps to `fraction` (0–1) of the way through, and plays from there.
pub fn seek(ui: &Weak<AppWindow>, fraction: f32) {
    {
        let mut now = NOW.lock().unwrap();
        let Some(p) = now.as_mut() else { return };
        p.at = (f64::from(fraction.clamp(0.0, 1.0)) * p.info.duration).min((p.info.duration - 0.5).max(0.0));
        p.generation += 1;
    }
    resume(ui);
}

/// Turns the sound off or on.
pub fn mute(ui: &Weak<AppWindow>) {
    let (muted, running) = {
        let mut now = NOW.lock().unwrap();
        let Some(p) = now.as_mut() else { return };
        p.muted = !p.muted;
        p.generation += 1;
        (p.muted, p.playing)
    };
    let _ = ui.upgrade_in_event_loop(move |u| u.global::<Player>().set_muted(muted));
    if running {
        resume(ui);
    }
}

/// A new sound level (0–100): taken up by the sound as it plays, so
/// nothing restarts (0 turns the sound off).
pub fn set_volume(ui: &Weak<AppWindow>, level: i32) {
    // Down to 0 is "off": the level before stays for turning it back on.
    if level > 0 {
        crate::ui::VOLUME.store(level.min(100) as u32, std::sync::atomic::Ordering::Relaxed);
    }
    let muted = level <= 0;
    let restart = {
        let mut now = NOW.lock().unwrap();
        match now.as_mut() {
            // Turning the sound on or off starts or stops its decoder.
            Some(p) if p.muted != muted => {
                p.muted = muted;
                p.generation += 1;
                p.playing
            }
            _ => false,
        }
    };
    let _ = ui.upgrade_in_event_loop(move |u| {
        let p = u.global::<Player>();
        p.set_muted(muted);
        if level > 0 {
            p.set_volume(level);
        }
    });
    if restart {
        resume(ui);
    }
}

/// No subtitles on phones yet.
pub fn subtitles(_ui: &Weak<AppWindow>) {}

/// Shows the video big over the window, or back in its message.
pub fn set_large(ui: &Weak<AppWindow>, large: bool) {
    let running = {
        let mut now = NOW.lock().unwrap();
        let Some(p) = now.as_mut() else { return };
        p.large = large;
        p.generation += 1;
        p.playing
    };
    let _ = ui.upgrade_in_event_loop(move |u| u.global::<Player>().set_large(large));
    if running {
        resume(ui);
    }
}

/// Stops and forgets what's playing.
pub fn stop(ui: &Weak<AppWindow>) {
    NOW.lock().unwrap().take();
    let _ = ui.upgrade_in_event_loop(|u| {
        let p = u.global::<Player>();
        p.set_id("".into());
        p.set_playing(false);
        p.set_loading(false);
        p.set_large(false);
        p.set_frame(SlintImage::default());
    });
}

fn pause(ui: &Weak<AppWindow>) {
    if let Some(p) = NOW.lock().unwrap().as_mut() {
        p.playing = false;
        p.generation += 1;
    }
    let _ = ui.upgrade_in_event_loop(|u| u.global::<Player>().set_playing(false));
}

/// Plays from where it is.
fn resume(ui: &Weak<AppWindow>) {
    let (path, at, info, muted, large, generation) = {
        let mut now = NOW.lock().unwrap();
        let Some(p) = now.as_mut() else { return };
        p.generation += 1;
        p.playing = true;
        // Played to the end: start over.
        if p.info.duration > 0.0 && p.at >= p.info.duration - 0.2 {
            p.at = 0.0;
        }
        (p.path.clone(), p.at, p.info, p.muted, p.large, p.generation)
    };
    let _ = ui.upgrade_in_event_loop(|u| {
        let p = u.global::<Player>();
        p.set_playing(true);
        p.set_loading(true);
    });
    let start = Instant::now() + Duration::from_millis(150);
    let max = if large { MAX_LARGE_PX } else { MAX_PX };
    if info.audio.is_some() && (!muted || info.video.is_none()) {
        let (path, ui) = (path.clone(), ui.clone());
        // With a picture, the picture's thread says when it's over.
        let leads = info.video.is_none();
        std::thread::spawn(move || {
            let played = play_sound(&path, info, at, start, generation, leads.then_some(&ui));
            if leads {
                finished(&ui, generation, played);
            }
        });
    }
    if info.video.is_some() {
        let ui = ui.clone();
        std::thread::spawn(move || {
            let shown = play_picture(&ui, &path, info, at, start, max, generation);
            finished(&ui, generation, shown);
        });
    }
}

fn current(generation: u64) -> bool {
    NOW.lock().unwrap().as_ref().is_some_and(|p| p.generation == generation)
}

/// It played out (not replaced): back to the preview, or from the top
/// again when looping (not when nothing could be played).
fn finished(ui: &Weak<AppWindow>, generation: u64, played: u64) {
    let done = {
        let mut now = NOW.lock().unwrap();
        match now.as_mut() {
            Some(p) if p.generation == generation => {
                p.at = 0.0;
                p.playing = false;
                true
            }
            _ => false,
        }
    };
    if done && played > 0 && LOOP.load(Ordering::Relaxed) {
        return resume(ui);
    }
    if done {
        let _ = ui.upgrade_in_event_loop(|u| {
            let p = u.global::<Player>();
            p.set_playing(false);
            p.set_loading(false);
            p.set_position(0.0);
            p.set_frame(SlintImage::default());
        });
    }
}

/// Android's media extractor over a file.
struct Extractor {
    ptr: *mut ffi::AMediaExtractor,
    _file: std::fs::File,
}

impl Extractor {
    fn open(path: &Path) -> Option<Self> {
        let file = std::fs::File::open(path).ok()?;
        let len = file.metadata().ok()?.len() as i64;
        // SAFETY: plain calls into libmediandk; the file stays open as long as the extractor.
        let ptr = unsafe { ffi::AMediaExtractor_new() };
        if ptr.is_null() {
            return None;
        }
        let me = Self { ptr, _file: file };
        let status = unsafe { ffi::AMediaExtractor_setDataSourceFd(me.ptr, me._file.as_raw_fd(), 0, len) };
        (status.0 == 0).then_some(me)
    }

    fn tracks(&self) -> usize {
        unsafe { ffi::AMediaExtractor_getTrackCount(self.ptr) }
    }

    fn format(&self, track: usize) -> Option<MediaFormat> {
        let f = std::ptr::NonNull::new(unsafe { ffi::AMediaExtractor_getTrackFormat(self.ptr, track) })?;
        // SAFETY: the extractor hands over a new format, which MediaFormat deletes.
        Some(unsafe { MediaFormat::from_ptr(f) })
    }

    fn select(&self, track: usize) {
        unsafe { ffi::AMediaExtractor_selectTrack(self.ptr, track) };
    }

    fn seek(&self, us: i64) {
        unsafe { ffi::AMediaExtractor_seekTo(self.ptr, us, ffi::SeekMode::AMEDIAEXTRACTOR_SEEK_PREVIOUS_SYNC) };
    }

    /// The next piece into `buf`, with its time (µs); `None` at the end.
    fn read(&self, buf: &mut [u8]) -> Option<(usize, i64)> {
        let n = unsafe { ffi::AMediaExtractor_readSampleData(self.ptr, buf.as_mut_ptr(), buf.len()) };
        if n < 0 {
            return None;
        }
        let t = unsafe { ffi::AMediaExtractor_getSampleTime(self.ptr) };
        unsafe { ffi::AMediaExtractor_advance(self.ptr) };
        Some((n as usize, t))
    }
}

impl Drop for Extractor {
    fn drop(&mut self) {
        unsafe { ffi::AMediaExtractor_delete(self.ptr) };
    }
}

fn mime(f: &mut MediaFormat) -> String {
    f.str("mime").unwrap_or_default().to_owned()
}

/// What's in the file: its length, its picture and sound tracks, and the
/// picture's size and turn.
fn probe(path: &Path) -> Option<Info> {
    let ex = Extractor::open(path)?;
    let mut info = Info { duration: 0.0, video: None, audio: None, width: 0, height: 0, rotation: 0 };
    for t in 0..ex.tracks() {
        let Some(mut f) = ex.format(t) else { continue };
        let m = mime(&mut f);
        if let Some(us) = f.i64("durationUs") {
            info.duration = info.duration.max(us as f64 / 1e6);
        }
        if m.starts_with("video/") && info.video.is_none() {
            info.video = Some(t);
            info.width = f.i32("width").unwrap_or(0).max(0) as u32;
            info.height = f.i32("height").unwrap_or(0).max(0) as u32;
            info.rotation = f.i32("rotation-degrees").unwrap_or(0).rem_euclid(360);
        } else if m.starts_with("audio/") && info.audio.is_none() {
            info.audio = Some(t);
        }
    }
    (info.video.is_some() || info.audio.is_some()).then_some(info)
}

/// Feeds the decoder from the extractor while it takes more; false once
/// the end has been sent.
fn feed(codec: &MediaCodec, ex: &Extractor, buf: &mut Vec<u8>) -> bool {
    loop {
        let Ok(DequeuedInputBufferResult::Buffer(mut input)) = codec.dequeue_input_buffer(Duration::ZERO) else { return true };
        let space = input.buffer_mut();
        if buf.len() < space.len() {
            buf.resize(space.len(), 0);
        }
        match ex.read(&mut buf[..space.len()]) {
            Some((n, t)) => {
                // SAFETY: `n` bytes were just written; MaybeUninit<u8> has u8's layout.
                for (d, s) in space.iter_mut().zip(&buf[..n]) {
                    d.write(*s);
                }
                let _ = codec.queue_input_buffer(input, 0, n, t.max(0) as u64, 0);
            }
            None => {
                let _ = codec.queue_input_buffer(input, 0, 0, 0, END_OF_STREAM);
                return false;
            }
        }
    }
}

/// Waits until `due` (from `start`), giving up when replaced. False if replaced.
fn wait_until(start: Instant, due: f64, generation: u64) -> bool {
    loop {
        if !current(generation) {
            return false;
        }
        let now = start.elapsed().as_secs_f64();
        if now >= due {
            return true;
        }
        std::thread::sleep(Duration::from_secs_f64((due - now).min(0.05)));
    }
}

/// How long a decoder drawing to the image reader may go without a
/// picture before the software decoder (into memory) is tried instead.
const NO_PICTURE_GIVE_UP: Duration = Duration::from_secs(4);

/// Where the decoder puts its pictures.
#[derive(Clone, Copy, PartialEq)]
enum Output {
    /// The phone's decoder (in hardware where it can) drawing into an image
    /// reader: the usual way, and quick.
    Reader,
    /// A software decoder writing into memory, which Whisker reads itself:
    /// for phones whose decoder never hands the reader a picture.
    Memory,
}

enum Outcome {
    Shown(u64),
    /// Nothing came out to show (see [`Output::Memory`]).
    NoPictures(String),
}

/// Decodes the picture from `at` and draws it, paced to real time from
/// `start`. Returns how many frames it showed.
fn play_picture(ui: &Weak<AppWindow>, path: &Path, info: Info, at: f64, start: Instant, max: (u32, u32), generation: u64) -> u64 {
    let why = match decode_picture(ui, path, info, at, start, max, generation, Output::Reader) {
        Outcome::Shown(n) => return n,
        Outcome::NoPictures(why) => why,
    };
    if !current(generation) {
        return 0;
    }
    tracing::warn!("no pictures from the phone's video decoder ({why}); trying the software one");
    // Started late: it catches up from where it is now.
    let start = Instant::now() + Duration::from_millis(100);
    match decode_picture(ui, path, info, at, start, max, generation, Output::Memory) {
        Outcome::Shown(n) => n,
        Outcome::NoPictures(why2) => {
            if current(generation) {
                let msg = format!("This phone couldn't show that video ({why}; {why2}). Hold it and choose Share to open it in another app.");
                let _ = ui.upgrade_in_event_loop(move |u| u.set_status(msg.into()));
            }
            0
        }
    }
}

/// Software decoders Android itself carries, by format (newer names first).
fn software_decoders(mime: &str) -> &'static [&'static str] {
    match mime {
        "video/avc" => &["c2.android.avc.decoder", "OMX.google.h264.decoder"],
        "video/hevc" => &["c2.android.hevc.decoder", "OMX.google.hevc.decoder"],
        "video/x-vnd.on2.vp8" => &["c2.android.vp8.decoder", "OMX.google.vp8.decoder"],
        "video/x-vnd.on2.vp9" => &["c2.android.vp9.decoder", "OMX.google.vp9.decoder"],
        "video/av01" => &["c2.android.av1.decoder"],
        "video/mp4v-es" => &["c2.android.mpeg4.decoder", "OMX.google.mpeg4.decoder"],
        "video/3gpp" => &["c2.android.h263.decoder", "OMX.google.h263.decoder"],
        _ => &[],
    }
}

/// How a decoder lays a picture out in memory ([`Output::Memory`]).
#[derive(Clone, Copy)]
struct Layout {
    stride: usize,
    slice: usize,
    /// U and V interleaved (NV12), not one after the other (I420).
    interleaved: bool,
    crop: (u32, u32, u32, u32),
}

impl Layout {
    fn of(f: &mut MediaFormat, w: u32, h: u32) -> Self {
        let w2 = f.i32("width").filter(|v| *v > 0).map_or(w, |v| v as u32);
        let h2 = f.i32("height").filter(|v| *v > 0).map_or(h, |v| v as u32);
        let stride = f.i32("stride").filter(|v| *v as u32 >= w2).map_or(w2, |v| v as u32) as usize;
        let slice = f.i32("slice-height").filter(|v| *v as u32 >= h2).map_or(h2, |v| v as u32) as usize;
        // 21: NV12, 39: packed NV12, and vendors' own (0x7FA3…) are NV12-like;
        // 19 and 0x7F420888 (flexible) come as I420 from software decoders.
        let color = f.i32("color-format").unwrap_or(19);
        let interleaved = matches!(color, 21 | 39) || (color as u32 & 0xFFFF_0000) == 0x7FA3_0000;
        let left = f.i32("crop-left").unwrap_or(0).max(0) as u32;
        let top = f.i32("crop-top").unwrap_or(0).max(0) as u32;
        let right = f.i32("crop-right").map_or(w2, |v| (v + 1).max(0) as u32).min(w2);
        let bottom = f.i32("crop-bottom").map_or(h2, |v| (v + 1).max(0) as u32).min(h2);
        let crop = if right > left && bottom > top { (left, top, right, bottom) } else { (0, 0, w2, h2) };
        Self { stride, slice, interleaved, crop }
    }

    fn planes<'a>(&self, data: &'a [u8]) -> Planes<'a> {
        let at = |n: usize| data.get(n..).unwrap_or_default();
        let luma = self.stride * self.slice;
        if self.interleaved {
            Planes { y: data, u: at(luma), v: at(luma + 1), y_row: self.stride, uv_row: self.stride, uv_pixel: 2 }
        } else {
            let half = self.stride.div_ceil(2);
            Planes { y: data, u: at(luma), v: at(luma + half * self.slice.div_ceil(2)), y_row: self.stride, uv_row: half, uv_pixel: 1 }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_picture(ui: &Weak<AppWindow>, path: &Path, info: Info, at: f64, start: Instant, max: (u32, u32), generation: u64, output: Output) -> Outcome {
    let Some(ex) = Extractor::open(path) else { return Outcome::NoPictures("the file can't be read".into()) };
    let Some(track) = info.video else { return Outcome::Shown(0) };
    let Some(mut format) = ex.format(track) else { return Outcome::NoPictures("no video format".into()) };
    let kind = mime(&mut format);
    let phones = || MediaCodec::from_decoder_type(&kind).map(|c| (c, format!("the phone's {kind} decoder")));
    let codec = match output {
        Output::Reader => phones(),
        Output::Memory => software_decoders(&kind).iter().find_map(|n| MediaCodec::from_codec_name(n).map(|c| (c, (*n).to_owned()))).or_else(phones),
    };
    let Some((codec, name)) = codec else { return Outcome::NoPictures(format!("no decoder for {kind}")) };
    let (w, h) = (info.width.max(2) as i32, info.height.max(2) as i32);
    let reader = match output {
        Output::Reader => match ImageReader::new(w, h, ImageFormat::YUV_420_888, 4) {
            Ok(r) => Some(r),
            Err(e) => return Outcome::NoPictures(format!("image reader: {e:?}")),
        },
        Output::Memory => None,
    };
    let window = match &reader {
        Some(r) => match r.window() {
            Ok(win) => Some(win),
            Err(e) => return Outcome::NoPictures(format!("image reader window: {e:?}")),
        },
        None => None,
    };
    if let Err(e) = codec.configure(&format, window.as_ref(), MediaCodecDirection::Decoder) {
        return Outcome::NoPictures(format!("{name} wouldn't take it: {e:?}"));
    }
    if let Err(e) = codec.start() {
        return Outcome::NoPictures(format!("{name} wouldn't start: {e:?}"));
    }
    ex.select(track);
    let at_us = (at * 1e6) as i64;
    ex.seek(at_us);
    let size = frame_size(info, max);
    let mut layout = Layout::of(&mut format, info.width, info.height);
    let mut buf = Vec::new();
    let mut more = true;
    let mut shown = 0u64;
    let mut rendered = 0u64;
    let mut outputs = 0u64;
    let mut last_drawn = f64::NEG_INFINITY;
    let began = Instant::now();
    let mut outcome = None;
    loop {
        if !current(generation) {
            break;
        }
        // Nothing out of the decoder, or pictures that can't be read: give
        // up on this way. (Frames before a seek point are only skipped.)
        if shown == 0 && (rendered > 0 || outputs == 0) && began.elapsed() > NO_PICTURE_GIVE_UP {
            outcome = Some(Outcome::NoPictures(if rendered == 0 { format!("{name} made no pictures") } else { format!("{name} drew {rendered} pictures Whisker couldn't read") }));
            break;
        }
        if more {
            more = feed(&codec, &ex, &mut buf);
        }
        let out = match codec.dequeue_output_buffer(Duration::from_millis(10)) {
            Ok(DequeuedOutputBufferInfoResult::Buffer(out)) => out,
            Ok(DequeuedOutputBufferInfoResult::OutputFormatChanged) => {
                layout = Layout::of(&mut codec.output_format(), info.width, info.height);
                continue;
            }
            Ok(_) => continue,
            Err(e) => {
                if shown == 0 {
                    outcome = Some(Outcome::NoPictures(format!("{name} stopped: {e:?}")));
                }
                break;
            }
        };
        outputs += 1;
        let pts = out.info().presentation_time_us();
        let end = out.info().flags() & END_OF_STREAM != 0;
        let t = pts as f64 / 1e6;
        // Frames before where it starts (decoded from the key frame before it),
        // more than FPS a second, and late ones are skipped (though one is
        // drawn every half second, so a slow phone still shows something).
        let late = start.elapsed().as_secs_f64() > t - at + 0.1;
        // (Drawn to a reader, a picture's buffer may say it holds nothing.)
        let empty = out.info().size() <= 0 && (end || output == Output::Memory);
        let skip = pts < at_us || empty || t - last_drawn < 1.0 / FPS || (late && t - last_drawn < 0.5);
        if skip {
            let _ = codec.release_output_buffer(out, false);
        } else {
            if !wait_until(start, t - at, generation) {
                let _ = codec.release_output_buffer(out, false);
                break;
            }
            let frame = match &reader {
                Some(reader) => {
                    let _ = codec.release_output_buffer(out, true);
                    rendered += 1;
                    latest(reader).and_then(|img| to_rgba(&img, info.rotation, size))
                }
                None => {
                    let (offset, len) = (out.info().offset().max(0) as usize, out.info().size().max(0) as usize);
                    let data = out.buffer();
                    let data = &data[offset.min(data.len())..(offset + len).min(data.len())];
                    rendered += 1;
                    let frame = convert(&layout.planes(data), layout.crop, info.rotation, size);
                    let _ = codec.release_output_buffer(out, false);
                    Some(frame)
                }
            };
            if let Some(frame) = frame {
                last_drawn = t;
                shown += 1;
                {
                    let mut now = NOW.lock().unwrap();
                    match now.as_mut() {
                        Some(p) if p.generation == generation => p.at = t,
                        _ => break,
                    }
                }
                let first = shown == 1;
                let _ = ui.upgrade_in_event_loop(move |u| {
                    let p = u.global::<Player>();
                    p.set_frame(SlintImage::from_rgba8(frame));
                    p.set_position(t as f32);
                    if first {
                        p.set_loading(false);
                    }
                });
            }
        }
        if end {
            break;
        }
    }
    let _ = codec.stop();
    // The decoder goes before the reader it draws into.
    drop(codec);
    drop(window);
    drop(reader);
    match outcome {
        Some(o) => o,
        None if shown == 0 && current(generation) && rendered > 0 => Outcome::NoPictures(format!("{name}'s pictures couldn't be read")),
        None => Outcome::Shown(shown),
    }
}

/// The picture the decoder just drew (it arrives a moment after).
fn latest(reader: &ImageReader) -> Option<Image> {
    for _ in 0..20 {
        if let Ok(AcquireResult::Image(img)) = reader.acquire_latest_image() {
            return Some(img);
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    None
}

/// Frame size: the picture's (turned as it's meant to be seen), fitted in
/// `max`, in even numbers.
fn frame_size(info: Info, max: (u32, u32)) -> (u32, u32) {
    let (w, h) = if info.rotation == 90 || info.rotation == 270 { (info.height, info.width) } else { (info.width, info.height) };
    let (w, h) = (f64::from(w.max(2)), f64::from(h.max(2)));
    let scale = (f64::from(max.0) / w).min(f64::from(max.1) / h).min(1.0);
    let even = |v: f64| ((v * scale) as u32).max(2) & !1;
    (even(w), even(h))
}

/// A decoded YUV picture as RGBA at `size`, turned by `rotation` degrees.
fn to_rgba(img: &Image, rotation: i32, size: (u32, u32)) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let crop = img.crop_rect().ok();
    let (iw, ih) = (img.width().ok()?, img.height().ok()?);
    let (left, top, right, bottom) = match crop {
        Some(c) if c.right > c.left && c.bottom > c.top => (c.left, c.top, c.right.min(iw), c.bottom.min(ih)),
        _ => (0, 0, iw, ih),
    };
    let planes = Planes {
        y: img.plane_data(0).ok()?,
        u: img.plane_data(1).ok()?,
        v: img.plane_data(2).ok()?,
        y_row: img.plane_row_stride(0).ok()? as usize,
        uv_row: img.plane_row_stride(1).ok()? as usize,
        uv_pixel: img.plane_pixel_stride(1).ok()? as usize,
    };
    Some(convert(&planes, (left as u32, top as u32, right as u32, bottom as u32), rotation, size))
}

/// The part of a YUV picture inside `crop` (left, top, right, bottom) as
/// RGBA at `size`, turned by `rotation` degrees.
fn convert(planes: &Planes, crop: (u32, u32, u32, u32), rotation: i32, size: (u32, u32)) -> SharedPixelBuffer<Rgba8Pixel> {
    let (left, top, right, bottom) = crop;
    let (cw, ch) = (right.saturating_sub(left).max(1), bottom.saturating_sub(top).max(1));
    let mut out = SharedPixelBuffer::<Rgba8Pixel>::new(size.0, size.1);
    let (ow, oh) = size;
    let px = out.make_mut_bytes();
    for oy in 0..oh {
        for ox in 0..ow {
            // Where this output pixel comes from in the (unturned) picture.
            let (sx, sy) = match rotation {
                90 => (oy * cw / oh, (ow - 1 - ox) * ch / ow),
                180 => ((ow - 1 - ox) * cw / ow, (oh - 1 - oy) * ch / oh),
                270 => ((oh - 1 - oy) * cw / oh, ox * ch / ow),
                _ => (ox * cw / ow, oy * ch / oh),
            };
            let (x, y) = (left as usize + sx as usize, top as usize + sy as usize);
            let rgb = planes.rgb(x, y);
            let i = ((oy * ow + ox) * 4) as usize;
            px[i..i + 4].copy_from_slice(&[rgb.0, rgb.1, rgb.2, 255]);
        }
    }
    out
}

struct Planes<'a> {
    y: &'a [u8],
    u: &'a [u8],
    v: &'a [u8],
    y_row: usize,
    uv_row: usize,
    uv_pixel: usize,
}

impl Planes<'_> {
    /// The pixel at (x, y) as RGB (BT.601, video range: what phones record).
    fn rgb(&self, x: usize, y: usize) -> (u8, u8, u8) {
        let luma = i32::from(*self.y.get(y * self.y_row + x).unwrap_or(&16));
        let c = (y / 2) * self.uv_row + (x / 2) * self.uv_pixel;
        let u = i32::from(*self.u.get(c).unwrap_or(&128)) - 128;
        let v = i32::from(*self.v.get(c).unwrap_or(&128)) - 128;
        let l = (luma - 16).max(0) * 298;
        let clamp = |n: i32| ((n + 128) >> 8).clamp(0, 255) as u8;
        (clamp(l + 409 * v), clamp(l - 100 * u - 208 * v), clamp(l + 516 * u))
    }
}

/// Decodes the sound from `at` and plays it, starting at `start`. With
/// `ui` (sound only, no picture) it also moves the position along. Returns
/// how many pieces it played.
fn play_sound(path: &Path, info: Info, at: f64, start: Instant, generation: u64, ui: Option<&Weak<AppWindow>>) -> u64 {
    let Some(ex) = Extractor::open(path) else { return 0 };
    let Some(track) = info.audio else { return 0 };
    let Some(mut format) = ex.format(track) else { return 0 };
    let Some(codec) = MediaCodec::from_decoder_type(&mime(&mut format)) else { return 0 };
    if codec.configure(&format, None, MediaCodecDirection::Decoder).is_err() || codec.start().is_err() {
        return 0;
    }
    ex.select(track);
    let at_us = (at * 1e6) as i64;
    ex.seek(at_us);
    let mut rate = format.i32("sample-rate").unwrap_or(48000);
    let mut channels = format.i32("channel-count").unwrap_or(2);
    let mut stream = None;
    let mut buf = Vec::new();
    let mut more = true;
    let mut played = 0u64;
    let mut last_shown = 0.0f64;
    if !wait_until(start, 0.0, generation) {
        return 0;
    }
    loop {
        if !current(generation) {
            break;
        }
        if more {
            more = feed(&codec, &ex, &mut buf);
        }
        let out = match codec.dequeue_output_buffer(Duration::from_millis(10)) {
            Ok(DequeuedOutputBufferInfoResult::Buffer(out)) => out,
            Ok(DequeuedOutputBufferInfoResult::OutputFormatChanged) => {
                let mut f = codec.output_format();
                rate = f.i32("sample-rate").unwrap_or(rate);
                channels = f.i32("channel-count").unwrap_or(channels);
                let _ = f.str("mime");
                stream = None;
                continue;
            }
            Ok(_) => continue,
            Err(_) => break,
        };
        let pts = out.info().presentation_time_us();
        let end = out.info().flags() & END_OF_STREAM != 0;
        let (offset, size) = (out.info().offset().max(0) as usize, out.info().size().max(0) as usize);
        if pts >= at_us && size > 0 {
            if stream.is_none() {
                stream = AudioStreamBuilder::new()
                    .ok()
                    .map(|b| b.direction(AudioDirection::Output).format(AudioFormat::PCM_I16).channel_count(channels).sample_rate(rate))
                    .and_then(|b| b.open_stream().ok());
                if let Some(s) = &stream {
                    let _ = s.request_start();
                }
            }
            if let Some(s) = &stream {
                let raw = &out.buffer()[offset..(offset + size).min(out.buffer().len())];
                // Below full level, the samples are scaled down here.
                let level = crate::ui::volume();
                let scaled: Vec<u8>;
                let data = if level < 100 {
                    scaled = raw
                        .chunks_exact(2)
                        .flat_map(|b| ((i32::from(i16::from_le_bytes([b[0], b[1]])) * level as i32 / 100) as i16).to_le_bytes())
                        .collect();
                    &scaled[..]
                } else {
                    raw
                };
                let frames = (data.len() / (2 * channels.max(1) as usize)) as i32;
                let mut done = 0i32;
                while done < frames && current(generation) {
                    let from = data[(done as usize) * 2 * channels.max(1) as usize..].as_ptr().cast();
                    // SAFETY: `from` points at `frames - done` whole frames of 16-bit sound in `data`.
                    match unsafe { s.write(from, frames - done, 100_000_000) } {
                        Ok(n) => done += n as i32,
                        Err(_) => break,
                    }
                }
                played += 1;
            }
            let t = pts as f64 / 1e6;
            if let Some(ui) = ui
                && (t - last_shown).abs() >= 0.25
            {
                last_shown = t;
                {
                    let mut now = NOW.lock().unwrap();
                    if let Some(p) = now.as_mut().filter(|p| p.generation == generation) {
                        p.at = t;
                    }
                }
                let first = played == 1;
                let _ = ui.upgrade_in_event_loop(move |u| {
                    let p = u.global::<Player>();
                    p.set_position(t as f32);
                    if first {
                        p.set_loading(false);
                    }
                });
            }
        }
        let _ = codec.release_output_buffer(out, false);
        if end {
            break;
        }
    }
    if let Some(s) = &stream {
        let _ = s.request_stop();
    }
    let _ = codec.stop();
    played
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_fit_and_stay_even() {
        let info = |w, h, rotation| Info { duration: 0.0, video: Some(0), audio: None, width: w, height: h, rotation };
        assert_eq!(frame_size(info(1920, 1080, 0), MAX_PX), (960, 540));
        assert_eq!(frame_size(info(1920, 1080, 90), MAX_PX), (404, 720));
    }
}
