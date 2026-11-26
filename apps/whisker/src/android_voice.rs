//! Voice messages on phones: the microphone through AAudio, and the
//! recording encoded as AAC in an MP4 (`.m4a`) by Android's own encoder
//! and muxer (no extra libraries). Element and Element X play these.

use super::*;
use std::path::Path;
use crate::voice::RATE;

/// Records until `stop`, handing samples (mono, [`RATE`]) to `push`. Says on
/// `ready` whether the microphone opened.
pub fn record(stop: &AtomicBool, ready: std::sync::mpsc::Sender<Result<(), String>>, push: fn(&[i16]), max: Duration) {
    let stream = AudioStreamBuilder::new().and_then(|b| {
        b.direction(AudioDirection::Input)
            .format(AudioFormat::PCM_I16)
            .channel_count(1)
            .sample_rate(RATE as i32)
            .open_stream()
    });
    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            let _ = ready.send(Err(format!("Couldn't open the microphone: {e:?}")));
            return;
        }
    };
    if let Err(e) = stream.request_start() {
        let _ = ready.send(Err(format!("The microphone wouldn't start: {e:?}")));
        return;
    }
    // Asked for RATE, but a phone may give its own.
    let rate = stream.sample_rate().max(8000) as u32;
    let _ = ready.send(Ok(()));
    let mut buf = vec![0i16; 960];
    let started = Instant::now();
    while !stop.load(Ordering::SeqCst) && started.elapsed() < max {
        // SAFETY: `buf` holds `buf.len()` samples, one channel.
        let n = frames_done(unsafe { stream.read(buf.as_mut_ptr().cast(), buf.len() as i32, 100_000_000) });
        match n {
            Ok(0) => {}
            Ok(n) => {
                let got = &buf[..(n as usize).min(buf.len())];
                if rate == RATE {
                    push(got);
                } else {
                    push(&resample(got, rate));
                }
            }
            Err(_) => break,
        }
    }
    let _ = stream.request_stop();
}

/// Plays a call's sound until `stop`: `take` fills each piece (one
/// channel, [`RATE`]), and the phone's sound output sets the pace.
pub fn play(stop: &AtomicBool, take: &dyn Fn(&mut [i16])) {
    let stream = AudioStreamBuilder::new().and_then(|b| {
        b.direction(AudioDirection::Output)
            .format(AudioFormat::PCM_I16)
            .channel_count(1)
            .sample_rate(RATE as i32)
            .open_stream()
    });
    let Ok(stream) = stream else { return };
    if stream.request_start().is_err() {
        return;
    }
    let mut piece = vec![0i16; 960];
    while !stop.load(Ordering::SeqCst) {
        take(&mut piece);
        let mut done = 0usize;
        while done < piece.len() && !stop.load(Ordering::SeqCst) {
            // SAFETY: `piece[done..]` holds that many samples, one channel.
            match frames_done(unsafe { stream.write(piece[done..].as_ptr().cast(), (piece.len() - done) as i32, 100_000_000) }) {
                Ok(n) => done += n as usize,
                Err(_) => return,
            }
        }
    }
    let _ = stream.request_stop();
}

/// `samples` at `from` Hz as [`RATE`] Hz (straight-line in between).
fn resample(samples: &[i16], from: u32) -> Vec<i16> {
    let n = (samples.len() as u64 * u64::from(RATE) / u64::from(from)) as usize;
    (0..n)
        .map(|i| {
            let pos = i as f64 * f64::from(from) / f64::from(RATE);
            let (a, frac) = (pos as usize, pos.fract());
            let x = f64::from(samples[a.min(samples.len() - 1)]);
            let y = f64::from(samples[(a + 1).min(samples.len() - 1)]);
            (x + (y - x) * frac) as i16
        })
        .collect()
}

/// The recording as AAC in an MP4 file: (bytes, type, extension).
pub fn encode(samples: &[i16]) -> Option<(Vec<u8>, &'static str, &'static str)> {
    // Private, and wiped when done.
    let dir = whisker_core::stores::scratch()?;
    let path = dir.path().join("voice.m4a");
    let ok = encode_to(samples, &path);
    let data = if ok { std::fs::read(&path).ok().filter(|d| !d.is_empty()) } else { None };
    drop(dir);
    let mut data = data?;
    // Nothing but the sound leaves the phone: no time made, no Android version.
    crate::voice::scrub_mp4(&mut data);
    Some((data, "audio/mp4", "m4a"))
}

