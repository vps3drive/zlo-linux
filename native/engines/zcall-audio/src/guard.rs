//! ClipGuard: turns the microphone's PulseAudio source down when its speech
//! clips, as the real engine's analog AGC does (AgcManagerDirect turns the OS
//! microphone volume; one knob, a few dB at a time). A USB headset at 0 dB
//! clipped hard. A 6 dB step every 0.2 s did not work: the level jumped back
//! toward 0 dB between steps and the processed mic then sat at -50..-80 dBFS.
//! One 3 dB step per second, and only when the window actually has
//! MIN_CLIPPED full-scale samples. Never below FLOOR_DB. The level stays for
//! the next calls. ZCALL_MIC_AGC=0: off. Digital AGC stays off
//! (webrtc.gain_control false): two AGCs on this path rasp.
//!
//! It also owns the call window's volume slider ('V') and the
//! "mic volume: N%" lines that feed it.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::log;
use crate::pulse;

struct State {
    source: Option<String>,
    on: bool,
    frames: u32,
    clipped: u32,
    last_step: Option<Instant>,
    total_clipped: u64,
    peak: u16,
    announced_pct: Option<u32>,
    announced_at: Option<Instant>,
    /// The level we last wrote, so a jump back toward 0 dB is visible.
    set_db: Option<f64>,
}

pub struct ClipGuard {
    st: Mutex<State>,
    vol_lock: Mutex<()>,
    busy: AtomicBool,
    /// The call window's slider set the level: no automatic step after it.
    manual: AtomicBool,
    pub steps: AtomicU32,
}

fn enabled(source: &Option<String>) -> bool {
    source.is_some() && std::env::var("ZCALL_MIC_AGC").as_deref() != Ok("0") && pulse::which("pactl")
}

impl ClipGuard {
    const WINDOW: u32 = 10; // frames (200 ms) per decision
    const CLIP: i16 = 31000; // |sample| counted as clipped (after resampling a full-scale flat top)
    const MIN_CLIPPED: u32 = 16; // clipped samples per window (0.5 %) that trigger a step
    const STEP_DB: f64 = 3.0;
    const STEP_SEC: Duration = Duration::from_secs(1);
    const FLOOR_DB: f64 = -24.0;

    pub fn new(source: Option<String>) -> Arc<Self> {
        let on = enabled(&source);
        let g = Arc::new(ClipGuard {
            st: Mutex::new(State {
                source: source.clone(),
                on,
                frames: 0,
                clipped: 0,
                last_step: None,
                total_clipped: 0,
                peak: 0,
                announced_pct: None,
                announced_at: None,
                set_db: None,
            }),
            vol_lock: Mutex::new(()),
            busy: AtomicBool::new(false),
            manual: AtomicBool::new(false),
            steps: AtomicU32::new(0),
        });
        if let Some(s) = &source {
            let db = pulse::volume_db(s).map_or("None".into(), |d| format!("{d}"));
            log!("mic gain guard {} for {s} (now {db} dB)", if on { "on" } else { "off" });
            g.announce();
        }
        g
    }

    pub fn set_source(&self, source: Option<String>) {
        let mut st = self.st.lock().unwrap();
        st.on = enabled(&source);
        st.source = source;
    }

    fn source(&self) -> Option<String> {
        self.st.lock().unwrap().source.clone()
    }

    /// "mic volume: N%" is the line zrtc-media.js shows on the call window's slider.
    pub fn announce(&self) {
        let Some(src) = self.source() else { return };
        if let Some(pct) = pulse::volume_percent(&src) {
            let mut st = self.st.lock().unwrap();
            st.announced_pct = Some(pct);
            st.announced_at = Some(Instant::now());
            drop(st);
            log!("mic volume: {pct}%");
        }
    }

