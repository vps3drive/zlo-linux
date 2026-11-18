//! Received audio: one small NetEq per source, mixed every 20 ms on the sound
//! card's clock.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::dsp::{rms, wsola_shrink};
use crate::dump::DUMP;
use crate::io::Sink;
use crate::log;
use crate::opus_codec::OpusDecoder;
use crate::protocol::{fault, to_bytes, FRAME_SAMPLES};

pub type Stats = HashMap<&'static str, u64>;

fn bump(stats: &mut Stats, key: &'static str) {
    *stats.entry(key).or_insert(0) += 1;
}

/// One received stream (one SSRC), a small NetEq: packets wait undecoded and
/// are decoded when the playout asks for the next 20 ms, so that
/// - a packet not there yet when it is due is waited for with PLC (NetEq's
///   Expand: a late packet still plays, the delay grows); once enough is
///   buffered after it, it is lost and becomes Opus FEC (from the next
///   packet) or PLC in its place, never a gap;
/// - after MAX_EXPAND frames of PLC with nothing to play, it goes quiet and
///   builds up the buffer again;
/// - the buffer depth follows the measured arrival jitter (min..max frames);
///   too much buffered (a burst, or the sender's clock faster than our card)
///   is shortened by one pitch period with overlap-add, like NetEq's
///   Accelerate. A whole 20 ms frame is never cut out: that click is the
///   crackle heard on a two-way call.
pub struct Stream {
    dec: OpusDecoder,
    pub min_target: usize,
    pub max_target: usize,
    packets: BTreeMap<i64, Vec<u8>>,
    next: Option<i64>,
    playing: bool,
    expand: usize,
    carry: Vec<i16>,
    transit: VecDeque<f64>,
    pub target: usize,
    pub last: f64,
    since_drop: usize,
}

impl Stream {
    pub const MIN_TARGET: usize = 5; // 100 ms: zrtc_config minAudioDelayMs (macOS NetEq's floor)
    const START_TARGET: usize = 5; // until the jitter is measured
    const MAX_EXPAND: usize = 5; // 100 ms of PLC before going quiet
    const WINDOW: usize = 250; // arrivals (5 s) used for the jitter estimate

    pub fn new(max_target: usize, min_target: Option<usize>, now: f64) -> Result<Self, String> {
        let min_target = min_target.unwrap_or(Self::MIN_TARGET).max(2);
        Ok(Stream {
            dec: OpusDecoder::new()?,
            min_target,
            max_target: max_target.max(min_target),
            packets: BTreeMap::new(),
            next: None,
            playing: false,
            expand: 0,
            carry: Vec::new(),
            transit: VecDeque::with_capacity(Self::WINDOW),
            target: Self::START_TARGET.max(min_target),
            last: now,
            since_drop: 0,
        })
    }

    pub fn idle(&self) -> bool {
        self.packets.is_empty() && self.carry.is_empty()
    }

    pub fn push(&mut self, idx: u32, payload: Vec<u8>, now: f64, stats: &mut Stats) {
        let idx = idx as i64;
        self.last = now;
        if self.next.map_or(false, |n| idx < n) {
            bump(stats, "late");
            return;
        }
        if self.packets.contains_key(&idx) {
            bump(stats, "dup");
            return;
        }
        self.packets.insert(idx, payload);
        // Arrival time minus send time (idx * 20 ms) up to a constant: its
        // spread over the last seconds is the jitter the buffer must absorb.
        if self.transit.len() == Self::WINDOW {
            self.transit.pop_front();
        }
        self.transit.push_back(now - idx as f64 * 0.02);
        if self.transit.len() >= 20 {
            let mut t: Vec<f64> = self.transit.iter().copied().collect();
            t.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let spread = t[(t.len() as f64 * 0.95) as usize - 1] - t[0];
            self.target = ((spread / 0.02).ceil() as usize + 1).max(self.min_target).min(self.max_target);
        }
    }

    fn decode(&mut self, payload: Option<&[u8]>, fec: bool, stats: &mut Stats) -> Vec<i16> {
        match self.dec.decode(payload, fec) {
            Some(pcm) => pcm.to_vec(),
            None => {
                bump(stats, "opus_fail");
                vec![0; FRAME_SAMPLES]
            }
        }
    }

