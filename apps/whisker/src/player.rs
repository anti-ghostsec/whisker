//! Plays videos inside the window, in the message itself: ffmpeg decodes
//! the picture (paced to real time) into frames shown over the preview, and
//! sends the sound straight to the system (PulseAudio, else ALSA), drawing
//! any subtitles onto the picture. Videos start muted. Seeking, subtitles,
//! the large view and turning the sound on restart ffmpeg at the right
//! place; pausing holds it where it is, and a new sound level is told to
//! it as it plays, so neither has to load the video again.
//!
//! A video plays either from a downloaded file or while it streams, read
//! through a [`Relay`] (the login never leaves Whisker). Sound alone (voice
//! messages, music) plays the same way, with no picture.

use crate::{AppWindow, Player};
use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer, Weak};
use std::io::Read;
use std::ffi::{OsStr, OsString};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use whisker_core::files::Relay;
use whisker_core::timeline::{SubtitleTrack, Subtitles, VideoInfo, probe_media_tracks};

/// Frames a second shown.
const FPS: u32 = 30;
/// Widest and tallest frame decoded, in the message and in the large view.
const MAX_PX: (u32, u32) = (960, 720);
const MAX_LARGE_PX: (u32, u32) = (1600, 900);

struct Playing {
    id: String,
    /// The file, or the relay's address.
    input: OsString,
    /// Serves `input` while the video streams.
    _relay: Option<Relay>,
    info: VideoInfo,
    /// Where in the video it is (or was paused), in seconds.
    at: f64,
    muted: bool,
    /// Subtitles drawn (when it has them).
    subtitles: bool,
    /// The subtitle tracks it has, and which of them is drawn.
    tracks: Vec<SubtitleTrack>,
    track: usize,
    /// Shown big, over the whole window.
    large: bool,
    /// Meant to be playing (ffmpeg may still be starting).
    playing: bool,
    child: Option<Child>,
    /// `child` is held still (paused), to carry on from where it is.
    frozen: bool,
    /// `child` sends sound to the system, so it can be told a new level.
    sounding: bool,
    /// `child`'s sound has a head start (see [`LEAD_FRAMES`]).
    lead: bool,
    /// When `child` was held still, and how many times its sound has been
    /// given its head start again after that.
    frozen_at: Option<std::time::Instant>,
    leads: u32,
    /// Bumped on every (re)start, so an old reader stops showing frames.
    generation: u64,
}

static NOW: Mutex<Option<Playing>> = Mutex::new(None);
/// How many starts there have been, of every video: each gets a number of
/// its own (what's kept by number outlives a video).
static STARTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The last start (`Playing::generation`) that showed a picture.
static SHOWN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Videos start over when they end.
static LOOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Turns playing videos over and over on or off.
pub fn toggle_loop(ui: &Weak<AppWindow>) {
    let on = !LOOP.fetch_xor(true, std::sync::atomic::Ordering::Relaxed);
    let _ = ui.upgrade_in_event_loop(move |u| u.global::<Player>().set_looping(on));
}

/// Whether this system can play videos in the window.
pub fn available() -> bool {
    static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| {
        whisker_core::tool("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
    })
}

/// Whether message `id` is the one loaded in the player.
pub fn is_loaded(id: &str) -> bool {
    NOW.lock().unwrap().as_ref().is_some_and(|p| p.id == id)
}

/// Starts playing the downloaded video `path` from message `id`. (Not on
/// Windows, where downloads play from memory: see `play_stream`.)
#[cfg(not(windows))]
pub fn play(ui: Weak<AppWindow>, id: String, path: std::path::PathBuf) -> bool {
    start(ui, id, path.into_os_string(), None)
}

/// Starts playing message `id`'s video while it streams through `relay`.
/// False when ffprobe can't make it out that way (then it's downloaded).
pub fn play_stream(ui: Weak<AppWindow>, id: String, relay: Relay) -> bool {
    start(ui, id, relay.url.clone().into(), Some(relay))
}

