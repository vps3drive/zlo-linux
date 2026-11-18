# Native Modules and Engines

This directory consolidates all Linux native implementations replacing proprietary macOS/Windows components of Zalo:

```text
native/
├── builder.js                # Unified CLI builder helper (Rust Cargo/NAPI)
├── addons/                   # N-API cdylib addons loaded dynamically via require()
│   ├── db-cross-v4/          # Backup decryption & E2EE message synchronization (Rust)
│   ├── file-utilities/       # Asynchronous filesystem scanning & pattern matching (Rust)
│   ├── file-utils/           # Filesystem access and file management utilities (Rust)
│   ├── mp4thumb/             # MP4 video thumbnail extractor (Rust)
│   ├── os-sys-info/          # Operating system metadata & PID inspector (Rust)
│   ├── zfile/                # Disk space, mount point & file copying utilities (Rust)
│   ├── zimage/               # Image compression & conversion addon (Rust)
│   ├── zjxl/                 # JPEG XL decoder & encoder (Rust)
│   ├── zlang/                # Language detection engine (Rust)
│   └── zwalker/              # Media directory garbage collector (Rust)
└── engines/                  # Standalone native subsystems
    ├── zcall/                # VoIP calling engine (signaling, ZRTP/SRTP & Rust audio)
    ├── zcall-audio/          # Call audio engine: mic/speaker, Opus, jitter buffer, AEC (Rust)
    ├── zcall-media/          # Call media process for ZCALL_MEDIA=rust: socket, SRTP, RTCP, video (Rust)
    ├── zocr/                 # OCR engine host daemon & ONNX runtime integration (Rust)
    └── zportal/              # Screenshot through the XDG desktop portal (Rust)
```

## Call media backends (`ZCALL_MEDIA`)

The call engine (`engines/zcall`, Node) keeps signalling and the call window;
the media of a call runs in one of two backends:

- `js` (default, `ZCALL_MEDIA` unset or `zrtc`): `engines/zcall/native-engine/backends/zrtc-media.js`
  owns the UDP socket and SRTP/RTCP in the engine, and spawns `zcall-audio`.
- `rust` (`ZCALL_MEDIA=rust`, opt-in): 1-1 calls run in the `zcall-media`
  process (`engines/zcall-media`): relay and P2P socket, SRTP, transport-cc /
  SR / RR / NACK / PLI, the send-rate controller, video both ways (camera, and
  the screen, which a 1-1 call sends on the camera stream) and `zcall-audio`,
  which it spawns itself. The engine talks to it through
  `backends/zrtc-media-rust.js` (one JSON object per line); every log line is
  the one `zrtc-media.js` writes, so the two can be diffed in
  `native-engine-*.log`. **Group calls always use `zrtc-media.js`**, whatever
  `ZCALL_MEDIA` says. Without the binary, `rust` falls back to `js`.

`engines/zcall-media` is also a library crate (ZRTP packets, SRTP, RTP, RTCP,
send-rate) checked against the JS modules with vectors they produce
(`tests/gen-vectors.js`, `tests/vectors.rs`).

## Building Native Addons

All native addons in `addons/` can be built using the unified builder:

```bash
# Build any C++ or Rust addon:
node native/builder.js native/addons/<addon-name>

# Examples:
node native/builder.js native/addons/db-cross-v4
node native/builder.js native/addons/zwalker
```

The unified builder automatically detects the toolchain (`binding.gyp` for C++ node-gyp, `Cargo.toml` for Rust) and outputs the compiled `.node` artifact.

## Rust Workspace

All Rust crates across `native/addons/` and `native/engines/` belong to the root Cargo workspace:

```bash
cargo test --workspace --locked
```
