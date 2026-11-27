//! Voice messages: recording from the microphone, then sending what was
//! recorded the way Element sends them (an audio message marked as voice,
//! with its length and a waveform). Phones record with Android's own
//! microphone and AAC encoder (see `android_voice.rs`); Linux records and
//! encodes (Ogg Opus) with ffmpeg. Elsewhere it isn't offered.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Sample rate recorded at (mono).
pub const RATE: u32 = 48_000;
/// Longest recording: it stops by itself then.
pub const MAX_LENGTH: Duration = Duration::from_secs(10 * 60);
/// Waveform points sent along.
const WAVEFORM_POINTS: usize = 100;

/// A finished recording, ready to send.
pub struct Recording {
    pub data: Vec<u8>,
    pub mime: &'static str,
    pub ext: &'static str,
    pub duration: Duration,
    /// Loudness over its length, 0–1024 (as Matrix voice messages carry it).
    pub waveform: Vec<u16>,
}

/// What's being recorded now: samples so far, and when it started.
struct Live {
    samples: Vec<i16>,
    started: Instant,
}

static LIVE: Mutex<Option<Live>> = Mutex::new(None);
/// Set to stop the recording thread.
static STOP: AtomicBool = AtomicBool::new(false);
static THREAD: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

/// Whether voice clips can be recorded here.
pub fn available() -> bool {
    if cfg!(target_os = "android") {
        true
    } else if cfg!(target_os = "linux") {
        whisker_core::timeline::ffmpeg_available()
    } else {
        false
    }
}

/// Starts recording. `Err` says why it can't (no microphone permission yet,
/// say: then Android has just been asked, and another tap starts it).
pub fn start() -> Result<(), String> {
    cancel();
    #[cfg(target_os = "android")]
    if !crate::droid::has_mic() {
        crate::droid::request_mic();
        return Err("Allow Whisker to use the microphone, then tap the microphone again.".into());
    }
    STOP.store(false, Ordering::SeqCst);
    *LIVE.lock().unwrap() = Some(Live { samples: Vec::new(), started: Instant::now() });
    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let handle = std::thread::spawn(move || capture(&STOP, tx, push, MAX_LENGTH));
    *THREAD.lock().unwrap() = Some(handle);
    // Whether the microphone opened.
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            cancel();
            Err(e)
        }
        Err(_) => {
            cancel();
            Err("The microphone didn't start.".into())
        }
    }
}

/// Listens to the microphone until `stop` (or for `max`), handing what it
/// hears (one channel, [`RATE`]) to `push`, and says on `ready` whether the
/// microphone opened. A call listens this way too.
pub fn capture(stop: &AtomicBool, ready: std::sync::mpsc::Sender<Result<(), String>>, push: fn(&[i16]), max: Duration) {
    #[cfg(target_os = "android")]
    crate::player::voice::record(stop, ready, push, max);
    #[cfg(all(target_os = "linux", not(target_os = "android")))]
    desktop::record(stop, ready, push, max);
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _ = (stop, push, max);
        let _ = ready.send(Err("The microphone isn't available here.".into()));
    }
}

/// Adds recorded samples (called by the recording thread).
fn push(samples: &[i16]) {
    if let Some(live) = LIVE.lock().unwrap().as_mut() {
        live.samples.extend_from_slice(samples);
    }
}

/// Whether a recording is going on.
pub fn recording() -> bool {
    LIVE.lock().unwrap().is_some()
}

/// How long it's been recording, and how loud it's been lately (0–1, the
/// last `bars` tenths of a second, oldest first) for the bars shown.
pub fn progress(bars: usize) -> Option<(Duration, Vec<f32>)> {
    let live = LIVE.lock().unwrap();
    let live = live.as_ref()?;
    let step = (RATE / 10) as usize;
    let tail = &live.samples[live.samples.len().saturating_sub(step * bars)..];
    let mut levels: Vec<f32> = tail.chunks(step).map(|c| loudness(c) as f32).collect();
    while levels.len() < bars {
        levels.insert(0, 0.0);
    }
    Some((live.started.elapsed(), levels))
}

/// Stops and throws away the recording.
pub fn cancel() {
    STOP.store(true, Ordering::SeqCst);
    if let Some(t) = THREAD.lock().unwrap().take() {
        let _ = t.join();
    }
    LIVE.lock().unwrap().take();
}

/// Stops and encodes the recording for sending. `None` for nothing (or a
/// moment) recorded, or when encoding failed.
pub fn finish() -> Result<Recording, String> {
    STOP.store(true, Ordering::SeqCst);
    if let Some(t) = THREAD.lock().unwrap().take() {
        let _ = t.join();
    }
    let Some(live) = LIVE.lock().unwrap().take() else { return Err("Nothing was recorded.".into()) };
    let samples = live.samples;
    if samples.len() < (RATE / 2) as usize {
        return Err("That was too short to send.".into());
    }
    let duration = Duration::from_secs_f64(samples.len() as f64 / f64::from(RATE));
    let waveform = waveform(&samples);
    #[cfg(target_os = "android")]
    let encoded = crate::player::voice::encode(&samples);
    #[cfg(all(target_os = "linux", not(target_os = "android")))]
    let encoded = desktop::encode(&samples);
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let encoded: Option<(Vec<u8>, &'static str, &'static str)> = None;
    let (data, mime, ext) = encoded.ok_or("The recording couldn't be encoded.")?;
    Ok(Recording { data, mime, ext, duration, waveform })
}

