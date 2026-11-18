//! WebRTC audio processing of PipeWire (module-echo-cancel) in a private
//! pipewire process: a processed microphone source and a speaker sink (the
//! echo reference) that exist as long as the process. The system's default
//! devices stay as they are.
//!
//! Realtime for the processing: module-rt asks xdg-desktop-portal first, and on
//! Debian 13 the portal fails ("Realtime error: Could not get pidns for pid
//! ...: Not a directory"), so the threads stayed at normal priority. RTKit
//! directly (rtportal.enabled = false) gives data-loop.0 RR 20.
//! The graph period stays 256 samples. A machine whose USB headset drives
//! PipeWire at quantum 1024 runs the canceller on that period;
//! node.force-quantum on these nodes locks the graph at 256 while they are
//! active and lets it go when the call ends. Quantum 512 broke the sink every
//! 10 ms (1 kHz played as 964.8 Hz, THD+N -3 dB instead of -40 dB), so nothing
//! here sets 512.
//! delay_agnostic is set false for the old WebRTC AEC2 plugin, which hunts the
//! delay during doubletalk and rasps. PipeWire 1.4's AEC3 build ignores it and
//! aligns playback with capture itself; do not also set buffer.play_delay.
//! voice_detection and transient_suppression default on in that build and are
//! what gates and rasps the near end while the far end talks. Zalo's desktop
//! path does not enable either. No gain control: webrtc.gain_control turns on
//! two digital AGCs at once, which pump the noise up and distort loud speech.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::log;
use crate::pulse;

const EC_CONF: &str = r#"context.properties = {
    log.level = 0
    default.clock.rate = 48000
    default.clock.quantum = 256
    default.clock.min-quantum = 256
    default.clock.max-quantum = 256
}
context.spa-libs = {
    audio.convert.* = audioconvert/libspa-audioconvert
    support.*       = support/libspa-support
}
context.modules = [
    { name = libpipewire-module-rt args = { nice.level = -11 rt.prio = 88 rtportal.enabled = false } flags = [ ifexists nofail ] }
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-adapter }
    { name = libpipewire-module-echo-cancel
      args = {
        library.name = aec/libspa-aec-webrtc
        aec.args = {
            webrtc.gain_control = false
            webrtc.noise_suppression = @NS@
            webrtc.high_pass_filter = @HPF@
            webrtc.voice_detection = @VAD@
            webrtc.transient_suppression = false
            webrtc.delay_agnostic = false
            webrtc.extended_filter = true
        }
        audio.rate = 16000
        audio.channels = 1
        audio.position = [ MONO ]
        capture.props  = { node.name = "@TAG@.capture" node.passive = true node.force-quantum = 256 @MIC@ }
        source.props   = { node.name = "@TAG@_source" node.description = "Zalo call microphone (processed)" node.force-quantum = 256 priority.session = 0 priority.driver = 0 }
        sink.props     = { node.name = "@TAG@_sink" node.description = "Zalo call speaker (processed)" node.force-quantum = 256 priority.session = 0 priority.driver = 0 }
        playback.props = { node.name = "@TAG@.playback" node.passive = true node.force-quantum = 256 @SPEAKER@ }
      }
    }
]
"#;

pub struct Processing {
    pub tag: String,
    pub source: String,
    pub sink: String,
    high_pass: bool,
    noise_suppression: bool,
    voice_detection: bool,
    proc: Option<Child>,
    conf: Option<PathBuf>,
}

/// A device name inside the config's quoted string.
fn quoted(name: &str) -> String {
    name.replace('\\', "\\\\").replace('"', "\\\"")
}

impl Processing {
    pub fn new(high_pass: bool, noise_suppression: bool, voice_detection: bool) -> Self {
        let tag = format!("zcall_ec_{}", std::process::id());
        Processing {
            source: format!("{tag}_source"),
            sink: format!("{tag}_sink"),
            tag,
            high_pass,
            noise_suppression,
            voice_detection,
            proc: None,
            conf: None,
        }
    }

    fn config(&self, mic: Option<&str>, speaker: Option<&str>) -> String {
        let target = |n: Option<&str>| n.map_or(String::new(), |n| format!("target.object = \"{}\" node.dont-reconnect = true", quoted(n)));
        let b = |v: bool| if v { "true" } else { "false" };
        EC_CONF
            .replace("@TAG@", &self.tag)
            .replace("@MIC@", &target(mic))
            .replace("@SPEAKER@", &target(speaker))
            .replace("@HPF@", b(self.high_pass))
            .replace("@NS@", b(self.noise_suppression))
            .replace("@VAD@", b(self.voice_detection))
    }

