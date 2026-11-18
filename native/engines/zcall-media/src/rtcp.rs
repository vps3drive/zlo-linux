//! RTCP the receiver owes the phone, port of native-engine/rtcp.js (which has
//! the captures behind each packet).
//!
//! Audio RTCP (relay 0x05, P2P kind 7): transport-cc every ~50 ms covering
//! audio and video together, SR + SDES ~1/s. Video RTCP (relay 0x0F, P2P
//! kind 9): RR + SDES, NACK, PLI. Sender SSRC = our UID, media SSRC = the
//! peer's UID. Formats per RFC 3550 / 4585 / transport-wide-cc-01.

use std::collections::{HashMap, HashSet, VecDeque};

pub const TWCC_EXT_ID: u8 = 5;
const NTP_EPOCH_OFFSET: f64 = 2208988800.0;
const MAX_TWCC_PACKETS: i64 = 400;

/// JS Math.round.
pub fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

fn be16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Transport-wide sequence number of an RTP packet (one-byte extension id 5).
pub fn read_tw_seq(rtp: &[u8]) -> Option<u16> {
    if rtp.len() < 16 || rtp[0] & 0x10 == 0 {
        return None;
    }
    let mut o = 12 + 4 * (rtp[0] & 0x0f) as usize;
    if o + 4 > rtp.len() || be16(rtp, o) != 0xbede {
        return None;
    }
    let end = rtp.len().min(o + 4 + 4 * be16(rtp, o + 2) as usize);
    o += 4;
    while o < end {
        let b = rtp[o];
        if b == 0 {
            o += 1;
            continue;
        }
        let id = b >> 4;
        let len = (b & 0x0f) as usize + 1;
        if id == 15 {
            return None;
        }
        if id == TWCC_EXT_ID && len == 2 && o + 3 <= end {
            return Some(be16(rtp, o + 1));
        }
        o += 1 + len;
    }
    None
}

// --- transport-cc --------------------------------------------------------------

#[derive(Default)]
pub struct TwccRecorder {
    arrivals: HashMap<i64, i64>, // unwrapped seq -> arrival in 250 us units
    top: Option<i64>,
    next: Option<i64>,
    fb_count: u32,
    pub sent: u64,
}

impl TwccRecorder {
    fn unwrap_seq(&self, seq: i64) -> i64 {
        let Some(top) = self.top else { return seq };
        let mut u = seq + (top - (top & 0xffff));
        if u - top > 0x8000 {
            u -= 0x10000;
        } else if top - u > 0x8000 {
            u += 0x10000;
        }
        u
    }

    pub fn add(&mut self, seq: u16, now_ms: f64) {
        let u = self.unwrap_seq(seq as i64);
        if self.next.is_none() {
            self.next = Some(u);
        }
        if u < self.next.unwrap() || self.arrivals.contains_key(&u) {
            return; // already reported, or relay + P2P twin
        }
        self.arrivals.insert(u, js_round(now_ms * 4.0) as i64);
        if self.top.map_or(true, |t| u > t) {
            self.top = Some(u);
        }
    }

    pub fn pending(&self) -> bool {
        matches!((self.next, self.top), (Some(n), Some(t)) if t >= n)
    }

    /// Feedback packets for what arrived since the last call (usually one).
    pub fn build(&mut self, sender_ssrc: u32, media_ssrc: u32) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while self.pending() {
            let base = self.next.unwrap();
            let end = self.top.unwrap().min(base + MAX_TWCC_PACKETS - 1);
            if !(base..=end).any(|s| self.arrivals.contains_key(&s)) {
                self.next = Some(end + 1);
                continue;
            }
            out.push(encode_twcc(sender_ssrc, media_ssrc, base, end, &self.arrivals, self.fb_count & 0xff));
            self.fb_count += 1;
            for s in base..=end {
                self.arrivals.remove(&s);
            }
            self.next = Some(end + 1);
            self.sent += 1;
        }
        out
    }
}

