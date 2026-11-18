//! zcall-media: the media side of a 1-1 call (ZCALL_MEDIA=rust), in place of
//! backends/zrtc-media.js. It owns the UDP socket (relay server and P2P),
//! SRTP, the RTCP we owe the peer and the send-rate controller, and runs
//! zcall-audio itself, so audio frames no longer pass through Node.
//!
//! Control from backends/zrtc-media-rust.js, one JSON object per line:
//!   stdin   {"cmd":"negotiated","params":{..},"sendUdp":true}
//!           {"cmd":"prepare","role":"caller"}
//!           {"cmd":"start","role":"callee","rtpSerIp":"ip:port","audio":{"bin":..,"args":[..]}}
//!           {"cmd":"mute","on":true} {"cmd":"speaker","off":true}
//!           {"cmd":"micVolume","pct":70} {"cmd":"device","kind":"mic","name":".."}
//!           {"cmd":"stop"}  then stdin closes and the process exits
//!   stdout  {"ev":"log","msg":".."}            a line for native-engine-*.log
//!           {"ev":"prepared","best":{..}|null,"port":N}
//!           {"ev":"emit","name":"timeout","args":[..]}   a backend event
//!           {"ev":"stats","stats":{..},"lastRxAt":ms}    once a second and at stop
//!
//! Every log line is the one zrtc-media.js writes for the same event, so the
//! two engines' logs can be diffed. Timings and thresholds are zrtc-media.js's.

use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};
use zcall_media::json::num;
use zcall_media::rtcp::{Kind, RtcpFeedback};
use zcall_media::rtp;
use zcall_media::send_rate::{fmt_num, Options, SendRate};
use zcall_media::srtp::{self, RocTracker, SessionKeys};
use zcall_media::zrtp::{self, cmd, msg, p2p, Packet};

// Timings of the real engine (zrtc-media.js).
const INIT_WAIT: Duration = Duration::from_millis(600);
const INIT_RETRY: Duration = Duration::from_millis(1000);
const INIT_TRIES: u32 = 3;
const PING_INTERVAL: Duration = Duration::from_millis(6000);
const TWCC_INTERVAL: Duration = Duration::from_millis(50);
const RTCP_REPORT_INTERVAL: Duration = Duration::from_millis(1000);
const RTCP_LOG_MS: f64 = 5000.0;
const AUDIO_TX_LOG_MS: f64 = 10000.0;
const STATUS_INTERVAL: Duration = Duration::from_millis(1000);
const MEDIA_TIMEOUT_MS: f64 = 20000.0;
const P2P_BIND_FAST: Duration = Duration::from_millis(100);
const P2P_BIND_SLOW: Duration = Duration::from_millis(400);
const P2P_BIND_GIVE_UP: Duration = Duration::from_millis(15000);
// stats() is read right before stop() for the call report: keep it fresh.
const STATS_INTERVAL: Duration = Duration::from_millis(250);

fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Random value below `n` (Math.floor(Math.random() * n)).
fn rand_below(n: u64) -> u64 {
    let mut b = [0u8; 8];
    unsafe { libc::getrandom(b.as_mut_ptr() as *mut libc::c_void, 8, 0) };
    u64::from_le_bytes(b) % n
}

fn out(v: Value) {
    let mut o = std::io::stdout().lock();
    let _ = writeln!(o, "{v}");
    let _ = o.flush();
}

fn log(msg: impl Into<String>) {
    out(json!({"ev": "log", "msg": msg.into()}));
}

fn emit(name: &str, args: Value) {
    out(json!({"ev": "emit", "name": name, "args": args}));
}

/// util.inspect of a JSON value inside an object literal ({ servers: 1, srtpMode: '1' }).
fn inspect(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::String(s)) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
        Some(Value::Number(n)) => fmt_num(n.as_f64().unwrap_or(f64::NAN)),
        Some(other) => other.to_string(),
    }
}

/// "host:port" or "[v6]:port" (parseHostPort).
fn parse_host_port(addr: Option<&Value>) -> Option<(String, u16)> {
    let s = addr?.as_str()?;
    if let Some(rest) = s.strip_prefix('[') {
        let (h, p) = rest.split_once("]:")?;
        return Some((h.to_string(), p.parse().ok()?));
    }
    let i = s.rfind(':')?;
    if i == 0 {
        return None;
    }
    let port: f64 = s[i + 1..].parse().ok()?;
    Some((s[..i].to_string(), port as u16))
}