    /// (Re)start on these devices (None: follow the default). False if it is not available.
    pub fn start(&mut self, mic: Option<&str>, speaker: Option<&str>) -> bool {
        self.stop();
        let conf = std::env::temp_dir().join(format!("zcall-ec-{}-{:x}.conf", std::process::id(), rand_nonce()));
        if let Err(e) = std::fs::write(&conf, self.config(mic, speaker)) {
            log!("audio processing unavailable: {e}");
            return false;
        }
        self.conf = Some(conf.clone());
        let mut cmd = Command::new("pipewire");
        cmd.arg("-c").arg(&conf).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        // The pipewire child goes away with us, even when we are killed.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        match cmd.spawn() {
            Ok(c) => self.proc = Some(c),
            Err(e) => {
                log!("audio processing unavailable: {e}");
                self.stop();
                return false;
            }
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Some(Ok(Some(_))) = self.proc.as_mut().map(|p| p.try_wait()) {
                break;
            }
            if pulse::names("sources").contains(&self.source) && pulse::names("sinks").contains(&self.sink) {
                log!("audio processing on (mic {}, speaker {}, quantum 256)", mic.unwrap_or("default"), speaker.unwrap_or("default"));
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        log!("audio processing did not start; using the devices directly");
        self.stop();
        false
    }

    pub fn stop(&mut self) {
        if let Some(mut p) = self.proc.take() {
            unsafe { libc::kill(p.id() as i32, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match p.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                    _ => {
                        let _ = p.kill();
                        let _ = p.wait();
                        break;
                    }
                }
            }
        }
        if let Some(c) = self.conf.take() {
            let _ = std::fs::remove_file(c);
        }
    }
}

impl Drop for Processing {
    fn drop(&mut self) {
        self.stop();
    }
}

fn rand_nonce() -> u128 {
    let mut b = [0u8; 8];
    unsafe { libc::getrandom(b.as_mut_ptr() as *mut libc::c_void, 8, 0) };
    u64::from_le_bytes(b) as u128
}

// --- realtime -----------------------------------------------------------------

fn sched_name(p: Option<i64>) -> &'static str {
    match p {
        Some(0) => "TS",
        Some(1) => "FIFO",
        Some(2) => "RR",
        Some(3) => "BATCH",
        Some(5) => "IDLE",
        Some(6) => "DL",
        _ => "?",
    }
}

fn policy(pid: u32, tid: u32) -> Option<i64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/stat")).ok()?;
    stat.rsplit_once(')')?.1.split_whitespace().nth(38)?.parse().ok()
}

/// (process name, pid, tid) of the audio threads (data-loop) of our PipeWire processes.
fn pipewire_loops() -> Vec<(String, u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let uid = unsafe { libc::getuid() };
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else { return out };
    for e in dir.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        if e.metadata().map(|m| m.uid() != uid).unwrap_or(true) {
            continue;
        }
        let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) else { continue };
        let comm = comm.trim().to_string();
        if comm != "pipewire" && comm != "pipewire-pulse" {
            continue;
        }
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else { continue };
        for t in tasks.flatten() {
            let Some(tid) = t.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            if std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/comm")).map_or(false, |c| c.starts_with("data-loop")) {
                out.push((comm.clone(), pid, tid));
            }
        }
    }
    out
}

/// e.g. "pipewire/812 RR, pipewire-pulse/815 RR" (one entry per audio thread).
pub fn rt_summary() -> String {
    let s: Vec<String> = pipewire_loops().iter().map(|(c, p, t)| format!("{c}/{p} {}", sched_name(policy(*p, *t)))).collect();
    if s.is_empty() {
        "-".into()
    } else {
        s.join(", ")
    }
}

/// PipeWire's audio threads should run SCHED_RR/FIFO (module-rt). When they
/// run at normal priority a busy CPU (the video call's software H.264) delays
/// them: the USB microphone overran 65-85 times and the speaker resynced 49-68
/// times in 30 s of load, 0 and 0 with them realtime. Ask RTKit for what
/// module-rt would have got. Only our own processes; nothing changes once
/// PipeWire restarts. ZCALL_PW_RT=0: only report.
pub fn pipewire_realtime() {
    let low: Vec<_> = pipewire_loops().into_iter().filter(|(_, p, t)| policy(*p, *t) == Some(0)).collect();
    if low.is_empty() {
        log!("realtime: {}", rt_summary());
        return;
    }
    if std::env::var("ZCALL_PW_RT").as_deref() == Ok("0") || !pulse::which("busctl") {
        log!("realtime: PipeWire audio at normal priority ({}); expect crackle when the CPU is busy", rt_summary());
        return;
    }
    let rtkit = ["org.freedesktop.RealtimeKit1", "/org/freedesktop/RealtimeKit1", "org.freedesktop.RealtimeKit1"];
    let prio = Command::new("busctl")
        .args(["--system", "get-property"])
        .args(rtkit)
        .arg("MaxRealtimePriority")
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().nth(1).and_then(|v| v.parse::<u32>().ok()))
        .unwrap_or(20);
    for (c, p, t) in &low {
        match Command::new("busctl")
            .args(["--system", "call"])
            .args(rtkit)
            .args(["MakeThreadRealtimeWithPID", "ttu", &p.to_string(), &t.to_string(), &prio.to_string()])
            .output()
        {
            Ok(o) if !o.status.success() => log!("realtime: RTKit refused {c}/{p} thread {t}: {}", String::from_utf8_lossy(&o.stderr).trim()),
            Ok(_) => {}
            Err(e) => {
                log!("realtime: RTKit unavailable: {e}");
                break;
            }
        }
    }
    log!("realtime: PipeWire audio was at normal priority, asked RTKit for RR {prio}: now {}", rt_summary());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_fills_every_placeholder() {
        let p = Processing::new(true, false, false);
        let c = p.config(Some("alsa_input.usb"), None);
        assert!(!c.contains('@'));
        assert!(c.contains("target.object = \"alsa_input.usb\" node.dont-reconnect = true"));
        assert!(c.contains("webrtc.noise_suppression = false"));
        assert!(c.contains("webrtc.high_pass_filter = true"));
        assert!(c.contains(&format!("node.name = \"{}\"", p.source)));
    }
}
