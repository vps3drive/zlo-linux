//! zportal: one silent screenshot through the XDG desktop portal
//! (org.freedesktop.portal.Screenshot), for the screenshot plugin.
//!
//!   zportal [--output PATH] [--interactive]
//!
//! Prints the path of the image on stdout and exits 0; on failure says why on
//! stderr and exits 1. With --output the portal's file is moved there (the
//! portal saves into the user's Pictures folder otherwise).
//!
//! GIO / GLib are loaded with dlopen at runtime, as portal.py did with ctypes:
//! no build-time dependency, and a clear error where they are missing.

use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::ptr;

type Ptr = *mut c_void;

type SignalCallback = extern "C" fn(Ptr, *const c_char, *const c_char, *const c_char, *const c_char, Ptr, Ptr);
type SourceFunc = extern "C" fn(Ptr) -> c_int;

#[repr(C)]
struct GError {
    domain: u32,
    code: c_int,
    message: *const c_char,
}

struct Lib(Ptr);

impl Lib {
    fn open(name: &str) -> Result<Lib, String> {
        let c = CString::new(name).unwrap();
        let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
        if h.is_null() {
            Err(format!("cannot load {name}"))
        } else {
            Ok(Lib(h))
        }
    }

    fn sym<T: Copy>(&self, name: &str) -> Result<T, String> {
        let c = CString::new(name).unwrap();
        let p = unsafe { libc::dlsym(self.0, c.as_ptr()) };
        if p.is_null() {
            return Err(format!("missing symbol {name}"));
        }
        // SAFETY: T is the function pointer type of `name`.
        Ok(unsafe { std::mem::transmute_copy(&p) })
    }
}

struct Gio {
    bus_get_sync: extern "C" fn(c_int, Ptr, *mut *mut GError) -> Ptr,
    get_unique_name: extern "C" fn(Ptr) -> *const c_char,
    call_sync: extern "C" fn(Ptr, *const c_char, *const c_char, *const c_char, *const c_char, Ptr, Ptr, c_int, c_int, Ptr, *mut *mut GError) -> Ptr,
    signal_subscribe: extern "C" fn(Ptr, *const c_char, *const c_char, *const c_char, *const c_char, *const c_char, c_int, SignalCallback, Ptr, Ptr) -> c_uint,
    variant_parse: extern "C" fn(Ptr, *const c_char, *const c_char, *mut *const c_char, *mut *mut GError) -> Ptr,
    child_value: extern "C" fn(Ptr, usize) -> Ptr,
    get_uint32: extern "C" fn(Ptr) -> u32,
    get_string: extern "C" fn(Ptr, *mut usize) -> *const c_char,
    lookup_value: extern "C" fn(Ptr, *const c_char, Ptr) -> Ptr,
    variant_unref: extern "C" fn(Ptr),
    loop_new: extern "C" fn(Ptr, c_int) -> Ptr,
    loop_run: extern "C" fn(Ptr),
    loop_quit: extern "C" fn(Ptr),
    timeout_add: extern "C" fn(c_uint, SourceFunc, Ptr) -> c_uint,
}

impl Gio {
    fn load() -> Result<Gio, String> {
        let gio = Lib::open("libgio-2.0.so.0")?;
        let glib = Lib::open("libglib-2.0.so.0")?;
        Ok(Gio {
            bus_get_sync: gio.sym("g_bus_get_sync")?,
            get_unique_name: gio.sym("g_dbus_connection_get_unique_name")?,
            call_sync: gio.sym("g_dbus_connection_call_sync")?,
            signal_subscribe: gio.sym("g_dbus_connection_signal_subscribe")?,
            variant_parse: glib.sym("g_variant_parse")?,
            child_value: glib.sym("g_variant_get_child_value")?,
            get_uint32: glib.sym("g_variant_get_uint32")?,
            get_string: glib.sym("g_variant_get_string")?,
            lookup_value: glib.sym("g_variant_lookup_value")?,
            variant_unref: glib.sym("g_variant_unref")?,
            loop_new: glib.sym("g_main_loop_new")?,
            loop_run: glib.sym("g_main_loop_run")?,
            loop_quit: glib.sym("g_main_loop_quit")?,
            timeout_add: glib.sym("g_timeout_add")?,
        })
    }
}