pub fn encode_twcc(sender_ssrc: u32, media_ssrc: u32, base: i64, end: i64, arrivals: &HashMap<i64, i64>, fb_count: u32) -> Vec<u8> {
    let count = end - base + 1;
    let first_q = (base..=end).find_map(|s| arrivals.get(&s).copied()).unwrap();
    let ref_units = first_q.div_euclid(256);
    let mut prev_q = ref_units * 256;
    let mut symbols = Vec::with_capacity(count as usize);
    let mut deltas = Vec::new();
    for s in base..=end {
        match arrivals.get(&s) {
            None => symbols.push(0u16),
            Some(&q) => {
                let d = q - prev_q;
                prev_q = q;
                if (0..=255).contains(&d) {
                    symbols.push(1);
                    deltas.push(d);
                } else {
                    symbols.push(2);
                    deltas.push(d.clamp(-32768, 32767));
                }
            }
        }
    }
    let mut chunks: Vec<u16> = Vec::new();
    let mut i = 0;
    while i < symbols.len() {
        let mut run = 1;
        while i + run < symbols.len() && symbols[i + run] == symbols[i] && run < 0x1fff {
            run += 1;
        }
        if run >= 8 {
            chunks.push((symbols[i] << 13) | run as u16);
            i += run;
            continue;
        }
        let mut v: u16 = 0xc000; // status vector, 2-bit symbols, 7 per chunk
        for k in 0..7 {
            let sym = symbols.get(i + k).copied().unwrap_or(0);
            v |= sym << (12 - 2 * k);
        }
        chunks.push(v);
        i += 7;
    }
    let delta_bytes: usize = symbols.iter().map(|&s| match s { 1 => 1, 2 => 2, _ => 0 }).sum();
    let len = 4 + 8 + 8 + 2 * chunks.len() + delta_bytes;
    let padded = (len + 3) & !3;
    let mut b = vec![0u8; padded];
    b[0] = 0x8f;
    b[1] = 205;
    b[2..4].copy_from_slice(&((padded / 4 - 1) as u16).to_be_bytes());
    b[4..8].copy_from_slice(&sender_ssrc.to_be_bytes());
    b[8..12].copy_from_slice(&media_ssrc.to_be_bytes());
    b[12..14].copy_from_slice(&((base & 0xffff) as u16).to_be_bytes());
    b[14..16].copy_from_slice(&(count as u16).to_be_bytes());
    let r = (ref_units & 0xffffff) as u32;
    b[16] = (r >> 16) as u8;
    b[17] = (r >> 8) as u8;
    b[18] = r as u8;
    b[19] = fb_count as u8;
    let mut o = 20;
    for c in &chunks {
        b[o..o + 2].copy_from_slice(&c.to_be_bytes());
        o += 2;
    }
    let mut di = 0;
    for &s in &symbols {
        if s == 1 {
            b[o] = deltas[di] as u8;
            o += 1;
            di += 1;
        } else if s == 2 {
            b[o..o + 2].copy_from_slice(&(deltas[di] as i16).to_be_bytes());
            o += 2;
            di += 1;
        }
    }
    b
}

#[derive(Debug, Clone, PartialEq)]
pub struct TwccFeedback {
    pub sender_ssrc: u32,
    pub media_ssrc: u32,
    pub base: u16,
    pub count: u16,
    pub ref_units: u32,
    pub fb_count: u8,
    pub status: Vec<u8>,
    pub delta_q: Vec<i32>,
}

pub fn parse_twcc(b: &[u8]) -> Option<TwccFeedback> {
    if b.len() < 20 || b[0] & 0x1f != 15 || b[1] != 205 {
        return None;
    }
    let count = be16(b, 14);
    let mut out = TwccFeedback {
        sender_ssrc: be32(b, 4),
        media_ssrc: be32(b, 8),
        base: be16(b, 12),
        count,
        ref_units: (b[16] as u32) << 16 | (b[17] as u32) << 8 | b[18] as u32,
        fb_count: b[19],
        status: Vec::new(),
        delta_q: Vec::new(),
    };
    let count = count as usize;
    let mut o = 20;
    while out.status.len() < count && o + 2 <= b.len() {
        let c = be16(b, o);
        o += 2;
        if c & 0x8000 == 0 {
            let sym = ((c >> 13) & 3) as u8;
            let run = (c & 0x1fff) as usize;
            for _ in 0..run {
                if out.status.len() >= count {
                    break;
                }
                out.status.push(sym);
            }
        } else if c & 0x4000 != 0 {
            for k in 0..7 {
                if out.status.len() >= count {
                    break;
                }
                out.status.push(((c >> (12 - 2 * k)) & 3) as u8);
            }
        } else {
            for k in 0..14 {
                if out.status.len() >= count {
                    break;
                }
                out.status.push(((c >> (13 - k)) & 1) as u8);
            }
        }
    }
    // A truncated packet: JS reads undefined / throws past the end; stop instead.
    for &s in &out.status {
        if s == 1 {
            let Some(&v) = b.get(o) else { break };
            out.delta_q.push(v as i32);
            o += 1;
        } else if s == 2 {
            if o + 2 > b.len() {
                break;
            }
            out.delta_q.push(i16::from_be_bytes([b[o], b[o + 1]]) as i32);
            o += 2;
        }
    }
    Some(out)
}

