//! MicLeveler: software make-up gain for the microphone, in front of the encoder.
//!
//! The real engine raises a weak microphone with one analog AGC (the OS
//! microphone volume, AgcManagerDirect). On a microphone already near 100 %
//! that cannot help, and PipeWire's own webrtc AGCs pump. Measured on a USB
//! headset: speech -28 dBFS, noise -46 dBFS at the source, -43 dBFS after the
//! processing: the phone heard nothing.
//!
//! One slow gain, applied to speech only, aimed at TARGET_DB. It never lifts
//! the noise floor above NOISE_CEIL_DB (a noisy microphone is not turned into
//! a hiss), and never exceeds full scale. What a first version did wrong: the
//! gain stepped at every 20 ms frame boundary (a rasp at the end of words),
//! fell by half its distance in one frame and climbed back at 3 %, started at
//! 0 dB after a 1 s learning pause (the first words quiet), and rose on the
//! echo of the far end too (the phone heard itself). Now:
//! - the gain moves sample by sample inside a frame, from the last frame's;
//! - it moves at most UP_DB / DOWN_DB per speech frame;
//! - it does not move while the far end sounds (far_active), so echo left by
//!   the canceller is never what it adapts to;
//! - the noise floor is known after 0.4 s, and the gain of the last call
//!   (ZCALL_LOG_DIR/mic-leveler.json) is where it starts.
//! ZCALL_MIC_GAIN=0 turns it off.

use std::collections::VecDeque;
use std::path::PathBuf;

use crate::log;

pub struct MicLeveler {
    far_active: Box<dyn Fn() -> bool + Send>,
    state_path: Option<PathBuf>,
    pub gain_db: f64,
    g_prev: f64,
    pub noise_db: f64,
    levels: VecDeque<f64>,
    n: u64,
    speech: u64,
    held: u64,
    speech_db: Vec<f64>,
}

impl MicLeveler {
    const TARGET_DB: f64 = -22.0; // wanted speech level, dBFS rms
    const NOISE_CEIL_DB: f64 = -38.0; // noise floor after the gain stays below this
    const MAX_GAIN_DB: f64 = 24.0;
    const SPEECH_OVER_NOISE_DB: f64 = 9.0;
    const FLOOR_DB: f64 = -60.0; // below this a frame is never speech
    const LEARN_FRAMES: usize = 20; // 0.4 s of noise floor before any change
    const UP_DB: f64 = 0.5; // per speech frame: +25 dB/s at most
    const DOWN_DB: f64 = 1.0; // per speech frame: a loud word comes down in 0.2 s
    const LOG_EVERY: u64 = 250; // frames: 5 s
    const LEVELS: usize = 250; // 5 s of frame levels

    pub fn new(far_active: Box<dyn Fn() -> bool + Send>, state_path: Option<PathBuf>) -> Self {
        let gain_db = Self::load(state_path.as_deref());
        if gain_db != 0.0 {
            log!("mic level: starting at +{gain_db:.1} dB (the last call)");
        }
        MicLeveler {
            far_active,
            state_path,
            gain_db,
            g_prev: 10f64.powf(gain_db / 20.0),
            noise_db: -70.0,
            levels: VecDeque::with_capacity(Self::LEVELS),
            n: 0,
            speech: 0,
            held: 0,
            speech_db: Vec::new(),
        }
    }

