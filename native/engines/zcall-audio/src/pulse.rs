//! PulseAudio / PipeWire: libpulse-simple streams (loaded at runtime) and the
//! pactl queries for device names and the microphone volume.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use crate::dl::Lib;
use crate::log;
use crate::protocol::{FRAME_BYTES, SAMPLE_RATE};

const PA_STREAM_PLAYBACK: c_int = 1;
const PA_STREAM_RECORD: c_int = 2;
const PA_SAMPLE_S16LE: c_int = 3;
const NONE: u32 = u32::MAX;

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

type Simple = *mut c_void;

struct Api {
    _lib: Lib,
    new: extern "C" fn(*const c_char, *const c_char, c_int, *const c_char, *const c_char, *const SampleSpec, *const c_void, *const BufferAttr, *mut c_int) -> Simple,
    free: extern "C" fn(Simple),
    write: extern "C" fn(Simple, *const c_void, usize, *mut c_int) -> c_int,
    read: extern "C" fn(Simple, *mut c_void, usize, *mut c_int) -> c_int,
    get_latency: extern "C" fn(Simple, *mut c_int) -> u64,
    strerror: Option<extern "C" fn(c_int) -> *const c_char>,
}

fn load() -> Result<Api, String> {
    let lib = Lib::open(&["libpulse-simple.so.0", "libpulse-simple.so"])?;
    // pa_strerror lives in libpulse, which libpulse-simple pulls in.
    let strerror = Lib::open(&["libpulse.so.0", "libpulse.so"]).ok().and_then(|l| {
        let f = l.sym("pa_strerror").ok();
        std::mem::forget(l); // keep it loaded for the process lifetime
        f
    });
    Ok(Api {
        new: lib.sym("pa_simple_new")?,
        free: lib.sym("pa_simple_free")?,
        write: lib.sym("pa_simple_write")?,
        read: lib.sym("pa_simple_read")?,
        get_latency: lib.sym("pa_simple_get_latency")?,
        strerror,
        _lib: lib,
    })
}

fn api() -> Result<&'static Api, String> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(load).as_ref().map_err(|e| e.clone())
}

pub fn simple_available() -> bool {
    api().is_ok()
}

fn describe(api: &Api, err: c_int) -> String {
    match api.strerror {
        Some(f) => {
            let p = f(err);
            if p.is_null() {
                format!("error {err}")
            } else {
                format!("{} ({err})", unsafe { CStr::from_ptr(p) }.to_string_lossy())
            }
        }
        None => format!("error {err}"),
    }
}

fn open(dir: c_int, device: Option<&str>, stream: &str, attr: BufferAttr) -> Result<(&'static Api, Simple), String> {
    let api = api()?;
    let spec = SampleSpec { format: PA_SAMPLE_S16LE, rate: SAMPLE_RATE, channels: 1 };
    let app = CString::new("Zalo").unwrap();
    let stream = CString::new(stream).unwrap();
    let dev = device.filter(|d| !d.is_empty()).and_then(|d| CString::new(d).ok());
    let mut err = 0;
    let s = (api.new)(
        std::ptr::null(),
        app.as_ptr(),
        dir,
        dev.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()),
        stream.as_ptr(),
        &spec,
        std::ptr::null(),
        &attr,
        &mut err,
    );
    if s.is_null() {
        return Err(format!("pa_simple_new failed: {}", describe(api, err)));
    }
    Ok((api, s))
}

/// A microphone stream (what parec --latency-msec=20 asked for: 20 ms fragments).
pub struct PulseIn {
    api: &'static Api,
    s: Simple,
}

unsafe impl Send for PulseIn {}

impl PulseIn {
    pub fn open(device: Option<&str>) -> Result<Self, String> {
        let attr = BufferAttr { maxlength: NONE, tlength: NONE, prebuf: NONE, minreq: NONE, fragsize: FRAME_BYTES as u32 };
        let (api, s) = open(PA_STREAM_RECORD, device, "Microphone", attr)?;
        Ok(Self { api, s })
    }

    pub fn read(&mut self, buf: &mut [u8]) -> Result<(), String> {
        let mut err = 0;
        if (self.api.read)(self.s, buf.as_mut_ptr() as *mut c_void, buf.len(), &mut err) < 0 {
            return Err(describe(self.api, err));
        }
        Ok(())
    }
}

impl Drop for PulseIn {
    fn drop(&mut self) {
        (self.api.free)(self.s);
    }
}

/// Playback through libpulse-simple. write() blocks until the stream has
/// room, so the playout loop runs on the sound card's clock, as the real
/// engine's render thread pulls 10 ms at a time from NetEq. A pipe into
/// pacat could not do that: it holds two seconds, so the time had to be
/// guessed with our own clock, which drifts away from the card's.
pub struct PulseOut {
    api: &'static Api,
    s: Simple,
}

unsafe impl Send for PulseOut {}

impl PulseOut {
    pub const LATENCY_MS: u32 = 60;

