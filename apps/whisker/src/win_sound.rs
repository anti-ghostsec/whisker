//! A video's sound on Windows. ffmpeg can't play to the speakers there, so
//! it sends the sound, decoded, to Whisker over a connection on this
//! computer only (127.0.0.1), and Whisker plays it through Windows' own
//! audio. One sound plays at a time: a new connection takes over from the
//! one before.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::io::Read;
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// Sound waiting to be played (samples, the channels interleaved).
struct Waiting {
    samples: VecDeque<i16>,
}

/// The most kept waiting, in seconds: beyond it, the connection is read no
/// faster than it plays (ffmpeg keeps time itself, so this rarely bites).
const KEEP_SECONDS: usize = 1;

/// What ffmpeg is told to send, and where: the output part of its command.
pub fn output_args() -> Option<&'static [&'static str]> {
    static ARGS: OnceLock<Option<&'static [&'static str]>> = OnceLock::new();
    *ARGS.get_or_init(|| match start() {
        Ok(args) => Some(args),
        Err(e) => {
            tracing::warn!("no sound for videos: {e}");
            None
        }
    })
}

fn start() -> Result<&'static [&'static str], String> {
    // The speakers' own rate and channels, so ffmpeg makes the sound as
    // they take it.
    let (rate, channels) = {
        let device = cpal::default_host().default_output_device().ok_or("no sound output")?;
        let config = device.default_output_config().map_err(|e| e.to_string())?;
        (config.sample_rate().0, config.channels())
    };
    let waiting = Arc::new(Mutex::new(Waiting { samples: VecDeque::new() }));
    let (opened, ok) = std::sync::mpsc::channel();
    {
        let waiting = waiting.clone();
        // The stream stays on the thread that made it.
        std::thread::spawn(move || {
            let stream = match play(&waiting) {
                Ok(s) => s,
                Err(e) => {
                    let _ = opened.send(Err(e));
                    return;
                }
            };
            let _ = opened.send(Ok(()));
            let _keep = stream;
            loop {
                std::thread::park();
            }
        });
    }
    ok.recv_timeout(Duration::from_secs(5)).map_err(|_| "the sound output didn't start".to_owned())??;

    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let latest = Arc::new(AtomicU64::new(0));
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            // Only from this computer (the address is 127.0.0.1 already).
            if !conn.peer_addr().is_ok_and(|a| a.ip().is_loopback()) {
                continue;
            }
            let mine = latest.fetch_add(1, Ordering::SeqCst) + 1;
            waiting.lock().unwrap().samples.clear();
            let (waiting, latest) = (waiting.clone(), latest.clone());
            let most = rate as usize * channels as usize * KEEP_SECONDS;
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 8192];
                let mut odd: Option<u8> = None;
                while latest.load(Ordering::SeqCst) == mine {
                    if waiting.lock().unwrap().samples.len() > most {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    let n = match conn.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let mut bytes: Vec<u8> = odd.take().into_iter().chain(buf[..n].iter().copied()).collect();
                    if bytes.len() % 2 == 1 {
                        odd = bytes.pop();
                    }
                    if latest.load(Ordering::SeqCst) != mine {
                        break;
                    }
                    let mut w = waiting.lock().unwrap();
                    w.samples.extend(bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
                }
            });
        }
    });
    let args = vec!["-f".to_owned(), "s16le".to_owned(), "-ar".to_owned(), rate.to_string(), "-ac".to_owned(), channels.to_string(), format!("tcp://127.0.0.1:{port}")];
    let args: Vec<&'static str> = args.into_iter().map(|a| &*Box::leak(a.into_boxed_str())).collect();
    Ok(Box::leak(args.into_boxed_slice()))
}

/// The speakers, taking what's waiting (silence when nothing is).
fn play(waiting: &Arc<Mutex<Waiting>>) -> Result<cpal::Stream, String> {
    let device = cpal::default_host().default_output_device().ok_or("no sound output")?;
    let config = device.default_output_config().map_err(|e| e.to_string())?;
    let format = config.sample_format();
    let config: cpal::StreamConfig = config.into();
    let failed = |e: cpal::StreamError| tracing::warn!("sound output: {e}");
    // What's waiting, into the speakers' buffer: one lock per buffer.
    fn fill<T>(waiting: &Mutex<Waiting>, out: &mut [T], convert: fn(i16) -> T) {
        let mut w = waiting.lock().unwrap();
        for s in out {
            *s = convert(w.samples.pop_front().unwrap_or(0));
        }
    }
    let (a, b, c) = (waiting.clone(), waiting.clone(), waiting.clone());
    let stream = match format {
        cpal::SampleFormat::F32 => device.build_output_stream(&config, move |out: &mut [f32], _| fill(&a, out, |s| f32::from(s) / 32768.0), failed, None),
        cpal::SampleFormat::I16 => device.build_output_stream(&config, move |out: &mut [i16], _| fill(&b, out, |s| s), failed, None),
        cpal::SampleFormat::U16 => device.build_output_stream(&config, move |out: &mut [u16], _| fill(&c, out, |s| (i32::from(s) + 32768) as u16), failed, None),
        other => return Err(format!("the sound output takes {other:?}, which Whisker doesn't make")),
    }
    .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}
