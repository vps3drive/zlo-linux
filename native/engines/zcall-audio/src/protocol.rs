//! Pipe framing between zrtc-media.js and this engine (big endian).
//!
//! stdout  'L' u8 level, u16 len, Opus   one encoded 20 ms microphone frame; level is
//!                                       its loudness in -dBov, 0..127 (RFC 6464, 127 =
//!                                       silence), for the group audio-level extension
//! stdin   'F' u32 idx, u32 src, u16 len, Opus
//!                                       one received frame; idx = ROC<<16 | seq
//!         'M' u8 on                     mute the microphone (sends silence)
//!         'S' u8 on                     speaker off (received audio is played as silence)
//!         'V' u8 percent                set the microphone's source volume (0..100, the
//!                                       call window's slider); stderr says "mic volume: N%"
//!                                       at the start and after every change
//!         'D' u8 kind, u16 len, name    use another PulseAudio source (kind 'i') or
//!                                       sink ('o') now; empty name: the default
//!
//! Logs go to stderr as "[audio-io] ..." (the prefix zrtc-media.js matches);
//! "fault: mic" / "fault: speaker" is a device that stopped for good.

pub const SAMPLE_RATE: u32 = 16000;
pub const FRAME_SAMPLES: usize = 320; // 20 ms at 16 kHz
pub const FRAME_BYTES: usize = FRAME_SAMPLES * 2; // s16le mono

pub const CMD_IN_FRAME: u8 = b'F';
pub const CMD_IN_MUTE: u8 = b'M';
pub const CMD_IN_SPEAKER: u8 = b'S';
pub const CMD_IN_VOLUME: u8 = b'V';
pub const CMD_IN_DEVICE: u8 = b'D';

pub const CMD_OUT_LEVEL: u8 = b'L';

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => {{
        eprintln!("[audio-io] {}", format!($($t)*));
    }};
}

/// The microphone or the speaker stopped for good: zrtc-media.js turns this
/// line into a notice in the call window. Without it a call with no sound
/// server sent nothing for its whole length and said nothing.
pub fn fault(kind: &str, why: impl std::fmt::Display) {
    crate::log!("fault: {kind} {why}");
}

pub fn to_bytes(pcm: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

pub fn from_bytes(raw: &[u8], out: &mut [i16]) {
    for (i, s) in out.iter_mut().enumerate() {
        *s = i16::from_le_bytes([raw[2 * i], raw[2 * i + 1]]);
    }
}