    /// The call window's slider (0..100 %). The level found there by ear
    /// stays: no automatic step after it in this call. A raise that arrives
    /// within 0.6 s of our own announce is the slider repeating its old
    /// position (72% then 100% 0.3 s later, and the knob jumped back toward 0 dB).
    pub fn set_percent(&self, pct: u8) {
        let Some(src) = self.source().filter(|_| pulse::which("pactl")) else {
            log!("mic volume: no PulseAudio source to set");
            return;
        };
        let pct = pct.min(100) as u32;
        let _v = self.vol_lock.lock().unwrap();
        {
            let st = self.st.lock().unwrap();
            if let (Some(a), Some(at)) = (st.announced_pct, st.announced_at) {
                if at.elapsed() < Duration::from_millis(600) && pct > a + 2 {
                    drop(st);
                    log!("mic volume: ignore {pct}% right after {a}%");
                    return;
                }
            }
        }
        if let Some(cur) = pulse::volume_percent(&src) {
            if cur.abs_diff(pct) <= 2 {
                return;
            }
        }
        let raw = (65536.0 * pct as f64 / 100.0).round() as u32;
        if !pulse::set_volume_raw(&src, raw) {
            log!("mic volume: pactl set-source-volume failed");
            return;
        }
        self.manual.store(true, Ordering::SeqCst);
        self.st.lock().unwrap().set_db = pulse::volume_db(&src);
        self.announce();
    }

    pub fn feed(self: &Arc<Self>, a: &[i16]) {
        let n = a.iter().filter(|&&v| v >= Self::CLIP || v <= -Self::CLIP).count() as u32;
        let peak = a.iter().map(|v| v.unsigned_abs()).max().unwrap_or(0);
        let mut st = self.st.lock().unwrap();
        st.clipped += n;
        st.total_clipped += n as u64;
        st.peak = st.peak.max(peak);
        st.frames += 1;
        if st.frames < Self::WINDOW {
            return;
        }
        let clipped = st.clipped;
        st.frames = 0;
        st.clipped = 0;
        let due = st.last_step.map_or(true, |t| t.elapsed() >= Self::STEP_SEC);
        if st.on && !self.manual.load(Ordering::SeqCst) && !self.busy.load(Ordering::SeqCst) && clipped >= Self::MIN_CLIPPED && due {
            st.last_step = Some(Instant::now());
            self.busy.store(true, Ordering::SeqCst);
            let me = Arc::clone(self);
            std::thread::spawn(move || {
                me.step(clipped, Self::STEP_DB);
                me.busy.store(false, Ordering::SeqCst);
            });
        }
    }

    fn step(&self, clipped: u32, step: f64) {
        let _v = self.vol_lock.lock().unwrap();
        if self.manual.load(Ordering::SeqCst) {
            return;
        }
        let Some(src) = self.source() else { return };
        let Some(cur) = pulse::volume_db(&src) else { return };
        if cur <= Self::FLOOR_DB + 0.01 {
            return;
        }
        if let Some(set) = self.st.lock().unwrap().set_db {
            if cur > set + 2.0 {
                log!("mic gain guard: volume back at {cur:.1} dB (last set {set:.1})");
            }
        }
        let new = (cur - step).max(Self::FLOOR_DB);
        // Raw volume (PulseAudio's cubic scale: dB = 60 log10(v / 65536)); pactl
        // misreads "-6.0dB" (it set -9 dB, and "-9.0dB" -18 dB).
        let raw = (65536.0 * 10f64.powf(new / 60.0)).round() as u32;
        pulse::set_volume_raw(&src, raw);
        self.steps.fetch_add(1, Ordering::Relaxed);
        let got = pulse::volume_db(&src);
        self.st.lock().unwrap().set_db = Some(got.unwrap_or(new));
        let got = got.map_or("None".into(), |g| format!("{g}"));
        log!("mic gain guard: {clipped} clipped samples in 200 ms, {src} {cur:.1} -> {got} dB");
        self.announce();
    }

    pub fn report(&self) -> String {
        let mut st = self.st.lock().unwrap();
        let r = format!("clipped {}, peak {:.1} dBFS", st.total_clipped, 20.0 * (st.peak.max(1) as f64 / 32768.0).log10());
        st.total_clipped = 0;
        st.peak = 0;
        r
    }
}