    fn take(&mut self) -> Vec<i16> {
        let n = FRAME_SAMPLES.min(self.carry.len());
        let mut head: Vec<i16> = self.carry.drain(..n).collect();
        head.resize(FRAME_SAMPLES, 0);
        head
    }

    /// One frame from a short carry plus newly decoded samples.
    fn emit(&mut self, pcm: Vec<i16>) -> Vec<i16> {
        self.carry.extend(pcm);
        self.take()
    }

    fn span(&self) -> i64 {
        match (self.packets.keys().next_back(), self.next) {
            (Some(&last), Some(next)) => last - next + 1,
            _ => 0,
        }
    }

    /// Decode ahead until `carry` holds n samples, or the next packet is missing.
    fn ensure(&mut self, n: usize, stats: &mut Stats) -> bool {
        while self.carry.len() < n {
            let Some(next) = self.next else { break };
            let Some(p) = self.packets.remove(&next) else { break };
            let pcm = self.decode(Some(&p), false, stats);
            self.carry.extend(pcm);
            self.next = Some(next + 1);
            self.expand = 0;
            bump(stats, "opus_ok");
        }
        self.carry.len() >= n
    }

    /// The next 20 ms, or None (not playing).
    pub fn pull(&mut self, stats: &mut Stats) -> Option<Vec<i16>> {
        if !self.playing {
            if self.carry.is_empty() && self.packets.len() < self.target {
                return None;
            }
            self.playing = true;
            if let Some(&first) = self.packets.keys().next() {
                if self.next.map_or(true, |n| first > n) {
                    self.next = Some(first);
                }
            }
        }
        self.since_drop += 1;
        // Far over the target (a burst, or the sender's clock jumped): drop
        // back to the target once. Shrinking every frame through that spike
        // is the rasp, and it also moves the echo delay so the canceller
        // gates the near end. NetEq resets a buffer that is this far past
        // its target instead of time-compressing the speech.
        let mut span = self.span();
        if span > self.target as i64 + 10 {
            let skipped = self.discard_to(self.target, stats);
            if skipped > 0 {
                log!("playout: buffer reset, skipped {} ms", skipped * 20);
            }
            span = self.span();
        }
        // A little over: one pitch period, at most every 200 ms, and only
        // while this frame is quiet. Loud speech keeps its pitch.
        if span > self.target as i64 + 2 && self.since_drop >= 10 && self.ensure(FRAME_SAMPLES * 2, stats) && rms(&self.carry[..FRAME_SAMPLES]) < 800.0 {
            self.since_drop = 0;
            bump(stats, "accelerate");
            let (played, rest) = wsola_shrink(&self.carry);
            self.carry = rest;
            return Some(played);
        }
        if self.ensure(FRAME_SAMPLES, stats) {
            return Some(self.take());
        }
        if !self.packets.is_empty() && (self.packets.len() >= self.target || self.expand >= Self::MAX_EXPAND) {
            // A hole, and the buffer waited long enough: the packet is lost.
            self.expand = 0;
            bump(stats, "lost");
            let next = self.next.unwrap_or(0);
            let following = self.packets.get(&(next + 1)).cloned();
            self.next = Some(next + 1);
            if let Some(p) = following {
                bump(stats, "fec");
                let pcm = self.decode(Some(&p), true, stats);
                return Some(self.emit(pcm));
            }
            bump(stats, "plc");
            let pcm = self.decode(None, false, stats);
            return Some(self.emit(pcm));
        }
        // Not there yet: conceal and wait for it (the delay grows by 20 ms;
        // Accelerate takes it back later), then go quiet and buffer up again.
        bump(stats, "underrun");
        if self.expand < Self::MAX_EXPAND {
            self.expand += 1;
            bump(stats, "plc");
            let pcm = self.decode(None, false, stats);
            return Some(self.emit(pcm));
        }
        self.expand = 0;
        self.playing = false;
        None
    }

