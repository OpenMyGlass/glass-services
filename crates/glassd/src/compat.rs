//! Two libc functions that Rust's standard library links against and that Android 4.4
//! (API 19, NDK r25c) does not export: `signal` (an inline function in the API 19 headers) and
//! `dl_iterate_phdr` (exported from API 21). Defined here, they make one binary for API 19 that
//! also runs on 6.0. Built only for Android.

use std::ffi::{c_int, c_void};

/// `signal` through `sigaction` (exported at API 19), with `SA_RESTART` like bionic's own `signal`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn signal(sig: c_int, handler: libc::sighandler_t) -> libc::sighandler_t {
    // SAFETY: zeroed is a valid sigaction; the pointers are valid for the call.
    unsafe {
        let mut new: libc::sigaction = std::mem::zeroed();
        let mut old: libc::sigaction = std::mem::zeroed();
        new.sa_sigaction = handler;
        new.sa_flags = libc::SA_RESTART;
        // The mask stays empty (zeroed): sigemptyset is an inline function at API 19 too.
        if libc::sigaction(sig, &new, &mut old) != 0 {
            return libc::SIG_ERR;
        }
        old.sa_sigaction
    }
}

/// Used by the standard library only to symbolize panic backtraces: report no objects, so a
/// backtrace shows addresses without names.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dl_iterate_phdr(
    _callback: Option<unsafe extern "C" fn(*mut c_void, usize, *mut c_void) -> c_int>,
    _data: *mut c_void,
) -> c_int {
    0
}
