//! What ffmpeg does for files being sent, done with the phone's own media
//! tools (phones have no ffmpeg): a still from a video, so it shows as a
//! picture in the chat rather than a file name, and a copy of a video or
//! sound without its metadata, written track by track by Android's muxer
//! (nothing re-encoded). Set up in [`install`].

use super::*;
use std::path::Path;
use std::io::Read;

/// Widest and tallest still made.
const POSTER_PX: (u32, u32) = (640, 640);
/// How long the decoder gets to hand over a still.
const POSTER_GIVE_UP: Duration = Duration::from_secs(5);

/// Hands these to the upload code, in place of ffmpeg.
pub fn install() {
    whisker_core::upload::set_media_tools(whisker_core::upload::MediaTools { poster, strip });
}

/// A still from early in the video at `path` (a second in, skipping fade-ins;
/// short clips their first frame), as a JPEG.
fn poster(path: &std::ffi::OsStr) -> Option<whisker_core::timeline::Poster> {
    let path = Path::new(path);
    let info = probe(&Src::File(path.to_owned()))?;
    info.video?;
    let size = frame_size(info, POSTER_PX);
    let last = (info.duration - 0.1).max(0.0);
    let rgba = [1.0f64.min(last), 0.0].into_iter().find_map(|at| still(path, info, at, size))?;
    let rgb = image::DynamicImage::ImageRgba8(image::RgbaImage::from_raw(size.0, size.1, rgba)?).to_rgb8();
    let mut jpeg = Vec::new();
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80);
    image::DynamicImage::ImageRgb8(rgb).write_with_encoder(enc).ok()?;
    let video_size = if info.rotation == 90 || info.rotation == 270 { (info.height, info.width) } else { (info.width, info.height) };
    Some(whisker_core::timeline::Poster {
        jpeg,
        size,
        video_size,
        duration: (info.duration > 0.0).then(|| Duration::from_secs_f64(info.duration)),
    })
}

/// The first picture at or after `at` seconds, as RGBA at `size`. Tries
/// Android's software decoders first (they write into memory, which is
/// what's read here), then the phone's own.
fn still(path: &Path, info: Info, at: f64, size: (u32, u32)) -> Option<Vec<u8>> {
    let ex = Extractor::open_file(path)?;
    let track = info.video?;
    let mut format = ex.format(track)?;
    let kind = mime(&mut format);
    ex.select(track);
    let software = software_decoders(&kind).iter().filter_map(|n| MediaCodec::from_codec_name(n));
    software.chain(MediaCodec::from_decoder_type(&kind)).find_map(|codec| decode_still(&codec, &ex, &mut format, info, at, size))
}