    /// Drop audio sitting ahead of `next` until `keep` frames remain.
    ///
    /// Returns how many 20 ms frames were skipped. The decoder continues at
    /// the first packet kept, so the skip is one gap, then the delay stays.
    fn discard_to(&mut self, keep: usize, stats: &mut Stats) -> i64 {
        let (Some(&last), Some(next)) = (self.packets.keys().next_back(), self.next) else { return 0 };
        let extra = last - next + 1 - keep as i64;
        if extra <= 0 {
            return 0;
        }
        let cut = next + extra;
        self.packets = self.packets.split_off(&cut);
        self.next = Some(self.packets.keys().next().copied().unwrap_or(cut));
        self.carry.clear();
        self.expand = 0;
        bump(stats, "reset");
        extra
    }
}

pub struct Inner {
    pub sources: BTreeMap<u32, Stream>,
    pub stats: Stats,
    lim: f64,
}

#[derive(Default)]
pub struct Period {
    pub dev_min: Option<f64>,
    pub dev_max: Option<f64>,
    pub mix_max: Duration,
}

/// Every 20 ms, the next frame of each source (Stream), summed, as the real
/// engine's OutputMixer + NetEq do (one source in a 1-1 call). With
/// libpulse-simple the loop is paced by the sound card (blocking writes);
/// with another sink (pacat, aplay, a file) by our own clock.
pub struct Playout {
    pub sink: Arc<Sink>,
    pub inner: Mutex<Inner>,
    pub period: Mutex<Period>,
    pub stop: AtomicBool,
    min_target: usize,
    max_target: usize,
    start: Instant,
    /// MicLeveler holds its gain while the far end sounds: what the canceller
    /// leaves of that echo must not be lifted with the voice.
    far_until_us: AtomicU64,
}

impl Playout {
    const IDLE_SEC: f64 = 5.0; // a source that sent nothing for this long is forgotten
    const REPORT_SEC: u64 = 30;
    // Two full-scale voices summed would clip at int16. Hold the mix to about
    // -2 dBFS and release slowly, as OutputMixer's limiter does. A hard clip
    // at the sum is the group-call crackle.
    const LIMIT: f64 = 26000.0;
    const FAR_RMS: f64 = 300.0; // ~ -40 dBFS: the far end is talking
    const FAR_HOLD: Duration = Duration::from_millis(350); // its echo comes back within this

    pub fn new(sink: Arc<Sink>, depth: usize, min_delay_ms: u32, max_delay_ms: u32) -> Arc<Self> {
        let min_target = ((min_delay_ms as f64 / 20.0).ceil() as usize).max(2);
        let max_target = if max_delay_ms > 0 { ((max_delay_ms as f64 / 20.0).ceil() as usize).max(min_target) } else { depth.max(min_target) };
        Arc::new(Playout {
            sink,
            inner: Mutex::new(Inner { sources: BTreeMap::new(), stats: Stats::new(), lim: 1.0 }),
            period: Mutex::new(Period::default()),
            stop: AtomicBool::new(false),
            min_target,
            max_target,
            start: Instant::now(),
            far_until_us: AtomicU64::new(0),
        })
    }

    fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    pub fn push(&self, idx: u32, payload: Vec<u8>, src: u32) {
        let now = self.now();
        let mut g = self.inner.lock().unwrap();
        let Inner { sources, stats, .. } = &mut *g;
        if !sources.contains_key(&src) {
            match Stream::new(self.max_target, Some(self.min_target), now) {
                Ok(s) => {
                    log!("playout: new source {src} (buffer {}..{} ms)", s.target * 20, s.max_target * 20);
                    sources.insert(src, s);
                }
                Err(e) => {
                    log!("playout: {e}");
                    return;
                }
            }
        }
        sources.get_mut(&src).unwrap().push(idx, payload, now, stats);
    }

    pub fn far_active(&self) -> bool {
        (self.start.elapsed().as_micros() as u64) < self.far_until_us.load(Ordering::Relaxed)
    }

    fn limit(lim: &mut f64, acc: &[i32], voices: usize) -> Vec<i16> {
        if voices < 2 {
            *lim += (1.0 - *lim) * 0.1;
            return acc.iter().map(|&v| v.clamp(-32768, 32767) as i16).collect();
        }
        let peak = acc.iter().map(|v| v.unsigned_abs()).max().unwrap_or(0) as f64;
        let target = if peak <= Self::LIMIT { 1.0 } else { Self::LIMIT / peak };
        if target < *lim {
            *lim = target;
        } else {
            *lim += (target - *lim) * 0.1;
        }
        acc.iter().map(|&v| ((v as f64 * *lim) as i64).clamp(-32768, 32767) as i16).collect()
    }

