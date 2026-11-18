//! Small signal helpers: loudness, RFC 6464 level, pitch-period time compression.

use crate::protocol::FRAME_SAMPLES;

pub fn rms(pcm: &[i16]) -> f64 {
    if pcm.is_empty() {
        return 0.0;
    }
    let sum: f64 = pcm.iter().map(|&v| (v as f64) * (v as f64)).sum();
    (sum / pcm.len() as f64).sqrt()
}

/// RFC 6464 level of a frame: -dBov, 0 (full scale) .. 127 (silence).
/// The macOS client's group audio says 41..46 for speech.
pub fn audio_level(pcm: &[i16]) -> u8 {
    let r = rms(pcm);
    if r < 1.0 {
        return 127;
    }
    (-20.0 * (r / 32768.0).log10()).round().clamp(0.0, 127.0) as u8
}

pub fn clamp16(v: f64) -> i16 {
    v.max(-32768.0).min(32767.0) as i16
}

/// Lag, in samples, of one pitch period inside 2.5..10 ms.
///
/// NetEq's Accelerate deletes one period and overlap-adds across the cut.
/// The window is the first 5 ms. A 1 kHz tone (period 16) matches a harmonic
/// inside the range, so the wave stays continuous.
pub fn pitch_lag(samples: &[i16]) -> usize {
    const WINDOW: usize = 80;
    let limit = 160.min(samples.len().saturating_sub(WINDOW + 1));
    if limit < 40 {
        return 80;
    }
    let mut best: Option<i64> = None;
    let mut best_lag = 80;
    let mut lag = 40;
    while lag <= limit {
        let score: i64 = (0..WINDOW).map(|i| samples[i] as i64 * samples[i + lag] as i64).sum();
        if best.map_or(true, |b| score > b) {
            best = Some(score);
            best_lag = lag;
        }
        lag += 2;
    }
    best_lag
}

/// Shorten pcm by one pitch period. Returns (20 ms to play, the rest).
///
/// 40 ms in becomes about 30-37 ms out: one frame now, the leftover stays in
/// the stream carry. The join is an overlap-add, so the sample stream does
/// not jump the way cutting a whole 20 ms frame does.
pub fn wsola_shrink(samples: &[i16]) -> (Vec<i16>, Vec<i16>) {
    let split = |v: &[i16]| {
        let mut head: Vec<i16> = v[..FRAME_SAMPLES.min(v.len())].to_vec();
        head.resize(FRAME_SAMPLES, 0);
        let rest = if v.len() > FRAME_SAMPLES { v[FRAME_SAMPLES..].to_vec() } else { Vec::new() };
        (head, rest)
    };
    if samples.len() < FRAME_SAMPLES * 2 {
        return split(samples);
    }
    let lag = pitch_lag(samples);
    let mut out = vec![0i16; samples.len() - lag];
    for i in 0..lag {
        let w = (i + 1) as f64 / (lag + 1) as f64;
        let mixed = samples[i] as f64 * (1.0 - w) + samples[i + lag] as f64 * w;
        out[i] = clamp16(mixed.round());
    }
    let rest = samples.len() - 2 * lag;
    out[lag..lag + rest].copy_from_slice(&samples[2 * lag..]);
    split(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_of_silence_and_full_scale() {
        assert_eq!(audio_level(&[0; 320]), 127);
        assert_eq!(audio_level(&[32767; 320]), 0);
    }

    #[test]
    fn shrink_removes_one_period() {
        // 200 Hz tone: period 80 samples.
        let tone: Vec<i16> = (0..640).map(|i| (8000.0 * (2.0 * std::f64::consts::PI * 200.0 * i as f64 / 16000.0).sin()) as i16).collect();
        let lag = pitch_lag(&tone);
        assert_eq!(lag, 80);
        let (played, rest) = wsola_shrink(&tone);
        assert_eq!(played.len(), 320);
        assert_eq!(played.len() + rest.len(), 640 - lag);
    }

    #[test]
    fn short_input_is_padded() {
        let (played, rest) = wsola_shrink(&[1; 100]);
        assert_eq!(played.len(), 320);
        assert!(rest.is_empty());
    }
}