enum Event {
    Cmd(Value),
    StdinClosed,
    Udp(Vec<u8>, SocketAddr),
    AudioFrame(u64, u8, Vec<u8>),
    AudioLine(u64, String),
    AudioExit(u64, String),
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Timer {
    InitFinish,
    InitRetry,
    Ping,
    Status,
    ReqForward8,
    MediaTimeout,
    Twcc,
    Reports,
    P2pBind,
    Stats,
}

struct Timers {
    list: Vec<(Instant, Option<Duration>, Timer)>,
}

impl Timers {
    fn add(&mut self, after: Duration, repeat: bool, t: Timer) {
        self.list.push((Instant::now() + after, repeat.then_some(after), t));
    }
    fn cancel(&mut self, t: Timer) {
        self.list.retain(|x| x.2 != t);
    }
    fn clear(&mut self) {
        self.list.clear();
    }
    fn next_deadline(&self) -> Option<Instant> {
        self.list.iter().map(|x| x.0).min()
    }
    /// Due timers, in deadline order; repeating ones are rescheduled.
    fn due(&mut self) -> Vec<Timer> {
        let now = Instant::now();
        let mut fired: Vec<(Instant, Timer)> = Vec::new();
        self.list.retain_mut(|x| {
            if x.0 > now {
                return true;
            }
            fired.push((x.0, x.2));
            match x.1 {
                Some(every) => {
                    x.0 += every;
                    if x.0 <= now {
                        x.0 = now + every; // fell behind: no burst of catch-up ticks
                    }
                    true
                }
                None => false,
            }
        });
        fired.sort_by_key(|f| f.0);
        fired.into_iter().map(|f| f.1).collect()
    }
}

#[derive(Clone)]
struct Server {
    host: String,
    port: u16,
    addr: Option<SocketAddr>,
}

struct Reply {
    server: usize,
    token: u32,
    rtt_ms: u64,
    public_addr: Option<String>,
}

struct Init {
    packet: Vec<u8>,
    tries: u32,
    sent_at: Instant,
    waiting: bool,
}

struct Audio {
    child: Child,
    stdin: Option<ChildStdin>,
    gen: u64,
}

#[derive(Default)]
struct Counters {
    tx: u64,
    rx: u64,
    rx_bad: u64,
    rx_second: u64,
    rx_p2p: u64,
}

#[derive(Default)]
struct VideoStats {
    pkts: u64,
    bad: u64,
    pt: BTreeMap<u8, u64>,
    marker: u64,
}

struct ATxWin {
    n: u64,
    last: f64,
    max_gap: f64,
    gaps40: u64,
    gaps60: u64,
    since: f64,
}

struct Media {
    tx: Sender<Event>,
    send_udp: bool,
    sock: Option<UdpSocket>,
    timers: Timers,
    params: Value,
    active: bool,
    started_at: f64,
    last_rx_at: f64,
    local_uid: u32,
    call_id: u32,
    callee: bool,
    sess_id: Vec<u8>,
    muted: bool,
    speaker_off: bool,
    devices: (Option<String>, Option<String>),
    servers: Vec<Server>,
    active_server: Option<usize>,
    server_token: u32,
    replies: Vec<Reply>,
    refusals: usize,
    refused: HashSet<usize>,
    ended: HashSet<String>,
    init: Option<Init>,
    keys: Option<SessionKeys>,
    rtcp: Option<RtcpFeedback>,
    send_rate: Option<SendRate>,
    seq: u16,
    ts: u32,
    tw_seq: u16,
    roc: u32,
    fwd_seq: u32,
    rx_roc: RocTracker,
    video_roc: RocTracker,
    counters: Counters,
    rx_types: BTreeMap<u8, u64>,
    video: VideoStats,
    video_noted: bool,
    key_requests: u64,
    p2p_path: Option<SocketAddr>,
    p2p_seen: bool,
    p2p_seq: u16,
    p2p_t0: Instant,
    p2p_cands: Vec<(SocketAddr, u32)>,
    audio: Option<Audio>,
    audio_gen: u64,
    /// "media: zcall-audio exited N", logged after its last stderr lines.
    audio_exit: Option<String>,
    audio_octets: u64,
    a_tx: ATxWin,
    mic_peak: u8,
    mic_level_at: f64,
    rtcp_logged_at: f64,
    down_prev: Option<(i64, i64)>,
}

impl Media {
    fn new(tx: Sender<Event>) -> Self {
        Media {
            tx,
            send_udp: false,
            sock: None,
            timers: Timers { list: Vec::new() },
            params: Value::Null,
            active: false,
            started_at: 0.0,
            last_rx_at: 0.0,
            local_uid: 0,
            call_id: 0,
            callee: false,
            sess_id: Vec::new(),
            muted: false,
            speaker_off: false,
            devices: (None, None),
            servers: Vec::new(),
            active_server: None,
            server_token: 0,
            replies: Vec::new(),
            refusals: 0,
            refused: HashSet::new(),
            ended: HashSet::new(),
            init: None,
            keys: None,
            rtcp: None,
            send_rate: None,
            seq: 0,
            ts: 0,
            tw_seq: 1,
            roc: 0,
            fwd_seq: 1,
            rx_roc: RocTracker::default(),
            video_roc: RocTracker::default(),
            counters: Counters::default(),
            rx_types: BTreeMap::new(),
            video: VideoStats::default(),
            video_noted: false,
            key_requests: 0,
            p2p_path: None,
            p2p_seen: false,
            p2p_seq: 0,
            p2p_t0: Instant::now(),
            p2p_cands: Vec::new(),
            audio: None,
            audio_gen: 0,
            audio_exit: None,
            audio_octets: 0,
            a_tx: ATxWin { n: 0, last: 0.0, max_gap: 0.0, gaps40: 0, gaps60: 0, since: now_ms() },
            mic_peak: 127,
            mic_level_at: 0.0,
            rtcp_logged_at: 0.0,
            down_prev: None,
        }
    }

    fn p(&self, k: &str) -> Option<&Value> {
        self.params.get(k)
    }

    fn p_u32(&self, k: &str) -> u32 {
        let n = num(self.p(k));
        if n.is_finite() { (n as i64) as u32 } else { 0 }
    }

    // --- socket ------------------------------------------------------------------