fn encode_to(samples: &[i16], path: &Path) -> bool {
    const CODEC_CONFIG: u32 = 2;
    let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path) else { return false };
    let Some(codec) = MediaCodec::from_encoder_type("audio/mp4a-latm") else { return false };
    let mut format = MediaFormat::new();
    format.set_str("mime", "audio/mp4a-latm");
    format.set_i32("sample-rate", RATE as i32);
    format.set_i32("channel-count", 1);
    format.set_i32("bitrate", 64_000);
    // AAC LC: what everything plays.
    format.set_i32("aac-profile", 2);
    format.set_i32("max-input-size", 16 * 1024);
    if codec.configure(&format, None, MediaCodecDirection::Encoder).is_err() || codec.start().is_err() {
        return false;
    }
    // SAFETY: plain calls into libmediandk; the file stays open while the muxer uses it.
    let muxer = unsafe { ffi::AMediaMuxer_new(file.as_raw_fd(), ffi::OutputFormat::AMEDIAMUXER_OUTPUT_FORMAT_MPEG_4) };
    if muxer.is_null() {
        let _ = codec.stop();
        return false;
    }
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let (mut fed, mut all_in, mut track, mut ok) = (0usize, false, None, true);
    let began = Instant::now();
    loop {
        if began.elapsed() > Duration::from_secs(120) {
            ok = false;
            break;
        }
        if !all_in && let Ok(DequeuedInputBufferResult::Buffer(mut input)) = codec.dequeue_input_buffer(Duration::from_millis(5)) {
            let space = input.buffer_mut();
            let n = space.len().min(bytes.len() - fed) & !1;
            for (d, s) in space.iter_mut().zip(&bytes[fed..fed + n]) {
                d.write(*s);
            }
            let us = (fed / 2) as u64 * 1_000_000 / u64::from(RATE);
            let last = fed + n >= bytes.len();
            let _ = codec.queue_input_buffer(input, 0, n, us, if last { END_OF_STREAM } else { 0 });
            fed += n;
            all_in = last;
        }
        match codec.dequeue_output_buffer(Duration::from_millis(5)) {
            Ok(DequeuedOutputBufferInfoResult::OutputFormatChanged) => {
                let out_format = codec.output_format();
                let i = unsafe { ffi::AMediaMuxer_addTrack(muxer, out_format.as_ptr()) };
                if i < 0 || unsafe { ffi::AMediaMuxer_start(muxer) }.0 != 0 {
                    ok = false;
                    break;
                }
                track = Some(i as usize);
            }
            Ok(DequeuedOutputBufferInfoResult::Buffer(out)) => {
                let info = out.info();
                let (flags, size, offset) = (info.flags(), info.size().max(0) as usize, info.offset().max(0) as usize);
                let end = flags & END_OF_STREAM != 0;
                // The codec's setup bytes go in the track's format, not as a piece.
                if let Some(t) = track
                    && flags & CODEC_CONFIG == 0
                    && size > 0
                {
                    let data = out.buffer();
                    if offset + size <= data.len() {
                        let piece = ffi::AMediaCodecBufferInfo { offset: 0, size: size as i32, presentationTimeUs: info.presentation_time_us(), flags: flags & 1 };
                        if unsafe { ffi::AMediaMuxer_writeSampleData(muxer, t, data[offset..].as_ptr(), &piece) }.0 != 0 {
                            ok = false;
                        }
                    }
                }
                let _ = codec.release_output_buffer(out, false);
                if end || !ok {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => {
                ok = false;
                break;
            }
        }
    }
    let _ = codec.stop();
    let stopped = track.is_some() && unsafe { ffi::AMediaMuxer_stop(muxer) }.0 == 0;
    unsafe { ffi::AMediaMuxer_delete(muxer) };
    drop(file);
    ok && stopped
}
