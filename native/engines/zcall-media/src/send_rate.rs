//! Which rung of the server's video ladder our camera is sent at, port of
//! native-engine/send-rate.js (see there for the captures and the test calls
//! behind every threshold).
//!
//! Signals: queueing delay and loss from transport-cc, the phone's report
//! blocks, missing feedback while we send video, and the loss of what we
//! receive. Steps up need UP_HOLD_S clean seconds of transport-cc and wait
//! longer after each step down that followed a step up. A group call passes
//! its spatial layers and never climbs past the one it starts on.

use std::collections::{HashMap, VecDeque};

use crate::json::{num as js_num, Value};
use crate::rtcp::{js_round, parse_twcc};
use crate::zrtp::Row;

const START_KBPS: f64 = 300.0;
const MIN_KBPS: f64 = 100.0;
const SAFETY_KEY_MS: f64 = 10000.0;
const DOWN_GAP_MS: f64 = 2000.0;
const UP_HOLD_S: u32 = 4;
const BACKOFF_MIN_MS: f64 = 10000.0;
const BACKOFF_MAX_MS: f64 = 60000.0;
const FEEDBACK_TIMEOUT_S: u32 = 2;
const QUEUE_HOLD_MS: f64 = 40.0;
const QUEUE_DOWN_MS: f64 = 150.0;
const DOWNLINK_HOLD: f64 = 0.02;
const DOWNLINK_DOWN: f64 = 0.10;
const BASE_WINDOW_S: f64 = 15.0;
const MIN_TWCC_PACKETS: u32 = 10;
const SENT_KEEP: usize = 4000;
const LOG_MS: f64 = 5000.0;

/// Landscape size of a 1-1 rung (res = the shorter side).
pub fn size_of(res: f64) -> Option<(u32, u32)> {
    match res {
        r if r == 240.0 => Some((426, 240)),
        r if r == 360.0 => Some((640, 360)),
        r if r == 480.0 => Some((848, 480)),
        r if r == 720.0 => Some((1280, 720)),
        _ => None,
    }
}

/// (id, res, fps, kbps)
pub const DEFAULT_LADDER: [(u32, u32, u32, u32); 7] =
    [(1, 240, 8, 30), (2, 240, 10, 100), (3, 360, 15, 300), (4, 360, 20, 500), (5, 360, 24, 700), (6, 480, 24, 900), (7, 720, 24, 1100)];

fn num(v: Option<&Value>, d: f64) -> f64 {
    let n = js_num(v);
    if n.is_finite() && n > 0.0 { n } else { d }
}

