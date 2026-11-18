//! ZCALL_AUDIO_DUMP=1 (off by default: it records the call) writes what was
//! played and what the microphone sent, raw 16 kHz mono s16le, to
//! <ZCALL_LOG_DIR>/call-<time>-played.raw and -mic.raw (after the processing,
//! before Opus; silence while muted), to find where speech breaks up.

use std::fs::File;
use std::io::Write;
use std::sync::{LazyLock, Mutex};

use crate::log;
use crate::protocol::to_bytes;

pub struct Dump {
    played: Mutex<Option<File>>,
    mic: Mutex<Option<File>>,
}

pub static DUMP: LazyLock<Dump> = LazyLock::new(Dump::open);

/// Local time as %Y%m%d-%H%M%S.
fn stamp() -> String {
    let t = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:04}{:02}{:02}-{:02}{:02}{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

impl Dump {
    fn open() -> Dump {
        let none = Dump { played: Mutex::new(None), mic: Mutex::new(None) };
        if std::env::var("ZCALL_AUDIO_DUMP").as_deref() != Ok("1") {
            return none;
        }
        let dir = std::env::var_os("ZCALL_LOG_DIR").filter(|d| !d.is_empty()).map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let stem = dir.join(format!("call-{}", stamp()));
        let path = |s: &str| format!("{}-{s}.raw", stem.display());
        match (File::create(path("played")), File::create(path("mic"))) {
            (Ok(p), Ok(m)) => {
                log!("dump: {} / -mic.raw (16 kHz mono s16le)", path("played"));
                Dump { played: Mutex::new(Some(p)), mic: Mutex::new(Some(m)) }
            }
            (Err(e), _) | (_, Err(e)) => {
                log!("dump unavailable: {e}");
                none
            }
        }
    }

    fn write(f: &Mutex<Option<File>>, pcm: &[i16]) {
        if let Some(f) = f.lock().unwrap().as_mut() {
            let _ = f.write_all(&to_bytes(pcm));
        }
    }

    pub fn write_played(&self, pcm: &[i16]) {
        Self::write(&self.played, pcm);
    }

    pub fn write_mic(&self, pcm: &[i16]) {
        Self::write(&self.mic, pcm);
    }
}
