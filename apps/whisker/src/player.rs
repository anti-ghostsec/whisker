//! Plays videos inside the window, in the message itself: ffmpeg decodes
//! the picture (paced to real time) into frames shown over the preview, and
//! sends the sound straight to the system (PulseAudio, else ALSA), drawing
//! any subtitles onto the picture. Videos start muted. Pausing, seeking,
//! muting, subtitles and the large view restart ffmpeg at the right place.

use crate::{AppWindow, Player};
use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer, Weak};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use whisker_core::timeline::{Subtitles, VideoInfo, probe_video};

/// Frames a second shown.
const FPS: u32 = 30;
/// Widest and tallest frame decoded, in the message and in the large view.
const MAX_PX: (u32, u32) = (960, 720);
const MAX_LARGE_PX: (u32, u32) = (1600, 900);

struct Playing {
    id: String,
    path: PathBuf,
    info: VideoInfo,
    /// Where in the video it is (or was paused), in seconds.
    at: f64,
    muted: bool,
    /// Subtitles drawn (when it has them).
    subtitles: bool,
    /// Shown big, over the whole window.
    large: bool,
    /// Meant to be playing (ffmpeg may still be starting).
    playing: bool,
    child: Option<Child>,
    /// Bumped on every (re)start, so an old reader stops showing frames.
    generation: u64,
}

static NOW: Mutex<Option<Playing>> = Mutex::new(None);
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
        Command::new("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
    })
}

/// Whether message `id` is the one loaded in the player.
pub fn is_loaded(id: &str) -> bool {
    NOW.lock().unwrap().as_ref().is_some_and(|p| p.id == id)
}