    fn mix(&self) -> Vec<i16> {
        let now = self.now();
        let mut g = self.inner.lock().unwrap();
        let Inner { sources, stats, lim } = &mut *g;
        let mut acc: Option<Vec<i32>> = None;
        let mut voices = 0;
        let gone: Vec<u32> = sources.iter().filter(|(_, s)| now - s.last > Self::IDLE_SEC && s.idle()).map(|(k, _)| *k).collect();
        for src in gone {
            sources.remove(&src);
            log!("playout: source {src} gone");
        }
        for s in sources.values_mut() {
            let Some(pcm) = s.pull(stats) else { continue };
            voices += 1;
            match acc.as_mut() {
                None => acc = Some(pcm.iter().map(|&v| v as i32).collect()),
                Some(a) => a.iter_mut().zip(&pcm).for_each(|(x, &v)| *x += v as i32),
            }
        }
        bump(stats, "mixed");
        match acc {
            None => vec![0; FRAME_SAMPLES],
            Some(a) => Self::limit(lim, &a, voices),
        }
    }

    fn report(&self) {
        let g = self.inner.lock().unwrap();
        let keys = ["opus_ok", "lost", "fec", "plc", "underrun", "accelerate", "reset", "late"];
        let counts: Vec<String> = keys.iter().filter_map(|k| g.stats.get(k).filter(|&&v| v > 0).map(|v| format!("{k}: {v}"))).collect();
        let targets: Vec<String> = g.sources.iter().map(|(k, s)| format!("{k}: {}", s.target * 20)).collect();
        log!("playout: {{{}}} buffer ms {{{}}}", counts.join(", "), targets.join(", "));
    }

    /// Prime the speaker with silence before the microphone opens.
    ///
    /// Zalo starts playout, then recording, whenever AEC is on. The canceller
    /// then already has a far-end reference. Opening the mic first makes the
    /// delay jump at the moment the other side speaks: the two-way rasp.
    pub fn prime(&self, frames: usize) {
        let silence = vec![0u8; FRAME_SAMPLES * 2];
        for _ in 0..frames {
            if let Err(e) = self.sink.write(&silence) {
                log!("playout prime failed: {e}");
                return;
            }
        }
        log!("playout primed {} ms before the microphone", frames * 20);
    }

    pub fn start(self: &Arc<Self>) {
        let me = Arc::clone(self);
        std::thread::Builder::new().name("playout".into()).spawn(move || me.clock()).unwrap();
    }

    fn clock(&self) {
        let paced = self.sink.paced;
        let mut next = Instant::now();
        let mut last_report = Instant::now();
        while !self.stop.load(Ordering::Relaxed) {
            let t = Instant::now();
            let pcm = self.mix();
            DUMP.write_played(&pcm);
            if !self.sink.silent.load(Ordering::Relaxed) && rms(&pcm) >= Self::FAR_RMS {
                let until = (self.start.elapsed() + Self::FAR_HOLD).as_micros() as u64;
                self.far_until_us.store(until, Ordering::Relaxed);
            }
            let latency = self.sink.latency_ms();
            {
                let mut p = self.period.lock().unwrap();
                p.mix_max = p.mix_max.max(t.elapsed());
                if let Some(ms) = latency {
                    p.dev_min = Some(p.dev_min.map_or(ms, |m| m.min(ms)));
                    p.dev_max = Some(p.dev_max.map_or(ms, |m| m.max(ms)));
                }
            }
            if let Err(e) = self.sink.write(&to_bytes(&pcm)) {
                log!("playout stopped: {e}");
                fault("speaker", e);
                return;
            }
            let now = Instant::now();
            if now - last_report >= Duration::from_secs(Self::REPORT_SEC) {
                last_report = now;
                self.report();
            }
            if paced {
                continue;
            }
            next += Duration::from_millis(20);
            if next > now {
                std::thread::sleep(next - now);
            } else if now - next > Duration::from_millis(200) {
                next = Instant::now(); // fell behind (suspend, load): start over
            }
        }
    }

