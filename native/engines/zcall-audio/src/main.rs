//! zcall-audio: the audio side of the native call engine: microphone -> Opus,
//! Opus -> speaker. Spawned by backends/zrtc-media.js; network, SRTP and
//! signalling stay in Node. Opus 16 kHz mono, 20 ms frames (opus/16000/1,
//! PT 112). The pipe framing is in protocol.rs. Stops when stdin closes.
//!
//!   --tone              send a 440 Hz tone instead of the microphone (for tests)
//!   --mix               group call (several sources); every call has one jitter
//!                       buffer per source (src = SSRC), decoded and mixed every
//!                       20 ms on the sound card's clock
//!   --mic / --speaker   PulseAudio source / sink to use (else ZCALL_MIC /
//!                       ZCALL_SPEAKER, else the default)
//!   --min-delay MS      least jitter buffer depth (minAudioDelayMs; 100 by default)
//!   --max-delay MS      most jitter buffer depth (the smaller of maxAudioDelayMs
//!                       and audioJitterMax; --jitter frames when neither is set)
//!   --bitrate BPS / --complexity N / --loss PCT / --no-fec   Opus encoder
//!   --no-aec / --no-ns / --no-high-pass / --vad              zrtcConfig processing
//!   --device ALSA       ALSA device (default: PulseAudio if running)
//!   --out FILE          write received audio to this file (.wav or raw) instead
//!   --no-mic            receive only
//!   --check             print what calls still need ("opus", "audio"), then exit
//!
//! With PipeWire the microphone goes through WebRTC echo cancellation, as the
//! real engine does (zrtc_config audioEchoCancellation), and received audio is
//! played through it as the echo reference (processing.rs). Noise suppression
//! is OFF unless ZCALL_NOISE_SUPPRESSION=1, whatever audioNoiseSuppression
//! says: on real recordings (USB headset, speech ~18 dB over the noise) it let
//! the first 1-2 s through, then cut the voice by 26-32 dB: the phone heard a
//! word and then nothing. MicLeveler then lifts the speech that is left to a
//! call level. Playback is primed before the microphone starts: Zalo's
//! AudioDevice starts playout before recording whenever AEC is on, so the
//! canceller already has a reference. ZCALL_AUDIO_PROCESSING=0 turns
//! processing off.
//!
//! PipeWire 1.4's libspa-aec-webrtc (the AEC3 build) has no switch for echo
//! level, AECM routing or comfort noise, so zrtcConfig's audioEchoLevel /
//! audioEchoRouting / aecForMobile / audioEchoComfortNoise are not applied.

mod diag;
mod dl;
mod dsp;
mod dump;
mod guard;
mod io;
mod leveler;
mod opus_codec;
mod playout;
mod processing;
mod protocol;
mod pulse;

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dump::DUMP;
use guard::ClipGuard;
use io::{Mic, Sink};
use leveler::MicLeveler;
use opus_codec::{EncoderConfig, OpusEncoder};
use playout::Playout;
use processing::Processing;
use protocol::*;

#[derive(Clone)]
struct Args {
    bitrate: i32,
    complexity: i32,
    loss: i32,
    no_fec: bool,
    no_high_pass: bool,
    no_ns: bool,
    no_aec: bool,
    vad: bool,
    jitter: usize,
    min_delay: u32,
    max_delay: u32,
    device: Option<String>,
    out: Option<PathBuf>,
    tone: bool,
    mix: bool,
    no_mic: bool,
    mic: Option<String>,
    speaker: Option<String>,
    check: bool,
}

