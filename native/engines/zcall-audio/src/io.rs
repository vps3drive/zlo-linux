//! The microphone and the speaker: libpulse-simple when a sound server runs
//! (parec / pacat if libpulse-simple is missing), else the ALSA tools
//! (arecord / aplay), or a file for --out.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::log;
use crate::protocol::{fault, FRAME_BYTES};
use crate::pulse::{self, PulseIn, PulseOut};

/// ALSA's default device, whatever card the machine has (--device picks another).
pub const DEFAULT_ALSA: &str = "default";

fn spawn_reader(cmd: &[String]) -> Result<(Child, ChildStdout), String> {
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", cmd[0]))?;
    let out = child.stdout.take().unwrap();
    Ok((child, out))
}

fn spawn_writer(cmd: &[String]) -> Result<(Child, ChildStdin), String> {
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", cmd[0]))?;
    let stdin = child.stdin.take().unwrap();
    Ok((child, stdin))
}

fn end(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

// --- microphone ---------------------------------------------------------------

enum Capture {
    Pulse(PulseIn),
    Cmd(Child, ChildStdout),
}

impl Capture {
    fn read(&mut self, buf: &mut [u8]) -> Result<(), String> {
        match self {
            Capture::Pulse(p) => p.read(buf),
            Capture::Cmd(_, out) => out.read_exact(buf).map_err(|e| e.to_string()),
        }
    }

    fn close(self) {
        if let Capture::Cmd(child, _) = self {
            end(child);
        }
    }
}

/// The microphone; switch() replaces its stream mid-call. The encoder thread
/// reads frames(), the stdin thread calls switch().
pub struct Mic {
    alsa_device: Option<String>,
    pulse: bool,
    pending: Mutex<Option<Option<String>>>,
    /// A device change is under way: a stream that ends now is being replaced.
    pub switching: AtomicBool,
    closed: AtomicBool,
    gap_max_us: AtomicU64,
    gaps: AtomicU32,
}

impl Mic {
    pub fn new(alsa_device: Option<String>, pulse: bool, source: Option<String>) -> Self {
        Mic {
            alsa_device,
            pulse,
            pending: Mutex::new(Some(source)),
            switching: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            gap_max_us: AtomicU64::new(0),
            gaps: AtomicU32::new(0),
        }
    }

    pub fn switch(&self, source: Option<String>) {
        *self.pending.lock().unwrap() = Some(source);
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    /// (longest wait for one 20 ms frame, waits over 60 ms) since the last call.
    pub fn take_gaps(&self) -> (Duration, u32) {
        (Duration::from_micros(self.gap_max_us.swap(0, Ordering::Relaxed)), self.gaps.swap(0, Ordering::Relaxed))
    }

    fn open(&self, source: Option<&str>) -> Result<Capture, String> {
        if self.alsa_device.is_none() && self.pulse {
            let src = pulse::present(source, "sources").or_else(pulse::real_mic);
            if pulse::simple_available() {
                let c = PulseIn::open(src.as_deref())?;
                log!("mic: libpulse-simple {}", src.as_deref().unwrap_or("default"));
                return Ok(Capture::Pulse(c));
            }
            let mut cmd = vec!["parec".to_string()];
            if let Some(s) = src {
                cmd.extend(["-d".to_string(), s]);
            }
            cmd.extend(["--raw", "--rate=16000", "--channels=1", "--format=s16le", "--latency-msec=20"].map(String::from));
            log!("mic: {}", cmd.join(" "));
            let (c, o) = spawn_reader(&cmd)?;
            return Ok(Capture::Cmd(c, o));
        }
        let dev = self.alsa_device.clone().unwrap_or_else(|| DEFAULT_ALSA.into());
        let cmd: Vec<String> = ["arecord", "-q", "-D", &dev, "-t", "raw", "-f", "S16_LE", "-r", "16000", "-c", "1"].map(String::from).to_vec();
        log!("mic: {}", cmd.join(" "));
        let (c, o) = spawn_reader(&cmd)?;
        Ok(Capture::Cmd(c, o))
    }

    /// Calls `each` with every 20 ms frame (s16le bytes) until the stream
    /// ends for good, close(), or `each` returns false.
    pub fn frames(&self, mut each: impl FnMut(&[u8]) -> bool) {
        let mut cap: Option<Capture> = None;
        let mut buf = vec![0u8; FRAME_BYTES];
        let mut last: Option<Instant> = None;
        loop {
            if self.closed.load(Ordering::SeqCst) {
                break;
            }
            if let Some(src) = self.pending.lock().unwrap().take() {
                if let Some(old) = cap.take() {
                    old.close();
                }
                match self.open(src.as_deref()) {
                    Ok(c) => cap = Some(c),
                    Err(e) => log!("mic: {e}"),
                }
            }
            let result = match cap.as_mut() {
                Some(c) => c.read(&mut buf),
                None => Err("no microphone stream".into()),
            };
            let now = Instant::now();
            if let Err(e) = result {
                if self.closed.load(Ordering::SeqCst) {
                    break;
                }
                // The processing restarts for another device before the
                // microphone is moved to it: wait for that switch.
                let deadline = now + Duration::from_secs(5);
                let mut replaced = false;
                while Instant::now() < deadline {
                    if self.pending.lock().unwrap().is_some() {
                        replaced = true;
                        break;
                    }
                    if !self.switching.load(Ordering::SeqCst) && Instant::now() > now + Duration::from_millis(200) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                if replaced {
                    if let Some(old) = cap.take() {
                        old.close();
                    }
                    last = None;
                    continue;
                }
                log!("microphone stream ended ({e})");
                fault("mic", "capture ended");
                break;
            }
            if let Some(l) = last {
                let gap = now - l;
                self.gap_max_us.fetch_max(gap.as_micros() as u64, Ordering::Relaxed);
                if gap > Duration::from_millis(60) {
                    self.gaps.fetch_add(1, Ordering::Relaxed);
                }
            }
            last = Some(now);
            if !each(&buf) {
                break;
            }
        }
        // End the capture before we exit. Left running, parec wrote into the
        // closed pipe and printed "write() failed: Broken pipe" at every hangup.
        if let Some(c) = cap {
            c.close();
        }
    }
}

// --- speaker ------------------------------------------------------------------

enum Output {
    Pulse(PulseOut),
    Cmd(Child, ChildStdin),
    Raw(File),
    Wav(File, u32),
    Null,
}

fn pacat(sink: Option<&str>) -> Vec<String> {
    let mut cmd = vec!["pacat".to_string()];
    if let Some(s) = sink {
        cmd.extend(["-d".to_string(), s.to_string()]);
    }
    cmd.extend(["--raw", "--rate=16000", "--channels=1", "--format=s16le", "--latency-msec=60"].map(String::from));
    cmd
}

fn wav_header(data_len: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&1u16.to_le_bytes()); // mono
    h[24..28].copy_from_slice(&16000u32.to_le_bytes());
    h[28..32].copy_from_slice(&32000u32.to_le_bytes());
    h[32..34].copy_from_slice(&2u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_len.to_le_bytes());
    h
}

/// Where received audio goes. `silent` plays silence instead (call speaker
/// off) and keeps the timing.
pub struct Sink {
    out: Mutex<Output>,
    pub silent: AtomicBool,
    failed: AtomicBool,
    /// Writes block on the sound card's clock (libpulse-simple).
    pub paced: bool,
}

impl Sink {
    /// The call's speaker: libpulse-simple on `name` when a sound server runs,
    /// else pacat, else aplay; a file for --out.
    pub fn open(out_file: Option<&std::path::Path>, pulse: bool, name: Option<&str>, alsa_device: Option<&str>) -> Sink {
        let wrap = |o: Output| Sink { paced: matches!(o, Output::Pulse(_)), out: Mutex::new(o), silent: AtomicBool::new(false), failed: AtomicBool::new(false) };
        if let Some(path) = out_file {
            let wav = path.extension().map_or(false, |e| e == "wav");
            return match File::create(path) {
                Ok(mut f) if wav => {
                    let _ = f.write_all(&wav_header(0));
                    wrap(Output::Wav(f, 0))
                }
                Ok(f) => wrap(Output::Raw(f)),
                Err(e) => {
                    fault("speaker", format!("{}: {e}", path.display()));
                    wrap(Output::Null)
                }
            };
        }
        if pulse {
            match PulseOut::open(name) {
                Ok(p) => return wrap(Output::Pulse(p)),
                Err(e) => log!("libpulse-simple unavailable, using pacat: {e}"),
            }
        }
        let cmd: Vec<String> = if pulse {
            pacat(name)
        } else {
            let dev = alsa_device.unwrap_or(DEFAULT_ALSA);
            ["aplay", "-q", "-D", dev, "-t", "raw", "-f", "S16_LE", "-r", "16000", "-c", "1"].map(String::from).to_vec()
        };
        log!("player: {}", cmd.join(" "));
        match spawn_writer(&cmd) {
            Ok((c, i)) => wrap(Output::Cmd(c, i)),
            Err(e) => {
                fault("speaker", e);
                wrap(Output::Null)
            }
        }
    }

    /// Play through another PulseAudio sink from now on.
    pub fn switch(&self, name: Option<&str>) {
        let mut out = self.out.lock().unwrap();
        match &*out {
            Output::Pulse(_) => match PulseOut::open(name) {
                Ok(p) => *out = Output::Pulse(p),
                Err(e) => log!("player: {e}"),
            },
            Output::Cmd(..) => {
                let cmd = pacat(name);
                log!("player: {}", cmd.join(" "));
                match spawn_writer(&cmd) {
                    Ok((c, i)) => {
                        if let Output::Cmd(old, stdin) = std::mem::replace(&mut *out, Output::Cmd(c, i)) {
                            drop(stdin);
                            let mut old = old;
                            let _ = old.wait();
                        }
                    }
                    Err(e) => log!("player: {e}"),
                }
            }
            _ => {}
        }
    }

    pub fn write(&self, pcm: &[u8]) -> Result<(), String> {
        let zeros;
        let pcm = if self.silent.load(Ordering::Relaxed) {
            zeros = vec![0u8; pcm.len()];
            &zeros[..]
        } else {
            pcm
        };
        let mut out = self.out.lock().unwrap();
        match &mut *out {
            Output::Pulse(p) => match p.write(pcm) {
                Ok(()) => self.failed.store(false, Ordering::Relaxed),
                Err(e) => {
                    // The device went away (processing restarted for another
                    // device): keep time until switch() brings a new stream.
                    if !self.failed.swap(true, Ordering::Relaxed) {
                        log!("pa_simple_write failed: {e}");
                    }
                    drop(out);
                    std::thread::sleep(Duration::from_millis(20));
                }
            },
            Output::Cmd(_, stdin) => stdin.write_all(pcm).and_then(|_| stdin.flush()).map_err(|e| e.to_string())?,
            Output::Raw(f) => f.write_all(pcm).map_err(|e| e.to_string())?,
            Output::Wav(f, n) => {
                f.write_all(pcm).map_err(|e| e.to_string())?;
                *n += pcm.len() as u32;
            }
            Output::Null => {}
        }
        Ok(())
    }

    /// Audio queued in front of the sound card now (ms).
    pub fn latency_ms(&self) -> Option<f64> {
        match &*self.out.lock().unwrap() {
            Output::Pulse(p) => p.latency_ms(),
            _ => None,
        }
    }

    pub fn close(&self) {
        let mut out = self.out.lock().unwrap();
        match std::mem::replace(&mut *out, Output::Null) {
            Output::Cmd(mut c, stdin) => {
                drop(stdin);
                let _ = c.wait();
            }
            Output::Wav(mut f, n) => {
                let _ = f.seek(SeekFrom::Start(0)).and_then(|_| f.write_all(&wav_header(n)));
            }
            _ => {}
        }
    }
}
