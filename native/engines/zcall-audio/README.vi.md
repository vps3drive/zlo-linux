# zcall-audio

[English](./README.md) | **Tiếng Việt**

Engine âm thanh của engine gọi native Zalo trên Linux: micro -> Opus -> mạng, mạng -> Opus -> loa. Thay thế bản Python `audio-io.py` (cùng `parec` / `pacat`) trước đây.

---

## 1. Phân tích nguyên nhân gốc rễ (Root Cause)

Ở các phiên bản trước của `zalo-for-linux`, tính năng gọi thoại và gọi video gặp phải độ trễ âm thanh tương đối lớn (~150ms), thỉnh thoảng bị rè hoặc giật âm thanh khi CPU bận, và phụ thuộc nhiều gói cài đặt bên ngoài.

### Kiến trúc trước đây:
```
[Node.js zrtc-media]
       │  (IPC pipe stdin/stdout)
       ▼
 [Python audio-io.py]
   ├── pipe ──> parec (tiến trình thu âm micro của PulseAudio)
   └── pipe ──> pacat (tiến trình phát âm thanh ra loa của PulseAudio)
```

### Các nút thắt cổ chai kỹ thuật:
1. **Chuyển ngữ cảnh đa tiến trình liên tục (Context Switching)**: Mỗi frame âm thanh 20ms (tần suất 50 lần/giây) phải luân chuyển qua 4 tiến trình OS riêng biệt (`node`, `python3`, `parec`, `pacat`) qua 6 đường Unix pipe. Việc này gây tiêu tốn tài nguyên CPU và làm gia tăng độ trễ điều phối tiến trình.
2. **Hiện tượng hụt bộ đệm (Buffer Underrun)**: Khi CPU có tải nền, việc trễ đọc/ghi pipe khiến luồng phát bị đứt quãng, gây tiếng lộp bộp (clipping) hoặc mất tiếng ngắn hạn.
3. **Phụ thuộc môi trường bên ngoài**: Người dùng bắt buộc phải cài đặt `python3` và bộ công cụ `pulseaudio-utils` (`parec`/`pacat`). Trên các bản phân phối Linux tối giản hoặc hệ thống immutable (như SteamOS, Silverblue), cuộc gọi sẽ bị lỗi ngay lập tức nếu thiếu các gói này.

---

## 2. Thông số kỹ thuật & Giao thức bóc tách được

Qua phân tích cấu trúc luồng cuộc gọi và gói tin mạng của Zalo, các thông số chuẩn và giao thức giao tiếp đã được xác định chính xác:

### Thông số luồng âm thanh
- **Tần số lấy mẫu (Sample Rate)**: 16,000 Hz (16 kHz Wideband).
- **Số kênh**: 1 (Mono).
- **Định dạng mẫu**: Signed 16-bit Little Endian PCM (`s16le`).
- **Thời lượng frame**: 20 ms / frame (320 mẫu PCM = 640 bytes raw).
- **Codec nén**: Opus (`OPUS_APPLICATION_VOIP`), bật in-band FEC, bitrate thích ứng từ 20 kbps đến 32 kbps.

### Giao thức nhị phân qua Stdin / Stdout

Giao tiếp giữa tiến trình điều phối `zrtc-media.js` và audio engine sử dụng định dạng nhị phân chuẩn big-endian:

#### Khung dữ liệu gửi đi (Engine -> Node stdout)
- **Tag `'L'` (0x4c)**: Frame âm thanh từ micro gửi lên mạng.
  - Byte 0: `0x4c` (`'L'`)
  - Byte 1: Mức âm lượng (chuẩn RFC 6464, thang logarit `-dBov` từ 0..127, trong đó 127 là yên lặng hoàn toàn, 0 là đỉnh tín hiệu 0 dBFS).
  - Byte 2-3: Độ dài payload `N` (số nguyên 16-bit big-endian).
  - Byte 4..(4+N): Dữ liệu Opus đã nén.

#### Khung dữ liệu nhận về (Node -> Engine stdin)
- **Tag `'F'` (0x46)**: Gói tin Opus từ đối phương gửi về để phát ra loa.
  - Byte 0: `0x46` (`'F'`)
  - Byte 1-4: Chỉ số thứ tự gói tin `idx` (số nguyên 32-bit big-endian).
  - Byte 5-8: Định danh nguồn đồng bộ `ssrc` (số nguyên 32-bit big-endian).
  - Byte 9-10: Độ dài payload `N` (số nguyên 16-bit big-endian).
  - Byte 11..(11+N): Dữ liệu Opus cần giải mã.