    fn load(path: Option<&std::path::Path>) -> f64 {
        let Some(path) = path else { return 0.0 };
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return 0.0, // the first call: start flat
            Err(e) => {
                log!("mic level: ignoring {} {e}", path.display());
                return 0.0;
            }
        };
        match parse_gain(&text) {
            Some(g) => g.clamp(0.0, Self::MAX_GAIN_DB),
            None => {
                log!("mic level: ignoring {} (no gain_db)", path.display());
                0.0
            }
        }
    }

    fn save(&self) {
        let Some(path) = &self.state_path else { return };
        let body = format!("{{\"gain_db\": {}}}", (self.gain_db * 100.0).round() / 100.0);
        if let Err(e) = std::fs::write(path, body) {
            log!("mic level: cannot save {e}");
        }
    }

    fn level_db(a: &[i16]) -> f64 {
        20.0 * (crate::dsp::rms(a).max(1.0) / 32768.0).log10()
    }

    pub fn process(&mut self, a: &mut [i16]) {
        let level = Self::level_db(a);
        if self.levels.len() == Self::LEVELS {
            self.levels.pop_front();
        }
        self.levels.push_back(level);
        self.n += 1;
        let learned = self.levels.len() >= Self::LEARN_FRAMES;
        if learned && (self.n % 25 == 0 || self.levels.len() == Self::LEARN_FRAMES) {
            let mut sorted: Vec<f64> = self.levels.iter().copied().collect();
            sorted.sort_by(|x, y| x.partial_cmp(y).unwrap());
            self.noise_db = sorted[sorted.len() / 10]; // 10th percentile: the pauses
        }
        if learned && level > Self::FLOOR_DB && level > self.noise_db + Self::SPEECH_OVER_NOISE_DB {
            if (self.far_active)() {
                self.held += 1;
            } else {
                self.speech += 1;
                self.speech_db.push(level);
                let ceiling = (Self::NOISE_CEIL_DB - self.noise_db).max(0.0).min(Self::MAX_GAIN_DB);
                let want = (Self::TARGET_DB - level).max(0.0).min(ceiling);
                self.gain_db += (want - self.gain_db).clamp(-Self::DOWN_DB, Self::UP_DB);
            }
        }
        if self.n % Self::LOG_EVERY == 0 {
            let held = if self.held > 0 { format!(", held {}% (far end)", 100 * self.held / Self::LOG_EVERY) } else { String::new() };
            if self.speech_db.is_empty() {
                log!("mic level: noise {:.0} dBFS, no speech, gain +{:.1} dB{held}", self.noise_db, self.gain_db);
            } else {
                let mut sp = std::mem::take(&mut self.speech_db);
                sp.sort_by(|x, y| x.partial_cmp(y).unwrap());
                log!(
                    "mic level: noise {:.0} dBFS, speech {:.0} dBFS before the gain, gain +{:.1} dB, speech in {}% of frames{held}",
                    self.noise_db,
                    sp[sp.len() / 2],
                    self.gain_db,
                    100 * self.speech / Self::LOG_EVERY
                );
            }
            self.speech = 0;
            self.held = 0;
            self.speech_db.clear();
            self.save();
        }
        let mut g = 10f64.powf(self.gain_db / 20.0);
        let mut g0 = self.g_prev;
        let peak = a.iter().map(|v| v.unsigned_abs()).max().unwrap_or(0);
        if peak > 0 {
            // Neither end of the ramp may clip this frame: a loud word right
            // after a high gain is limited from its first sample.
            let lim = 32000.0 / peak as f64;
            g = g.min(lim);
            g0 = g0.min(lim);
        }
        self.g_prev = g;
        if (g - 1.0).abs() < 0.01 && (g0 - 1.0).abs() < 0.01 {
            return;
        }
        let n = a.len() as f64;
        let step = (g - g0) / n;
        for (i, v) in a.iter_mut().enumerate() {
            let x = (*v as f64 * (g0 + step * (i + 1) as f64)) as i64;
            *v = x.clamp(-32768, 32767) as i16;
        }
    }
}

/// {"gain_db": 6.5} -> 6.5
fn parse_gain(text: &str) -> Option<f64> {
    let rest = &text[text.find("\"gain_db\"")? + "\"gain_db\"".len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let end = rest.find(|c: char| !(c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'))).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_file_round_trip() {
        assert_eq!(parse_gain("{\"gain_db\": 6.5}"), Some(6.5));
        assert_eq!(parse_gain("{\"gain_db\":-1}"), Some(-1.0));
        assert_eq!(parse_gain("{}"), None);
    }

    #[test]
    fn quiet_speech_is_lifted_slowly() {
        let mut l = MicLeveler::new(Box::new(|| false), None);
        let noise: Vec<i16> = (0..320).map(|i| if i % 2 == 0 { 20 } else { -20 }).collect();
        for _ in 0..30 {
            let mut f = noise.clone();
            l.process(&mut f);
        }
        // Speech around -40 dBFS rms.
        let speech: Vec<i16> = (0..320).map(|i| ((i as f64 * 0.3).sin() * 460.0) as i16).collect();
        let mut f = speech.clone();
        l.process(&mut f);
        assert!(l.gain_db > 0.0 && l.gain_db <= MicLeveler::UP_DB + 1e-9);
        // Held while the far end talks.
        let mut held = MicLeveler::new(Box::new(|| true), None);
        for _ in 0..30 {
            let mut f = noise.clone();
            held.process(&mut f);
        }
        let mut f = speech;
        held.process(&mut f);
        assert_eq!(held.gain_db, 0.0);
    }
}
