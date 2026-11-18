//! Runtime loading of the system's shared libraries (libopus, libpulse-simple).
//!
//! The binary does not link them: a machine without libpulse can still run
//! the engine through the ALSA tools, and `--check` can say what is missing
//! instead of the dynamic loader refusing to start the process.

use std::ffi::{c_void, CString};

pub struct Lib(*mut c_void);

// The handle is only used to look up symbols; dlsym is thread-safe.
unsafe impl Send for Lib {}
unsafe impl Sync for Lib {}

impl Lib {
    /// The first of `names` (sonames) that loads.
    pub fn open(names: &[&str]) -> Result<Lib, String> {
        for name in names {
            let c = CString::new(*name).unwrap();
            let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if !h.is_null() {
                return Ok(Lib(h));
            }
        }
        Err(format!("cannot load {}", names.join(" / ")))
    }

    /// A function pointer of type `T` for `name`.
    pub fn sym<T: Copy>(&self, name: &str) -> Result<T, String> {
        assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<*mut c_void>());
        let c = CString::new(name).unwrap();
        let p = unsafe { libc::dlsym(self.0, c.as_ptr()) };
        if p.is_null() {
            return Err(format!("missing symbol {name}"));
        }
        // SAFETY: the caller names the C signature of `name` as T.
        Ok(unsafe { std::mem::transmute_copy(&p) })
    }
}
