//! ZRTPPacket wire format, port of native-engine/zrtp-packet.js (which has the
//! disassembly and capture notes behind every field).
//!
//!  - Media (MsgType 3/5/7/13/15): 1 byte type + u32 LE server token + payload.
//!  - Control (1, client -> server) and reply (2, server -> client):
//!      0 type | 1 magic | 2 u32 0 | 6 u32 seq | 10 u32 local UID | 14 u32 token
//!      18 u16 cmd | 20 u8 subCmd | 21.. payload
//!  - P2P (0x7f): 7f | 00 | role | u32 LE callId | kind | 00 | ...

use crate::json::{num, Value};

pub mod msg {
    pub const CONTROL: u8 = 0x01;
    pub const SERVER_REPLY: u8 = 0x02;
    pub const AUDIO_RTP: u8 = 0x03;
    pub const AUDIO_RTP_DOWN: u8 = 0x04;
    pub const AUDIO_FEC: u8 = 0x05; // RTCP audio in the captures
    pub const AUDIO_RTCP: u8 = 0x07;
    pub const VIDEO_RTP: u8 = 0x0d;
    pub const VIDEO_DOWN: u8 = 0x0e;
    pub const VIDEO_FEC: u8 = 0x0f; // RTCP video
    pub const P2P_EXT: u8 = 0x7f;
}

pub mod cmd {
    pub const PING: u16 = 2;
    pub const END_CALL: u16 = 3;
    pub const ECHO: u16 = 5;
    pub const INIT_CALL_CALLER: u16 = 11;
    pub const INIT_CALL_CALLEE: u16 = 12;
    pub const CHANGE_ADDRESS: u16 = 14;
    pub const REQ_FORWARD: u16 = 32;
    pub const GROUP_MESSAGE: u16 = 33;
    pub const ROOM_UPDATED: u16 = 50;
    pub const INIT_ZAVI_PING: u16 = 0x33;
}

pub const MAGIC_BYTE: u8 = 0x7e;
pub const CONTROL_HEADER_LEN: usize = 21;
pub const MEDIA_HEADER_LEN: usize = 5;

pub fn is_media_type(t: u8) -> bool {
    matches!(t, 0x03 | 0x04 | 0x05 | 0x07 | 0x0d | 0x0e | 0x0f)
}

pub fn wrap_media(msg_type: u8, token: u32, payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(MEDIA_HEADER_LEN + payload.len());
    b.push(msg_type);
    b.extend_from_slice(&token.to_le_bytes());
    b.extend_from_slice(payload);
    b
}

#[derive(Clone, Copy)]
pub struct ControlHeader {
    pub msg_type: u8,
    pub magic: u8,
    pub field02: u32,
    pub seq: u32,
    pub uid: u32,
    pub token: u32,
    pub cmd: u16,
    pub sub_cmd: u8,
}

impl ControlHeader {
    pub fn new(cmd: u16) -> Self {
        ControlHeader { msg_type: msg::CONTROL, magic: MAGIC_BYTE, field02: 0, seq: 0, uid: 0, token: 0, cmd, sub_cmd: 0 }
    }

    pub fn bytes(&self) -> Vec<u8> {
        let mut b = vec![0u8; CONTROL_HEADER_LEN];
        b[0] = self.msg_type;
        b[1] = self.magic;
        b[2..6].copy_from_slice(&self.field02.to_le_bytes());
        b[6..10].copy_from_slice(&self.seq.to_le_bytes());
        b[10..14].copy_from_slice(&self.uid.to_le_bytes());
        b[14..18].copy_from_slice(&self.token.to_le_bytes());
        b[18..20].copy_from_slice(&self.cmd.to_le_bytes());
        b[20] = self.sub_cmd;
        b
    }
}

fn u16str(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u16).to_le_bytes());
    out.extend_from_slice(data);
}