fn decode_still(codec: &MediaCodec, ex: &Extractor, format: &mut MediaFormat, info: Info, at: f64, size: (u32, u32)) -> Option<Vec<u8>> {
    codec.configure(format, None, MediaCodecDirection::Decoder).ok()?;
    codec.start().ok()?;
    let at_us = (at * 1e6) as i64;
    ex.seek(at_us);
    let mut layout = Layout::of(format, info.width, info.height);
    let mut buf = Vec::new();
    let mut more = true;
    let began = Instant::now();
    let mut found = None;
    while found.is_none() && began.elapsed() < POSTER_GIVE_UP {
        if more {
            more = feed(codec, ex, &mut buf);
        }
        match codec.dequeue_output_buffer(Duration::from_millis(10)) {
            Ok(DequeuedOutputBufferInfoResult::Buffer(out)) => {
                let end = out.info().flags() & END_OF_STREAM != 0;
                let (offset, len) = (out.info().offset().max(0) as usize, out.info().size().max(0) as usize);
                // Frames before `at` (from the key frame before it) are only decoded.
                if len > 0 && (out.info().presentation_time_us() >= at_us || end) {
                    let data = out.buffer();
                    let data = &data[offset.min(data.len())..(offset + len).min(data.len())];
                    found = Some(convert(&layout.planes(data), layout.crop, info.rotation, size).as_bytes().to_vec());
                }
                let _ = codec.release_output_buffer(out, false);
                if end {
                    break;
                }
            }
            Ok(DequeuedOutputBufferInfoResult::OutputFormatChanged) => layout = Layout::of(&mut codec.output_format(), info.width, info.height),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = codec.stop();
    found
}

/// `data` (a video, or sound when `!video`, called `name`) without its
/// metadata: only its picture and sound tracks are copied into a new file,
/// so the place it was recorded, the phone, the dates and the like stay
/// behind (along with data tracks, where phones keep location and sensor
/// readings). The picture's turn is kept. `None` for formats Android's
/// muxer can't write (it writes MP4, 3GP and WebM) or when it fails.
fn strip(data: &[u8], name: &str, video: bool) -> Option<Vec<u8>> {
    let ext = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    let format = match ext.as_str() {
        "mp4" | "m4v" | "mov" | "m4a" => ffi::OutputFormat::AMEDIAMUXER_OUTPUT_FORMAT_MPEG_4,
        "3gp" | "3gpp" => ffi::OutputFormat::AMEDIAMUXER_OUTPUT_FORMAT_THREE_GPP,
        "webm" => ffi::OutputFormat::AMEDIAMUXER_OUTPUT_FORMAT_WEBM,
        _ => return None,
    };
    // Private, and wiped when done.
    let dir = whisker_core::stores::scratch()?;
    let (src, dst) = (dir.path().join(format!("in.{ext}")), dir.path().join(format!("out.{ext}")));
    std::fs::write(&src, data).ok()?;
    let ok = remux(&src, &dst, format, video);
    let mut out = Vec::new();
    let read = ok && std::fs::File::open(&dst).and_then(|mut f| f.read_to_end(&mut out)).is_ok();
    drop(dir);
    // Android's muxer adds its own: the Android version (sometimes the
    // make and model) and the time it wrote the file. Wiped, sizes kept.
    if format != ffi::OutputFormat::AMEDIAMUXER_OUTPUT_FORMAT_WEBM {
        crate::voice::scrub_mp4(&mut out);
    }
    (read && !out.is_empty()).then_some(out)
}

/// Copies `src`'s picture (when `video`) and sound tracks into `dst`.
fn remux(src: &Path, dst: &Path, format: ffi::OutputFormat, video: bool) -> bool {
    let Some(ex) = Extractor::open_file(src) else { return false };
    let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(dst) else { return false };
    // SAFETY: plain calls into libmediandk; the file stays open while the muxer uses it.
    let muxer = unsafe { ffi::AMediaMuxer_new(file.as_raw_fd(), format) };
    if muxer.is_null() {
        return false;
    }
    let mut tracks = std::collections::HashMap::new();
    let (mut rotation, mut biggest, mut has_video) = (0, 1 << 20, false);
    for t in 0..ex.tracks() {
        let Some(mut f) = ex.format(t) else { continue };
        let m = mime(&mut f);
        let wanted = m.starts_with("audio/") || (video && m.starts_with("video/"));
        if !wanted {
            continue;
        }
        if m.starts_with("video/") {
            rotation = f.i32("rotation-degrees").unwrap_or(0).rem_euclid(360);
            has_video = true;
        }
        biggest = biggest.max(f.i32("max-input-size").unwrap_or(0).max(0) as usize);
        let i = unsafe { ffi::AMediaMuxer_addTrack(muxer, f.as_ptr()) };
        if i < 0 {
            unsafe { ffi::AMediaMuxer_delete(muxer) };
            return false;
        }
        ex.select(t);
        tracks.insert(t as i32, i as usize);
    }
    // A video that lost its picture isn't the video any more.
    if tracks.is_empty() || (video && !has_video) {
        unsafe { ffi::AMediaMuxer_delete(muxer) };
        return false;
    }
    if rotation != 0 {
        unsafe { ffi::AMediaMuxer_setOrientationHint(muxer, rotation) };
    }
    if unsafe { ffi::AMediaMuxer_start(muxer) }.0 != 0 {
        unsafe { ffi::AMediaMuxer_delete(muxer) };
        return false;
    }
    // Room for the biggest piece (a high-bitrate key frame can be large).
    let mut buf = vec![0u8; biggest.max(8 << 20)];
    let mut ok = true;
    loop {
        let track = unsafe { ffi::AMediaExtractor_getSampleTrackIndex(ex.ptr) };
        if track < 0 {
            break;
        }
        let n = unsafe { ffi::AMediaExtractor_readSampleData(ex.ptr, buf.as_mut_ptr(), buf.len()) };
        if n < 0 {
            ok = false;
            break;
        }
        if let Some(&out) = tracks.get(&track) {
            let info = ffi::AMediaCodecBufferInfo {
                offset: 0,
                size: n as i32,
                presentationTimeUs: unsafe { ffi::AMediaExtractor_getSampleTime(ex.ptr) }.max(0),
                // A key frame (the extractor's "sync" flag) is the muxer's key frame flag too.
                flags: unsafe { ffi::AMediaExtractor_getSampleFlags(ex.ptr) } & 1,
            };
            if unsafe { ffi::AMediaMuxer_writeSampleData(muxer, out, buf.as_ptr(), &info) }.0 != 0 {
                ok = false;
                break;
            }
        }
        if !unsafe { ffi::AMediaExtractor_advance(ex.ptr) } {
            break;
        }
    }
    let stopped = unsafe { ffi::AMediaMuxer_stop(muxer) }.0 == 0;
    unsafe { ffi::AMediaMuxer_delete(muxer) };
    drop(file);
    ok && stopped
}