    pub fn open(device: Option<&str>) -> Result<Self, String> {
        let attr = BufferAttr { maxlength: NONE, tlength: FRAME_BYTES as u32 * Self::LATENCY_MS / 20, prebuf: NONE, minreq: NONE, fragsize: NONE };
        let (api, s) = open(PA_STREAM_PLAYBACK, device, "Call", attr)?;
        log!("player: libpulse-simple {}, {} ms", device.unwrap_or("default"), Self::LATENCY_MS);
        Ok(Self { api, s })
    }

    pub fn write(&mut self, pcm: &[u8]) -> Result<(), String> {
        let mut err = 0;
        if (self.api.write)(self.s, pcm.as_ptr() as *const c_void, pcm.len(), &mut err) < 0 {
            return Err(describe(self.api, err));
        }
        Ok(())
    }

    /// Audio queued in front of the sound card now (ms).
    pub fn latency_ms(&self) -> Option<f64> {
        let mut err = 0;
        let us = (self.api.get_latency)(self.s, &mut err);
        (err == 0).then(|| us as f64 / 1000.0)
    }
}

impl Drop for PulseOut {
    fn drop(&mut self) {
        (self.api.free)(self.s);
    }
}

// --- pactl ------------------------------------------------------------------

/// stdout of a short command, None if it failed or took over 3 s.
pub fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let mut child = Command::new(cmd)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let out = child.wait_with_output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn which(cmd: &str) -> bool {
    std::env::var_os("PATH").map_or(false, |p| {
        std::env::split_paths(&p).any(|d| {
            let f = d.join(cmd);
            std::fs::metadata(&f).map(|m| m.is_file() && std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o111 != 0).unwrap_or(false)
        })
    })
}

/// A PulseAudio server (or pipewire-pulse) answers.
pub fn running() -> bool {
    if run("pactl", &["info"]).is_some() {
        return true;
    }
    // No pactl installed: its socket is there.
    if std::env::var_os("PULSE_SERVER").is_some() {
        return true;
    }
    std::env::var_os("XDG_RUNTIME_DIR").map_or(false, |d| std::path::Path::new(&d).join("pulse/native").exists())
}

/// Names of the PulseAudio "sources" or "sinks" (empty if pactl fails).
pub fn names(kind: &str) -> Vec<String> {
    run("pactl", &["list", "short", kind])
        .map(|out| out.lines().filter_map(|l| l.split('\t').nth(1).map(str::to_string)).collect())
        .unwrap_or_default()
}

/// `name` if that device exists now (a headset may have been unplugged), else None.
pub fn present(name: Option<&str>, kind: &str) -> Option<String> {
    let name = name.filter(|n| !n.is_empty())?;
    if names(kind).iter().any(|n| n == name) {
        return Some(name.to_string());
    }
    log!("{name} not found; using the default");
    None
}

pub fn default_source() -> Option<String> {
    run("pactl", &["get-default-source"]).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// A real microphone when the default source is silent by design.
///
/// Installing Iriun Webcam loads snd-aloop, and PipeWire may then pick its
/// loopback (or a monitor) as the default source: the call goes out silent.
/// ZCALL_MIC names a source explicitly.
pub fn real_mic() -> Option<String> {
    if let Ok(m) = std::env::var("ZCALL_MIC") {
        if !m.is_empty() {
            return Some(m);
        }
    }
    let default = run("pactl", &["get-default-source"])?.trim().to_string();
    if !default.is_empty() && !default.contains("aloop") && !default.ends_with(".monitor") {
        return None;
    }
    let found = names("sources").into_iter().find(|n| n.starts_with("alsa_input.") && !n.contains("aloop"));
    if let Some(n) = &found {
        log!("default source {default} is a loopback; using {n}");
    }
    found
}

/// "Volume: mono: 65536 / 100% / 0.00 dB" (the first channel).
fn source_volume(source: &str) -> Option<String> {
    run("pactl", &["get-source-volume", source])
}

/// The source volume as pactl and GNOME's slider show it (raw / 65536).
pub fn volume_percent(source: &str) -> Option<u32> {
    parse_percent(&source_volume(source)?)
}

pub fn volume_db(source: &str) -> Option<f64> {
    parse_db(&source_volume(source)?)
}

fn parse_percent(out: &str) -> Option<u32> {
    let head = out.split('%').next()?;
    head.rsplit('/').next()?.trim().parse().ok()
}

fn parse_db(out: &str) -> Option<f64> {
    let head = out.split(" dB").next()?;
    let v = head.rsplit('/').next()?.trim();
    if v == "-inf" {
        return Some(f64::NEG_INFINITY);
    }
    v.parse().ok()
}

/// Set the raw volume (65536 = 100 %, 0 dB).
pub fn set_volume_raw(source: &str, raw: u32) -> bool {
    run("pactl", &["set-source-volume", source, &raw.to_string()]).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pactl_volume_is_parsed() {
        let mono = "Volume: mono: 47186 /  72% / -8.56 dB\n";
        assert_eq!(parse_percent(mono), Some(72));
        assert_eq!(parse_db(mono), Some(-8.56));
        let stereo = "Volume: front-left: 65536 / 100% / 0.00 dB,   front-right: 65536 / 100% / 0.00 dB\n        balance 0.00\n";
        assert_eq!(parse_percent(stereo), Some(100));
        assert_eq!(parse_db(stereo), Some(0.0));
    }
}
