//! RTP headers of what we send, port of native-engine/rtp.js.
//!
//! 1-1 audio, as ZaloCall writes it:
//!   90 70 | seq | ts | ssrc | be de 00 01 | 51 <tw seq hi> <tw seq lo> 00
//! V=2, X=1, PT 112 (opus/16000/1), timestamp +320 per 20 ms frame, SSRC =
//! local UID, one-byte header extension id 5 = transport-wide sequence.
//! Group audio and video add the extensions the macOS client sends (audio
//! level id 2, abs-send-time id 3, SFU layer id 12/14, capture NTP time id 13);
//! see rtp.js for the captures behind each byte.

pub const AUDIO_PT: u8 = 112;
pub const FRAME_SAMPLES: u32 = 320;
pub const TWCC_EXT_ID: u8 = 5;
const AUDIO_LEVEL_EXT_ID: u8 = 2;
const ABS_SEND_TIME_EXT_ID: u8 = 3;
const CAPTURE_NTP_EXT_ID: u8 = 13;
const NTP_UNIX_OFFSET: f64 = 2208988800.0;

fn fixed(h: &mut [u8], seq: u16, ts: u32, ssrc: u32, marker: bool, pt: u8) {
    h[0] = 0x90;
    h[1] = (if marker { 0x80 } else { 0 }) | (pt & 0x7f);
    h[2..4].copy_from_slice(&seq.to_be_bytes());
    h[4..8].copy_from_slice(&ts.to_be_bytes());
    h[8..12].copy_from_slice(&ssrc.to_be_bytes());
}

pub fn audio_header(seq: u16, ts: u32, ssrc: u32, tw_seq: u16, marker: bool, pt: u8) -> Vec<u8> {
    let mut h = vec![0u8; 20];
    fixed(&mut h, seq, ts, ssrc, marker, pt);
    h[12..14].copy_from_slice(&0xbedeu16.to_be_bytes());
    h[14..16].copy_from_slice(&1u16.to_be_bytes());
    h[16] = (TWCC_EXT_ID << 4) | 1;
    h[17..19].copy_from_slice(&tw_seq.to_be_bytes());
    h
}

/// id 13 element at o: header byte, low 24 bits of the NTP seconds, 32-bit fraction.
fn write_capture_ntp(h: &mut [u8], mut o: usize, capture_ms: f64) -> usize {
    h[o] = (CAPTURE_NTP_EXT_ID << 4) | 6;
    o += 1;
    let ntp = capture_ms / 1000.0 + NTP_UNIX_OFFSET;
    let sec = ntp.floor();
    let frac = ((ntp - sec) * 4294967296.0).floor() as u64 as u32;
    let sec = sec as i64;
    h[o] = (sec >> 16) as u8;
    h[o + 1] = (sec >> 8) as u8;
    h[o + 2] = sec as u8;
    h[o + 3..o + 7].copy_from_slice(&frac.to_be_bytes());
    o + 7
}

pub struct GroupAudio {
    pub seq: u16,
    pub ts: u32,
    pub ssrc: u32,
    pub tw_seq: u16,
    pub marker: bool,
    pub pt: u8,
    pub capture_ms: f64,
    pub level: u8,
}

pub fn group_audio_header(a: &GroupAudio) -> Vec<u8> {
    let mut h = vec![0u8; 32];
    fixed(&mut h, a.seq, a.ts, a.ssrc, a.marker, a.pt);
    h[12..14].copy_from_slice(&0xbedeu16.to_be_bytes());
    h[14..16].copy_from_slice(&4u16.to_be_bytes());
    h[16] = AUDIO_LEVEL_EXT_ID << 4;
    h[17] = a.level.min(127);
    h[18] = (TWCC_EXT_ID << 4) | 1;
    h[19..21].copy_from_slice(&a.tw_seq.to_be_bytes());
    write_capture_ntp(&mut h, 21, a.capture_ms);
    h
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SfuExt {
    pub id: u8,
    pub long: bool,
}

pub struct GroupVideo {
    pub seq: u16,
    pub ts: u32,
    pub ssrc: u32,
    pub tw_seq: u16,
    pub marker: bool,
    pub pt: u8,
    pub sfu: Option<SfuExt>,
    pub layer_seq: u16,
    pub capture_ms: f64,
    pub send_ms: f64,
}

/// Byte 0 of the SFU extension (ZCALL_SFU_BYTE0, hex, overrides it for a test).
pub fn sfu_byte0() -> u8 {
    std::env::var("ZCALL_SFU_BYTE0").ok().and_then(|v| u8::from_str_radix(v.trim().trim_start_matches("0x"), 16).ok()).unwrap_or(0x03)
}

pub fn group_video_header(v: &GroupVideo) -> Vec<u8> {
    let long = v.sfu.map_or(false, |s| s.long);
    let id = v.sfu.map(|s| s.id).filter(|&i| i != 0).unwrap_or(if long { 14 } else { 12 });
    let data_len = if long { 7 } else { 3 };
    let ext_bytes = 4 + 3 + (1 + data_len) + 8;
    let padded = (ext_bytes + 3) & !3;
    let mut h = vec![0u8; 12 + 4 + padded];
    fixed(&mut h, v.seq, v.ts, v.ssrc, v.marker, v.pt);
    h[12..14].copy_from_slice(&0xbedeu16.to_be_bytes());
    h[14..16].copy_from_slice(&((padded / 4) as u16).to_be_bytes());
    let mut o = 16;
    h[o] = (ABS_SEND_TIME_EXT_ID << 4) | 2;
    o += 1;
    let abs = ((v.send_ms * 262.144).floor() as i64).rem_euclid(0x1000000);
    h[o] = (abs >> 16) as u8;
    h[o + 1] = (abs >> 8) as u8;
    h[o + 2] = abs as u8;
    o += 3;
    h[o] = (TWCC_EXT_ID << 4) | 1;
    h[o + 1..o + 3].copy_from_slice(&v.tw_seq.to_be_bytes());
    o += 3;
    h[o] = ((id & 0x0f) << 4) | if long { 6 } else { 2 };
    h[o + 1] = sfu_byte0();
    h[o + 2..o + 4].copy_from_slice(&v.layer_seq.to_be_bytes());
    o += 4;
    if long {
        o += 4;
    }
    write_capture_ntp(&mut h, o, v.capture_ms);
    h
}