// --- reports, key frame and NACK requests ----------------------------------------

pub fn ntp_now(now_ms: f64) -> (u32, u32) {
    let sec = (now_ms / 1000.0).floor();
    let frac = (((now_ms % 1000.0) / 1000.0) * 4294967296.0).floor();
    (((sec + NTP_EPOCH_OFFSET) as i64) as u32, (frac as i64) as u32)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReportBlock {
    pub ssrc: u32,
    pub fraction_lost: u8,
    pub cum_lost: i32,
    pub ext_high_seq: u32,
    pub jitter: u32,
    pub lsr: u32,
    pub dlsr: u32,
}

fn report_block(b: &mut [u8], o: usize, blk: &ReportBlock) {
    b[o..o + 4].copy_from_slice(&blk.ssrc.to_be_bytes());
    b[o + 4] = blk.fraction_lost;
    let c = (blk.cum_lost as u32) & 0xffffff;
    b[o + 5] = (c >> 16) as u8;
    b[o + 6] = (c >> 8) as u8;
    b[o + 7] = c as u8;
    b[o + 8..o + 12].copy_from_slice(&blk.ext_high_seq.to_be_bytes());
    b[o + 12..o + 16].copy_from_slice(&blk.jitter.to_be_bytes());
    b[o + 16..o + 20].copy_from_slice(&blk.lsr.to_be_bytes());
    b[o + 20..o + 24].copy_from_slice(&blk.dlsr.to_be_bytes());
}

/// Sender report (PT 200) with reception blocks, as ZaloCall's 52-byte SR.
pub fn build_sr(ssrc: u32, now_ms: f64, rtp_ts: u32, packets: u32, octets: u32, blocks: &[ReportBlock]) -> Vec<u8> {
    let mut b = vec![0u8; 28 + 24 * blocks.len()];
    b[0] = 0x80 | blocks.len() as u8;
    b[1] = 200;
    let words = (b.len() / 4 - 1) as u16;
    b[2..4].copy_from_slice(&words.to_be_bytes());
    b[4..8].copy_from_slice(&ssrc.to_be_bytes());
    let (msw, lsw) = ntp_now(now_ms);
    b[8..12].copy_from_slice(&msw.to_be_bytes());
    b[12..16].copy_from_slice(&lsw.to_be_bytes());
    b[16..20].copy_from_slice(&rtp_ts.to_be_bytes());
    b[20..24].copy_from_slice(&packets.to_be_bytes());
    b[24..28].copy_from_slice(&octets.to_be_bytes());
    for (i, blk) in blocks.iter().enumerate() {
        report_block(&mut b, 28 + 24 * i, blk);
    }
    b
}

/// Receiver report (PT 201), for a stream we only receive (video).
pub fn build_rr(ssrc: u32, blocks: &[ReportBlock]) -> Vec<u8> {
    let mut b = vec![0u8; 8 + 24 * blocks.len()];
    b[0] = 0x80 | blocks.len() as u8;
    b[1] = 201;
    let words = (b.len() / 4 - 1) as u16;
    b[2..4].copy_from_slice(&words.to_be_bytes());
    b[4..8].copy_from_slice(&ssrc.to_be_bytes());
    for (i, blk) in blocks.iter().enumerate() {
        report_block(&mut b, 8 + 24 * i, blk);
    }
    b
}

/// SDES with an empty CNAME: the 12 bytes ZaloCall appends.
pub fn build_sdes(ssrc: u32) -> Vec<u8> {
    let mut b = vec![0u8; 12];
    b[0] = 0x81;
    b[1] = 202;
    b[2..4].copy_from_slice(&2u16.to_be_bytes());
    b[4..8].copy_from_slice(&ssrc.to_be_bytes());
    b[8] = 1;
    b
}

pub fn build_pli(sender_ssrc: u32, media_ssrc: u32) -> Vec<u8> {
    let mut b = vec![0u8; 12];
    b[0] = 0x81;
    b[1] = 206;
    b[2..4].copy_from_slice(&2u16.to_be_bytes());
    b[4..8].copy_from_slice(&sender_ssrc.to_be_bytes());
    b[8..12].copy_from_slice(&media_ssrc.to_be_bytes());
    b
}

/// Generic NACK (RTPFB fmt 1): one PID + BLP per 17 packets, seqs in order.
pub fn build_nack(sender_ssrc: u32, media_ssrc: u32, seqs: &[u16]) -> Vec<u8> {
    let mut items: Vec<(u16, u16)> = Vec::new();
    let mut i = 0;
    while i < seqs.len() {
        let pid = seqs[i];
        let mut blp: u16 = 0;
        let mut j = i + 1;
        while j < seqs.len() {
            let d = seqs[j].wrapping_sub(pid);
            if !(1..=16).contains(&d) {
                break;
            }
            blp |= 1 << (d - 1);
            j += 1;
        }
        items.push((pid, blp));
        i = j;
    }
    let mut b = vec![0u8; 12 + 4 * items.len()];
    b[0] = 0x81;
    b[1] = 205;
    let words = (b.len() / 4 - 1) as u16;
    b[2..4].copy_from_slice(&words.to_be_bytes());
    b[4..8].copy_from_slice(&sender_ssrc.to_be_bytes());
    b[8..12].copy_from_slice(&media_ssrc.to_be_bytes());
    for (k, (pid, blp)) in items.iter().enumerate() {
        b[12 + 4 * k..14 + 4 * k].copy_from_slice(&pid.to_be_bytes());
        b[14 + 4 * k..16 + 4 * k].copy_from_slice(&blp.to_be_bytes());
    }
    b
}

// Lost video packets per sender: a hole of at most 64 is asked for at once,
// then every 40 ms, five times, forgotten after 400 ms. A larger jump is a
// seek (the caller asks for a key frame). A packet over half the space behind
// the highest is old and never moves the highest back.
const NACK_MAX_HOLE: u16 = 64;
const NACK_RETRY_MS: f64 = 40.0;
const NACK_MAX_TRIES: u32 = 5;
const NACK_DROP_MS: f64 = 400.0;
const NACK_KEEP: usize = 128;

struct Miss {
    seq: u16,
    at: f64,
    tries: u32,
}

struct NackStream {
    highest: u16,
    miss: Vec<Miss>, // insertion order, as the JS Map
}

#[derive(Default)]
pub struct NackTracker {
    streams: HashMap<u32, NackStream>,
}

impl NackTracker {
    /// One received RTP sequence. Returns (ssrc, seqs) to NACK now.
    pub fn note(&mut self, ssrc: u32, seq: u16, now: f64) -> Vec<(u32, Vec<u16>)> {
        let Some(st) = self.streams.get_mut(&ssrc) else {
            self.streams.insert(ssrc, NackStream { highest: seq, miss: Vec::new() });
            return Vec::new();
        };
        let ahead = seq.wrapping_sub(st.highest);
        if ahead == 0 {
            return Self::due(st, ssrc, now);
        }
        if ahead >= 0x8000 {
            st.miss.retain(|m| m.seq != seq);
            return Self::due(st, ssrc, now);
        }
        if ahead > NACK_MAX_HOLE {
            st.highest = seq;
            st.miss.clear();
            return Vec::new();
        }
        for d in 1..ahead {
            let missing = st.highest.wrapping_add(d);
            if !st.miss.iter().any(|m| m.seq == missing) {
                st.miss.push(Miss { seq: missing, at: now, tries: 0 });
            }
        }
        st.highest = seq;
        st.miss.retain(|m| m.seq != seq);
        while st.miss.len() > NACK_KEEP {
            st.miss.remove(0);
        }
        Self::due(st, ssrc, now)
    }

    fn due(st: &mut NackStream, ssrc: u32, now: f64) -> Vec<(u32, Vec<u16>)> {
        let mut seqs = Vec::new();
        st.miss.retain_mut(|m| {
            if now - m.at > NACK_DROP_MS {
                return false;
            }
            if m.tries >= NACK_MAX_TRIES || now < m.at + m.tries as f64 * NACK_RETRY_MS {
                return true;
            }
            m.tries += 1;
            seqs.push(m.seq);
            true
        });
        if seqs.is_empty() {
            return Vec::new();
        }
        let high = st.highest;
        // Oldest first, so 65535 then 0 stays one NACK item across the wrap (stable, as JS sort).
        seqs.sort_by_key(|&s| std::cmp::Reverse(high.wrapping_sub(s)));
        vec![(ssrc, seqs)]
    }
}

// --- reception statistics (RFC 3550 A.1 / A.3 / 6.4.1) -----------------------------

pub struct RxStats {
    clock_hz: f64,
    pub ssrc: u32,
    pub started: bool,
    base_seq: i64,
    max_seq: u16,
    cycles: i64,
    received: i64,
    expected_prior: i64,
    received_prior: i64,
    jitter: f64,
    transit: Option<f64>,
    seen: HashSet<i64>,
    seen_order: VecDeque<i64>,
    lsr: u32,
    lsr_at: f64,
}

impl RxStats {
    pub fn new(clock_hz: u32) -> Self {
        RxStats {
            clock_hz: clock_hz as f64,
            ssrc: 0,
            started: false,
            base_seq: 0,
            max_seq: 0,
            cycles: 0,
            received: 0,
            expected_prior: 0,
            received_prior: 0,
            jitter: 0.0,
            transit: None,
            seen: HashSet::new(),
            seen_order: VecDeque::new(),
            lsr: 0,
            lsr_at: 0.0,
        }
    }

    pub fn on_packet(&mut self, rtp: &[u8], now_ms: f64) {
        let seq = be16(rtp, 2);
        let ts = be32(rtp, 4);
        if !self.started {
            self.started = true;
            self.ssrc = be32(rtp, 8);
            self.base_seq = seq as i64;
            self.max_seq = seq;
        } else {
            let d = seq.wrapping_sub(self.max_seq);
            if d != 0 && d < 0x8000 {
                if seq < self.max_seq {
                    self.cycles += 0x10000;
                }
                self.max_seq = seq;
            }
        }
        let ext = self.cycles + seq as i64;
        if !self.seen.insert(ext) {
            return;
        }
        self.seen_order.push_back(ext);
        if self.seen.len() > 512 {
            if let Some(old) = self.seen_order.pop_front() {
                self.seen.remove(&old);
            }
        }
        self.received += 1;
        let arrival = now_ms * self.clock_hz / 1000.0;
        let transit = arrival - ts as f64;
        if let Some(prev) = self.transit {
            let dd = (transit - prev).abs();
            self.jitter += (dd - self.jitter) / 16.0;
        }
        self.transit = Some(transit);
    }

    pub fn on_sr(&mut self, buf: &[u8], now_ms: f64) {
        if buf.len() < 28 {
            return;
        }
        self.lsr = ((be32(buf, 8) & 0xffff) << 16) | (be32(buf, 12) >> 16);
        self.lsr_at = now_ms;
    }

    pub fn block(&mut self, now_ms: f64) -> Option<ReportBlock> {
        if !self.started {
            return None;
        }
        let ext_high = self.cycles + self.max_seq as i64;
        let expected = ext_high - self.base_seq + 1;
        let lost = expected - self.received;
        let exp_int = expected - self.expected_prior;
        let rec_int = self.received - self.received_prior;
        self.expected_prior = expected;
        self.received_prior = self.received;
        let lost_int = exp_int - rec_int;
        let fraction = if exp_int <= 0 || lost_int <= 0 { 0 } else { ((lost_int * 256) as f64 / exp_int as f64).floor().min(255.0) as u8 };
        Some(ReportBlock {
            ssrc: self.ssrc,
            fraction_lost: fraction,
            cum_lost: lost.clamp(-0x800000, 0x7fffff) as i32,
            ext_high_seq: ext_high as u32,
            jitter: js_round(self.jitter) as i64 as u32,
            lsr: self.lsr,
            dlsr: if self.lsr_at != 0.0 { js_round(((now_ms - self.lsr_at) * 65536.0) / 1000.0) as i64 as u32 } else { 0 },
        })
    }
}

#[derive(Default, Debug, Clone, Copy, PartialEq)]
pub struct FeedbackCounters {
    pub twcc: u64,
    pub reports: u64,
    pub pli: u64,
    pub nack: u64,
}

/// All of it, for one call: fed every received RTP / RTCP packet, asked for what to send.
pub struct RtcpFeedback {
    pub local_ssrc: u32,
    pub twcc: TwccRecorder,
    pub audio: RxStats,
    pub video: RxStats,
    pub peer_ssrc: u32,
    pub counters: FeedbackCounters,
}

#[derive(PartialEq, Clone, Copy)]
pub enum Kind {
    Audio,
    Video,
}

impl RtcpFeedback {
    pub fn new(local_ssrc: u32) -> Self {
        RtcpFeedback {
            local_ssrc,
            twcc: TwccRecorder::default(),
            audio: RxStats::new(16000),
            video: RxStats::new(90000),
            peer_ssrc: 0,
            counters: FeedbackCounters::default(),
        }
    }

    pub fn on_rtp(&mut self, kind: Kind, rtp: &[u8], now_ms: f64) {
        if rtp.len() < 12 {
            return;
        }
        if self.peer_ssrc == 0 {
            self.peer_ssrc = be32(rtp, 8);
        }
        if let Some(tw) = read_tw_seq(rtp) {
            self.twcc.add(tw, now_ms);
        }
        match kind {
            Kind::Video => self.video.on_packet(rtp, now_ms),
            Kind::Audio => self.audio.on_packet(rtp, now_ms),
        }
    }

    /// Compound RTCP from the phone: remember its SR time for our DLSR.
    pub fn on_rtcp(&mut self, kind: Kind, buf: &[u8], now_ms: f64) {
        let st = if kind == Kind::Video { &mut self.video } else { &mut self.audio };
        let mut o = 0;
        while o + 4 <= buf.len() {
            if buf[o] >> 6 != 2 {
                return;
            }
            let len = (be16(buf, o + 2) as usize + 1) * 4;
            if buf[o + 1] == 200 && o + len <= buf.len() {
                st.on_sr(&buf[o..o + len], now_ms);
            }
            o += len;
        }
    }

    pub fn twcc_packets(&mut self) -> Vec<Vec<u8>> {
        if self.peer_ssrc == 0 {
            return Vec::new();
        }
        let pk = self.twcc.build(self.local_ssrc, self.peer_ssrc);
        self.counters.twcc += pk.len() as u64;
        pk
    }

    /// Audio channel: SR (we send audio) with the audio reception block, + SDES.
    pub fn audio_report(&mut self, now_ms: f64, rtp_ts: u32, packets: u32, octets: u32) -> Vec<u8> {
        let blk = self.audio.block(now_ms);
        self.counters.reports += 1;
        let mut b = build_sr(self.local_ssrc, now_ms, rtp_ts, packets, octets, blk.as_slice());
        b.extend(build_sdes(self.local_ssrc));
        b
    }

    /// Video channel: RR with the video reception block, + SDES. None before any video.
    pub fn video_report(&mut self, now_ms: f64) -> Option<Vec<u8>> {
        let blk = self.video.block(now_ms)?;
        self.counters.reports += 1;
        let mut b = build_rr(self.local_ssrc, &[blk]);
        b.extend(build_sdes(self.local_ssrc));
        Some(b)
    }

    pub fn pli(&mut self) -> Option<Vec<u8>> {
        if !self.video.started {
            return None;
        }
        self.counters.pli += 1;
        Some(build_pli(self.local_ssrc, self.video.ssrc))
    }
}