fn start(ui: Weak<AppWindow>, id: String, input: OsString, relay: Option<Relay>) -> bool {
    stop(&ui);
    // Still loading, as far as anyone watching can tell: stopping what
    // played before cleared the dots, and looking at the file takes a
    // while (it's read from the server), so it looked as if nothing was
    // happening until the dots came back with the first picture.
    let shown = id.clone();
    let _ = ui.upgrade_in_event_loop(move |u| {
        let p = u.global::<Player>();
        p.set_id(shown.into());
        p.set_loading(true);
    });
    let Some((info, tracks)) = probe_media_tracks(&input) else { return false };
    // Sound alone needs somewhere to go.
    let sound_only = info.width == 0;
    if sound_only && sound_output().is_none() {
        return false;
    }
    let duration = info.duration.map_or(0.0, |d| d.as_secs_f64());
    let has_subtitles = !tracks.is_empty();
    let labels: Vec<slint::SharedString> = tracks.iter().map(|t| t.label.as_str().into()).collect();
    // Text subtitles are read from the whole file before the first frame:
    // when streaming, they're there to turn on, not on at the start.
    let shown = has_subtitles && !(relay.is_some() && tracks.first().is_some_and(|t| t.kind == Subtitles::Text));
    // Everything starts muted (no sudden loud sound); the speaker turns it on.
    let playing = Playing { id: id.clone(), input, _relay: relay, info, at: 0.0, muted: true, subtitles: shown, tracks, track: 0, large: false, playing: false, child: None, frozen: false, sounding: false, lead: false, frozen_at: None, leads: 0, generation: 0 };
    *NOW.lock().unwrap() = Some(playing);
    let _ = ui.upgrade_in_event_loop(move |u| {
        let p = u.global::<Player>();
        p.set_id(id.into());
        p.set_frame(Image::default());
        p.set_duration(duration as f32);
        p.set_position(0.0);
        p.set_muted(true);
        p.set_has_subtitles(has_subtitles);
        p.set_subtitles(shown);
        p.set_subtitle_tracks(slint::ModelRc::new(slint::VecModel::from(labels)));
        p.set_subtitle_track(0);
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
        let length = p.info.duration.map_or(0.0, |d| d.as_secs_f64());
        p.at = (f64::from(fraction.clamp(0.0, 1.0)) * length).min((length - 0.5).max(0.0));
        kill(p);
    }
    resume(ui);
}

/// Turns the sound off or on.
pub fn mute(ui: &Weak<AppWindow>) {
    let (muted, running) = {
        let mut now = NOW.lock().unwrap();
        let Some(p) = now.as_mut() else { return };
        p.muted = !p.muted;
        (p.muted, retune(p))
    };
    let _ = ui.upgrade_in_event_loop(move |u| u.global::<Player>().set_muted(muted));
    if running {
        resume(ui);
    }
}

/// A new sound level (0–100): turns the sound on (0 turns it off). ffmpeg
/// takes it as it plays; only sound that was off has to start.
pub fn set_volume(ui: &Weak<AppWindow>, level: i32) {
    // Down to 0 is "off": the level before stays for turning it back on.
    if level > 0 {
        crate::ui::VOLUME.store(level.min(100) as u32, std::sync::atomic::Ordering::Relaxed);
    }
    let muted = level <= 0;
    let changed = NOW.lock().unwrap().as_mut().map(|p| {
        p.muted = muted;
        retune(p)
    });
    let _ = ui.upgrade_in_event_loop(move |u| {
        let p = u.global::<Player>();
        p.set_muted(muted);
        if level > 0 {
            p.set_volume(level);
        }
    });
    if changed == Some(true) {
        resume(ui);
    }
}

/// Shows or hides the subtitles.
pub fn subtitles(ui: &Weak<AppWindow>) {
    let Some((on, running)) = restart_with(|p| p.subtitles = !p.subtitles && !p.tracks.is_empty()).map(|p| (p.subtitles, p.running)) else { return };
    let _ = ui.upgrade_in_event_loop(move |u| u.global::<Player>().set_subtitles(on));
    if running {
        resume(ui);
    }
}

/// Draws subtitle track `choice` (as listed), or none (-1).
pub fn pick_subtitles(ui: &Weak<AppWindow>, choice: i32) {
    let picked = restart_with(|p| match usize::try_from(choice).ok().filter(|n| *n < p.tracks.len()) {
        Some(n) => {
            p.track = n;
            p.subtitles = true;
        }
        None => p.subtitles = false,
    })
    .map(|p| (p.subtitles, p.running));
    let Some((on, running)) = picked else { return };
    let _ = ui.upgrade_in_event_loop(move |u| {
        let p = u.global::<Player>();
        p.set_subtitles(on);
        if on {
            p.set_subtitle_track(choice);
        }
    });
    if running {
        resume(ui);
    }
}

/// Shows the video big over the window, or back in its message. It's
/// decoded again at the size it's shown.
pub fn set_large(ui: &Weak<AppWindow>, large: bool) {
    let Some(running) = restart_with(|p| p.large = large).map(|p| p.running) else { return };
    let _ = ui.upgrade_in_event_loop(move |u| u.global::<Player>().set_large(large));
    if running {
        resume(ui);
    }
}

struct Changed {
    subtitles: bool,
    running: bool,
}

/// Changes the video's settings, stopping ffmpeg so it can start again
/// with them. Says whether it was playing.
fn restart_with(change: impl FnOnce(&mut Playing)) -> Option<Changed> {
    let mut now = NOW.lock().unwrap();
    let p = now.as_mut()?;
    change(p);
    let running = p.playing;
    kill(p);
    Some(Changed { subtitles: p.subtitles, running })
}

/// Stops and forgets the video.
pub fn stop(ui: &Weak<AppWindow>) {
    if let Some(mut p) = NOW.lock().unwrap().take() {
        kill(&mut p);
    }
    let _ = ui.upgrade_in_event_loop(|u| {
        let p = u.global::<Player>();
        p.set_id("".into());
        p.set_playing(false);
        p.set_loading(false);
        p.set_large(false);
        p.set_frame(Image::default());
    });
}

/// After the sound was turned on or off, or its level changed: an ffmpeg
/// that's sending sound is told the new level and plays on (starting it
/// again for every step of the slider stopped the video each time, and
/// asked the server for it again). Otherwise it's stopped, to start with
/// sound. Says whether to start it again now.
fn retune(p: &mut Playing) -> bool {
    let level = if p.muted { 0 } else { crate::ui::volume() };
    if p.sounding && tell_volume(p, level) {
        return false;
    }
    // Silent, and meant to be.
    if p.muted && !p.sounding {
        return false;
    }
    let running = p.playing;
    kill(p);
    running
}

/// Tells the running ffmpeg a new sound level: its "c" key sends a
/// command to a filter, here the volume one.
fn tell_volume(p: &mut Playing, level: u32) -> bool {
    use std::io::Write;
    let Some(stdin) = p.child.as_mut().and_then(|c| c.stdin.as_mut()) else { return false };
    let command = format!("cvolume -1 volume {:.5}\n", crate::ui::gain(level));
    stdin.write_all(command.as_bytes()).and_then(|()| stdin.flush()).is_ok()
}

/// Holds ffmpeg still, or lets it carry on. False when it can't be (it
/// has ended, or this isn't Linux): then it's stopped and started again.
#[cfg(target_os = "linux")]
fn freeze(p: &mut Playing, on: bool) -> bool {
    let Some(c) = p.child.as_mut() else { return false };
    if !matches!(c.try_wait(), Ok(None)) {
        return false;
    }
    let Ok(pid) = libc::pid_t::try_from(c.id()) else { return false };
    // SAFETY: a signal to a child that hasn't been waited for, so the
    // number is still its own.
    let done = unsafe { libc::kill(pid, if on { libc::SIGSTOP } else { libc::SIGCONT }) } == 0;
    if done {
        p.frozen = on;
        p.frozen_at = on.then(std::time::Instant::now);
    }
    done
}

#[cfg(not(target_os = "linux"))]
fn freeze(_p: &mut Playing, _on: bool) -> bool {
    false
}

/// ffmpeg goes when Whisker does, however Whisker ends: one held still
/// by a pause would otherwise stay for good. (The signal comes when the
/// thread that started it ends, and that thread reads from it until it's
/// gone.)
#[cfg(target_os = "linux")]
fn tie_to_whisker(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: only prctl, which is safe to call between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn tie_to_whisker(_cmd: &mut Command) {}

fn pause(ui: &Weak<AppWindow>) {
    if let Some(p) = NOW.lock().unwrap().as_mut() {
        p.playing = false;
        // A video is held where it is: stopping ffmpeg meant opening the
        // video and finding the place again on every play, and a few
        // pauses in a row had the server asked over and over. (Sound alone
        // is timed by the clock, and starts again quickly.)
        if !(p.info.width > 0 && freeze(p, true)) {
            kill(p);
        }
    }
    let _ = ui.upgrade_in_event_loop(|u| u.global::<Player>().set_playing(false));
}

fn kill(p: &mut Playing) {
    p.generation = STARTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    p.frozen = false;
    p.frozen_at = None;
    p.sounding = false;
    p.lead = false;
    p.leads = 0;
    if let Some(mut c) = p.child.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// Frame size: the video's, fitted in `max`, in even numbers.
fn frame_size(info: &VideoInfo, max: (u32, u32)) -> (u32, u32) {
    let (w, h) = (f64::from(info.width.max(2)), f64::from(info.height.max(2)));
    let scale = (f64::from(max.0) / w).min(f64::from(max.1) / h).min(1.0);
    let even = |v: f64| ((v * scale) as u32).max(2) & !1;
    (even(w), even(h))
}

/// How to send the sound: the first of PulseAudio (PipeWire speaks it) or
/// ALSA that takes a moment of silence, found once.
pub(crate) fn sound_output() -> Option<&'static [&'static str]> {
    // Windows: ffmpeg has no way to the speakers; Whisker plays it.
    #[cfg(windows)]
    return crate::win_sound::output_args();
    #[cfg(not(windows))]
    static FOUND: std::sync::OnceLock<Option<&'static [&'static str]>> = std::sync::OnceLock::new();
    #[cfg(not(windows))]
    *FOUND.get_or_init(|| {
        let tries: [&'static [&'static str]; 2] = [&["-f", "pulse", "Whisker"], &["-f", "alsa", "default"]];
        tries.into_iter().find(|out| {
            whisker_core::tool("ffmpeg")
                .args(["-v", "error", "-nostdin", "-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo", "-t", "0.05"])
                .args(*out)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
    })
}

/// `path` written for an ffmpeg filter option: escaped once for the
/// option, then again for the filter graph.
fn filter_path(path: &OsStr) -> String {
    let escape = |s: &str, special: &str| s.chars().fold(String::new(), |mut out, c| {
        if special.contains(c) {
            out.push('\\');
        }
        out.push(c);
        out
    });
    escape(&escape(&path.to_string_lossy(), "\\':"), "\\'[],;")
}

/// The longest wait the pacing makes up for by hurrying, in seconds:
/// after anything longer (a pause) it carries on at the normal speed.
const PACE_LIMIT: &str = "0.5";
const PACE_LIMIT_FOR: std::time::Duration = std::time::Duration::from_millis(500);

/// The sound's head start on the picture, in frames (a seventh of a
/// second): that much silence goes to the speakers first, so there's
/// always some sound waiting there, and the picture holds its first frame
/// as long so the two still match. Sent exactly when it was due, with
/// nothing waiting, the sound ran dry every time it came a moment late or
/// the sound card took a little more than it was given: a stutter every
/// second or so.
const LEAD_FRAMES: u32 = 4;
const LEAD_MS: u32 = LEAD_FRAMES * 1000 / FPS;

/// Whether this ffmpeg can be told to put silence in as it plays (its
/// `adelay` takes commands, from ffmpeg 6): needed to give the sound its
/// head start again after a pause, so without it there's no head start.
fn lead_supported() -> bool {
    static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| {
        whisker_core::tool("ffmpeg")
            .args(["-hide_banner", "-h", "filter=adelay"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .is_ok_and(|o| {
                String::from_utf8_lossy(&o.stdout).lines().any(|l| {
                    let mut words = l.split_whitespace();
                    words.next() == Some("delays") && words.nth(1).is_some_and(|flags| flags.contains('T'))
                })
            })
    })
}

/// After a pause long enough for the sound waiting at the speakers to have
/// played out (and the pacing to start afresh): the same silence again, so
/// the sound has its head start back and the picture catches up with what
/// was heard after it stopped. Written before ffmpeg is let go, so it's
/// the first thing it reads.
fn lead_again(p: &mut Playing) {
    use std::io::Write;
    if !p.lead || p.frozen_at.is_none_or(|t| t.elapsed() < PACE_LIMIT_FOR) {
        return;
    }
    let Some(stdin) = p.child.as_mut().and_then(|c| c.stdin.as_mut()) else { return };
    // The filter counts from what it was last told (nothing, at first).
    p.leads += 1;
    let command = format!("cadelay -1 delays all:{}\n", p.leads * LEAD_MS);
    let _ = stdin.write_all(command.as_bytes()).and_then(|()| stdin.flush());
}

fn size_filter(size: (u32, u32)) -> String {
    format!("fps={FPS},scale={}:{}", size.0, size.1)
}

/// `subtitles`: the kind to draw, and which of the file's subtitle tracks.
fn ffmpeg(path: &OsStr, at: f64, size: (u32, u32), sound: Option<&[&str]>, subtitles: Option<(Subtitles, usize)>, lead: bool) -> std::io::Result<Child> {
    let mut cmd = whisker_core::tool("ffmpeg");
    // The picture's first frame held while the sound's silence plays.
    let hold = if lead { format!("tpad=start={LEAD_FRAMES}:start_mode=clone,") } else { String::new() };
    // Paced by the `realtime` filters, not `-re`: after a pause they carry
    // on at the normal speed, where `-re` raced to make up the time.
    let fit = format!("fps={FPS},scale={}:{},{hold}realtime=limit={PACE_LIMIT}", size.0, size.1);
    cmd.args(["-v", "error", "-ss", &format!("{at:.3}")]);
    // Text subtitles are drawn by time, so the times must stay the file's.
    if subtitles.is_some_and(|s| s.0 == Subtitles::Text) {
        cmd.arg("-copyts");
    }
    cmd.args(whisker_core::timeline::ffmpeg_guard(path));
    cmd.arg("-i").arg(path);
    match subtitles {
        Some((Subtitles::Text, n)) => cmd.args(["-map", "0:v:0", "-vf", &format!("{},subtitles=filename={}:si={n},{hold}realtime=limit={PACE_LIMIT}", size_filter(size), filter_path(path))]),
        Some((Subtitles::Bitmap, n)) => cmd.args(["-filter_complex", &format!("[0:v:0][0:s:{n}]overlay,{fit}[v]"), "-map", "[v]"]),
        None => cmd.args(["-map", "0:v:0", "-vf", &fit]),
    };
    cmd.args(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"]);
    if let Some(out) = sound {
        cmd.args(["-map", "0:a:0"]);
        // The volume filter is always there, to be told a new level.
        let ahead = if lead { format!(",adelay={LEAD_MS}:all=1") } else { String::new() };
        cmd.args(["-filter:a", &format!("volume={:.5},arealtime=limit={PACE_LIMIT}{ahead}", crate::ui::gain(crate::ui::volume()))]);
        cmd.args(out);
    }
    tie_to_whisker(&mut cmd);
    // Its keys are read from a pipe: that's how it's told a new sound level.
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
}

/// Plays from where it is.
fn resume(ui: &Weak<AppWindow>) {
    let (input, at, info, muted, subtitles, large, generation) = {
        let mut now = NOW.lock().unwrap();
        let Some(p) = now.as_mut() else { return };
        // Paused: it carries on from where it was held.
        if p.frozen {
            lead_again(p);
        }
        if p.frozen && freeze(p, false) {
            p.playing = true;
            drop(now);
            let _ = ui.upgrade_in_event_loop(|u| u.global::<Player>().set_playing(true));
            return;
        }
        kill(p);
        p.playing = true;
        // Played to the end: start over.
        if p.info.duration.is_some_and(|d| p.at >= d.as_secs_f64() - 0.2) {
            p.at = 0.0;
        }
        let subtitles = if p.subtitles { p.tracks.get(p.track).map(|t| (t.kind, t.index)) } else { None };
        (p.input.clone(), p.at, p.info, p.muted, subtitles, p.large, p.generation)
    };
    crate::ui::video_starting(ui, move || {
        NOW.lock().unwrap().as_ref().is_some_and(|p| p.generation == generation) && SHOWN.load(std::sync::atomic::Ordering::Relaxed) != generation
    });
    let ui = ui.clone();
    let max = if large { MAX_LARGE_PX } else { MAX_PX };
    std::thread::spawn(move || run(ui, input, at, info, muted, subtitles, max, generation));
}

/// Starts ffmpeg and shows its frames until it ends or is replaced.
#[allow(clippy::too_many_arguments)]
fn run(ui: Weak<AppWindow>, input: OsString, at: f64, info: VideoInfo, muted: bool, subtitles: Option<(Subtitles, usize)>, max: (u32, u32), generation: u64) {
    if info.width == 0 {
        let played = play_sound(&ui, &input, at, muted, generation);
        return ended(&ui, generation, played);
    }
    let size = frame_size(&info, max);
    let sound = if info.audio && !muted { sound_output() } else { None };
    let current = || NOW.lock().unwrap().as_ref().is_some_and(|p| p.generation == generation);
    if !current() {
        return;
    }
    let lead = sound.is_some() && lead_supported();
    let started = ffmpeg(&input, at, size, sound, subtitles, lead).ok().and_then(|mut child| {
        let out = child.stdout.take()?;
        let mut now = NOW.lock().unwrap();
        match now.as_mut() {
            Some(p) if p.generation == generation => {
                p.child = Some(child);
                p.sounding = sound.is_some();
                p.lead = lead;
                Some(out)
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                None
            }
        }
    });
    let shown = match started {
        Some(mut out) => show_frames(&ui, &mut out, size, at, if lead { u64::from(LEAD_FRAMES) } else { 0 }, generation),
        None => 0,
    };
    ended(&ui, generation, shown);
}

/// After ffmpeg stopped: played out (not replaced), it's back to the
/// preview, ready to play again (or starts over, when looping).
fn ended(ui: &Weak<AppWindow>, generation: u64, shown: u64) {
    let ui = ui.clone();
    let finished = {
        let mut now = NOW.lock().unwrap();
        match now.as_mut() {
            Some(p) if p.generation == generation => {
                if let Some(mut c) = p.child.take() {
                    let _ = c.wait();
                }
                p.at = 0.0;
                p.playing = false;
                true
            }
            _ => false,
        }
    };
    // Looping: from the top again (not when nothing could be shown).
    if finished && shown > 0 && LOOP.load(std::sync::atomic::Ordering::Relaxed) {
        return resume(&ui);
    }
    if finished {
        let _ = ui.upgrade_in_event_loop(|u| {
            let p = u.global::<Player>();
            p.set_playing(false);
            p.set_loading(false);
            p.set_position(0.0);
            p.set_frame(Image::default());
        });
    }
}

/// Sound alone: ffmpeg plays it to the system, and the position moves
/// along with the clock (`muted`: silently). Returns how many tenths of a
/// second it played.
fn play_sound(ui: &Weak<AppWindow>, input: &OsStr, at: f64, muted: bool, generation: u64) -> u64 {
    let Some(out) = sound_output() else { return 0 };
    let mut cmd = whisker_core::tool("ffmpeg");
    cmd.args(["-v", "error", "-ss", &format!("{at:.3}")]);
    cmd.args(whisker_core::timeline::ffmpeg_guard(input));
    cmd.args(["-re", "-i"]).arg(input).args(["-map", "0:a:0"]);
    let level = if muted { 0 } else { crate::ui::volume() };
    // The same head start as a video's sound (see [`LEAD_FRAMES`]).
    let lead = if lead_supported() { f64::from(LEAD_MS) / 1000.0 } else { 0.0 };
    let ahead = if lead > 0.0 { format!(",adelay={LEAD_MS}:all=1") } else { String::new() };
    cmd.args(["-filter:a", &format!("volume={:.5}{ahead}", crate::ui::gain(level))]);
    cmd.args(out);
    tie_to_whisker(&mut cmd);
    let Ok(child) = cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else { return 0 };
    {
        let mut now = NOW.lock().unwrap();
        match now.as_mut() {
            Some(p) if p.generation == generation => {
                p.child = Some(child);
                p.sounding = true;
            }
            _ => {
                let mut child = child;
                let _ = child.kill();
                let _ = child.wait();
                return 0;
            }
        }
    }
    let began = std::time::Instant::now();
    let mut ticks = 0u64;
    loop {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let position = at + (began.elapsed().as_secs_f64() - lead).max(0.0);
        {
            let mut now = NOW.lock().unwrap();
            match now.as_mut() {
                Some(p) if p.generation == generation => {
                    if p.child.as_mut().is_none_or(|c| !matches!(c.try_wait(), Ok(None))) {
                        return ticks;
                    }
                    p.at = position;
                }
                _ => return ticks,
            }
        }
        ticks += 1;
        let first = ticks == 1;
        if first {
            SHOWN.store(generation, std::sync::atomic::Ordering::Relaxed);
        }
        let _ = ui.upgrade_in_event_loop(move |u| {
            let p = u.global::<Player>();
            p.set_position(position as f32);
            if first {
                p.set_loading(false);
            }
        });
    }
}

/// Reads frames from ffmpeg into the window. Returns how many it showed.
/// `held`: how many times the first frame comes before the video moves on
/// (see [`LEAD_FRAMES`]).
fn show_frames(ui: &Weak<AppWindow>, out: &mut impl Read, size: (u32, u32), at: f64, held: u64, generation: u64) -> u64 {
    let bytes = (size.0 * size.1 * 4) as usize;
    let mut shown = 0u64;
    loop {
        let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(size.0, size.1);
        if out.read_exact(buf.make_mut_bytes()).is_err() || buf.as_bytes().len() != bytes {
            return shown;
        }
        let position = at + shown.saturating_sub(held) as f64 / f64::from(FPS);
        {
            let mut now = NOW.lock().unwrap();
            match now.as_mut() {
                Some(p) if p.generation == generation => p.at = position,
                _ => return shown,
            }
        }
        shown += 1;
        let first = shown == 1;
        if first {
            SHOWN.store(generation, std::sync::atomic::Ordering::Relaxed);
        }
        let _ = ui.upgrade_in_event_loop(move |u| {
            let p = u.global::<Player>();
            p.set_frame(Image::from_rgba8(buf));
            p.set_position(position as f32);
            if first {
                p.set_loading(false);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_fit_and_stay_even() {
        let info = |w, h| VideoInfo { width: w, height: h, duration: None, audio: false, subtitles: None };
        assert_eq!(frame_size(&info(1920, 1080), MAX_PX), (960, 540));
        assert_eq!(frame_size(&info(641, 361), MAX_PX), (640, 360));
        assert_eq!(frame_size(&info(1080, 1920), MAX_PX), (404, 720));
        assert_eq!(frame_size(&info(1920, 1080), MAX_LARGE_PX), (1600, 900));
    }

    #[test]
    fn escapes_paths_for_filters() {
        assert_eq!(filter_path(OsStr::new("/tmp/a.mkv")), "/tmp/a.mkv");
        assert_eq!(filter_path(OsStr::new("/x/it's: [a],b.mkv")), "/x/it\\\\\\'s\\\\: \\[a\\]\\,b.mkv");
    }
}
