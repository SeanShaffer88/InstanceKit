//! C ABI surface — everything declared in
//! include/instancekit.h. Conventions (house standard):
//! - `*mut c_char` results are caller-owned; free with
//!   `in_free_string`. On failure they are null and the
//!   thread-local last-error (code + message) says why.
//! - `in_release` returns an `IN_*` code directly and sets
//!   the last-error on failure.
//! - Nothing unwinds across the FFI boundary (catch_unwind
//!   everywhere; release profile also sets panic = "abort").
//! - All state is serialized internally; every entry point
//!   may be called from any thread.

// The exported functions ARE the raw-pointer C ABI declared
// in include/instancekit.h; pointer validity is the
// documented contract.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::panic::catch_unwind;

use crate::{
    acquire_with, core_poll, core_release, parse_acquire_spec, parse_poll_spec, parse_release_spec,
    platform, InResult, ERR_INTERNAL, ERR_INVALID_ARG, IN_OK,
};

thread_local! {
    static LAST_ERROR: RefCell<(i32, CString)> =
        RefCell::new((0, CString::new("").unwrap()));
}

pub fn set_error(code: i32, msg: &str) {
    let clean: String = msg.chars().filter(|&c| c != '\0').collect();
    LAST_ERROR.with(|e| {
        *e.borrow_mut() = (
            code,
            CString::new(clean).unwrap_or_else(|_| CString::new("error").unwrap()),
        );
    });
}

fn clear_error() {
    set_error(IN_OK, "");
}

fn fail(code: i32, msg: &str) -> c_int {
    set_error(code, msg);
    code as c_int
}

fn panic_code() -> c_int {
    set_error(ERR_INTERNAL, "internal panic caught at the FFI boundary");
    ERR_INTERNAL as c_int
}

fn guard<T, F: FnOnce() -> T>(f: F) -> std::thread::Result<T> {
    catch_unwind(std::panic::AssertUnwindSafe(f))
}

fn cstr<'a>(ptr: *const c_char, what: &str) -> InResult<&'a str> {
    if ptr.is_null() {
        return Err((ERR_INVALID_ARG, format!("{what} must not be null")));
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| (ERR_INVALID_ARG, format!("{what} must be valid UTF-8")))
}