/// What the Response signal and the timer hand back to `capture`.
struct State {
    gio: Gio,
    main_loop: Ptr,
    request: CString,
    response: Option<u32>,
    uri: Option<String>,
    timed_out: bool,
}

fn gerror_message(err: *mut GError, fallback: &str) -> String {
    if err.is_null() {
        return fallback.to_string();
    }
    let msg = unsafe { (*err).message };
    if msg.is_null() {
        fallback.to_string()
    } else {
        unsafe { CStr::from_ptr(msg) }.to_string_lossy().into_owned()
    }
}

extern "C" fn on_response(_c: Ptr, _s: *const c_char, path: *const c_char, _i: *const c_char, _m: *const c_char, params: Ptr, data: Ptr) {
    let st = unsafe { &mut *(data as *mut State) };
    if path.is_null() || unsafe { CStr::from_ptr(path) } != st.request.as_c_str() {
        return;
    }
    let g = &st.gio;
    let response = (g.child_value)(params, 0);
    let code = (g.get_uint32)(response);
    (g.variant_unref)(response);
    st.response = Some(code);
    if code == 0 {
        let results = (g.child_value)(params, 1);
        let key = CString::new("uri").unwrap();
        let value = (g.lookup_value)(results, key.as_ptr(), ptr::null_mut());
        if !value.is_null() {
            let s = (g.get_string)(value, ptr::null_mut());
            if !s.is_null() {
                st.uri = Some(unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned());
            }
            (g.variant_unref)(value);
        }
        (g.variant_unref)(results);
    }
    (g.loop_quit)(st.main_loop);
}