fn env_nonempty(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

fn parse_args() -> Args {
    let mut a = Args {
        // 1-1 default. A group call passes zrtcConfig.audioBitrate (kbps * 1000) instead.
        bitrate: 32000,
        complexity: 10,
        loss: 5,
        no_fec: false,
        no_high_pass: false,
        no_ns: false,
        no_aec: false,
        vad: false,
        jitter: 10,
        min_delay: 100,
        max_delay: 0,
        device: None,
        out: None,
        tone: false,
        mix: false,
        no_mic: false,
        mic: env_nonempty("ZCALL_MIC"),
        speaker: env_nonempty("ZCALL_SPEAKER"),
        check: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        // --key=value as well as --key value.
        let (key, inline) = match arg.split_once('=') {
            Some((k, v)) if k.starts_with("--") => (k.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut val = || inline.clone().or_else(|| it.next());
        let num = |v: Option<String>, what: &str| -> Option<i64> {
            let n = v.as_deref().and_then(|s| s.trim().parse::<i64>().ok());
            if n.is_none() {
                log!("ignoring {what} {}", v.unwrap_or_default());
            }
            n
        };
        match key.as_str() {
            "--bitrate" => a.bitrate = num(val(), "--bitrate").map_or(a.bitrate, |n| n as i32),
            "--complexity" => a.complexity = num(val(), "--complexity").map_or(a.complexity, |n| n as i32),
            "--loss" => a.loss = num(val(), "--loss").map_or(a.loss, |n| n as i32),
            "--jitter" => a.jitter = num(val(), "--jitter").map_or(a.jitter, |n| n.max(1) as usize),
            "--min-delay" => a.min_delay = num(val(), "--min-delay").map_or(a.min_delay, |n| n.max(0) as u32),
            "--max-delay" => a.max_delay = num(val(), "--max-delay").map_or(a.max_delay, |n| n.max(0) as u32),
            "--no-fec" => a.no_fec = true,
            "--no-high-pass" => a.no_high_pass = true,
            "--no-ns" => a.no_ns = true,
            "--no-aec" => a.no_aec = true,
            "--vad" => a.vad = true,
            "--device" => a.device = val(),
            "--out" => a.out = val().map(PathBuf::from),
            "--tone" => a.tone = true,
            "--mix" => a.mix = true,
            "--no-mic" => a.no_mic = true,
            "--mic" => a.mic = val().filter(|v| !v.is_empty()),
            "--speaker" => a.speaker = val().filter(|v| !v.is_empty()),
            "--check" => a.check = true,
            "--help" | "-h" => {
                println!("usage: zcall-audio [--tone] [--mix] [--mic SOURCE] [--speaker SINK] [--min-delay MS] [--max-delay MS] [--bitrate BPS] [--complexity N] [--no-fec] [--no-aec] [--no-ns] [--no-high-pass] [--vad] [--device ALSA] [--out FILE] [--no-mic] [--check]");
                std::process::exit(0);
            }
            other => log!("unknown argument {other}"),
        }
    }
    a
}

/// What calls still need on this machine, as the plugin's install hint keys:
/// "opus" (libopus) and "audio" (libpulse-simple, or the ALSA tools).
fn check() {
    let mut need = Vec::new();
    if !opus_codec::available() {
        need.push("opus");
    }
    if !pulse::simple_available() && !(pulse::which("parec") && pulse::which("pacat")) && !(pulse::which("arecord") && pulse::which("aplay")) {
        need.push("audio");
    }
    println!("{}", need.join(" "));
}

fn read_exact(inp: &mut impl Read, n: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; n];
    inp.read_exact(&mut buf).ok().map(|_| buf)
}

/// Mic frames (or the test tone) -> leveler -> Opus -> stdout.
fn encoder_loop(args: &Args, muted: Arc<AtomicBool>, mic: Option<Arc<Mic>>, guard: Option<Arc<ClipGuard>>, playout: Arc<Playout>) {
    let fec = !args.no_fec;
    let mut enc = match OpusEncoder::new(&EncoderConfig { bitrate: args.bitrate, complexity: args.complexity, fec, loss: args.loss }) {
        Ok(e) => e,
        Err(e) => {
            log!("{e}");
            fault("mic", "no Opus encoder");
            return;
        }
    };
    log!("encode {} bps complexity {} fec {} high-pass {}", args.bitrate, args.complexity, fec as u8, !args.no_high_pass);
    let state = env_nonempty("ZCALL_LOG_DIR").map(|d| PathBuf::from(d).join("mic-leveler.json"));
    let mut leveler = (!args.tone && std::env::var("ZCALL_MIC_GAIN").as_deref() != Ok("0")).then(|| {
        let p = Arc::clone(&playout);
        MicLeveler::new(Box::new(move || p.far_active()), state)
    });
    let mut out = std::io::stdout().lock();
    let mut opus = vec![0u8; 1500];
    let mut pcm = vec![0i16; FRAME_SAMPLES];
    let silence = vec![0i16; FRAME_SAMPLES];
    let mut sent = 0u64;

    let mut each = |raw: &[u8]| -> bool {
        from_bytes(raw, &mut pcm);
        let is_muted = muted.load(Ordering::Relaxed);
        // The clip guard sees the microphone as it comes (its own clipping);
        // the leveler's gain goes to the encoder and the dump.
        if let Some(g) = &guard {
            if !is_muted {
                g.feed(&pcm);
            }
        }
        if let Some(l) = leveler.as_mut() {
            l.process(&mut pcm);
        }
        let frame = if is_muted { &silence } else { &pcm };
        DUMP.write_mic(frame);
        let Some(size) = enc.encode(frame, &mut opus) else { return true };
        let level = dsp::audio_level(frame);
        let hdr = [CMD_OUT_LEVEL, level, (size >> 8) as u8, size as u8];
        if out.write_all(&hdr).and_then(|_| out.write_all(&opus[..size])).and_then(|_| out.flush()).is_err() {
            return false;
        }
        sent += 1;
        if sent == 1 {
            log!("first microphone frame encoded");
        }
        true
    };

    match mic {
        Some(m) if !args.tone => m.frames(each),
        _ => {
            // 440 Hz tone on our own 20 ms clock.
            let start = Instant::now();
            let mut phase = 0f64;
            let mut n = 0u32;
            let mut raw = vec![0u8; FRAME_BYTES];
            loop {
                for i in 0..FRAME_SAMPLES {
                    let v = (8000.0 * phase.sin()) as i16;
                    raw[2 * i..2 * i + 2].copy_from_slice(&v.to_le_bytes());
                    phase += 2.0 * std::f64::consts::PI * 440.0 / 16000.0;
                }
                n += 1;
                let due = start + Duration::from_millis(20) * n;
                let now = Instant::now();
                if due > now {
                    std::thread::sleep(due - now);
                }
                if !each(&raw) {
                    break;
                }
            }
        }
    }
}

fn main() {
    let args = parse_args();
    if args.check {
        check();
        return;
    }
    // The pipe to Node may close under us: report it as an error, not a signal.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };

    if !opus_codec::available() {
        log!("libopus not found (install opus / libopus0)");
    }
    let pulse = args.out.is_none() && args.device.is_none() && pulse::running();
    // The devices picked (None: the default); with processing, what it is attached to.
    let chosen = Arc::new(Mutex::new((
        if pulse { pulse::present(args.mic.as_deref(), "sources") } else { None },
        if pulse { pulse::present(args.speaker.as_deref(), "sinks") } else { None },
    )));
    // --no-aec skips the module entirely: PipeWire 1.4's plugin cannot turn the
    // canceller off while keeping the noise suppressor.
    let want_proc = pulse && std::env::var("ZCALL_AUDIO_PROCESSING").as_deref() != Ok("0") && !args.no_aec && pulse::which("pipewire");
    // Noise suppression only on request (see the top of this file).
    let ns_on = !args.no_ns && std::env::var("ZCALL_NOISE_SUPPRESSION").as_deref() == Ok("1");
    let mut proc = want_proc.then(|| Processing::new(!args.no_high_pass, ns_on, args.vad));
    let max_shown = if args.max_delay > 0 { args.max_delay } else { args.jitter as u32 * 20 };
    log!(
        "audio config: aec {} ns {} high-pass {} vad {} delay {}..{} ms{}",
        !args.no_aec,
        ns_on,
        !args.no_high_pass,
        args.vad,
        args.min_delay,
        max_shown,
        if args.mix { " (group)" } else { "" }
    );

    let start_processing = |proc: &mut Option<Processing>, chosen: &(Option<String>, Option<String>)| -> bool {
        match proc {
            Some(p) => {
                let mic = chosen.0.clone().or_else(pulse::real_mic);
                p.start(mic.as_deref(), chosen.1.as_deref())
            }
            None => false,
        }
    };
    let mut processed = start_processing(&mut proc, &chosen.lock().unwrap());
    if pulse {
        std::thread::spawn(processing::pipewire_realtime);
    }
    let speaker_name = if processed { proc.as_ref().map(|p| p.sink.clone()) } else { chosen.lock().unwrap().1.clone() };
    let sink = Arc::new(Sink::open(args.out.as_deref(), pulse, speaker_name.as_deref(), args.device.as_deref()));
    // Speaker reference before the microphone, then the playout clock.
    let playout = Playout::new(Arc::clone(&sink), args.jitter, args.min_delay, args.max_delay);
    if processed && sink.paced {
        playout.prime(5);
    }
    playout.start();

    let muted = Arc::new(AtomicBool::new(false));
    let mut mic: Option<Arc<Mic>> = None;
    let mut guard: Option<Arc<ClipGuard>> = None;
    if !args.no_mic {
        if !args.tone {
            let src = if processed { proc.as_ref().map(|p| p.source.clone()) } else { chosen.lock().unwrap().0.clone() };
            mic = Some(Arc::new(Mic::new(args.device.clone(), pulse, src)));
            if pulse {
                let g_src = chosen.lock().unwrap().0.clone().or_else(pulse::real_mic).or_else(pulse::default_source);
                guard = Some(ClipGuard::new(g_src));
            }
        }
        let (m, g, p, mu) = (mic.clone(), guard.clone(), Arc::clone(&playout), Arc::clone(&muted));
        let a = args.clone();
        std::thread::Builder::new().name("encoder".into()).spawn(move || encoder_loop(&a, mu, m, g, p)).unwrap();
    }
    if pulse && std::env::var("ZCALL_AUDIO_DIAG").as_deref() != Ok("0") {
        let tag = proc.as_ref().map_or("zcall_ec_".to_string(), |p| p.tag.clone());
        diag::start(Arc::clone(&playout), mic.clone(), guard.clone(), tag);
    }

    let mut inp = std::io::stdin().lock();
    loop {
        let mut kind = [0u8; 1];
        if inp.read_exact(&mut kind).is_err() {
            break;
        }
        match kind[0] {
            CMD_IN_FRAME => {
                let Some(hdr) = read_exact(&mut inp, 10) else { break };
                let idx = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
                let src = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
                let n = u16::from_be_bytes([hdr[8], hdr[9]]) as usize;
                let Some(payload) = read_exact(&mut inp, n) else { break };
                playout.push(idx, payload, src);
            }
            CMD_IN_SPEAKER => {
                let Some(on) = read_exact(&mut inp, 1) else { break };
                sink.silent.store(on[0] != 0, Ordering::Relaxed);
                log!("{}", if on[0] != 0 { "speaker off" } else { "speaker on" });
            }
            CMD_IN_VOLUME => {
                let Some(pct) = read_exact(&mut inp, 1) else { break };
                match &guard {
                    Some(g) => {
                        let g = Arc::clone(g);
                        std::thread::spawn(move || g.set_percent(pct[0]));
                    }
                    None => log!("mic volume: no microphone level to set"),
                }
            }
            CMD_IN_MUTE => {
                let Some(on) = read_exact(&mut inp, 1) else { break };
                muted.store(on[0] != 0, Ordering::Relaxed);
                log!("{}", if on[0] != 0 { "mute" } else { "unmute" });
            }
            CMD_IN_DEVICE => {
                let Some(hdr) = read_exact(&mut inp, 3) else { break };
                let which = hdr[0];
                let n = u16::from_be_bytes([hdr[1], hdr[2]]) as usize;
                let Some(name) = read_exact(&mut inp, n) else { break };
                let name = String::from_utf8_lossy(&name).into_owned();
                let name = (!name.is_empty()).then_some(name);
                let label = if which == b'i' { "microphone" } else { "speaker" };
                log!("device change: {label} {}", name.as_deref().unwrap_or("default"));
                if !pulse || (which != b'i' && which != b'o') {
                    continue;
                }
                if let Some(m) = &mic {
                    m.switching.store(true, Ordering::SeqCst);
                }
                {
                    let mut ch = chosen.lock().unwrap();
                    if which == b'i' {
                        ch.0 = pulse::present(name.as_deref(), "sources");
                        if let Some(g) = &guard {
                            g.set_source(ch.0.clone().or_else(pulse::real_mic).or_else(pulse::default_source));
                        }
                    } else {
                        ch.1 = pulse::present(name.as_deref(), "sinks");
                    }
                }
                let ch = chosen.lock().unwrap().clone();
                if processed {
                    // Re-attach the processing; its devices come back under the same
                    // names. Speaker first, then the microphone, as at the start.
                    processed = start_processing(&mut proc, &ch);
                    let p = proc.as_ref().unwrap();
                    sink.switch(if processed { Some(p.sink.as_str()) } else { ch.1.as_deref() });
                    if processed && sink.paced {
                        playout.prime(5);
                    }
                    if let Some(m) = &mic {
                        m.switch(if processed { Some(p.source.clone()) } else { ch.0.clone() });
                    }
                } else if which == b'i' {
                    if let Some(m) = &mic {
                        m.switch(ch.0.clone());
                    }
                } else {
                    sink.switch(ch.1.as_deref());
                }
                if let Some(m) = &mic {
                    m.switching.store(false, Ordering::SeqCst);
                }
            }
            other => {
                log!("bad frame kind {other:#04x}");
                break;
            }
        }
    }

    // The capture first: it reads from the processing's source.
    if let Some(m) = &mic {
        m.close();
    }
    playout.flush();
    sink.close();
    if let Some(mut p) = proc.take() {
        p.stop();
    }
    log!("stats {}", playout.stats_line());
    // Threads blocked in a device read must not hold the exit.
    std::process::exit(0);
}