/// Starts playing the downloaded video `path` from message `id`.
pub fn play(ui: Weak<AppWindow>, id: String, path: PathBuf) -> bool {
    stop(&ui);
    let Some(info) = probe_video(&path) else { return false };
    let duration = info.duration.map_or(0.0, |d| d.as_secs_f64());
    let has_subtitles = info.subtitles.is_some();
    *NOW.lock().unwrap() = Some(Playing { id: id.clone(), path, info, at: 0.0, muted: true, subtitles: has_subtitles, large: false, playing: false, child: None, generation: 0 });
    let _ = ui.upgrade_in_event_loop(move |u| {
        let p = u.global::<Player>();
        p.set_id(id.into());
        p.set_frame(Image::default());
        p.set_duration(duration as f32);
        p.set_position(0.0);
        p.set_muted(true);
        p.set_has_subtitles(has_subtitles);
        p.set_subtitles(has_subtitles);
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
        let running = p.playing;
        kill(p);
        (p.muted, running)
    };
    let _ = ui.upgrade_in_event_loop(move |u| u.global::<Player>().set_muted(muted));
    if running {
        resume(ui);
    }
}

/// Shows or hides the subtitles.
pub fn subtitles(ui: &Weak<AppWindow>) {
    let Some((on, running)) = restart_with(|p| p.subtitles = !p.subtitles && p.info.subtitles.is_some()).map(|p| (p.subtitles, p.running)) else { return };
    let _ = ui.upgrade_in_event_loop(move |u| u.global::<Player>().set_subtitles(on));
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

fn pause(ui: &Weak<AppWindow>) {
    if let Some(p) = NOW.lock().unwrap().as_mut() {
        p.playing = false;
        kill(p);
    }
    let _ = ui.upgrade_in_event_loop(|u| u.global::<Player>().set_playing(false));
}

fn kill(p: &mut Playing) {
    p.generation += 1;
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
fn sound_output() -> Option<&'static [&'static str]> {
    static FOUND: std::sync::OnceLock<Option<&'static [&'static str]>> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| {
        let tries: [&'static [&'static str]; 2] = [&["-f", "pulse", "Whisker"], &["-f", "alsa", "default"]];
        tries.into_iter().find(|out| {
            Command::new("ffmpeg")
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
fn filter_path(path: &Path) -> String {
    let escape = |s: &str, special: &str| s.chars().fold(String::new(), |mut out, c| {
        if special.contains(c) {
            out.push('\\');
        }
        out.push(c);
        out
    });
    escape(&escape(&path.to_string_lossy(), "\\':"), "\\'[],;")
}

fn ffmpeg(path: &Path, at: f64, size: (u32, u32), sound: Option<&[&str]>, subtitles: Option<Subtitles>) -> std::io::Result<Child> {
    let mut cmd = Command::new("ffmpeg");
    let fit = format!("fps={FPS},scale={}:{}", size.0, size.1);
    cmd.args(["-v", "error", "-nostdin", "-ss", &format!("{at:.3}")]);
    // Text subtitles are drawn by time, so the times must stay the file's.
    if subtitles == Some(Subtitles::Text) {
        cmd.arg("-copyts");
    }
    cmd.args(["-re", "-i"]).arg(path);
    match subtitles {
        Some(Subtitles::Text) => cmd.args(["-map", "0:v:0", "-vf", &format!("{fit},subtitles=filename={}:si=0", filter_path(path))]),
        Some(Subtitles::Bitmap) => cmd.args(["-filter_complex", &format!("[0:v:0][0:s:0]overlay,{fit}[v]"), "-map", "[v]"]),
        None => cmd.args(["-map", "0:v:0", "-vf", &fit]),
    };
    cmd.args(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"]);
    if let Some(out) = sound {
        cmd.args(["-map", "0:a:0"]).args(out);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
}

/// Plays from where it is.
fn resume(ui: &Weak<AppWindow>) {
    let (path, at, info, muted, subtitles, large, generation) = {
        let mut now = NOW.lock().unwrap();
        let Some(p) = now.as_mut() else { return };
        kill(p);
        p.playing = true;
        // Played to the end: start over.
        if p.info.duration.is_some_and(|d| p.at >= d.as_secs_f64() - 0.2) {
            p.at = 0.0;
        }
        (p.path.clone(), p.at, p.info, p.muted, p.subtitles, p.large, p.generation)
    };
    let _ = ui.upgrade_in_event_loop(|u| {
        let p = u.global::<Player>();
        p.set_playing(true);
        p.set_loading(true);
    });
    let ui = ui.clone();
    let subtitles = if subtitles { info.subtitles } else { None };
    let max = if large { MAX_LARGE_PX } else { MAX_PX };
    std::thread::spawn(move || run(ui, path, at, info, muted, subtitles, max, generation));
}

/// Starts ffmpeg and shows its frames until it ends or is replaced.
#[allow(clippy::too_many_arguments)]
fn run(ui: Weak<AppWindow>, path: PathBuf, at: f64, info: VideoInfo, muted: bool, subtitles: Option<Subtitles>, max: (u32, u32), generation: u64) {
    let size = frame_size(&info, max);
    let sound = if info.audio && !muted { sound_output() } else { None };
    let current = || NOW.lock().unwrap().as_ref().is_some_and(|p| p.generation == generation);
    if !current() {
        return;
    }
    let started = ffmpeg(&path, at, size, sound, subtitles).ok().and_then(|mut child| {
        let out = child.stdout.take()?;
        let mut now = NOW.lock().unwrap();
        match now.as_mut() {
            Some(p) if p.generation == generation => {
                p.child = Some(child);
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
        Some(mut out) => show_frames(&ui, &mut out, size, at, generation),
        None => 0,
    };
    // Played out (not replaced): back to the preview, ready to play again.
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

/// Reads frames from ffmpeg into the window. Returns how many it showed.
fn show_frames(ui: &Weak<AppWindow>, out: &mut impl Read, size: (u32, u32), at: f64, generation: u64) -> u64 {
    let bytes = (size.0 * size.1 * 4) as usize;
    let mut shown = 0u64;
    loop {
        let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(size.0, size.1);
        if out.read_exact(buf.make_mut_bytes()).is_err() || buf.as_bytes().len() != bytes {
            return shown;
        }
        let position = at + shown as f64 / f64::from(FPS);
        {
            let mut now = NOW.lock().unwrap();
            match now.as_mut() {
                Some(p) if p.generation == generation => p.at = position,
                _ => return shown,
            }
        }
        shown += 1;
        let first = shown == 1;
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
        assert_eq!(filter_path(Path::new("/tmp/a.mkv")), "/tmp/a.mkv");
        assert_eq!(filter_path(Path::new("/x/it's: [a],b.mkv")), "/x/it\\\\\\'s\\\\: \\[a\\]\\,b.mkv");
    }
}