    pub fn flush(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let mut g = self.inner.lock().unwrap();
        g.sources.clear();
    }

    pub fn stats_line(&self) -> String {
        let g = self.inner.lock().unwrap();
        let mut v: Vec<String> = g.stats.iter().map(|(k, v)| format!("{k}: {v}")).collect();
        v.sort();
        format!("{{{}}}", v.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opus_codec::{EncoderConfig, OpusEncoder};

    fn encoded(n: usize) -> Vec<Vec<u8>> {
        let mut enc = OpusEncoder::new(&EncoderConfig { bitrate: 32000, complexity: 10, fec: true, loss: 5 }).unwrap();
        let mut out = vec![0u8; 1500];
        let mut phase = 0.0f64;
        (0..n)
            .map(|_| {
                let pcm: Vec<i16> = (0..FRAME_SAMPLES)
                    .map(|_| {
                        phase += 2.0 * std::f64::consts::PI * 440.0 / 16000.0;
                        (phase.sin() * 8000.0) as i16
                    })
                    .collect();
                let len = enc.encode(&pcm, &mut out).unwrap();
                out[..len].to_vec()
            })
            .collect()
    }

    fn opus_or_skip() -> bool {
        if crate::opus_codec::available() {
            return true;
        }
        eprintln!("libopus not available; skipped");
        false
    }

    #[test]
    fn waits_for_target_then_plays_in_order() {
        if !opus_or_skip() {
            return;
        }
        let mut stats = Stats::new();
        let mut s = Stream::new(10, Some(5), 0.0).unwrap();
        let pkts = encoded(8);
        for (i, p) in pkts.iter().enumerate().take(4) {
            s.push(100 + i as u32, p.clone(), 0.0, &mut stats);
        }
        assert!(s.pull(&mut stats).is_none(), "under the target: not playing yet");
        s.push(104, pkts[4].clone(), 0.0, &mut stats);
        for _ in 0..5 {
            assert_eq!(s.pull(&mut stats).unwrap().len(), FRAME_SAMPLES);
        }
        assert_eq!(stats["opus_ok"], 5);
        // Late packet (already played) is dropped.
        s.push(101, pkts[1].clone(), 0.0, &mut stats);
        assert_eq!(stats["late"], 1);
    }

    #[test]
    fn hole_becomes_fec_once_buffer_is_deep() {
        if !opus_or_skip() {
            return;
        }
        let mut stats = Stats::new();
        let mut s = Stream::new(10, Some(2), 0.0).unwrap();
        let pkts = encoded(6);
        // 0, 1 then a hole at 2, then 3, 4, 5.
        for i in [0usize, 1, 3, 4, 5] {
            s.push(i as u32, pkts[i].clone(), 0.0, &mut stats);
        }
        // The hole is concealed (PLC) while the buffer waits for it, then
        // declared lost and rebuilt from packet 3's in-band FEC.
        for _ in 0..11 {
            assert!(s.pull(&mut stats).is_some());
        }
        assert_eq!(stats.get("lost"), Some(&1));
        assert_eq!(stats.get("fec"), Some(&1));
        assert_eq!(stats["opus_ok"], 5);
    }

    #[test]
    fn far_burst_is_reset_to_target() {
        if !opus_or_skip() {
            return;
        }
        let mut stats = Stats::new();
        let mut s = Stream::new(10, Some(2), 0.0).unwrap();
        let pkts = encoded(1);
        for i in 0..30u32 {
            s.push(i, pkts[0].clone(), 0.0, &mut stats);
        }
        s.pull(&mut stats);
        assert_eq!(stats.get("reset"), Some(&1));
        assert!(s.span() <= s.target as i64);
    }

    #[test]
    fn limiter_holds_two_loud_voices() {
        let mut lim = 1.0;
        let acc = vec![60000i32; FRAME_SAMPLES];
        let out = Playout::limit(&mut lim, &acc, 2);
        assert!(out.iter().all(|&v| v as f64 <= Playout::LIMIT + 1.0));
    }
}
