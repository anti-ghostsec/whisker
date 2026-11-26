//! Changes a voice before it leaves the device, for calls: the pitch and
//! the character of the voice (its formants) move together, by an amount
//! picked afresh for each call and drifting a little as the call goes on,
//! so two calls don't sound like the same person and what's heard can't be
//! put back exactly by playing it faster or slower.
//!
//! It disguises a voice from someone listening; it isn't proof against a
//! laboratory comparing recordings (the rhythm and the words stay yours).
//!
//! How: the sound is read back from a short loop of what was just said by
//! two heads that slide along it (faster or slower than it was written,
//! which raises or lowers the pitch), each faded out as it reaches the end
//! of the loop and jumps back, while the other is at its loudest.

/// Sound a second: what calls use.
pub const RATE: u32 = 48_000;

/// The loop the heads slide along, in samples (40 ms): long enough to
/// hold a few turns of the lowest voice, short enough not to echo.
const LOOP: f32 = 1920.0;
/// Room for the loop and the sample being written, a power of two.
const HELD: usize = 4096;

/// How far the pitch drifts either way as the call goes on.
const DRIFT: f32 = 0.03;

pub struct Mask {
    held: Vec<f32>,
    /// Where the next sample goes.
    at: usize,
    /// How far along the loop the first head is, 0–1.
    head: f32,
    /// The pitch: 1 is unchanged, lower is deeper.
    pitch: f32,
    /// The drift: where its slow wave is, how fast it turns, and the
    /// numbers that pick a new speed each time round.
    wave: f32,
    turn: f32,
    drifts: bool,
    seed: u64,
}

impl Mask {
    /// A voice for one call: deeper or higher by an amount picked at
    /// random, never close to the speaker's own.
    pub fn for_call() -> Self {
        let mut bytes = [0u8; 8];
        // Without random numbers it still changes the voice, by a fixed amount.
        let _ = getrandom::fill(&mut bytes);
        let seed = u64::from_le_bytes(bytes) | 1;
        let mut mask = Self::with(1.0, true, seed);
        let pick = mask.random();
        // Deeper (0.78–0.88) three times in four: it sounds like a
        // person; higher (1.16–1.28) is thinner, but as far from the
        // speaker's own.
        mask.pitch = if mask.random() < 0.75 { 0.78 + 0.10 * pick } else { 1.16 + 0.12 * pick };
        mask
    }

    /// A voice changed by exactly `pitch` (for tests: `drifts` off keeps it there).
    pub fn with(pitch: f32, drifts: bool, seed: u64) -> Self {
        Self { held: vec![0.0; HELD], at: 0, head: 0.0, pitch, wave: 0.0, turn: 0.6, drifts, seed: seed | 1 }
    }

    /// The pitch picked for this call.
    pub fn pitch(&self) -> f32 {
        self.pitch
    }

    /// A number from 0 to 1 (xorshift: quick, and nothing here needs more).
    fn random(&mut self) -> f32 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        (self.seed >> 40) as f32 / (1u64 << 24) as f32
    }

    /// What's `back` samples before the one just written, between the two
    /// samples either side of it.
    fn read(&self, back: f32) -> f32 {
        let whole = back as usize;
        let part = back - whole as f32;
        let a = self.held[(self.at + HELD - 1 - whole) & (HELD - 1)];
        let b = self.held[(self.at + HELD - 2 - whole) & (HELD - 1)];
        a + (b - a) * part
    }

    /// Changes `samples` (one channel, [`RATE`] a second) where they are.
    pub fn apply(&mut self, samples: &mut [i16]) {
        use std::f32::consts::PI;
        for s in samples {
            self.held[self.at] = f32::from(*s);
            self.at = (self.at + 1) & (HELD - 1);
            let pitch = if self.drifts {
                self.wave += self.turn * 2.0 * PI / RATE as f32;
                if self.wave >= 2.0 * PI {
                    self.wave -= 2.0 * PI;
                    // Each turn of the drift takes a different time (0.3–0.9 a second).
                    self.turn = 0.3 + 0.6 * self.random();
                }
                self.pitch * (1.0 + DRIFT * self.wave.sin())
            } else {
                self.pitch
            };
            // A head reading slower than the sound is written falls
            // further back (a deeper voice); a faster one catches up.
            self.head += (1.0 - pitch) / LOOP;
            self.head -= self.head.floor();
            let other = (self.head + 0.5).fract();
            // Each head is silent where it jumps from one end of the loop
            // to the other, and the two always add up to all of the sound.
            let loud = (PI * self.head).sin().powi(2);
            let mixed = loud * self.read(self.head * LOOP) + (1.0 - loud) * self.read(other * LOOP);
            *s = mixed.clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32, seconds: f32) -> Vec<i16> {
        (0..(RATE as f32 * seconds) as usize).map(|n| ((n as f32 * hz * 2.0 * std::f32::consts::PI / RATE as f32).sin() * 12_000.0) as i16).collect()
    }

    /// How many times a second the sound goes up through nothing.
    fn pitch_of(samples: &[i16]) -> f32 {
        let ups = samples.windows(2).filter(|w| w[0] < 0 && w[1] >= 0).count();
        ups as f32 * RATE as f32 / samples.len() as f32
    }

    #[test]
    fn lowers_and_raises_the_pitch() {
        for (pitch, want) in [(0.8, 160.0), (1.25, 250.0)] {
            let mut sound = tone(200.0, 2.0);
            let mut mask = Mask::with(pitch, false, 1);
            // In the pieces a call sends (20 ms).
            for piece in sound.chunks_mut(960) {
                mask.apply(piece);
            }
            // After the loop has filled.
            let got = pitch_of(&sound[RATE as usize / 2..]);
            assert!((got - want).abs() < want * 0.04, "pitch {pitch}: {got} Hz, wanted about {want}");
        }
    }

    #[test]
    fn stays_as_loud_and_without_clicks() {
        let mut sound = tone(200.0, 2.0);
        Mask::with(0.82, true, 7).apply(&mut sound);
        let after = &sound[RATE as usize / 2..];
        let loudness = (after.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / after.len() as f64).sqrt();
        // A 12,000 tone is about 8,500 loud; the two heads can part cancel.
        assert!((4_000.0..10_000.0).contains(&loudness), "loudness {loudness}");
        // No jump bigger than a tone twice as high and at full level would make.
        let jump = after.windows(2).map(|w| (i32::from(w[1]) - i32::from(w[0])).abs()).max().unwrap();
        assert!(jump < 2_000, "jump {jump}");
    }

    #[test]
    fn each_call_gets_a_voice_away_from_your_own() {
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..50 {
            let pitch = Mask::for_call().pitch();
            assert!((0.78..=0.88).contains(&pitch) || (1.16..=1.28).contains(&pitch), "pitch {pitch}");
            seen.insert((pitch * 1000.0) as u32);
        }
        assert!(seen.len() > 10, "{} different voices in 50 calls", seen.len());
    }
}