    fn init_socket(&mut self) {
        if self.sock.is_some() || !self.send_udp {
            return;
        }
        let sock = match UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(e) => {
                log(format!("media: socket error: {e}"));
                return;
            }
        };
        let rx = sock.try_clone().expect("clone udp socket");
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 65536];
            loop {
                match rx.recv_from(&mut buf) {
                    Ok((n, from)) => {
                        if tx.send(Event::Udp(buf[..n].to_vec(), from)).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        // Closed on stop, or a transient error (ICMP unreachable).
                        if e.kind() == std::io::ErrorKind::ConnectionRefused {
                            continue;
                        }
                        return;
                    }
                }
            }
        });
        self.sock = Some(sock);
    }

    fn send_to(&self, packet: &[u8], to: Option<SocketAddr>, what: &str) {
        let (Some(sock), Some(to)) = (&self.sock, to) else { return };
        if let Err(e) = sock.send_to(packet, to) {
            log(format!("media: {what} send error: {e}"));
        }
    }

    fn server_addr(&self, i: usize) -> Option<SocketAddr> {
        self.servers.get(i).and_then(|s| s.addr)
    }

    fn send_server(&self, packet: &[u8], i: Option<usize>, what: &str) {
        if let Some(i) = i {
            self.send_to(packet, self.server_addr(i), what);
        }
    }

    // --- negotiation -------------------------------------------------------------

    fn on_negotiated(&mut self, params: Value, send_udp: bool) {
        self.params = params;
        self.send_udp = send_udp;
        self.muted = self.p("muted").and_then(Value::as_bool).unwrap_or(false);
        self.speaker_off = self.p("speakerOff").and_then(Value::as_bool).unwrap_or(false);
        self.local_uid = self.p_u32("localUid");
        self.call_id = self.p_u32("callId");
        self.callee = self.p("role").and_then(Value::as_str) == Some("callee");
        self.sess_id = match self.p("sessId") {
            Some(Value::String(s)) => s.as_bytes().to_vec(),
            Some(Value::Null) | None => Vec::new(),
            Some(other) => other.to_string().into_bytes(),
        };
        self.servers.clear();
        if let Some(list) = self.params.get("servers").and_then(Value::as_array).cloned() {
            for s in &list {
                let addr = s.get("rtpaddr").filter(|v| v.as_str().map_or(false, |x| !x.is_empty()))
                    .or_else(|| s.get("rtp").filter(|v| v.as_str().map_or(false, |x| !x.is_empty())))
                    .or_else(|| s.get("rtpAddress"));
                if let Some((host, port)) = parse_host_port(addr) {
                    let resolved = (host.as_str(), port).to_socket_addrs().ok().and_then(|mut a| a.find(|x| x.is_ipv4()));
                    self.servers.push(Server { host, port, addr: resolved });
                }
            }
        }
        self.active_server = None;
        self.server_token = 0;
        self.replies.clear();
        self.ended.clear();
        self.reset_counters();
        let rtcp_on = std::env::var("ZCALL_RTCP").as_deref() != Ok("0") && self.local_uid != 0;
        self.rtcp = rtcp_on.then(|| RtcpFeedback::new(self.local_uid));
        let rate_on = self.rtcp.is_some() && std::env::var("ZCALL_SEND_RATE").as_deref() != Ok("0");
        self.send_rate = None;
        if rate_on {
            let config = self.p("config").cloned().unwrap_or(Value::Null);
            match SendRate::new(&config, Options {
                local_ssrc: self.local_uid,
                log: Box::new(|l| log(l)),
                on_profile: Box::new(|p| {
                    emit("sendProfile", json!([{"width": p.width, "height": p.height, "fps": p.fps, "bitrate": p.bitrate,
                        "keyMs": p.key_ms, "captureBig": p.capture_big, "rung": p.rung}]))
                }),
                now: Box::new(now_ms),
                layers: None,
                start_index: 0,
                env_rung: std::env::var("ZCALL_VIDEO_RUNG").ok(),
                env_adapt_off: std::env::var("ZCALL_VIDEO_ADAPT").as_deref() == Ok("0"),
            }) {
                Ok(s) => self.send_rate = Some(s),
                Err(e) => log(format!("send-rate: {e}")),
            }
        }
        log(format!(
            "media: negotiated {{ servers: {}, srtpMode: {}, sendUdp: {} }}",
            self.servers.len(),
            inspect(self.p("srtpMode")),
            self.send_udp
        ));
        self.keys = None;
        if self.sess_id.len() >= 30 {
            match srtp::master_from_sess_id(&self.sess_id).and_then(srtp::session_keys) {
                Ok(k) => self.keys = Some(k),
                Err(e) => log(format!("media: SRTP key setup failed {e}")),
            }
        }
    }

    fn reset_counters(&mut self) {
        self.seq = rand_below(0x8000) as u16;
        self.ts = rand_below(0x7fffffff) as u32;
        self.tw_seq = 1;
        self.roc = 0;
        self.fwd_seq = 1;
        self.rx_roc = RocTracker::default();
        self.video_roc = RocTracker::default();
        self.counters = Counters::default();
        self.rx_types.clear();
        self.video = VideoStats::default();
        self.video_noted = false;
        self.key_requests = 0;
        self.p2p_path = None;
        self.p2p_seen = false;
        self.p2p_seq = 0;
        self.audio_octets = 0;
        self.a_tx = ATxWin { n: 0, last: 0.0, max_gap: 0.0, gaps40: 0, gaps60: 0, since: now_ms() };
        self.rtcp_logged_at = 0.0;
        self.down_prev = None;
        self.last_rx_at = 0.0;
        self.started_at = 0.0;
    }

    // --- InitZRTP ----------------------------------------------------------------

    fn prepare(&mut self, role: &str) {
        self.active = true;
        if !self.send_udp {
            log("media: observe-only (no --send-udp), not contacting servers");
            out(json!({"ev": "prepared", "best": null, "port": 0}));
            return;
        }
        self.init_socket();
        let packet = zrtp::init_zrtp(role == "caller", self.local_uid, self.p_u32("peerUid"), self.call_id, &self.sess_id, &[], 2);
        self.refusals = 0;
        self.refused.clear();
        self.init = Some(Init { packet, tries: 0, sent_at: Instant::now(), waiting: false });
        self.init_send();
        self.timers.add(INIT_RETRY, false, Timer::InitRetry);
    }

    fn init_send(&mut self) {
        let Some(init) = self.init.as_mut() else { return };
        init.tries += 1;
        init.sent_at = Instant::now();
        let packet = init.packet.clone();
        for i in 0..self.servers.len() {
            if !self.replies.iter().any(|r| r.server == i) {
                self.send_server(&packet, Some(i), "InitZRTP");
            }
        }
    }

    fn init_retry(&mut self) {
        let Some(init) = self.init.as_ref() else { return };
        if !self.replies.is_empty() || self.refusals > 0 {
            return;
        }
        if init.tries >= INIT_TRIES {
            self.init_finish();
            return;
        }
        self.init_send();
        self.timers.add(INIT_RETRY, false, Timer::InitRetry);
    }

    fn on_init_reply(&mut self) {
        let Some(init) = self.init.as_mut() else { return };
        if self.replies.len() + self.refusals >= self.servers.len() {
            self.timers.cancel(Timer::InitFinish);
            self.init_finish();
            return;
        }
        if !init.waiting {
            init.waiting = true;
            self.timers.add(INIT_WAIT, false, Timer::InitFinish);
        }
    }

    fn init_finish(&mut self) {
        if self.init.take().is_none() {
            return;
        }
        self.timers.cancel(Timer::InitRetry);
        if self.replies.is_empty() {
            log(if self.refusals > 0 { "media: every server refused InitZRTP" } else { "media: no server answered InitZRTP" });
            out(json!({"ev": "prepared", "best": null, "port": self.local_port()}));
            return;
        }
        let mut best = 0;
        for (i, r) in self.replies.iter().enumerate() {
            if r.rtt_ms < self.replies[best].rtt_ms {
                best = i;
            }
        }
        let b = &self.replies[best];
        self.active_server = Some(b.server);
        self.server_token = b.token;
        log(format!("media: {}/{} servers answered, best rtt {} ms", self.replies.len(), self.servers.len(), b.rtt_ms));
        let s = &self.servers[b.server];
        let best_json = json!({"server": {"host": s.host, "port": s.port}, "token": b.token, "rttMs": b.rtt_ms, "publicAddr": b.public_addr});
        out(json!({"ev": "prepared", "best": best_json, "port": self.local_port()}));
    }

    fn local_port(&self) -> u16 {
        self.sock.as_ref().and_then(|s| s.local_addr().ok()).map_or(0, |a| a.port())
    }

    // --- start / stop ------------------------------------------------------------

    fn start(&mut self, role: &str, rtp_ser_ip: Option<&str>, audio: Option<&Value>) {
        self.active = true;
        if !self.send_udp {
            log(format!("media: start role={role} (observe-only)"));
            return;
        }
        let want = rtp_ser_ip.and_then(|s| parse_host_port(Some(&Value::from(s))));
        let matched = want.as_ref().and_then(|(h, p)| self.replies.iter().find(|r| &self.servers[r.server].host == h && self.servers[r.server].port == *p));
        if let Some(m) = matched {
            self.active_server = Some(m.server);
            self.server_token = m.token;
        } else if want.is_some() {
            log("media: peer chose a server we have no token for; keeping ours");
        }
        if self.active_server.is_none() {
            log("media: no server");
            emit("failed", json!(["no-server"]));
            return;
        }
        let others: Vec<(usize, u32)> = self.replies.iter().filter(|r| Some(r.server) != self.active_server).map(|r| (r.server, r.token)).collect();
        for (s, t) in others {
            self.send_end_call(s, t);
        }
        log("media: started");
        self.started_at = now_ms();
        self.last_rx_at = now_ms();
        self.timers.add(PING_INTERVAL, true, Timer::Ping);
        self.timers.add(STATUS_INTERVAL, true, Timer::Status);
        self.timers.add(Duration::from_millis(200), false, Timer::ReqForward8);
        self.timers.add(Duration::from_millis(2000), true, Timer::MediaTimeout);
        if self.rtcp.is_some() {
            self.timers.add(TWCC_INTERVAL, true, Timer::Twcc);
            self.timers.add(RTCP_REPORT_INTERVAL, true, Timer::Reports);
        }
        if let Some(a) = audio.filter(|a| !a.is_null()) {
            self.start_audio(a);
        }
        self.p2p_start();
    }

    fn send_end_call(&mut self, server: usize, token: u32) {
        let s = &self.servers[server];
        let key = format!("{}:{}", s.host, s.port);
        if !self.ended.insert(key) {
            return;
        }
        let pkt = zrtp::end_call(self.local_uid, token, self.call_id);
        self.send_server(&pkt, Some(server), "EndCall");
    }

    fn stats_json(&self) -> Value {
        let c = &self.counters;
        json!({"tx": c.tx, "rx": c.rx, "rxBad": c.rx_bad, "rxSecond": c.rx_second, "rxP2p": c.rx_p2p,
            "durationMs": if self.started_at > 0.0 { now_ms() - self.started_at } else { 0.0 }})
    }

    fn send_stats(&self) {
        out(json!({"ev": "stats", "stats": self.stats_json(), "lastRxAt": self.last_rx_at}));
    }

    fn stop(&mut self) {
        self.send_stats();
        if let Some(sr) = self.send_rate.take() {
            let t = &sr.totals;
            let totals = json!({"rung": sr.profile().rung, "twccPkts": t.twcc_pkts, "twccLost": t.twcc_lost, "rrBlocks": t.rr_blocks, "changes": t.changes, "downs": t.downs});
            log(format!("send-rate: call totals {totals}"));
        }
        if self.active && !self.params.is_null() {
            if let (Some(s), true) = (self.active_server, self.server_token != 0) {
                self.send_end_call(s, self.server_token);
            }
            let all: Vec<(usize, u32)> = self.replies.iter().map(|r| (r.server, r.token)).collect();
            for (s, t) in all {
                self.send_end_call(s, t);
            }
        }
        if self.started_at > 0.0 {
            let c = &self.counters;
            let mut stopped = Map::new();
            stopped.insert("tx".into(), json!(c.tx));
            stopped.insert("rx".into(), json!(c.rx));
            stopped.insert("rxBad".into(), json!(c.rx_bad));
            stopped.insert("rxSecond".into(), json!(c.rx_second));
            stopped.insert("rxP2p".into(), json!(c.rx_p2p));
            stopped.insert("rxTypes".into(), Value::Object(self.rx_types.iter().map(|(k, v)| (k.to_string(), json!(v))).collect()));
            stopped.insert("video".into(), json!({"pkts": self.video.pkts, "bad": self.video.bad,
                "pt": self.video.pt.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<Map<_, _>>(), "head": {}, "marker": self.video.marker}));
            stopped.insert("videoTx".into(), json!(0));
            stopped.insert("keyRequests".into(), json!(self.key_requests));
            if let Some(r) = &self.rtcp {
                stopped.insert("rtcp".into(), json!({"twcc": r.counters.twcc, "reports": r.counters.reports, "pli": r.counters.pli, "nack": r.counters.nack}));
            }
            stopped.insert("members".into(), json!({}));
            stopped.insert("videoMembers".into(), json!({}));
            log(format!("media: stopped {}", Value::Object(stopped)));
        } else if self.counters.rx > 0 {
            log(format!("media: setup audio rx {} before the call connected", self.counters.rx));
        }
        self.started_at = 0.0;
        self.active = false;
        self.init = None;
        self.timers.clear();
        if let Some(mut a) = self.audio.take() {
            drop(a.stdin.take()); // zcall-audio stops when its stdin closes
            let deadline = Instant::now() + Duration::from_millis(1000);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = a.child.try_wait() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = a.child.kill();
            if let Ok(status) = a.child.wait() {
                use std::os::unix::process::ExitStatusExt;
                let code = status.code().map_or_else(|| signal_name(status.signal().unwrap_or(0)), |c| c.to_string());
                self.audio_exit = Some(format!("media: zcall-audio exited {code}"));
            }
        }
        // Let EndCall leave before the socket goes.
        std::thread::sleep(Duration::from_millis(100));
        self.sock = None;
    }

    // --- timers ------------------------------------------------------------------

    fn on_timer(&mut self, t: Timer) {
        match t {
            Timer::InitFinish => self.init_finish(),
            Timer::InitRetry => self.init_retry(),
            Timer::Ping => {
                let pkt = zrtp::ping(self.local_uid, self.server_token, &self.sess_id);
                self.send_server(&pkt, self.active_server, "Ping");
            }
            Timer::Status => {
                let rx = self.counters.rx_second;
                self.counters.rx_second = 0;
                let seq = self.fwd_seq;
                self.fwd_seq = self.fwd_seq.wrapping_add(1);
                let pkt = zrtp::req_forward(self.local_uid, self.server_token, seq, 10, &zrtp::req_forward_status(rx as u32, 0, 0));
                self.send_server(&pkt, self.active_server, "ReqForward");
            }
            Timer::ReqForward8 => {
                let pkt = zrtp::req_forward(self.local_uid, self.server_token, 0, 8, &[4, 0, 0, 0, 0, 0, 0, 0]);
                self.send_server(&pkt, self.active_server, "ReqForward");
            }
            Timer::MediaTimeout => {
                if now_ms() - self.last_rx_at > MEDIA_TIMEOUT_MS {
                    log("media: no audio from peer");
                    emit("timeout", json!([]));
                }
            }
            Timer::Twcc => {
                let pk = self.rtcp.as_mut().map(|r| r.twcc_packets()).unwrap_or_default();
                for p in pk {
                    self.send_rtcp(false, &p);
                }
            }
            Timer::Reports => self.send_reports(),
            Timer::P2pBind => self.p2p_bind(),
            Timer::Stats => self.send_stats(),
        }
    }

    // --- RTCP ----------------------------------------------------------------------

    fn send_rtcp(&self, video: bool, rtcp: &[u8]) {
        if !self.active || self.active_server.is_none() || self.started_at == 0.0 {
            return;
        }
        let t = if video { msg::VIDEO_FEC } else { msg::AUDIO_FEC };
        self.send_server(&zrtp::wrap_media(t, self.server_token, rtcp), self.active_server, "RTCP");
        if self.p2p_path.is_some() {
            let kind = if video { p2p::VIDEO_RTCP } else { p2p::AUDIO_RTCP };
            self.send_to(&zrtp::p2p_wrap(self.callee, self.call_id, kind, rtcp), self.p2p_path, "P2P RTCP");
        }
    }

    /// Share of the phone's audio packets that did not arrive since the last call.
    fn downlink_loss(&mut self) -> Option<f64> {
        let (expected, received) = self.rtcp.as_ref()?.audio.counts()?;
        let prev = self.down_prev.unwrap_or((expected, received));
        self.down_prev = Some((expected, received));
        let (exp, rec) = (expected - prev.0, received - prev.1);
        (exp >= 10).then(|| ((exp - rec) as f64 / exp as f64).clamp(0.0, 1.0))
    }

    fn send_reports(&mut self) {
        if self.rtcp.is_none() {
            return;
        }
        let now = now_ms();
        if self.send_rate.is_some() {
            // The camera is not sent by this backend yet: never "sending".
            let loss = self.downlink_loss();
            self.send_rate.as_mut().unwrap().tick(false, loss);
        }
        let (ts, tx, octets) = (self.ts, self.counters.tx as u32, self.audio_octets as u32);
        let rep = self.rtcp.as_mut().unwrap().audio_report(now, ts, tx, octets);
        self.send_rtcp(false, &rep);
        if let Some(v) = self.rtcp.as_mut().unwrap().video_report(now) {
            self.send_rtcp(true, &v);
        }
        if now - self.rtcp_logged_at >= RTCP_LOG_MS {
            self.rtcp_logged_at = now;
            let r = self.rtcp.as_mut().unwrap();
            let c = r.counters;
            let mut line = format!("rtcp: sent twcc {}, reports {}, pli {}, nack {}", c.twcc, c.reports, c.pli, c.nack);
            if r.video.started {
                // JS reads a fresh block here, which also moves the interval counters.
                if let Some(b) = r.video.block(now) {
                    line += &format!("; video from phone: {} pkts, lost {}, jitter {}", self.video.pkts - self.video.bad, b.cum_lost, b.jitter);
                }
            }
            log(line);
        }
    }

    fn peer_rtcp(&mut self, kind: Kind, buf: &[u8]) {
        if let Some(r) = self.rtcp.as_mut() {
            r.on_rtcp(kind, buf, now_ms());
        }
        if let Some(s) = self.send_rate.as_mut() {
            s.on_rtcp(if kind == Kind::Video { 'v' } else { 'a' }, buf);
        }
    }

    /// PSFB PLI / FIR aimed at our SSRC: the window encodes a key frame.
    fn on_video_rtcp(&mut self, buf: &[u8]) {
        let mut o = 0;
        while o + 12 <= buf.len() {
            if buf[o] >> 6 != 2 {
                return;
            }
            let len = (u16::from_be_bytes([buf[o + 2], buf[o + 3]]) as usize + 1) * 4;
            let fmt = buf[o] & 0x1f;
            if buf[o + 1] == 206 && (fmt == 1 || (fmt == 4 && o + 16 <= buf.len())) {
                let at = if fmt == 1 { o + 8 } else { o + 12 };
                let target = u32::from_be_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]);
                if target == self.local_uid {
                    self.key_requests += 1;
                    emit("keyframe", json!([if fmt == 1 { "pli" } else { "fir" }]));
                }
            }
            o += len;
        }
    }

    // --- P2P -----------------------------------------------------------------------

    fn p2p_start(&mut self) {
        self.p2p_cands.clear();
        if let Some(list) = self.params.get("p2pCandidates").and_then(Value::as_array).cloned() {
            for (index, c) in list.iter().enumerate() {
                let ip = c.get("ip").and_then(Value::as_str).unwrap_or("");
                let port = num(c.get("port"));
                let v4 = ip.split('.').count() == 4 && ip.split('.').all(|x| !x.is_empty() && x.chars().all(|d| d.is_ascii_digit()));
                if v4 && port > 0.0 {
                    if let Ok(a) = format!("{ip}:{}", port as u16).parse::<SocketAddr>() {
                        self.p2p_cands.push((a, index as u32));
                    }
                }
            }
        }
        if self.p2p_cands.is_empty() {
            return;
        }
        self.p2p_t0 = Instant::now();
        log(format!("media: P2P binding to {} candidate(s)", self.p2p_cands.len()));
        self.p2p_bind();
    }

    fn p2p_bind(&mut self) {
        let elapsed = self.p2p_t0.elapsed();
        if !self.active || self.p2p_path.is_some() || elapsed > P2P_BIND_GIVE_UP {
            return;
        }
        for (addr, index) in self.p2p_cands.clone() {
            self.send_to(&zrtp::p2p_bind(self.callee, self.call_id, self.p2p_seq, index), Some(addr), "P2P bind");
        }
        self.p2p_seq = self.p2p_seq.wrapping_add(1);
        self.timers.add(if elapsed < Duration::from_millis(1000) { P2P_BIND_FAST } else { P2P_BIND_SLOW }, false, Timer::P2pBind);
    }

    fn p2p_up(&mut self, from: SocketAddr) {
        if self.p2p_path == Some(from) {
            return;
        }
        self.p2p_path = Some(from);
        log("media: P2P path up");
    }

    fn on_p2p(&mut self, m: &[u8], from: SocketAddr) {
        if m.len() < 8 {
            return;
        }
        let kind = m[7];
        if !self.p2p_seen {
            self.p2p_seen = true;
            log(format!("media: first P2P packet from the peer (kind {kind})"));
        }
        let h = p2p::HEADER_LEN;
        if kind == p2p::BIND_PEER && m.len() >= 19 {
            let ans = zrtp::p2p_bind_answer(self.callee, self.call_id, m, &format!("{}|{}", from.ip(), from.port()));
            self.send_to(&ans, Some(from), "P2P answer");
        } else if kind == p2p::KEEPALIVE && m.get(9) == Some(&1) {
            self.send_to(&zrtp::p2p_pong(self.callee, m), Some(from), "P2P pong");
            self.p2p_up(from);
        } else if kind == p2p::VIDEO && m.len() > h + 12 {
            self.p2p_up(from);
            self.on_video(&m[h..]);
        } else if kind == p2p::VIDEO_RTCP && m.len() > h {
            self.on_video_rtcp(&m[h..]);
            self.peer_rtcp(Kind::Video, &m[h..]);
        } else if kind == p2p::AUDIO_RTCP && m.len() > h {
            self.peer_rtcp(Kind::Audio, &m[h..]);
        } else if kind == p2p::AUDIO && m.len() > h + 12 {
            self.counters.rx_p2p += 1;
            self.p2p_up(from);
            self.on_audio(&m[h..]);
        }
    }

    // --- receive -------------------------------------------------------------------

    fn on_udp(&mut self, m: &[u8], from: SocketAddr) {
        if let Some(&t) = m.first() {
            *self.rx_types.entry(t).or_insert(0) += 1;
        }
        match m.first() {
            Some(&msg::P2P_EXT) => {
                if self.active && !self.params.is_null() {
                    self.on_p2p(m, from);
                }
                return;
            }
            Some(&msg::VIDEO_FEC) if m.len() > 5 => {
                if self.active && self.started_at > 0.0 {
                    self.on_video_rtcp(&m[5..]);
                    self.peer_rtcp(Kind::Video, &m[5..]);
                }
                return;
            }
            Some(&msg::AUDIO_FEC) if m.len() > 5 => {
                if self.active && self.started_at > 0.0 {
                    self.peer_rtcp(Kind::Audio, &m[5..]);
                }
                return;
            }
            _ => {}
        }
        let Some(p) = zrtp::unwrap(m) else { return };
        match p {
            Packet::Control(c) if c.msg_type == msg::SERVER_REPLY && (c.cmd == cmd::INIT_CALL_CALLER || c.cmd == cmd::INIT_CALL_CALLEE) => {
                let Some(server) = self.servers.iter().position(|s| s.addr == Some(from)) else { return };
                if self.replies.iter().any(|r| r.server == server) {
                    return;
                }
                let res = c.res.unwrap_or(u32::MAX);
                if res != 0 {
                    // One refusal per server, as zrtc-media.js keeps them.
                    if !self.refused.insert(server) {
                        return;
                    }
                    log(format!("media: InitZRTP refused, res = {} reply {}", c.res.map_or("undefined".into(), |r| r.to_string()), hex(m)));
                    self.refusals += 1;
                    self.on_init_reply();
                    return;
                }
                let sent_at = self.init.as_ref().map_or(Instant::now(), |i| i.sent_at);
                self.replies.push(Reply { server, token: c.server_token.unwrap_or(0), rtt_ms: sent_at.elapsed().as_millis() as u64, public_addr: c.public_addr.clone() });
                self.on_init_reply();
            }
            Packet::Control(c) if c.msg_type == msg::SERVER_REPLY && c.cmd == cmd::END_CALL => {
                let from_active = self.active_server.and_then(|i| self.server_addr(i)) == Some(from);
                let got = c.call_id.unwrap_or(0);
                let peer = from_active && !(got != 0 && self.call_id != 0 && got != self.call_id);
                if peer {
                    if self.active && self.started_at > 0.0 {
                        log("media: EndCall from the active server");
                        emit("peerEnd", json!([]));
                    }
                } else {
                    let call = if got != 0 { got.to_string() } else { "-".into() };
                    log(format!("media: EndCall ignored from {}:{} call {call} (not the active server)", from.ip(), from.port()));
                }
            }
            Packet::Media { msg_type: msg::AUDIO_RTP_DOWN, payload, .. } => self.on_audio(payload),
            Packet::Media { msg_type: msg::VIDEO_DOWN, payload, .. } => self.on_video(payload),
            _ => {}
        }
    }

    fn on_audio(&mut self, rtp: &[u8]) {
        let Some(keys) = &self.keys else { return };
        if rtp.len() < 12 {
            return;
        }
        let seq = u16::from_be_bytes([rtp[2], rtp[3]]);
        let roc = self.rx_roc.guess(seq);
        let Some(got) = srtp::unprotect_rtp(rtp, keys, roc) else {
            self.counters.rx_bad += 1;
            return;
        };
        self.rx_roc.update(seq, roc);
        let now = now_ms();
        if let Some(r) = self.rtcp.as_mut() {
            r.on_rtp(Kind::Audio, rtp, now);
        }
        self.counters.rx += 1;
        self.counters.rx_second += 1;
        self.last_rx_at = now;
        if self.counters.rx == 1 {
            log("media: first audio from peer");
        }
        // earlyAudioPhase: the answer may have been lost on the control channel.
        if self.started_at == 0.0 {
            match self.counters.rx {
                1 => emit("earlyAudio", json!(["first"])),
                50 => emit("earlyAudio", json!(["stream"])),
                _ => {}
            }
        }
        if let Some(a) = self.audio.as_mut() {
            if let Some(stdin) = a.stdin.as_mut() {
                let mut f = Vec::with_capacity(11 + got.payload.len());
                f.push(b'F');
                f.extend_from_slice(&(((roc as u64) * 0x10000 + seq as u64) as u32).to_be_bytes());
                f.extend_from_slice(&got.ssrc.to_be_bytes());
                f.extend_from_slice(&(got.payload.len() as u16).to_be_bytes());
                f.extend_from_slice(&got.payload);
                let _ = stdin.write_all(&f);
            }
        }
    }

    /// Received video: not shown by this backend yet, but decrypted so its
    /// transport-wide numbers reach the transport-cc we send (else the phone's
    /// estimator reads every video packet as lost).
    fn on_video(&mut self, rtp: &[u8]) {
        let Some(keys) = &self.keys else { return };
        if rtp.len() < 12 {
            return;
        }
        let seq = u16::from_be_bytes([rtp[2], rtp[3]]);
        let roc = self.video_roc.guess(seq);
        self.video.pkts += 1;
        if srtp::unprotect_rtp(rtp, keys, roc).is_none() {
            self.video.bad += 1;
            return;
        }
        self.video_roc.update(seq, roc);
        self.last_rx_at = now_ms();
        if let Some(r) = self.rtcp.as_mut() {
            r.on_rtp(Kind::Video, rtp, self.last_rx_at);
        }
        let pt = rtp[1] & 0x7f;
        *self.video.pt.entry(pt).or_insert(0) += 1;
        if rtp[1] & 0x80 != 0 {
            self.video.marker += 1;
        }
        if self.video.pkts == 1 {
            log(format!("media: first video from peer (PT {pt})"));
        }
        if !self.video_noted {
            self.video_noted = true;
            log("media: video is not shown with ZCALL_MEDIA=rust yet (audio only)");
        }
    }

    // --- audio (zcall-audio) -----------------------------------------------------------

    fn start_audio(&mut self, a: &Value) {
        // The lines zrtc-media.js logs while it builds the command.
        for l in a.get("logs").and_then(Value::as_array).into_iter().flatten() {
            if let Some(l) = l.as_str() {
                log(l);
            }
        }
        let Some(bin) = a.get("bin").and_then(Value::as_str) else { return };
        let args: Vec<String> = a.get("args").and_then(Value::as_array).map(|v| v.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        let mut child = match Command::new(bin).args(&args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
            Ok(c) => c,
            Err(e) => {
                log(format!("media: zcall-audio exited {e}"));
                emit("audioFault", json!(["audio"]));
                return;
            }
        };
        self.audio_gen += 1;
        let gen = self.audio_gen;
        let (mut stdout, stderr) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            // 'L' u8 level, u16 len, Opus (or 'A' u16 len, Opus from older engines).
            let mut hdr = [0u8; 1];
            loop {
                if stdout.read_exact(&mut hdr).is_err() {
                    return;
                }
                let (level, len) = match hdr[0] {
                    b'L' => {
                        let mut h = [0u8; 3];
                        if stdout.read_exact(&mut h).is_err() {
                            return;
                        }
                        (h[0], u16::from_be_bytes([h[1], h[2]]) as usize)
                    }
                    b'A' => {
                        let mut h = [0u8; 2];
                        if stdout.read_exact(&mut h).is_err() {
                            return;
                        }
                        (127, u16::from_be_bytes(h) as usize)
                    }
                    _ => continue,
                };
                let mut opus = vec![0u8; len];
                if stdout.read_exact(&mut opus).is_err() || tx.send(Event::AudioFrame(gen, level, opus)).is_err() {
                    return;
                }
            }
        });
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if tx.send(Event::AudioLine(gen, line)).is_err() {
                    return;
                }
            }
        });
        self.audio = Some(Audio { stdin: child.stdin.take(), child, gen });
        // Exit watcher: poll, since the child is owned here.
        if self.muted {
            self.audio_write(&[b'M', 1]);
        }
        if self.speaker_off {
            self.audio_write(&[b'S', 1]);
        }
    }

    fn audio_write(&mut self, b: &[u8]) {
        if let Some(stdin) = self.audio.as_mut().and_then(|a| a.stdin.as_mut()) {
            let _ = stdin.write_all(b);
        }
    }

    fn check_audio_exit(&mut self) {
        let Some(a) = self.audio.as_mut() else { return };
        if let Ok(Some(status)) = a.child.try_wait() {
            let gen = a.gen;
            let code = match status.code() {
                Some(c) => c.to_string(),
                None => {
                    use std::os::unix::process::ExitStatusExt;
                    signal_name(status.signal().unwrap_or(0))
                }
            };
            let _ = self.tx.send(Event::AudioExit(gen, code));
            self.audio = None;
        }
    }

    fn on_audio_line(&mut self, gen: u64, l: &str) {
        if l.trim().is_empty() {
            return;
        }
        let current = self.audio.as_ref().map_or(false, |a| a.gen == gen);
        let body = l.strip_prefix("[audio-io] ");
        log(match body {
            Some(b) => format!("audio: {b}"),
            None => l.to_string(),
        });
        let Some(b) = body else { return };
        if let Some(rest) = b.strip_prefix("fault: ") {
            let kind = rest.split(|c: char| !c.is_alphanumeric()).next().unwrap_or("");
            if (kind == "mic" || kind == "speaker") && current {
                emit("audioFault", json!([kind]));
            }
        }
        if let Some(rest) = b.strip_prefix("mic volume: ") {
            if let Some(pct) = rest.strip_suffix('%').or_else(|| rest.split('%').next()).and_then(|v| v.parse::<u32>().ok()) {
                if current {
                    emit("micVolume", json!([pct]));
                }
            }
        }
    }

    fn on_audio_frame(&mut self, level: u8, opus: &[u8]) {
        self.send_audio_frame(opus);
        // The call window's mic meter: the loudest frame of every 100 ms.
        self.mic_peak = self.mic_peak.min(level);
        let now = now_ms();
        if now - self.mic_level_at < 100.0 {
            return;
        }
        self.mic_level_at = now;
        emit("micLevel", json!([self.mic_peak]));
        self.mic_peak = 127;
    }

    fn send_audio_frame(&mut self, opus: &[u8]) {
        if !self.active || self.active_server.is_none() {
            return;
        }
        let Some(keys) = &self.keys else { return };
        let hdr = rtp::audio_header(self.seq, self.ts, self.local_uid, self.tw_seq, false, rtp::AUDIO_PT);
        let pkt = srtp::protect_rtp(&hdr, opus, keys, self.seq, self.local_uid, self.roc);
        self.send_server(&zrtp::wrap_media(msg::AUDIO_RTP, self.server_token, &pkt), self.active_server, "audio");
        // Same packet over P2P once the peer reached us: it drops duplicates by seq.
        if self.p2p_path.is_some() {
            self.send_to(&zrtp::p2p_wrap(self.callee, self.call_id, p2p::AUDIO, &pkt), self.p2p_path, "P2P audio");
        }
        let tw = self.tw_seq;
        if let Some(s) = self.send_rate.as_mut() {
            s.on_sent(tw, None);
        }
        self.counters.tx += 1;
        self.audio_octets += opus.len() as u64;
        self.note_audio_tx();
        self.seq = self.seq.wrapping_add(1);
        if self.seq == 0 {
            self.roc += 1;
        }
        self.ts = self.ts.wrapping_add(rtp::FRAME_SAMPLES);
        self.tw_seq = self.tw_seq.wrapping_add(1);
    }

    /// When our 20 ms frames really leave: one line per 10 s.
    fn note_audio_tx(&mut self) {
        let now = now_ms();
        let w = &mut self.a_tx;
        if w.last > 0.0 {
            let gap = now - w.last;
            w.max_gap = w.max_gap.max(gap);
            if gap > 40.0 {
                w.gaps40 += 1;
            }
            if gap > 60.0 {
                w.gaps60 += 1;
            }
        }
        w.last = now;
        w.n += 1;
        if now - w.since < AUDIO_TX_LOG_MS {
            return;
        }
        log(format!(
            "audio tx: {} frames in {:.1} s, max gap {} ms, gaps >40 ms {}, >60 ms {}{}",
            w.n,
            (now - w.since) / 1000.0,
            fmt_num(w.max_gap),
            w.gaps40,
            w.gaps60,
            if self.p2p_path.is_some() { ", relay + P2P" } else { ", relay" }
        ));
        self.a_tx = ATxWin { n: 0, last: now, max_gap: 0.0, gaps40: 0, gaps60: 0, since: now };
    }

    // --- commands --------------------------------------------------------------------

    fn on_cmd(&mut self, c: &Value) -> bool {
        let s = |k: &str| c.get(k).and_then(Value::as_str);
        match s("cmd").unwrap_or("") {
            "negotiated" => self.on_negotiated(c.get("params").cloned().unwrap_or(Value::Null), c.get("sendUdp").and_then(Value::as_bool).unwrap_or(false)),
            "prepare" => self.prepare(s("role").unwrap_or("caller")),
            "start" => self.start(s("role").unwrap_or("caller"), s("rtpSerIp"), c.get("audio")),
            "mute" => {
                self.muted = c.get("on").and_then(Value::as_bool).unwrap_or(false);
                let on = self.muted as u8;
                self.audio_write(&[b'M', on]);
            }
            "speaker" => {
                self.speaker_off = c.get("off").and_then(Value::as_bool).unwrap_or(false);
                let off = self.speaker_off as u8;
                self.audio_write(&[b'S', off]);
            }
            "micVolume" => {
                let pct = num(c.get("pct"));
                if pct.is_finite() {
                    self.audio_write(&[b'V', pct.round().clamp(0.0, 100.0) as u8]);
                }
            }
            "device" => {
                let kind = s("kind").unwrap_or("");
                let name = s("name").unwrap_or("").to_string();
                if kind != "mic" && kind != "speaker" {
                    return true;
                }
                if kind == "mic" {
                    self.devices.0 = Some(name.clone());
                } else {
                    self.devices.1 = Some(name.clone());
                }
                let n = name.as_bytes();
                let mut b = vec![b'D', if kind == "mic" { b'i' } else { b'o' }, (n.len() >> 8) as u8, n.len() as u8];
                b.extend_from_slice(n);
                self.audio_write(&b);
            }
            "stop" => {
                self.stop();
                return false;
            }
            other => log(format!("media: unknown command {other}")),
        }
        true
    }
}

