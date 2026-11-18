//! SRTP AES_CM_128_HMAC_SHA1_80 as used by Zalo zrtc (libsrtp2), port of
//! native-engine/srtp-aes.js.
//!
//! Master key+salt = first 30 bytes of sessId (parseCallConfig when srtpKey
//! empty). Session keys via the RFC 3711 KDF. Wire: RTP header (with its
//! extension) + encrypted Opus + 10-byte HMAC.

use aes::cipher::{BlockEncrypt, KeyInit};
use aes::Aes128;
use hmac::{Hmac, Mac};
use sha1::Sha1;

#[derive(Clone)]
pub struct SessionKeys {
    pub enc: [u8; 16],
    pub auth: [u8; 20],
    pub salt: [u8; 14],
    cipher: Aes128,
}

impl std::fmt::Debug for SessionKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionKeys{..}")
    }
}

/// AES-CM keystream: iv with a 16-bit block counter in its last two bytes.
fn aes_cm_expand(cipher: &Aes128, iv: &[u8; 16], out: &mut [u8]) {
    for (counter, chunk) in out.chunks_mut(16).enumerate() {
        let mut blk = aes::Block::clone_from_slice(iv);
        blk[14] = (counter >> 8) as u8;
        blk[15] = counter as u8;
        cipher.encrypt_block(&mut blk);
        chunk.copy_from_slice(&blk[..chunk.len()]);
    }
}

fn kdf(master_key: &Aes128, master_salt: &[u8], label: u8, out: &mut [u8]) {
    let mut x = [0u8; 16];
    let n = master_salt.len().min(14);
    x[..n].copy_from_slice(&master_salt[..n]);
    x[7] ^= label;
    aes_cm_expand(master_key, &x, out);
}

/// Session keys from the 30-byte master key + salt.
pub fn session_keys(master30: &[u8]) -> Result<SessionKeys, &'static str> {
    if master30.len() < 30 {
        return Err("master key+salt must be 30 bytes");
    }
    let mk = Aes128::new_from_slice(&master30[..16]).unwrap();
    let ms = &master30[16..30];
    let mut enc = [0u8; 16];
    let mut auth = [0u8; 20];
    let mut salt = [0u8; 14];
    kdf(&mk, ms, 0x00, &mut enc);
    kdf(&mk, ms, 0x01, &mut auth);
    kdf(&mk, ms, 0x02, &mut salt);
    Ok(SessionKeys { cipher: Aes128::new_from_slice(&enc).unwrap(), enc, auth, salt })
}

/// The first 30 bytes of the session id.
pub fn master_from_sess_id(sess_id: &[u8]) -> Result<&[u8], &'static str> {
    if sess_id.len() < 30 {
        return Err("sessId shorter than 30");
    }
    Ok(&sess_id[..30])
}

pub fn packet_iv(salt: &[u8; 14], ssrc: u32, seq: u16, roc: u32) -> [u8; 16] {
    let idx = (roc as u64) * 0x10000 + seq as u64;
    let shifted = idx << 16;
    let mut iv = [0u8; 16];
    iv[4..8].copy_from_slice(&ssrc.to_be_bytes());
    iv[8..12].copy_from_slice(&((shifted >> 32) as u32).to_be_bytes());
    iv[12..16].copy_from_slice(&(shifted as u32).to_be_bytes());
    for i in 0..14 {
        iv[i] ^= salt[i];
    }
    iv
}

pub struct RtpView<'a> {
    pub seq: u16,
    pub ssrc: u32,
    pub hdr: &'a [u8],
    pub payload: &'a [u8],
}

/// Header (with CSRCs and extension) and payload of an RTP packet.
pub fn parse_rtp(buf: &[u8]) -> Option<RtpView<'_>> {
    if buf.len() < 12 {
        return None;
    }
    let x = (buf[0] >> 4) & 1;
    let seq = u16::from_be_bytes([buf[2], buf[3]]);
    let ssrc = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let mut off = 12 + 4 * (buf[0] & 0x0f) as usize;
    if x == 1 {
        if buf.len() < off + 4 {
            return None;
        }
        let words = u16::from_be_bytes([buf[off + 2], buf[off + 3]]) as usize;
        off += 4 + 4 * words;
    }
    if off > buf.len() {
        return None;
    }
    Some(RtpView { seq, ssrc, hdr: &buf[..off], payload: &buf[off..] })
}

fn tag(keys: &SessionKeys, hdr: &[u8], body: &[u8], roc: u32) -> [u8; 10] {
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(&keys.auth).unwrap();
    mac.update(hdr);
    mac.update(body);
    mac.update(&roc.to_be_bytes());
    let full = mac.finalize().into_bytes();
    let mut out = [0u8; 10];
    out.copy_from_slice(&full[..10]);
    out
}

pub struct Unprotected {
    pub seq: u16,
    pub ssrc: u32,
    pub payload: Vec<u8>,
}

/// Check the tag and decrypt; None if it does not authenticate.
pub fn unprotect_rtp(rtp: &[u8], keys: &SessionKeys, roc: u32) -> Option<Unprotected> {
    let p = parse_rtp(rtp)?;
    if p.payload.len() < 10 {
        return None;
    }
    let (body, got) = p.payload.split_at(p.payload.len() - 10);
    let want = tag(keys, p.hdr, body, roc);
    // Constant time compare.
    if want.iter().zip(got).fold(0u8, |a, (x, y)| a | (x ^ y)) != 0 {
        return None;
    }
    let iv = packet_iv(&keys.salt, p.ssrc, p.seq, roc);
    let mut plain = vec![0u8; body.len()];
    aes_cm_expand(&keys.cipher, &iv, &mut plain);
    for (o, b) in plain.iter_mut().zip(body) {
        *o ^= b;
    }
    Some(Unprotected { seq: p.seq, ssrc: p.ssrc, payload: plain })
}

/// header + encrypted payload + 10-byte tag.
pub fn protect_rtp(header: &[u8], payload: &[u8], keys: &SessionKeys, seq: u16, ssrc: u32, roc: u32) -> Vec<u8> {
    let iv = packet_iv(&keys.salt, ssrc, seq, roc);
    let mut out = Vec::with_capacity(header.len() + payload.len() + 10);
    out.extend_from_slice(header);
    let start = out.len();
    out.resize(start + payload.len(), 0);
    aes_cm_expand(&keys.cipher, &iv, &mut out[start..]);
    for (o, b) in out[start..].iter_mut().zip(payload) {
        *o ^= b;
    }
    let t = tag(keys, header, &out[start..], roc);
    out.extend_from_slice(&t);
    out
}

/// RFC 3711 §3.3.1 rollover-counter guess for received packets.
#[derive(Default, Clone)]
pub struct RocTracker {
    last: Option<u16>,
    roc: u32,
}

impl RocTracker {
    pub fn guess(&self, seq: u16) -> u32 {
        let Some(last) = self.last else { return 0 };
        let (last, seq) = (last as i32, seq as i32);
        if last - seq > 0x8000 {
            return self.roc + 1;
        }
        if seq - last > 0x8000 && self.roc > 0 {
            return self.roc - 1;
        }
        self.roc
    }

    pub fn update(&mut self, seq: u16, roc: u32) {
        let newer = match self.last {
            None => true,
            Some(last) => (roc as u64) * 0x10000 + seq as u64 > (self.roc as u64) * 0x10000 + last as u64,
        };
        if newer {
            self.last = Some(seq);
            self.roc = roc;
        }
    }
}
