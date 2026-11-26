//! A call's sound on this device: the microphone into the call (its voice
//! changed on the way, unless that's turned off) and the other side's
//! voice out of the speakers.
//!
//! The microphone is listened to the way voice messages record (ffmpeg on
//! Linux, Android's own on phones). Nothing takes this side's own voice
//! back out of what the microphone hears from the speakers, so without
//! headphones the other side may hear an echo of themselves.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use whisker_core::call_media::{FRAME, Link, Playout};
use whisker_core::voice_mask::Mask;

/// Set to stop the microphone and the speaker.
static STOP: AtomicBool = AtomicBool::new(true);

/// Where what the microphone hears goes.
struct Mic {
    /// Heard, not yet a whole piece.
    pending: Vec<i16>,
    /// The voice changer, when it's on.
    mask: Option<Mask>,
    muted: Arc<AtomicBool>,
    out: tokio::sync::mpsc::Sender<[i16; FRAME]>,
}

static MIC: Mutex<Option<Mic>> = Mutex::new(None);

/// What the microphone heard (called by its thread): cut into pieces, the
/// voice changed, and sent on. While muted the pieces are made silence
/// before the changer gets them, so nothing said while muted is held in it
/// to come out afterwards.
fn push(samples: &[i16]) {
    let mut mic = MIC.lock().unwrap();
    let Some(mic) = mic.as_mut() else { return };
    mic.pending.extend_from_slice(samples);
    while mic.pending.len() >= FRAME {
        let mut piece = [0i16; FRAME];
        piece.copy_from_slice(&mic.pending[..FRAME]);
        mic.pending.drain(..FRAME);
        if mic.muted.load(Ordering::Relaxed) {
            piece = [0; FRAME];
        }
        if let Some(mask) = mic.mask.as_mut() {
            mask.apply(&mut piece);
        }
        // Full (the connection is slow): this piece is dropped, not queued
        // to be heard late.
        let _ = mic.out.try_send(piece);
    }
}

/// Starts the call's sound: the microphone into `link`, and what `link`
/// hears out of the speakers. `Err` says why the microphone can't be used.
pub fn start(rt: &tokio::runtime::Handle, link: Arc<Link>, change_voice: bool, muted: Arc<AtomicBool>) -> Result<(), String> {
    stop();
    STOP.store(false, Ordering::SeqCst);
    let (out, mut pieces) = tokio::sync::mpsc::channel::<[i16; FRAME]>(8);
    *MIC.lock().unwrap() = Some(Mic { pending: Vec::new(), mask: change_voice.then(Mask::for_call), muted, out });
    let (ready, opened) = std::sync::mpsc::channel();
    std::thread::spawn(move || crate::voice::capture(&STOP, ready, push, Duration::from_secs(24 * 3600)));
    match opened.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            stop();
            return Err(e);
        }
        Err(_) => {
            stop();
            return Err("The microphone didn't start.".into());
        }
    }
    let playout = link.playout();
    rt.spawn(async move {
        while let Some(piece) = pieces.recv().await {
            // A piece that can't be sent (not connected yet) is just lost.
            let _ = link.say(&piece).await;
        }
    });
    std::thread::spawn(move || speaker(&STOP, &playout));
    Ok(())
}

/// Stops the call's sound (the microphone closes).
pub fn stop() {
    STOP.store(true, Ordering::SeqCst);
    // Dropping the sender ends the task that sends the pieces on.
    *MIC.lock().unwrap() = None;
}

/// Plays what's heard until `stop`: a piece every 20 ms, through ffmpeg to
/// the system's sound (as videos do).
#[cfg(all(target_os = "linux", not(target_os = "android")))]
fn speaker(stop: &AtomicBool, playout: &Playout) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::Instant;
    let Some(out) = crate::player::sound_output() else { return };
    let child = Command::new("ffmpeg")
        .args(["-v", "error", "-probesize", "32", "-analyzeduration", "0", "-fflags", "nobuffer", "-f", "s16le", "-ar", "48000", "-ac", "1", "-i", "pipe:0"])
        .args(out)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return };
    let Some(mut pipe) = child.stdin.take() else { return };
    // A tenth of a second of silence first, so there's always some sound
    // waiting at the speakers (sent just as it's due, it stutters).
    let _ = pipe.write_all(&[0u8; FRAME * 2 * 5]);
    let mut due = Instant::now();
    let mut piece = [0i16; FRAME];
    let mut bytes = [0u8; FRAME * 2];
    while !stop.load(Ordering::SeqCst) {
        playout.take(&mut piece);
        for (b, s) in bytes.chunks_exact_mut(2).zip(piece) {
            b.copy_from_slice(&s.to_le_bytes());
        }
        if pipe.write_all(&bytes).is_err() {
            break;
        }
        due += Duration::from_millis(20);
        match due.checked_duration_since(Instant::now()) {
            Some(wait) => std::thread::sleep(wait),
            // Fallen far behind (the computer was busy): carry on from now.
            None if due.elapsed() > Duration::from_millis(200) => due = Instant::now(),
            None => {}
        }
    }
    drop(pipe);
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(target_os = "android")]
fn speaker(stop: &AtomicBool, playout: &Playout) {
    crate::player::voice::play(stop, &|piece: &mut [i16]| playout.take(piece));
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn speaker(_stop: &AtomicBool, _playout: &Playout) {}