fn signal_name(sig: i32) -> String {
    match sig {
        1 => "SIGHUP",
        2 => "SIGINT",
        6 => "SIGABRT",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        15 => "SIGTERM",
        _ => return format!("signal {sig}"),
    }
    .into()
}

fn main() {
    // A closed stdout (Node gone) must not kill us mid-EndCall.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    let (tx, rx): (Sender<Event>, Receiver<Event>) = mpsc::channel();
    let stdin_tx = tx.clone();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(v) => {
                    if stdin_tx.send(Event::Cmd(v)).is_err() {
                        return;
                    }
                }
                Err(e) => log(format!("media: bad command ({e}): {line}")),
            }
        }
        let _ = stdin_tx.send(Event::StdinClosed);
    });

    let mut m = Media::new(tx);
    m.timers.add(STATS_INTERVAL, true, Timer::Stats);
    loop {
        let wait = m.timers.next_deadline().map_or(Duration::from_millis(200), |d| d.saturating_duration_since(Instant::now())).min(Duration::from_millis(200));
        match rx.recv_timeout(wait) {
            Ok(Event::Cmd(c)) => {
                if !m.on_cmd(&c) {
                    break;
                }
            }
            Ok(Event::StdinClosed) => {
                if m.active || m.audio.is_some() {
                    m.stop();
                }
                break;
            }
            Ok(Event::Udp(b, from)) => m.on_udp(&b, from),
            Ok(Event::AudioFrame(gen, level, opus)) => {
                if m.audio.as_ref().map_or(false, |a| a.gen == gen) {
                    m.on_audio_frame(level, &opus);
                }
            }
            Ok(Event::AudioLine(gen, line)) => m.on_audio_line(gen, &line),
            Ok(Event::AudioExit(gen, code)) => {
                log(format!("media: zcall-audio exited {code}"));
                if m.active && m.audio_gen == gen {
                    emit("audioFault", json!(["audio"]));
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if m.timers.list.iter().all(|t| t.2 != Timer::Stats) {
            m.timers.add(STATS_INTERVAL, true, Timer::Stats);
        }
        for t in m.timers.due() {
            m.on_timer(t);
        }
        m.check_audio_exit();
    }
    // zcall-audio's last lines (its "stats" at exit) are still on their way
    // from its stderr reader: log them before going.
    let deadline = Instant::now() + Duration::from_millis(300);
    while let Ok(ev) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        if let Event::AudioLine(gen, line) = ev {
            m.on_audio_line(gen, &line);
        }
    }
    if let Some(l) = m.audio_exit.take() {
        log(l);
    }
}