#### Lệnh điều khiển (Node -> Engine stdin)
- **`'M' (0x4d)` - Tắt/Bật Micro (Mute)**: `[0x4d, <0|1>]` (`1`: mute, phát yên lặng; `0`: bật thu âm).
- **`'S' (0x53)` - Tắt/Bật Loa**: `[0x53, <0|1>]` (`1`: ngắt tiếng phát; `0`: phát bình thường).
- **`'V' (0x56)` - Âm lượng phần mềm**: `[0x56, <phần trăm 0..100>]`.
- **`'D' (0x44)` - Đổi thiết bị âm thanh**: `[0x44, <'i'|'o'>, <len_hi>, <len_lo>, <tên_thiết_bị_utf8>]`.

---

## 3. Cách thức triển khai (Implementation)

`zcall-audio` là một binary Rust duy nhất và là engine âm thanh duy nhất: bản
Python `audio-io.py` trước đây đã bị xoá. Đây là bản port từng module của
script đó, đã đối chiếu khớp từng bit với bản Python trên cùng đầu vào (quyết
định của jitter buffer, PCM đầu ra, gain của MicLeveler).

```
[Node.js zrtc-media]
       │  (stdin/stdout IPC pipe)
       ▼
[zcall-audio]
   ├── libpulse-simple (dlopen) ──> PipeWire / PulseAudio, hoặc arecord/aplay nếu không có
   ├── libopus (dlopen)         ──> encode / decode, FEC, PLC
   └── pipewire -c (riêng)      ──> khử tiếng vọng WebRTC (module-echo-cancel)
```

libopus và libpulse-simple được nạp lúc chạy (`dl.rs`), nên binary không phụ
thuộc chúng lúc link và `zcall-audio --check` có thể báo thiếu gì (plugin dùng
kết quả này để gợi ý lệnh cài đặt).

| Module | Chức năng |
| --- | --- |
| `io.rs` | Micro (libpulse-simple, parec hoặc arecord) và loa (libpulse-simple, pacat, aplay hoặc file `--out`), đổi được giữa cuộc gọi |
| `playout.rs` | Mỗi nguồn (SSRC) một NetEq nhỏ: độ sâu thích ứng theo jitter đo được (`--min-delay` / `--max-delay`), PLC khi chờ, FEC cho gói mất, Accelerate theo chu kỳ cao độ (WSOLA), reset khi dồn gói; mix các nguồn có limiter theo clock của sound card |
| `processing.rs` | Tiến trình `pipewire` riêng chạy module-echo-cancel (AEC; khử nhiễu chỉ bật khi `ZCALL_NOISE_SUPPRESSION=1`), và xin RTKit realtime cho luồng audio của PipeWire |
| `leveler.rs` | MicLeveler: gain bù chậm chỉ cho tiếng nói, giữ nguyên khi đầu bên kia nói, nhớ qua các cuộc gọi |
| `guard.rs` | ClipGuard: hạ âm lượng source PulseAudio khi tiếng nói bị clip; thanh trượt âm lượng của cửa sổ gọi |
| `diag.rs`, `dump.rs` | Dòng `diag:` mỗi 10 s (`ZCALL_AUDIO_DIAG=0` để tắt); ghi PCM thô khi `ZCALL_AUDIO_DUMP=1` |
| `dsp.rs` | RMS, mức RFC 6464, pitch lag / WSOLA |

Các biến môi trường của `audio-io.py` giữ nguyên ý nghĩa: `ZCALL_MIC`,
`ZCALL_SPEAKER`, `ZCALL_AUDIO_PROCESSING`, `ZCALL_NOISE_SUPPRESSION`,
`ZCALL_MIC_GAIN`, `ZCALL_MIC_AGC`, `ZCALL_PW_RT`, `ZCALL_AUDIO_DIAG`,
`ZCALL_AUDIO_DUMP`, `ZCALL_LOG_DIR`.

## 4. Biên dịch & Kiểm thử

### Biên dịch
```bash
cargo build --release -p zcall-audio
```

Binary đầu ra tại `target/release/zcall-audio` và được copy vào `zcall-native/native-engine/zcall-audio` khi chạy `npm run main`.

### Kiểm thử
```bash
# Chạy unit test trong Rust (RFC 6464, Jitter reorder, PLC)
cargo test -p zcall-audio

# Chạy toàn bộ integration test suite
npm test
```

---

## 5. Đóng góp & Bản quyền

- **Tác giả hiện thực**: Nghiên cứu dịch ngược clean-room và lập trình native cho Linux bởi [@ngxccc](https://github.com/ngxccc).
- **Tích hợp & Phân phối**: Duy trì bởi [Việt Nam Linux Family](https://github.com/VN-Linux-Family) dưới giấy phép BSD-3-Clause.