/// How loud a stretch is, 0–1 (its RMS, made to look right: speech is
/// mostly far below full scale).
fn loudness(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    let rms = (sum / samples.len() as f64).sqrt() / 32768.0;
    (rms * 4.0).sqrt().min(1.0)
}

/// [`WAVEFORM_POINTS`] loudness values over the whole recording, scaled so
/// the loudest is 1024.
pub fn waveform(samples: &[i16]) -> Vec<u16> {
    let per = samples.len().div_ceil(WAVEFORM_POINTS).max(1);
    let raw: Vec<f64> = samples.chunks(per).map(loudness).collect();
    let top = raw.iter().copied().fold(0.0, f64::max).max(1e-6);
    raw.iter().map(|v| ((v / top) * 1024.0).round().clamp(0.0, 1024.0) as u16).collect()
}

/// Linux: ffmpeg records from PulseAudio (else ALSA) as raw samples, and
/// encodes them as Ogg Opus, the format Element sends.
#[cfg(all(target_os = "linux", not(target_os = "android")))]
mod desktop {
    use super::RATE;
    use std::io::{Read, Write};
    use std::process::Stdio;
    use std::sync::atomic::{AtomicBool, Ordering};

    pub fn record(stop: &AtomicBool, ready: std::sync::mpsc::Sender<Result<(), String>>, push: fn(&[i16]), max: std::time::Duration) {
        let rate = RATE.to_string();
        let mut child = None;
        let mut why = String::new();
        // The sound server, then ALSA's default, then each sound card that
        // can record, straight (a custom ALSA default can fail to open).
        let mut inputs = vec![("pulse".to_owned(), "default".to_owned()), ("alsa".to_owned(), "default".to_owned())];
        inputs.extend(capture_cards().into_iter().map(|d| ("alsa".to_owned(), d)));
        for (format, device) in inputs {
            let Ok(mut c) = whisker_core::tool("ffmpeg")
                .args(["-v", "error", "-nostdin", "-f", &format, "-i", &device])
                .args(["-ac", "1", "-ar", &rate, "-f", "s16le", "pipe:1"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            else {
                why = "ffmpeg couldn't be started".into();
                continue;
            };
            // One that can't open the microphone stops at once.
            std::thread::sleep(std::time::Duration::from_millis(300));
            if matches!(c.try_wait(), Ok(None)) {
                // Its complaints from here on aren't read: don't let them fill a pipe.
                drop(c.stderr.take());
                child = Some(c);
                break;
            }
            let mut err = String::new();
            if let Some(mut e) = c.stderr.take() {
                let _ = e.read_to_string(&mut err);
            }
            let _ = c.wait();
            if let Some(line) = err.lines().rev().find(|l| !l.trim().is_empty()) {
                why = format!("{format} {device}: {}", line.trim());
            }
        }
        let Some(mut child) = child else {
            let _ = ready.send(Err(format!("Couldn't open the microphone (through PulseAudio or ALSA). {why}")));
            return;
        };
        let _ = ready.send(Ok(()));
        let mut out = child.stdout.take().expect("piped");
        let mut buf = vec![0u8; (RATE / 20) as usize * 2];
        let started = std::time::Instant::now();
        while !stop.load(Ordering::SeqCst) && started.elapsed() < max {
            let Ok(n) = out.read(&mut buf) else { break };
            if n == 0 {
                break;
            }
            let samples: Vec<i16> = buf[..n & !1].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
            push(&samples);
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    /// ALSA names for each sound card part that can record ("plughw:2,0").
    fn capture_cards() -> Vec<String> {
        let list = std::fs::read_to_string("/proc/asound/pcm").unwrap_or_default();
        list.lines()
            .filter(|l| l.contains("capture"))
            .filter_map(|l| {
                let (card, dev) = l.split(':').next()?.split_once('-')?;
                Some(format!("plughw:{},{}", card.trim().parse::<u32>().ok()?, dev.trim().parse::<u32>().ok()?))
            })
            .collect()
    }

    pub fn encode(samples: &[i16]) -> Option<(Vec<u8>, &'static str, &'static str)> {
        let rate = RATE.to_string();
        let mut child = whisker_core::tool("ffmpeg")
            .args(["-v", "error", "-nostdin", "-f", "s16le", "-ac", "1", "-ar", &rate, "-i", "pipe:0"])
            .args(["-c:a", "libopus", "-b:a", "32k", "-application", "voip", "-map_metadata", "-1", "-fflags", "+bitexact", "-f", "ogg", "pipe:1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut stdin = child.stdin.take()?;
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        // Written from another thread, so a full output pipe can't stall it.
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
        let out = child.wait_with_output().ok()?;
        let _ = writer.join();
        (out.status.success() && !out.stdout.is_empty()).then_some((out.stdout, "audio/ogg", "ogg"))
    }
}

/// Strips what an MP4 recording says beyond the sound itself. The times it
/// was made and changed are zeroed (movie, track and media headers), and any
/// user data or metadata (`udta`, `meta`: Android's muxer puts the Android
/// version there, and can put the phone's make and model) is wiped and
/// turned into padding (`free`), so every size and offset in the file stays
/// as it was and players read it the same.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn scrub_mp4(data: &mut [u8]) {
    scrub_boxes(data, 0);
}

fn scrub_boxes(data: &mut [u8], depth: u32) {
    let mut at = 0usize;
    while data.len() - at >= 8 {
        let left = (data.len() - at) as u64;
        let (head, size) = match u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) {
            0 => (8, left),
            1 if left >= 16 => (16, u64::from_be_bytes(data[at + 8..at + 16].try_into().unwrap())),
            1 => return,
            n => (8, u64::from(n)),
        };
        if size < head || size > left {
            return;
        }
        let end = at + size as usize;
        let kind: [u8; 4] = data[at + 4..at + 8].try_into().unwrap();
        let body = &mut data[at + head as usize..end];
        match &kind {
            b"moov" | b"trak" | b"mdia" | b"minf" if depth < 8 => scrub_boxes(body, depth + 1),
            b"udta" | b"meta" => {
                body.fill(0);
                data[at + 4..at + 8].copy_from_slice(b"free");
            }
            // Version, flags, then the two times: 32 bits each, or 64 in version 1.
            b"mvhd" | b"tkhd" | b"mdhd" if !body.is_empty() => {
                let times = if body[0] == 1 { 4..20 } else { 4..12 };
                if body.len() >= times.end {
                    body[times].fill(0);
                }
            }
            _ => {}
        }
        at = end;
    }
}

#[cfg(test)]
mod tests {
    fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(body);
        b
    }

    #[test]
    fn mp4_times_and_metadata_are_wiped_and_the_sound_kept() {
        let mut mvhd = vec![0, 0, 0, 0];
        mvhd.extend_from_slice(&[0xAA; 8]);
        mvhd.extend_from_slice(&[0x11; 20]);
        let mut tkhd = vec![1, 0, 0, 7];
        tkhd.extend_from_slice(&[0xBB; 16]);
        tkhd.extend_from_slice(&[0x22; 20]);
        let mut mdhd = vec![0, 0, 0, 0];
        mdhd.extend_from_slice(&[0xCC; 8]);
        mdhd.extend_from_slice(&[0x33; 8]);
        let udta = mp4_box(b"udta", b"com.android.version 14");
        let meta = mp4_box(b"meta", b"\0\0\0\0Pixel");
        let mdia = mp4_box(b"mdia", &mp4_box(b"mdhd", &mdhd));
        let trak = mp4_box(b"trak", &[mp4_box(b"tkhd", &tkhd), mdia, udta.clone()].concat());
        let moov = mp4_box(b"moov", &[mp4_box(b"mvhd", &mvhd), trak, udta, meta].concat());
        let sound = mp4_box(b"mdat", &[0x5A; 64]);
        let mut file = [mp4_box(b"ftyp", b"M4A \0\0\0\0"), moov, sound.clone()].concat();
        let len = file.len();
        super::scrub_mp4(&mut file);
        assert_eq!(file.len(), len);
        let has = |f: &[u8], n: &[u8]| f.windows(n.len()).any(|w| w == n);
        assert!(!has(&file, &[0xAA; 8]) && !has(&file, &[0xBB; 16]) && !has(&file, &[0xCC; 8]));
        assert!(has(&file, &[0x11; 20]) && has(&file, &[0x22; 20]) && has(&file, &[0x33; 8]));
        assert!(!has(&file, b"udta") && !has(&file, b"meta") && !has(&file, b"android") && !has(&file, b"Pixel"));
        assert!(file.ends_with(&sound));
        assert!(file.starts_with(&mp4_box(b"ftyp", b"M4A \0\0\0\0")));
    }

    #[test]
    fn broken_mp4_sizes_are_left_alone() {
        for mut f in [vec![0, 0, 0, 1, b'm', b'o', b'o', b'v'], vec![0, 0, 0, 4, b'm', b'o', b'o', b'v', 1], vec![0xFF; 12], vec![]] {
            super::scrub_mp4(&mut f);
        }
    }

    #[test]
    fn waveform_is_scaled_to_the_loudest() {
        let mut s = vec![0i16; 48_000];
        for (i, v) in s.iter_mut().enumerate().skip(24_000) {
            *v = if i % 2 == 0 { 8000 } else { -8000 };
        }
        let w = super::waveform(&s);
        assert_eq!(w.len(), 100);
        assert_eq!(w[0], 0);
        assert_eq!(*w.iter().max().unwrap(), 1024);
    }
}