extern "C" fn on_timeout(data: Ptr) -> c_int {
    let st = unsafe { &mut *(data as *mut State) };
    st.timed_out = true;
    (st.gio.loop_quit)(st.main_loop);
    0 // G_SOURCE_REMOVE
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// file:///home/a/Pictures/Screenshot%20x.png -> /home/a/Pictures/Screenshot x.png
fn uri_to_path(uri: &str) -> String {
    match uri.strip_prefix("file://") {
        Some(rest) => {
            // file://host/path: drop the authority (empty or localhost).
            let path = match rest.find('/') {
                Some(i) => &rest[i..],
                None => rest,
            };
            let path = path.split(['?', '#']).next().unwrap_or(path);
            percent_decode(path)
        }
        None => uri.to_string(),
    }
}

fn random_token() -> String {
    let mut buf = [0u8; 16];
    let ok = unsafe { libc::getrandom(buf.as_mut_ptr() as Ptr, buf.len(), 0) } == buf.len() as isize;
    if !ok {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        buf.copy_from_slice(&(t ^ (std::process::id() as u128) << 64).to_le_bytes());
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn capture(output: Option<&Path>, interactive: bool, timeout_ms: u32) -> Result<PathBuf, String> {
    let gio = Gio::load()?;
    let mut err: *mut GError = ptr::null_mut();
    let bus = (gio.bus_get_sync)(2, ptr::null_mut(), &mut err); // G_BUS_TYPE_SESSION
    if bus.is_null() {
        return Err(format!("Cannot connect to session bus: {}", gerror_message(err, "Unknown error")));
    }

    let token = format!("zalo_screenshot_{}", random_token());
    let unique = (gio.get_unique_name)(bus);
    if unique.is_null() {
        return Err("Session bus connection has no unique name".into());
    }
    let sender = unsafe { CStr::from_ptr(unique) }.to_string_lossy().trim_start_matches(':').replace('.', "_");
    let request = CString::new(format!("/org/freedesktop/portal/desktop/request/{sender}/{token}")).unwrap();

    let main_loop = (gio.loop_new)(ptr::null_mut(), 0);
    let state = Box::into_raw(Box::new(State { gio, main_loop, request, response: None, uri: None, timed_out: false }));
    let st = unsafe { &mut *state };
    let g = &st.gio;

    let dest = CString::new("org.freedesktop.portal.Desktop").unwrap();
    let req_iface = CString::new("org.freedesktop.portal.Request").unwrap();
    let response = CString::new("Response").unwrap();
    // Subscribe before the call: the Response may come right after it.
    (g.signal_subscribe)(bus, dest.as_ptr(), req_iface.as_ptr(), response.as_ptr(), st.request.as_ptr(), ptr::null(), 0, on_response, state as Ptr, ptr::null_mut());

    let text = CString::new(format!(
        "('', {{'handle_token': <'{token}'>, 'interactive': <{}>}})",
        if interactive { "true" } else { "false" }
    ))
    .unwrap();
    let mut perr: *mut GError = ptr::null_mut();
    let params = (g.variant_parse)(ptr::null_mut(), text.as_ptr(), ptr::null(), ptr::null_mut(), &mut perr);
    if params.is_null() {
        return Err(format!("Bad request parameters: {}", gerror_message(perr, "parse error")));
    }

    let object = CString::new("/org/freedesktop/portal/desktop").unwrap();
    let iface = CString::new("org.freedesktop.portal.Screenshot").unwrap();
    let method = CString::new("Screenshot").unwrap();
    let mut cerr: *mut GError = ptr::null_mut();
    let reply = (g.call_sync)(bus, dest.as_ptr(), object.as_ptr(), iface.as_ptr(), method.as_ptr(), params, ptr::null_mut(), 0, 10000, ptr::null_mut(), &mut cerr);
    if reply.is_null() {
        return Err(format!("Screenshot portal request failed: {}", gerror_message(cerr, "Unknown D-Bus call error")));
    }
    (g.variant_unref)(reply);

    (g.timeout_add)(timeout_ms, on_timeout, state as Ptr);
    (g.loop_run)(main_loop);

    if st.timed_out {
        return Err(format!("Screenshot portal timed out after {timeout_ms}ms waiting for response"));
    }
    match st.response {
        Some(0) | None => {}
        Some(code) => {
            let desc = match code {
                1 => "User cancelled".to_string(),
                2 => "Request failed/denied".to_string(),
                c => format!("Error code {c}"),
            };
            return Err(format!("Portal response returned non-zero code {code}: {desc}"));
        }
    }
    let result = st.uri.as_deref().map(uri_to_path).unwrap_or_default();
    let result_path = PathBuf::from(&result);
    if result.is_empty() || !result_path.is_file() {
        return Err(format!("Portal did not produce a valid image file (got: {result})"));
    }

    let Some(dest) = output else { return Ok(result_path) };
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Move it out of the user's Pictures folder; copy + delete across filesystems.
    let moved = std::fs::rename(&result_path, dest).is_ok()
        || std::fs::copy(&result_path, dest).map(|_| {
            let _ = std::fs::remove_file(&result_path);
        }).is_ok();
    if moved {
        Ok(dest.to_path_buf())
    } else {
        eprintln!("Warning: failed to move portal output to {}", dest.display());
        Ok(result_path)
    }
}

fn main() {
    let mut output: Option<PathBuf> = None;
    let mut interactive = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--output" => output = args.next().map(PathBuf::from),
            "--interactive" => interactive = true,
            "--help" | "-h" => {
                println!("usage: zportal [--output PATH] [--interactive]");
                return;
            }
            _ => {}
        }
    }
    match capture(output.as_deref(), interactive, 10000) {
        Ok(p) => println!("{}", p.display()),
        Err(e) => {
            eprintln!("Portal error: {e}");
            exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uri_is_decoded() {
        assert_eq!(uri_to_path("file:///home/a/Pictures/Screenshot%20from%202026.png"), "/home/a/Pictures/Screenshot from 2026.png");
        assert_eq!(uri_to_path("file://localhost/tmp/a%C3%A1.png"), "/tmp/aá.png");
        assert_eq!(uri_to_path("/tmp/plain.png"), "/tmp/plain.png");
        assert_eq!(uri_to_path("file:///tmp/100%.png"), "/tmp/100%.png");
    }

    #[test]
    fn token_is_hex() {
        let t = random_token();
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
