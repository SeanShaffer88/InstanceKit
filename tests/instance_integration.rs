//! Windows-only end-to-end test through the real FFI: two
//! acquires in one process (the named mutex makes the
//! second one a secondary), a forwarded argument set, the
//! primary polling it back, and release freeing the name.
//!
//! This file compiles to nothing off Windows. It has never
//! run anywhere but Windows CI — see docs/PARITY.md.
#![cfg(windows)]

use instance_kit::ffi::{in_acquire, in_free_string, in_poll, in_release};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

fn cs(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn take(p: *mut c_char) -> String {
    assert!(!p.is_null(), "FFI returned null");
    let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
    in_free_string(p);
    s
}

fn token_of(result: &str) -> u64 {
    let at = result
        .find("\"token\":")
        .expect("primary result has a token")
        + "\"token\":".len();
    result[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap()
}

#[test]
fn primary_secondary_forward_poll_release() {
    // Unique per test process so parallel runs never meet.
    let name = format!("InstanceKitIt{}", std::process::id());

    // First acquire wins the election.
    let primary = take(in_acquire(cs(&format!(r#"{{"name":"{name}"}}"#)).as_ptr()));
    assert!(primary.contains(r#""role":"primary""#), "{primary}");
    let token = token_of(&primary);

    // Second acquire with args forwards them and is told so.
    let secondary = take(in_acquire(
        cs(&format!(
            r#"{{"name":"{name}","forward_args":["opened.txt",42]}}"#
        ))
        .as_ptr(),
    ));
    assert_eq!(secondary, r#"{"role":"secondary","forwarded":true}"#);

    // A third acquire without args forwards nothing.
    let third = take(in_acquire(cs(&format!(r#"{{"name":"{name}"}}"#)).as_ptr()));
    assert_eq!(third, r#"{"role":"secondary","forwarded":false}"#);

    // The primary polls the forwarded args back, verbatim.
    let got = take(in_poll(
        cs(&format!(r#"{{"token":{token},"timeout_ms":5000}}"#)).as_ptr(),
    ));
    assert_eq!(got, r#"{"args":["opened.txt",42]}"#);

    // Then the queue is empty: a short poll times out to null.
    let empty = take(in_poll(
        cs(&format!(r#"{{"token":{token},"timeout_ms":100}}"#)).as_ptr(),
    ));
    assert_eq!(empty, r#"{"args":null}"#);

    // Release tears it down; a second release is NOT_FOUND.
    assert_eq!(
        in_release(cs(&format!(r#"{{"token":{token}}}"#)).as_ptr()),
        0
    );
    assert_eq!(
        in_release(cs(&format!(r#"{{"token":{token}}}"#)).as_ptr()),
        3
    );

    // With the name freed, the next acquire is primary again.
    let again = take(in_acquire(cs(&format!(r#"{{"name":"{name}"}}"#)).as_ptr()));
    assert!(again.contains(r#""role":"primary""#), "{again}");
    let token2 = token_of(&again);
    assert_eq!(
        in_release(cs(&format!(r#"{{"token":{token2}}}"#)).as_ptr()),
        0
    );
}