/// `${x}` of a JS value.
pub fn js_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => fmt_num(n.as_f64().unwrap_or(f64::NAN)),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Array(a) => a.iter().map(|x| if x.is_null() { String::new() } else { js_str(x) }).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// JS number to string (integers without ".0").
pub fn fmt_num(x: f64) -> String {
    if x.is_nan() {
        "NaN".into()
    } else if x.is_infinite() {
        if x > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else if x == 0.0 {
        "0".into()
    } else {
        format!("{x}")
    }
}

#[derive(Clone, Debug)]
pub struct Rung {
    pub id: Value,
    pub res: Option<f64>,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub kbps: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub bitrate: f64,
    pub key_ms: f64,
    pub capture_big: bool,
    pub rung: Value,
}

#[derive(Default, Clone, Debug, PartialEq)]
pub struct Totals {
    pub twcc_pkts: u64,
    pub twcc_lost: u64,
    pub rr_blocks: u64,
    pub changes: u64,
    pub downs: u64,
}

#[derive(Default)]
struct Window {
    twcc_recv: u32,
    twcc_lost: u32,
    rr: Vec<f64>,
    delays: Vec<f64>,
}

pub struct SendRate {
    local_ssrc: u32,
    log: Box<dyn FnMut(String) + Send>,
    on_profile: Box<dyn FnMut(Profile) + Send>,
    now: Box<dyn Fn() -> f64 + Send>,
    seen_twcc: bool,
    pub ladder: Vec<Rung>,
    pub level: usize,
    ceiling: usize,
    arm_timeout: bool,
    fixed: bool,
    high_loss: f64,
    low_loss: f64,
    key_ms: f64,
    capture_big: bool,
    sent: HashMap<u16, f64>,
    sent_order: VecDeque<u16>,
    win: Window,
    min_delays: Vec<(f64, f64)>,
    types: Vec<(String, u64)>,
    good: u32,
    no_feedback: u32,
    last_down_at: f64,
    last_up_at: f64,
    backoff_ms: f64,
    last_log_at: f64,
    history: Vec<String>,
    pub totals: Totals,
}

pub struct Options {
    pub local_ssrc: u32,
    pub log: Box<dyn FnMut(String) + Send>,
    pub on_profile: Box<dyn FnMut(Profile) + Send>,
    pub now: Box<dyn Fn() -> f64 + Send>,
    /// Group: the spatial layers (rows of the video plan) and the one we start on.
    pub layers: Option<Vec<Row>>,
    pub start_index: usize,
    /// ZCALL_VIDEO_RUNG / ZCALL_VIDEO_ADAPT (read by the caller).
    pub env_rung: Option<String>,
    pub env_adapt_off: bool,
}

impl SendRate {
    pub fn new(config: &Value, o: Options) -> Result<Self, String> {
        let empty = serde_json::Map::new();
        let c = config.as_object().unwrap_or(&empty);
        let get = |k: &str| c.get(k);
        let (ladder, level, ceiling, arm_timeout) = match o.layers.as_ref().filter(|l| !l.is_empty()) {
            Some(layers) => {
                let ladder: Vec<Rung> = layers
                    .iter()
                    .enumerate()
                    .map(|(i, p)| Rung { id: Value::from(i as u64), res: None, width: p.width, height: p.height, fps: num(Some(&Value::from(p.fps)), 15.0), kbps: p.kbps as f64 })
                    .filter(|p| p.width > 0 && p.height > 0 && p.kbps > 0.0)
                    .collect();
                if ladder.is_empty() {
                    return Err("group send-rate ladder is empty".into());
                }
                let level = o.start_index.min(ladder.len() - 1);
                (ladder, level, level, true)
            }
            None => {
                let (max_w, max_h) = (num(get("maxWidth"), 1280.0), num(get("maxHeight"), 720.0));
                let (max_fps, max_kbps) = (num(get("codecMaxFps"), 24.0), num(get("maxBitrate"), 1500.0));
                let src: Vec<(Value, f64, Value, f64)> = match get("bwProfiles").and_then(|v| v.as_array()).filter(|a| !a.is_empty()) {
                    Some(a) => a
                        .iter()
                        .map(|p| {
                            let g = |k: &str| p.as_object().and_then(|o| o.get(k));
                            (g("id").cloned().unwrap_or(Value::Null), js_num(g("res")), g("fps").cloned().unwrap_or(Value::Null), js_num(g("bitrate")))
                        })
                        .collect(),
                    None => DEFAULT_LADDER.iter().map(|&(id, res, fps, kbps)| (Value::from(id), res as f64, Value::from(fps), kbps as f64)).collect(),
                };
                let fps_of = |v: &Value| if v.is_null() { 15.0 } else { num(Some(v), 15.0) };
                let mut ladder: Vec<Rung> = src
                    .iter()
                    .map(|(id, res, fps, kbps)| {
                        let (w, h) = size_of(*res).unwrap_or((0, 0));
                        Rung { id: id.clone(), res: Some(*res), width: w, height: h, fps: fps_of(fps).min(max_fps), kbps: *kbps }
                    })
                    .filter(|p| p.width > 0 && p.kbps >= MIN_KBPS && p.kbps <= max_kbps && p.width as f64 <= max_w && p.height as f64 <= max_h)
                    .collect();
                ladder.sort_by(|a, b| a.kbps.partial_cmp(&b.kbps).unwrap_or(std::cmp::Ordering::Equal));
                if ladder.is_empty() {
                    ladder = DEFAULT_LADDER
                        .iter()
                        .filter(|p| p.3 as f64 >= MIN_KBPS)
                        .map(|&(id, res, fps, kbps)| {
                            let (w, h) = size_of(res as f64).unwrap();
                            Rung { id: Value::from(id), res: Some(res as f64), width: w, height: h, fps: fps as f64, kbps: kbps as f64 }
                        })
                        .collect();
                }
                let start_kbps = num(get("startupBitrate"), START_KBPS);
                let mut start = 0;
                for (i, p) in ladder.iter().enumerate() {
                    if p.kbps <= start_kbps {
                        start = i;
                    }
                }
                if let Some(r) = o.env_rung.as_deref().filter(|r| !r.is_empty()) {
                    if let Some(i) = ladder.iter().position(|p| js_str(&p.id) == r) {
                        start = i;
                    }
                }
                let top = ladder.len() - 1;
                (ladder, start, top, false)
            }
        };
        let capture_big = ladder.iter().any(|p| p.width > 640);
        let mut me = SendRate {
            local_ssrc: o.local_ssrc,
            log: o.log,
            on_profile: o.on_profile,
            now: o.now,
            seen_twcc: false,
            ladder,
            level,
            ceiling,
            arm_timeout,
            fixed: o.env_adapt_off,
            high_loss: num(get("bweSendSideHighLoss"), 10.0) / 100.0,
            low_loss: num(get("bweSendSideLowLoss"), 2.0) / 100.0,
            key_ms: num(get("keyFrameIntervalMs"), SAFETY_KEY_MS),
            capture_big,
            sent: HashMap::new(),
            sent_order: VecDeque::new(),
            win: Window::default(),
            min_delays: Vec::new(),
            types: Vec::new(),
            good: 0,
            no_feedback: 0,
            last_down_at: 0.0,
            last_up_at: 0.0,
            backoff_ms: BACKOFF_MIN_MS,
            last_log_at: 0.0,
            history: Vec::new(),
            totals: Totals::default(),
        };
        let rung = |p: &Rung| match p.res {
            Some(res) => format!("{}:{}p{}/{}k", js_str(&p.id), fmt_num(res), fmt_num(p.fps), fmt_num(p.kbps)),
            None => format!("{}:{}x{}/{}fps/{}k", js_str(&p.id), p.width, p.height, fmt_num(p.fps), fmt_num(p.kbps)),
        };
        let line = format!(
            "send-rate: ladder {}, start {}, loss marks {}..{} %, queue marks {}..{} ms, key every {} ms{}{}",
            me.ladder.iter().map(rung).collect::<Vec<_>>().join(" "),
            me.name(),
            fmt_num(me.low_loss * 100.0),
            fmt_num(me.high_loss * 100.0),
            QUEUE_HOLD_MS,
            QUEUE_DOWN_MS,
            fmt_num(me.key_ms),
            if me.arm_timeout { format!(", no climb past id {}", js_str(&me.ladder[me.ceiling].id)) } else { String::new() },
            if me.fixed { ", adaptation OFF (ZCALL_VIDEO_ADAPT=0)" } else { "" }
        );
        (me.log)(line);
        let p = me.profile();
        (me.on_profile)(p);
        Ok(me)
    }

    fn name(&self) -> String {
        let p = &self.ladder[self.level];
        format!("id {} {}x{} {} fps {} kbps", js_str(&p.id), p.width, p.height, fmt_num(p.fps), fmt_num(p.kbps))
    }

    /// What the window encodes: size, fps, bitrate, key interval.
    pub fn profile(&self) -> Profile {
        let p = &self.ladder[self.level];
        Profile { width: p.width, height: p.height, fps: p.fps, bitrate: p.kbps * 1000.0, key_ms: self.key_ms, capture_big: self.capture_big, rung: p.id.clone() }
    }

    /// Every packet we send (audio and video share the transport-wide sequence).
    pub fn on_sent(&mut self, tw_seq: u16, ms: Option<f64>) {
        let ms = ms.unwrap_or_else(|| (self.now)());
        if self.sent.insert(tw_seq, ms).is_none() {
            self.sent_order.push_back(tw_seq);
        }
        if self.sent.len() > SENT_KEEP {
            if let Some(old) = self.sent_order.pop_front() {
                self.sent.remove(&old);
            }
        }
    }

    /// Compound RTCP from the phone (either channel); `kind` is 'a' or 'v'.
    pub fn on_rtcp(&mut self, kind: char, buf: &[u8]) {
        let mut o = 0;
        while o + 4 <= buf.len() {
            if buf[o] >> 6 != 2 {
                return;
            }
            let len = (u16::from_be_bytes([buf[o + 2], buf[o + 3]]) as usize + 1) * 4;
            if o + len > buf.len() {
                return;
            }
            let (pt, fmt) = (buf[o + 1], buf[o] & 0x1f);
            let key = format!("{kind}{pt}/{fmt}");
            match self.types.iter_mut().find(|(k, _)| *k == key) {
                Some((_, n)) => *n += 1,
                None => self.types.push((key, 1)),
            }
            let pkt = &buf[o..o + len];
            if pt == 205 && fmt == 15 {
                self.on_twcc(pkt);
            } else if pt == 200 || pt == 201 {
                let first = if pt == 200 { 28 } else { 8 };
                for i in 0..fmt as usize {
                    let b = first + 24 * i;
                    if b + 24 > len {
                        break;
                    }
                    if u32::from_be_bytes([pkt[b], pkt[b + 1], pkt[b + 2], pkt[b + 3]]) == self.local_ssrc {
                        self.win.rr.push(pkt[b + 4] as f64 / 256.0);
                    }
                }
            }
            o += len;
        }
    }

    fn on_twcc(&mut self, pkt: &[u8]) {
        let Some(fb) = parse_twcc(pkt) else { return };
        if fb.sender_ssrc == self.local_ssrc {
            return;
        }
        let mut arrival = fb.ref_units as f64 * 64.0;
        let mut d = 0;
        for (i, &st) in fb.status.iter().enumerate() {
            if st == 0 {
                self.win.twcc_lost += 1;
                continue;
            }
            self.win.twcc_recv += 1;
            arrival += fb.delta_q.get(d).map_or(f64::NAN, |&q| q as f64 / 4.0);
            d += 1;
            if let Some(&sent) = self.sent.get(&(fb.base.wrapping_add(i as u16))) {
                self.win.delays.push(arrival - sent);
            }
        }
    }

    /// Once a second. `sending`: our camera is on; `down_loss`: share of the
    /// phone's packets we did not get in that second (0..1).
    pub fn tick(&mut self, sending: bool, down_loss: Option<f64>) {
        let now = (self.now)();
        let w = std::mem::take(&mut self.win);
        let total = w.twcc_recv + w.twcc_lost;
        self.totals.twcc_pkts += total as u64;
        self.totals.twcc_lost += w.twcc_lost as u64;
        self.totals.rr_blocks += w.rr.len() as u64;
        let twcc = total >= MIN_TWCC_PACKETS;
        let (mut loss, mut src) = (None, "");
        if twcc {
            loss = Some(w.twcc_lost as f64 / total as f64);
            src = "twcc";
        } else if !w.rr.is_empty() {
            loss = Some(w.rr.iter().cloned().fold(f64::NEG_INFINITY, f64::max));
            src = "rr";
        }
        let mut queue: Option<f64> = None;
        if !w.delays.is_empty() {
            let mut sorted = w.delays.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            self.min_delays.push((now, sorted[0]));
            self.min_delays.retain(|(t, _)| now - t <= BASE_WINDOW_S * 1000.0);
            let base = self.min_delays.iter().map(|(_, v)| *v).fold(f64::INFINITY, f64::min);
            queue = Some(js_round(sorted[sorted.len() / 2] - base).max(0.0));
        }
        self.no_feedback = if twcc { 0 } else { self.no_feedback + 1 };
        if twcc {
            self.seen_twcc = true;
        }
        let opt = |v: Option<f64>, f: &dyn Fn(f64) -> String| v.map_or("-".to_string(), f);
        self.history.push(format!(
            "{}/{}/{}",
            opt(loss, &|l| fmt_num(js_round(l * 1000.0) / 10.0)),
            opt(queue, &|q| fmt_num(q)),
            opt(down_loss, &|d| fmt_num(js_round(d * 1000.0) / 10.0))
        ));
        if !self.fixed && sending {
            let pct = |x: f64| format!("{:.1} %", x * 100.0);
            let mut down: Option<String> = None;
            let mut hold = false;
            if self.no_feedback >= FEEDBACK_TIMEOUT_S && !(self.arm_timeout && !self.seen_twcc) {
                down = Some(format!("no transport-cc for {} s", self.no_feedback));
            } else if let Some(l) = loss.filter(|&l| l > self.high_loss) {
                down = Some(format!("loss {} ({src}) > {} %", pct(l), fmt_num(self.high_loss * 100.0)));
            } else if let Some(q) = queue.filter(|&q| q > QUEUE_DOWN_MS) {
                down = Some(format!("queue {} ms > {} ms", fmt_num(q), QUEUE_DOWN_MS));
            } else if let Some(d) = down_loss.filter(|&d| d > DOWNLINK_DOWN) {
                down = Some(format!("we lose {} of the phone's packets", pct(d)));
            } else if !twcc || loss.map_or(false, |l| l >= self.low_loss) || queue.map_or(false, |q| q > QUEUE_HOLD_MS) || down_loss.map_or(false, |d| d > DOWNLINK_HOLD) {
                hold = true;
            }
            if let Some(why) = down {
                self.good = 0;
                if self.level > 0 && now - self.last_down_at >= DOWN_GAP_MS {
                    // A step down soon after a step up: that rung was too much, wait longer next time.
                    self.backoff_ms = if now - self.last_up_at < 15000.0 { (self.backoff_ms * 2.0).min(BACKOFF_MAX_MS) } else { BACKOFF_MIN_MS };
                    self.last_down_at = now;
                    self.totals.downs += 1;
                    self.set(self.level - 1, &why);
                }
            } else if hold {
                self.good = 0;
            } else {
                self.good += 1;
                if self.good >= UP_HOLD_S && self.level < self.ceiling && now - self.last_down_at >= self.backoff_ms {
                    self.good = 0;
                    self.last_up_at = now;
                    let why = format!(
                        "clean for {UP_HOLD_S} s (loss {}, queue {})",
                        loss.map_or("NaN %".into(), pct),
                        queue.map_or("-".into(), |q| format!("{} ms", fmt_num(q)))
                    );
                    self.set(self.level + 1, &why);
                }
            }
        }
        if now - self.last_log_at >= LOG_MS {
            self.last_log_at = now;
            let types = if self.types.is_empty() { "none".to_string() } else { self.types.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(" ") };
            self.types.clear();
            let line = format!(
                "send-rate: {}{}; loss%/queue ms/downlink% per s [{}]; rtcp from phone {types}",
                self.name(),
                if sending { "" } else { " (camera off)" },
                self.history.join(" ")
            );
            (self.log)(line);
            self.history.clear();
        }
    }

    fn set(&mut self, level: usize, why: &str) {
        let from = self.name();
        self.level = level;
        self.totals.changes += 1;
        let line = format!("send-rate: {from} -> {} ({why})", self.name());
        (self.log)(line);
        let p = self.profile();
        (self.on_profile)(p);
    }
}
