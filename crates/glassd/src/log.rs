//! Minimal logging to standard error.

use std::sync::atomic::{AtomicBool, Ordering};

static VERBOSE: AtomicBool = AtomicBool::new(false);

pub fn set_verbose(v: bool) {
    VERBOSE.store(v, Ordering::Relaxed);
}

pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// Logs when `--verbose` is on.
#[macro_export]
macro_rules! info {
    ($($a:tt)*) => {
        if $crate::log::verbose() {
            eprintln!("glassd: {}", format_args!($($a)*))
        }
    };
}

/// Always logs.
#[macro_export]
macro_rules! warn {
    ($($a:tt)*) => {
        eprintln!("glassd: warning: {}", format_args!($($a)*))
    };
}