/// Turn a core result into the FFI string contract:
/// caller-owned JSON on success, null + last-error on
/// failure.
fn out_result(result: InResult<String>) -> *mut c_char {
    match result {
        Ok(text) => match CString::new(text) {
            Ok(s) => {
                clear_error();
                s.into_raw()
            }
            Err(_) => {
                set_error(ERR_INTERNAL, "result string contained a NUL byte");
                std::ptr::null_mut()
            }
        },
        Err((code, msg)) => {
            set_error(code, &msg);
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "C" fn in_version() -> *const c_char {
    static VERSION_C: &[u8] = b"0.1.0\0";
    VERSION_C.as_ptr() as *const c_char
}

#[no_mangle]
pub extern "C" fn in_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().1.as_ptr())
}

#[no_mangle]
pub extern "C" fn in_last_error_code() -> c_int {
    LAST_ERROR.with(|e| e.borrow().0) as c_int
}

/// # Safety (caller contract): `p` is null or a string this
/// library returned and has not been freed yet. Documented
/// in the header.
#[no_mangle]
pub extern "C" fn in_free_string(p: *mut c_char) {
    if !p.is_null() {
        unsafe {
            drop(CString::from_raw(p));
        }
    }
}

fn acquire_impl(spec_json: *const c_char) -> *mut c_char {
    let inner = (|| -> InResult<String> {
        let text = cstr(spec_json, "spec_json")?;
        let spec = parse_acquire_spec(text)?;
        let mut backend = platform::Backend::new();
        acquire_with(&mut backend, &spec)
        // The backend is dropped here on every path: on the
        // primary path its mutex handle has moved into the
        // registered PrimaryHandle, so the drop closes
        // nothing; on an error path it closes a mutex that
        // never got a listener (see platform docs).
    })();
    out_result(inner)
}

#[no_mangle]
pub extern "C" fn in_acquire(spec_json: *const c_char) -> *mut c_char {
    match guard(|| acquire_impl(spec_json)) {
        Ok(p) => p,
        Err(_) => {
            set_error(ERR_INTERNAL, "internal panic caught at the FFI boundary");
            std::ptr::null_mut()
        }
    }
}

fn poll_impl(spec_json: *const c_char) -> *mut c_char {
    let inner = (|| -> InResult<String> {
        let text = cstr(spec_json, "spec_json")?;
        let spec = parse_poll_spec(text)?;
        core_poll(spec.token, spec.timeout_ms)
    })();
    out_result(inner)
}

#[no_mangle]
pub extern "C" fn in_poll(spec_json: *const c_char) -> *mut c_char {
    match guard(|| poll_impl(spec_json)) {
        Ok(p) => p,
        Err(_) => {
            set_error(ERR_INTERNAL, "internal panic caught at the FFI boundary");
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "C" fn in_release(spec_json: *const c_char) -> c_int {
    match guard(|| -> c_int {
        let inner = (|| -> InResult<()> {
            let text = cstr(spec_json, "spec_json")?;
            let token = parse_release_spec(text)?;
            core_release(token)
        })();
        match inner {
            Ok(()) => {
                clear_error();
                IN_OK as c_int
            }
            Err((code, msg)) => fail(code, &msg),
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ERR_NOT_FOUND, ERR_PARSE};

    fn cs(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    fn take(p: *mut c_char) -> String {
        assert!(!p.is_null(), "got null, last error: {:?}", unsafe {
            CStr::from_ptr(in_last_error())
        });
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
        in_free_string(p);
        s
    }

    #[test]
    fn ffi_version_and_free_null() {
        let v = unsafe { CStr::from_ptr(in_version()) }.to_str().unwrap();
        assert_eq!(v, "0.1.0");
        in_free_string(std::ptr::null_mut()); // must not crash
    }

    #[test]
    fn ffi_acquire_spec_errors() {
        // Null spec.
        assert!(in_acquire(std::ptr::null()).is_null());
        assert_eq!(in_last_error_code(), ERR_INVALID_ARG as c_int);
        // Malformed JSON -> ERR_PARSE.
        assert!(in_acquire(cs(r#"{"name":"#).as_ptr()).is_null());
        assert_eq!(in_last_error_code(), ERR_PARSE as c_int);
        // Bad name -> ERR_INVALID_ARG.
        assert!(in_acquire(cs(r#"{"name":"has space"}"#).as_ptr()).is_null());
        assert_eq!(in_last_error_code(), ERR_INVALID_ARG as c_int);
    }

    #[cfg(not(windows))]
    #[test]
    fn ffi_acquire_not_supported_off_windows() {
        assert!(in_acquire(cs(r#"{"name":"ffi-probe"}"#).as_ptr()).is_null());
        assert_eq!(in_last_error_code(), crate::ERR_NOT_SUPPORTED as c_int);
    }

    #[test]
    fn ffi_poll_and_release_unknown_token() {
        // No acquire can have minted token 987654321 here.
        assert!(in_poll(cs(r#"{"token":987654321,"timeout_ms":0}"#).as_ptr()).is_null());
        assert_eq!(in_last_error_code(), ERR_NOT_FOUND as c_int);
        assert_eq!(
            in_release(cs(r#"{"token":987654321}"#).as_ptr()),
            ERR_NOT_FOUND as c_int
        );
        // Bad specs still fail as parse/arg errors first.
        assert!(in_poll(cs(r#"{"token":1}"#).as_ptr()).is_null());
        assert_eq!(in_last_error_code(), ERR_INVALID_ARG as c_int);
        assert_eq!(in_release(cs(r#"not json"#).as_ptr()), ERR_PARSE as c_int);
    }

    #[test]
    fn ffi_full_cycle_against_registered_primary() {
        // Register a primary directly (the state machine's
        // own path, minus the OS backend) and drive it
        // through the real FFI: poll its mailbox, then
        // release through in_release.
        let mailbox = std::sync::Arc::new(crate::Mailbox::new());
        let token =
            crate::register_primary(mailbox.clone(), crate::platform::PrimaryHandle::detached());
        mailbox.push(crate::json::parse(r#"{"file":"a.txt"}"#).unwrap());

        let out = take(in_poll(
            cs(&format!(r#"{{"token":{token},"timeout_ms":1000}}"#)).as_ptr(),
        ));
        assert_eq!(out, r#"{"args":{"file":"a.txt"}}"#);
        let out = take(in_poll(
            cs(&format!(r#"{{"token":{token},"timeout_ms":0}}"#)).as_ptr(),
        ));
        assert_eq!(out, r#"{"args":null}"#);

        assert_eq!(
            in_release(cs(&format!(r#"{{"token":{token}}}"#)).as_ptr()),
            IN_OK as c_int
        );
        // And the token is gone afterwards.
        assert!(in_poll(cs(&format!(r#"{{"token":{token},"timeout_ms":0}}"#)).as_ptr()).is_null());
        assert_eq!(in_last_error_code(), ERR_NOT_FOUND as c_int);
    }
}