fn u32le(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn hdr(magic: u8, uid: u32, token: u32, seq: u32, cmd: u16, sub_cmd: u8) -> Vec<u8> {
    ControlHeader { magic, uid, token, seq, sub_cmd, ..ControlHeader::new(cmd) }.bytes()
}

/// RequestInitZRTP (cmd 11 caller / 12 callee). An empty extra is not written.
pub fn init_zrtp(caller: bool, uid: u32, peer_uid: u32, call_id: u32, sess_id: &[u8], extra: &[u8], sub_cmd: u8) -> Vec<u8> {
    let mut b = hdr(MAGIC_BYTE, uid, 0, 0, if caller { cmd::INIT_CALL_CALLER } else { cmd::INIT_CALL_CALLEE }, sub_cmd);
    u32le(&mut b, call_id);
    u32le(&mut b, peer_uid);
    u16str(&mut b, sess_id);
    if !extra.is_empty() {
        u16str(&mut b, extra);
    }
    b
}

/// [debugLoopback 0, 5, sending video, (1 on InitZRTP)]
fn group_extra(video_on: bool, init: bool) -> Vec<u8> {
    let mut b = vec![0, 5, video_on as u8];
    if init {
        b.push(1);
    }
    b
}

pub const GROUP_INIT_LAYOUTS: [&str; 4] = ["zero", "sub6", "callId", "callIdHost"];

pub struct GroupInit<'a> {
    pub host: bool,
    pub uid: u32,
    pub sess_id: &'a [u8],
    pub video_on: bool,
    pub call_id: u32,
    pub host_call: u32,
    pub layout: &'a str,
}

pub fn group_init_zrtp(g: &GroupInit) -> Vec<u8> {
    let sub6 = g.layout == "sub6";
    let mut b = hdr(MAGIC_BYTE, g.uid, 0, 0, if g.host { cmd::INIT_CALL_CALLER } else { cmd::INIT_CALL_CALLEE }, if sub6 { 6 } else { 4 });
    if sub6 {
        u32le(&mut b, g.call_id);
        b.extend_from_slice(&[g.host as u8, 5, 0]);
    } else {
        u32le(&mut b, if g.layout == "zero" { 0 } else { g.call_id });
        u32le(&mut b, if g.layout == "callIdHost" { g.host_call } else { 0 });
    }
    u16str(&mut b, g.sess_id);
    u16str(&mut b, &group_extra(g.video_on, true));
    b
}

pub fn share_init_zrtp(uid: u32, sess_id: &[u8]) -> Vec<u8> {
    let mut b = hdr(MAGIC_BYTE, uid, 0, 0, cmd::INIT_CALL_CALLEE, 4);
    u32le(&mut b, 0);
    u32le(&mut b, 0);
    u16str(&mut b, sess_id);
    u16str(&mut b, &[0, 0xff, 1, 0]);
    b
}

pub fn share_zavi_ping(uid: u32, token: u32, sess_id: &[u8]) -> Vec<u8> {
    let mut b = hdr(MAGIC_BYTE, uid, token, 0, cmd::INIT_ZAVI_PING, 1);
    u32le(&mut b, 0);
    u32le(&mut b, 0);
    u16str(&mut b, sess_id);
    b
}

pub fn zavi_ping(uid: u32, token: u32, sess_id: &[u8], video_on: bool) -> Vec<u8> {
    let mut b = share_zavi_ping(uid, token, sess_id);
    u16str(&mut b, &group_extra(video_on, false));
    b
}

