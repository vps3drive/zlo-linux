//! One "diag:" log line every DIAG_SEC during the call, for the test calls:
//! the playout counters of the period, the card stream's fill, the
//! microphone's longest gap, and what PipeWire saw: the quantum of the graph,
//! new overruns (pw-top ERR) of the nodes we use, and speaker resyncs in its
//! journal. ZCALL_AUDIO_DIAG=0 turns it off.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::guard::ClipGuard;
use crate::io::Mic;
use crate::log;
use crate::playout::{Playout, Stats};
use crate::processing::rt_summary;
use crate::pulse;

const DIAG_SEC: u64 = 10;
const KEYS: [&str; 10] = ["opus_ok", "lost", "fec", "plc", "underrun", "accelerate", "accelerate_loud", "reset", "late", "dup"];

/// (quantum, rate, err, driver) of the running nodes of the last pw-top snapshot.
fn pw_nodes() -> HashMap<String, (i64, i64, i64, bool)> {
    let mut nodes = HashMap::new();
    let Ok(out) = std::process::Command::new("pw-top").args(["-b", "-n", "2"]).output() else { return nodes };
    let out = String::from_utf8_lossy(&out.stdout);
    let last = out.rsplit("S   ID").next().unwrap_or("");
    for line in last.lines().skip(1) {
        let c: Vec<&str> = line.split_whitespace().collect();
        if c.len() < 10 || c[0] != "R" {
            continue;
        }
        let follower = c[9..].contains(&"+");
        if let (Ok(q), Ok(r), Ok(e)) = (c[2].parse(), c[3].parse(), c[8].parse()) {
            nodes.insert(c[c.len() - 1].to_string(), (q, r, e, !follower));
        }
    }
    nodes
}

fn resyncs(since: u64) -> Option<u64> {
    if !pulse::which("journalctl") {
        return None;
    }
    let out = std::process::Command::new("journalctl")
        .args(["--user", "-u", "pipewire", "--since", &format!("@{since}"), "-o", "cat", "--no-pager"])
        .output()
        .ok()?;
    let mut n = 0;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if line.contains("resync") {
            n += 1;
            if line.contains("suppressed") {
                if let Some(v) = line.rsplit('(').next().and_then(|s| s.split_whitespace().next()).and_then(|s| s.parse::<u64>().ok()) {
                    n += v;
                }
            }
        }
    }
    Some(n)
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn start(playout: Arc<Playout>, mic: Option<Arc<Mic>>, guard: Option<Arc<ClipGuard>>, tag: String) {
    std::thread::Builder::new()
        .name("diag".into())
        .spawn(move || run(playout, mic, guard, tag))
        .unwrap();
}

fn run(p: Arc<Playout>, mic: Option<Arc<Mic>>, guard: Option<Arc<ClipGuard>>, tag: String) {
    let has_pw = pulse::which("pw-top");
    let mut prev = Stats::new();
    let mut prev_err: HashMap<String, i64> = HashMap::new();
    let mut since = now_secs();
    loop {
        for _ in 0..DIAG_SEC * 10 {
            if p.stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let (stats, buf) = {
            let g = p.inner.lock().unwrap();
            let buf: Vec<String> = g.sources.iter().map(|(k, s)| format!("{k}: {}", s.target * 20)).collect();
            (g.stats.clone(), buf)
        };
        let d: Vec<String> = KEYS
            .iter()
            .filter_map(|k| {
                let delta = stats.get(k).copied().unwrap_or(0) - prev.get(k).copied().unwrap_or(0);
                (delta > 0).then(|| format!("{k}: {delta}"))
            })
            .collect();
        prev = stats;
        let (dev, mix) = {
            let mut per = p.period.lock().unwrap();
            let dev = match (per.dev_min, per.dev_max) {
                (Some(a), Some(b)) => format!("{a:.0}..{b:.0}"),
                _ => "-".into(),
            };
            let mix = per.mix_max.as_secs_f64() * 1000.0;
            *per = Default::default();
            (dev, mix)
        };
        let mut mic_s = "-".to_string();
        if let Some(m) = &mic {
            let (gap, stalls) = m.take_gaps();
            mic_s = format!("max gap {:.0} ms, stalls {stalls}", gap.as_secs_f64() * 1000.0);
            if let Some(g) = &guard {
                mic_s += &format!(", {}, gain steps {}", g.report(), g.steps.load(Ordering::Relaxed));
            }
        }
        let mut pw = String::new();
        if has_pw {
            let nodes = pw_nodes();
            let ours: Vec<_> = nodes
                .iter()
                .filter(|(n, _)| n.starts_with("alsa_") || n.starts_with("bluez_") || n.starts_with(&tag) || *n == "Zalo" || *n == "parec")
                .collect();
            let drivers: Vec<String> = ours.iter().filter(|(_, v)| v.3).map(|(n, v)| format!("{n} q{}", v.0)).collect();
            let mut xr = Vec::new();
            for (n, v) in &ours {
                if n.starts_with("alsa_") || n.starts_with("bluez_") {
                    if let Some(&old) = prev_err.get(*n) {
                        if v.2 > old {
                            let parts: Vec<&str> = n.split('.').collect();
                            xr.push(format!("{}.{} +{}", parts[0], parts[parts.len() - 1], v.2 - old));
                        }
                    }
                    prev_err.insert((*n).clone(), v.2);
                }
            }
            pw = format!(
                "; pipewire driver {}, xruns {}",
                if drivers.is_empty() { "-".into() } else { drivers.join(", ") },
                if xr.is_empty() { "0".into() } else { xr.join(", ") }
            );
        }
        let now = now_secs();
        let rs = resyncs(since).map_or(String::new(), |n| format!(", resyncs {n}"));
        since = now;
        log!(
            "diag: playout {{{}}}, buffer ms {{{}}}, card queue ms {dev}, mix max {mix:.1} ms; mic {mic_s}{pw}{rs}; rt {}",
            d.join(", "),
            buf.join(", "),
            rt_summary()
        );
    }
}
