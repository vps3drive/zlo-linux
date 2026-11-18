# Các Module và Engine Native

Thư mục này tập trung toàn bộ các bản triển khai native cho Linux thay thế các thành phần độc quyền của Zalo trên macOS và Windows:

```text
native/
├── builder.js                # Công cụ build hợp nhất (Rust Cargo/NAPI)
├── addons/                   # Các native addon N-API nạp động qua require()
│   ├── db-cross-v4/          # Giải mã bản sao lưu & đồng bộ tin nhắn E2EE (Rust)
│   ├── file-utilities/       # Quét hệ thống tệp bất đồng bộ & khớp mẫu (Rust)
│   ├── file-utils/           # Tiện ích truy cập & quản lý tệp (Rust)
│   ├── mp4thumb/             # Trích xuất ảnh thu nhỏ từ video MP4 (Rust)
│   ├── os-sys-info/          # Đọc thông tin hệ điều hành & PID (Rust)
│   ├── zfile/                # Thông tin ổ đĩa, dung lượng & sao chép tệp (Rust)
│   ├── zimage/               # Nén & chuyển đổi định dạng ảnh (Rust)
│   ├── zjxl/                 # Giải mã & mã hóa ảnh JPEG XL (Rust)
│   ├── zlang/                # Nhận diện ngôn ngữ văn bản (Rust)
│   └── zwalker/              # Dọn rác bộ nhớ đệm thư mục media (Rust)
└── engines/                  # Các hệ thống con độc lập
    ├── zcall/                # Engine gọi điện thoại (báo hiệu, ZRTP/SRTP & Rust audio)
    ├── zcall-audio/          # Engine âm thanh cuộc gọi: micro/loa, Opus, jitter buffer, AEC (Rust)
    ├── zcall-media/          # Tiến trình media cuộc gọi cho ZCALL_MEDIA=rust: socket, SRTP, RTCP, video (Rust)
    ├── zocr/                 # Daemon nhận diện chữ OCR & ONNX runtime (Rust)
    └── zportal/              # Chụp màn hình qua XDG desktop portal (Rust)
```

## Hai backend media của cuộc gọi (`ZCALL_MEDIA`)

Engine gọi điện (`engines/zcall`, chạy bằng Node) lo phần báo hiệu và cửa sổ
gọi; phần media của cuộc gọi chạy ở một trong hai backend:

- `js` (mặc định, khi không đặt `ZCALL_MEDIA` hoặc là `zrtc`):
  `engines/zcall/native-engine/backends/zrtc-media.js` giữ UDP socket và
  SRTP/RTCP ngay trong engine, và chạy `zcall-audio`.
- `rust` (`ZCALL_MEDIA=rust`, phải bật tay): cuộc gọi 1-1 chạy trong tiến
  trình `zcall-media` (`engines/zcall-media`): socket relay và P2P, SRTP,
  transport-cc / SR / RR / NACK / PLI, bộ điều khiển send-rate, video hai
  chiều (camera, và màn hình, vốn đi chung luồng camera ở cuộc gọi 1-1), và tự
  chạy `zcall-audio`. Engine nói chuyện với nó qua
  `backends/zrtc-media-rust.js` (mỗi dòng một object JSON); mọi dòng log giống
  hệt dòng `zrtc-media.js` ghi, nên có thể diff hai bản trong
  `native-engine-*.log`. **Cuộc gọi nhóm luôn dùng `zrtc-media.js`**, bất kể
  `ZCALL_MEDIA`. Thiếu binary thì `rust` tự lùi về `js`.

`engines/zcall-media` đồng thời là thư viện (gói ZRTP, SRTP, RTP, RTCP,
send-rate), được đối chiếu với các module JS bằng vector do chính chúng sinh
ra (`tests/gen-vectors.js`, `tests/vectors.rs`).

## Biên dịch Native Addons

Tất cả các native addon trong `addons/` đều có thể biên dịch bằng công cụ hợp nhất:

```bash
# Biên dịch addon C++ hoặc Rust bất kỳ:
node native/builder.js native/addons/<tên-addon>

# Ví dụ:
node native/builder.js native/addons/db-cross-v4
node native/builder.js native/addons/zwalker
```

Công cụ tự động nhận diện toolchain (`binding.gyp` cho C++ node-gyp, `Cargo.toml` cho Rust) và xuất file nhị phân `.node`.

## Rust Workspace

Toàn bộ các crate Rust trong `native/addons/` và `native/engines/` đều nằm trong Cargo workspace gốc:

```bash
cargo test --workspace --locked
```