/// CMD_ZAVI_ROOM_UPDATED: u32 LE UIDs (None unless a multiple of 4 bytes).
pub fn parse_room_updated(data: &[u8]) -> Option<Vec<u32>> {
    if data.len() % 4 != 0 {
        return None;
    }
    Some(data.chunks(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
}

/// initZRTPPacketRequestPing overwrites the magic with 1.
pub fn ping(uid: u32, token: u32, sess_id: &[u8]) -> Vec<u8> {
    let mut b = hdr(0x01, uid, token, 0, cmd::PING, 1);
    u16str(&mut b, sess_id);
    b
}

pub fn end_call(uid: u32, token: u32, call_id: u32) -> Vec<u8> {
    let mut b = hdr(MAGIC_BYTE, uid, token, 0, cmd::END_CALL, 0);
    u32le(&mut b, call_id);
    b
}

pub fn req_forward(uid: u32, token: u32, seq: u32, sub_cmd: u8, payload: &[u8]) -> Vec<u8> {
    let mut b = hdr(0x01, uid, token, seq, cmd::REQ_FORWARD, sub_cmd);
    b.extend_from_slice(payload);
    b
}

/// ReqForward sub 10 body: [20, a, 1, 1, audio packets received in the last second, b].
pub fn req_forward_status(rx_per_second: u32, a: u32, b5: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(24);
    for v in [20, a, 1, 1, rx_per_second, b5] {
        u32le(&mut b, v);
    }
    b
}

/// sub 13: ask the SFU for each member's layer (u32 UID + u8 qualityId each).
pub fn group_vid_qual_req(uid: u32, token: u32, seq: u32, entries: &[(u32, u8)]) -> Vec<u8> {
    let mut body = vec![entries.len() as u8];
    for (u, q) in entries {
        u32le(&mut body, *u);
        body.push(*q);
    }
    let mut b = hdr(0x01, uid, token, seq, cmd::REQ_FORWARD, 0x0d);
    u16str(&mut b, &body);
    b
}

#[derive(Clone, Debug, PartialEq)]
pub struct LayerEntry {
    pub id: u32,
    pub a: u32,
    pub b: u32,
    pub c: u32,
}

/// sub 12: our layers, 9 bytes each (id, kbps, width, height).
pub fn group_vid_qual_config(uid: u32, token: u32, seq: u32, layers: &[LayerEntry]) -> Vec<u8> {
    let mut body = vec![layers.len() as u8];
    for l in layers {
        body.push(l.id as u8);
        u32le(&mut body, l.a);
        body.extend_from_slice(&(l.b as u16).to_le_bytes());
        body.extend_from_slice(&(l.c as u16).to_le_bytes());
    }
    let mut b = hdr(0x01, uid, token, seq, cmd::REQ_FORWARD, 0x0c);
    u16str(&mut b, &body);
    b
}

/// cmd 33, no body (sub 14 MultiSync, sub 15 reference clock).
pub fn group_sync(uid: u32, token: u32, seq: u32, sub_cmd: u8) -> Vec<u8> {
    hdr(0x01, uid, token, seq, cmd::GROUP_MESSAGE, sub_cmd)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Encode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub key_ms: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VideoPlan {
    pub layers: Vec<LayerEntry>,
    pub encode: Encode,
    pub send: Vec<LayerEntry>,
    pub sfu: Option<crate::rtp::SfuExt>,
    pub rows: Vec<Row>,
    pub start: usize,
}

fn trunc(v: f64) -> i64 {
    v.trunc() as i64
}

/// Layers the macOS client announces on sub 12, from zrtcConfig.groupcall
/// (GroupZRtcConfig::fromJson + BuildQualityBitrateConfigs). None when the
/// table is empty or unusable: the real sender then skips the packet.
/// `cam_layer` is ZCALL_GROUP_CAM_LAYER (the spatial row we encode, 1 by default).
pub fn group_video_plan(config: &Value, cam_layer: Option<f64>) -> Option<VideoPlan> {
    let config = config.as_object()?;
    let gc = config.get("groupcall")?.as_object()?;
    let spatial_layers = gc.get("spatialLayers")?.as_array()?;
    let tn = num(gc.get("temporalNum"));
    let temporal = if tn.is_finite() { (trunc(tn) & 0xff) as u32 } else { 0 };
    if temporal == 0 {
        return None;
    }
    let pos = |k: &str, d: i64| {
        let v = num(config.get(k));
        if v > 0.0 { trunc(v) } else { d }
    };
    let (max_w, max_h, key_ms) = (pos("maxWidth", 1280), pos("maxHeight", 720), pos("keyFrameIntervalMs", 1000));
    let mut layers = Vec::new();
    let mut rows = Vec::new();
    let mut spatial = 0u32;
    for raw in spatial_layers {
        let Some(raw) = raw.as_object() else { continue };
        let (fnum, fden) = (num(raw.get("factorNum")), num(raw.get("factorDen")));
        if !fnum.is_finite() || !fden.is_finite() {
            continue;
        }
        let n = trunc(fnum) & 0xff;
        let d = trunc(fden) & 0xff;
        let kbps = num(raw.get("bitrateKbps"));
        let fps = num(raw.get("fps"));
        if d == 0 || !kbps.is_finite() || !fps.is_finite() || fps <= 0.0 {
            continue;
        }
        let width = trunc(max_w as f64 * n as f64 / d as f64) & !1;
        let height = trunc(max_h as f64 * n as f64 / d as f64) & !1;
        if width <= 0 || height <= 0 {
            continue;
        }
        let divided = trunc(kbps);
        for t in 0..temporal {
            // JS `1 << n`: n is taken mod 32, and 1 << 31 is negative.
            let shift = (temporal - 1 - t) & 31;
            let divisor = if shift == 31 { -2147483648.0 } else { (1u64 << shift) as f64 };
            let wire = (divided as f64 / divisor).trunc() as i64 as u32; // >>> 0
            layers.push(LayerEntry { id: spatial * 3 + t, a: wire, b: width as u32, c: height as u32 });
            if t == 0 {
                rows.push(Row { width: width as u32, height: height as u32, fps: trunc(fps) as u32, kbps: wire });
            }
        }
        spatial += 1;
    }
    if layers.is_empty() || rows.is_empty() {
        return None;
    }
    let want = cam_layer.unwrap_or(1.0);
    let want = if want.is_finite() { trunc(want) } else { 0 };
    let start = (want.max(0) as usize).min(rows.len() - 1);
    let row = rows[start].clone();
    let encode = Encode { width: row.width, height: row.height, fps: row.fps, bitrate: row.kbps * 1000, key_ms: key_ms as u32 };
    let send = vec![LayerEntry { id: 0, a: row.kbps, b: row.width, c: row.height }];
    let codec = num(gc.get("codec"));
    let sfu = if codec == 60.0 || codec == 61.0 {
        Some(crate::rtp::SfuExt { id: 12, long: false })
    } else if codec == 4.0 {
        None
    } else {
        Some(crate::rtp::SfuExt { id: 14, long: true })
    };
    Some(VideoPlan { layers, encode, send, sfu, rows, start })
}

// --- P2P -----------------------------------------------------------------------

pub mod p2p {
    pub const HEADER_LEN: usize = 9;
    pub const BIND: u8 = 1;
    pub const BIND_PEER: u8 = 2;
    pub const BIND_ANSWER: u8 = 3;
    pub const KEEPALIVE: u8 = 4;
    pub const AUDIO: u8 = 6;
    pub const AUDIO_RTCP: u8 = 7;
    pub const VIDEO: u8 = 8;
    pub const VIDEO_RTCP: u8 = 9;
}

fn p2p_header(callee: bool, call_id: u32, kind: u8) -> Vec<u8> {
    let mut h = vec![0u8; p2p::HEADER_LEN];
    h[0] = msg::P2P_EXT;
    h[2] = callee as u8;
    h[3..7].copy_from_slice(&call_id.to_le_bytes());
    h[7] = kind;
    h
}

pub fn p2p_bind(callee: bool, call_id: u32, seq: u16, index: u32) -> Vec<u8> {
    let mut b = p2p_header(callee, call_id, p2p::BIND);
    b.extend_from_slice(&seq.to_le_bytes());
    b.extend_from_slice(&index.to_be_bytes());
    b
}

/// Answer a kind 2 from the peer, seen coming from peer_addr ("ip|port").
pub fn p2p_bind_answer(callee: bool, call_id: u32, request: &[u8], peer_addr: &str) -> Vec<u8> {
    let mut b = p2p_header(callee, call_id, p2p::BIND_ANSWER);
    b.extend_from_slice(&request[9.min(request.len())..15.min(request.len())]);
    let addr: Vec<u8> = peer_addr.chars().map(|c| c as u32 as u8).collect(); // latin1
    u32le(&mut b, addr.len() as u32);
    b.extend_from_slice(&addr);
    b
}

pub fn p2p_pong(callee: bool, ping: &[u8]) -> Vec<u8> {
    let mut pong = ping.to_vec();
    if pong.len() > 2 {
        pong[2] = callee as u8;
    }
    if pong.len() > 9 {
        pong[9] = 2;
    }
    pong
}

pub fn p2p_wrap(callee: bool, call_id: u32, kind: u8, body: &[u8]) -> Vec<u8> {
    let mut b = p2p_header(callee, call_id, kind);
    b.extend_from_slice(body);
    b
}

// --- parsing -------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Packet<'a> {
    /// Media: RTP / RTCP after the 1-byte (downlink) or 5-byte header.
    Media { msg_type: u8, token: Option<u32>, payload: &'a [u8] },
    MediaTruncated { msg_type: u8 },
    Control(Control<'a>),
    P2p { b1: u8, b2: u8, field38: u32, cmd: u16, data: &'a [u8] },
    Other { msg_type: u8 },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Control<'a> {
    pub msg_type: u8,
    pub magic: u8,
    pub field02: u32,
    pub seq: u32,
    pub uid: u32,
    pub token: u32,
    pub cmd: u16,
    pub sub_cmd: u8,
    pub data: &'a [u8],
    /// InitZRTP reply: result (0 = accepted), callId, media token, our public "ip|port".
    pub res: Option<u32>,
    pub call_id: Option<u32>,
    pub server_token: Option<u32>,
    pub public_addr: Option<String>,
    pub peer_uid: Option<u32>,
    pub sess_id: Option<&'a [u8]>,
    pub extra: Option<&'a [u8]>,
}

fn le32(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

fn le16(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}

fn slice(d: &[u8], a: usize, b: usize) -> &[u8] {
    let a = a.min(d.len());
    &d[a..b.min(d.len()).max(a)]
}

pub fn unwrap(buf: &[u8]) -> Option<Packet<'_>> {
    let &t = buf.first()?;
    if t == msg::AUDIO_RTP_DOWN || t == msg::VIDEO_DOWN {
        return Some(Packet::Media { msg_type: t, token: None, payload: &buf[1..] });
    }
    if is_media_type(t) {
        if buf.len() < MEDIA_HEADER_LEN {
            return Some(Packet::MediaTruncated { msg_type: t });
        }
        return Some(Packet::Media { msg_type: t, token: Some(le32(buf, 1)), payload: &buf[5..] });
    }
    if (t == msg::CONTROL || t == msg::SERVER_REPLY) && buf.len() >= CONTROL_HEADER_LEN {
        let data = &buf[CONTROL_HEADER_LEN..];
        let mut c = Control {
            msg_type: t,
            magic: buf[1],
            field02: le32(buf, 2),
            seq: le32(buf, 6),
            uid: le32(buf, 10),
            token: le32(buf, 14),
            cmd: le16(buf, 18),
            sub_cmd: buf[20],
            data,
            ..Default::default()
        };
        let init = c.cmd == cmd::INIT_CALL_CALLER || c.cmd == cmd::INIT_CALL_CALLEE;
        if init && t == msg::SERVER_REPLY && data.len() >= 4 {
            let res = le32(data, 0);
            c.res = Some(res);
            if res == 0 && data.len() >= 12 {
                c.call_id = Some(le32(data, 4));
                c.server_token = Some(le32(data, 8));
                if data.len() >= 14 {
                    let n = le16(data, 12) as usize;
                    c.public_addr = Some(slice(data, 14, 14 + n).iter().map(|&b| b as char).collect());
                }
            }
        } else if init && data.len() >= 10 {
            c.call_id = Some(le32(data, 0));
            c.peer_uid = Some(le32(data, 4));
            let n1 = le16(data, 8) as usize;
            c.sess_id = Some(slice(data, 10, 10 + n1));
            let off = 10 + n1;
            if data.len() >= off + 2 {
                let n2 = le16(data, off) as usize;
                c.extra = Some(slice(data, off + 2, off + 2 + n2));
            }
        } else if c.cmd == cmd::PING && data.len() >= 2 {
            c.sess_id = Some(slice(data, 2, 2 + le16(data, 0) as usize));
        } else if c.cmd == cmd::END_CALL && data.len() >= 4 {
            c.call_id = Some(le32(data, 0));
        }
        return Some(Packet::Control(c));
    }
    if t == msg::P2P_EXT && buf.len() >= 9 {
        return Some(Packet::P2p { b1: buf[1], b2: buf[2], field38: le32(buf, 3), cmd: le16(buf, 7), data: &buf[9..] });
    }
    Some(Packet::Other { msg_type: t })
}
