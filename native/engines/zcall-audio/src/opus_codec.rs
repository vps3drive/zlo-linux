//! libopus (loaded at runtime): the 16 kHz mono encoder and decoders.

use std::ffi::{c_int, c_uchar, c_void};
use std::sync::OnceLock;

use crate::dl::Lib;
use crate::protocol::{FRAME_SAMPLES, SAMPLE_RATE};

const OPUS_APPLICATION_VOIP: c_int = 2048;
const OPUS_SET_BITRATE: c_int = 4002;
const OPUS_SET_COMPLEXITY: c_int = 4010;
const OPUS_SET_INBAND_FEC: c_int = 4012;
const OPUS_SET_PACKET_LOSS_PERC: c_int = 4014;
const OPUS_SET_SIGNAL: c_int = 4024;
const OPUS_SIGNAL_VOICE: c_int = 3001;

/// Longest frame a decoder may return (120 ms at 16 kHz).
pub const MAX_DECODE: usize = 1920;

struct Api {
    _lib: Lib,
    encoder_create: extern "C" fn(i32, c_int, c_int, *mut c_int) -> *mut c_void,
    encode: extern "C" fn(*mut c_void, *const i16, c_int, *mut c_uchar, i32) -> i32,
    // opus_encoder_ctl is variadic; every request used here takes one int.
    encoder_ctl: unsafe extern "C" fn(*mut c_void, c_int, ...) -> c_int,
    encoder_destroy: extern "C" fn(*mut c_void),
    decoder_create: extern "C" fn(i32, c_int, *mut c_int) -> *mut c_void,
    decode: extern "C" fn(*mut c_void, *const c_uchar, i32, *mut i16, c_int, c_int) -> c_int,
    decoder_destroy: extern "C" fn(*mut c_void),
}

fn load() -> Result<Api, String> {
    let lib = Lib::open(&["libopus.so.0", "libopus.so"])?;
    Ok(Api {
        encoder_create: lib.sym("opus_encoder_create")?,
        encode: lib.sym("opus_encode")?,
        encoder_ctl: lib.sym("opus_encoder_ctl")?,
        encoder_destroy: lib.sym("opus_encoder_destroy")?,
        decoder_create: lib.sym("opus_decoder_create")?,
        decode: lib.sym("opus_decode")?,
        decoder_destroy: lib.sym("opus_decoder_destroy")?,
        _lib: lib,
    })
}

fn api() -> Result<&'static Api, String> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(load).as_ref().map_err(|e| e.clone())
}

/// Whether libopus can be loaded (for --check).
pub fn available() -> bool {
    api().is_ok()
}

pub struct EncoderConfig {
    pub bitrate: i32,
    pub complexity: i32,
    pub fec: bool,
    pub loss: i32,
}

pub struct OpusEncoder {
    api: &'static Api,
    handle: *mut c_void,
}

unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    pub fn new(cfg: &EncoderConfig) -> Result<Self, String> {
        let api = api()?;
        let mut err = 0;
        let handle = (api.encoder_create)(SAMPLE_RATE as i32, 1, OPUS_APPLICATION_VOIP, &mut err);
        if err != 0 || handle.is_null() {
            return Err(format!("opus_encoder_create failed ({err})"));
        }
        unsafe {
            (api.encoder_ctl)(handle, OPUS_SET_BITRATE, cfg.bitrate as c_int);
            (api.encoder_ctl)(handle, OPUS_SET_COMPLEXITY, cfg.complexity as c_int);
            (api.encoder_ctl)(handle, OPUS_SET_SIGNAL, OPUS_SIGNAL_VOICE);
            (api.encoder_ctl)(handle, OPUS_SET_INBAND_FEC, cfg.fec as c_int);
            (api.encoder_ctl)(handle, OPUS_SET_PACKET_LOSS_PERC, if cfg.fec { cfg.loss } else { 0 } as c_int);
        }
        Ok(Self { api, handle })
    }

    /// One 20 ms frame into `out`; the encoded size, or None on error.
    pub fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Option<usize> {
        let n = (self.api.encode)(self.handle, pcm.as_ptr(), FRAME_SAMPLES as c_int, out.as_mut_ptr(), out.len() as i32);
        (n > 0).then_some(n as usize)
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        (self.api.encoder_destroy)(self.handle);
    }
}

pub struct OpusDecoder {
    api: &'static Api,
    handle: *mut c_void,
    buf: Vec<i16>,
}

unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    pub fn new() -> Result<Self, String> {
        let api = api()?;
        let mut err = 0;
        let handle = (api.decoder_create)(SAMPLE_RATE as i32, 1, &mut err);
        if err != 0 || handle.is_null() {
            return Err(format!("opus_decoder_create failed ({err})"));
        }
        Ok(Self { api, handle, buf: vec![0; MAX_DECODE] })
    }

    /// Decode a packet (None: packet loss concealment). PLC and FEC ask for
    /// exactly one 20 ms frame: with the 1920-sample maximum libopus conceals
    /// 120 ms per lost packet. Returns the samples, or None on error.
    pub fn decode(&mut self, payload: Option<&[u8]>, fec: bool) -> Option<&[i16]> {
        let size = if fec || payload.is_none() { FRAME_SAMPLES } else { MAX_DECODE };
        let (ptr, len) = match payload {
            Some(p) if !p.is_empty() => (p.as_ptr(), p.len() as i32),
            _ => (std::ptr::null(), 0),
        };
        let n = (self.api.decode)(self.handle, ptr, len, self.buf.as_mut_ptr(), size as c_int, fec as c_int);
        (n > 0).then(|| &self.buf[..n as usize])
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        (self.api.decoder_destroy)(self.handle);
    }
}
