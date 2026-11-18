# zcall-audio

**English** | [Tiếng Việt](./README.vi.md)

Audio engine of the native Zalo call engine on Linux: microphone -> Opus -> network, network -> Opus -> speaker. It replaces the former Python `audio-io.py` (with `parec` / `pacat`).

---

## 1. Root Cause Analysis

In previous versions of `zalo-for-linux`, voice and video calls suffered from noticeable audio latency (~150ms), occasional audio crackling under CPU load, and strict dependency requirements.

### The Previous Architecture:
```
[Node.js zrtc-media]
       │  (IPC pipe stdin/stdout)
       ▼
 [Python audio-io.py]
   ├── pipe ──> parec (PulseAudio recording process)
   └── pipe ──> pacat (PulseAudio playback process)
```

### Architectural Bottlenecks:
1. **Multi-Process Context Switching**: Handling a 20ms audio frame (50 times/second) across 4 separate OS processes (`node`, `python3`, `parec`, `pacat`) through 6 Unix pipes incurred continuous scheduling overhead and context switches.
2. **Buffer Underruns**: When the system was busy, process scheduling jitter delayed pipe reads/writes, causing buffer underruns, latency spikes, and audio clipping.
3. **Rigid External Dependencies**: Users were forced to install `python3` and `pulseaudio-utils` (`parec`/`pacat`), causing calls to fail outright on minimal or immutable Linux distributions.

---

## 2. Technical Findings & Specifications

Through analysis of Zalo's call protocol and audio traffic, the exact specifications and framing interfaces were identified:

### Audio Stream Parameters
- **Sample Rate**: 16,000 Hz (16 kHz wideband audio).
- **Channels**: 1 (Mono).
- **Format**: Signed 16-bit Little Endian PCM (`s16le`).
- **Frame Duration**: 20 ms per frame (320 samples = 640 bytes raw PCM).
- **Audio Codec**: Opus (`OPUS_APPLICATION_VOIP`), in-band FEC, dynamic bitrate between 20 kbps and 32 kbps.

### IPC Framing Protocol (Binary Stdin / Stdout)

Communication between `zrtc-media.js` and the audio engine uses a binary protocol over standard input/output:

#### Outbound Frames (Engine -> Node stdout)
- **Tag `'L'` (0x4c)**: Microphone capture frame.
  - Byte 0: `0x4c` (`'L'`)
  - Byte 1: Audio loudness level (RFC 6464, `-dBov` scaled 0..127, where 127 is complete silence and 0 is 0 dBFS peak).
  - Bytes 2-3: Payload length `N` (big-endian 16-bit integer).
  - Bytes 4..(4+N): Compressed Opus audio payload.

#### Inbound Frames (Node -> Engine stdin)
- **Tag `'F'` (0x46)**: Inbound peer audio packet.
  - Byte 0: `0x46` (`'F'`)
  - Bytes 1-4: Packet sequence index (big-endian 32-bit integer).
  - Bytes 5-8: Synchronization source identifier (`ssrc`, big-endian 32-bit integer).
  - Bytes 9-10: Payload length `N` (big-endian 16-bit integer).
  - Bytes 11..(11+N): Compressed Opus audio payload.

#### Control Commands (Node -> Engine stdin)
- **`'M' (0x4d)` - Microphone Mute**: `[0x4d, <0|1>]` (`1` = muted/silence, `0` = unmuted).
- **`'S' (0x53)` - Speaker Mute**: `[0x53, <0|1>]` (`1` = playback off, `0` = playback on).
- **`'V' (0x56)` - Software Gain Volume**: `[0x56, <percent 0..100>]`.
- **`'D' (0x44)` - Audio Device Switch**: `[0x44, <'i'|'o'>, <len_hi>, <len_lo>, <device_name_utf8>]`.

---

## 3. Implementation Mechanics

`zcall-audio` is a single Rust binary and the only audio engine: the Python
`audio-io.py` it replaces has been removed. It is a module-by-module port of
that script, checked bit-exact against it on the same inputs (jitter buffer
decisions and output PCM, MicLeveler gain).

```
[Node.js zrtc-media]
       │  (stdin/stdout IPC pipe)
       ▼
[zcall-audio]
   ├── libpulse-simple (dlopen) ──> PipeWire / PulseAudio, or arecord/aplay without one
   ├── libopus (dlopen)         ──> encode / decode, FEC, PLC
   └── pipewire -c (private)    ──> WebRTC echo cancellation (module-echo-cancel)
```

libopus and libpulse-simple are loaded at runtime (`dl.rs`), so the binary has
no link-time dependency on them and `zcall-audio --check` can name what is
missing (the plugin turns that into an install hint).

| Module | What it does |
| --- | --- |
| `io.rs` | Microphone (libpulse-simple, parec or arecord) and speaker (libpulse-simple, pacat, aplay or `--out` file), switchable mid-call |
| `playout.rs` | One small NetEq per source (SSRC): adaptive depth from measured jitter (`--min-delay` / `--max-delay`), PLC while waiting, FEC for lost packets, pitch-period Accelerate (WSOLA), reset on bursts; sources mixed with a limiter on the sound card's clock |
| `processing.rs` | Private `pipewire` process running module-echo-cancel (AEC; noise suppression only with `ZCALL_NOISE_SUPPRESSION=1`), and RTKit realtime for PipeWire's audio threads |
| `leveler.rs` | MicLeveler: slow speech-only make-up gain, held while the far end talks, remembered across calls |
| `guard.rs` | ClipGuard: steps the PulseAudio source volume down when speech clips; the call window's volume slider |
| `diag.rs`, `dump.rs` | `diag:` line every 10 s (`ZCALL_AUDIO_DIAG=0` off); raw PCM dump with `ZCALL_AUDIO_DUMP=1` |
| `dsp.rs` | RMS, RFC 6464 level, pitch lag / WSOLA |

The environment switches of `audio-io.py` keep their meaning: `ZCALL_MIC`,
`ZCALL_SPEAKER`, `ZCALL_AUDIO_PROCESSING`, `ZCALL_NOISE_SUPPRESSION`,
`ZCALL_MIC_GAIN`, `ZCALL_MIC_AGC`, `ZCALL_PW_RT`, `ZCALL_AUDIO_DIAG`,
`ZCALL_AUDIO_DUMP`, `ZCALL_LOG_DIR`.

## 4. Building & Testing

### Building
```bash
cargo build --release -p zcall-audio
```

Binary is output to `target/release/zcall-audio` and deployed to `zcall-native/native-engine/zcall-audio` during `npm run main`.

### Testing
```bash
# Run unit tests (RFC 6464 levels, jitter reorder, PLC)
cargo test -p zcall-audio

# Run full integration test suite
npm test
```

---

## 5. Credits & Attribution

- **Implementation**: Clean-room reverse engineering and native Linux implementation by [@ngxccc](https://github.com/ngxccc).
- **Integration**: Maintained by [Việt Nam Linux Family](https://github.com/VN-Linux-Family) under the BSD-3-Clause license.
