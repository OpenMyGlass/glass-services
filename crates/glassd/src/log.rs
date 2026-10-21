//! Minimal logging to standard error.

use std::sync::atomic::{AtomicBool, Ordering};

static VERBOSE: AtomicBool = AtomicBool::new(false);

pub fn set_verbose(v: bool) {
    VERBOSE.store(v, Ordering::Relaxed);
}

pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// Writes one line to standard error; errors (a closed stderr) are ignored, never a panic.
pub fn write(args: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr().lock(), "{args}");
}

/// Logs when `--verbose` is on.
#[macro_export]
macro_rules! info {
    ($($a:tt)*) => {
        if $crate::log::verbose() {
            $crate::log::write(format_args!("glassd: {}", format_args!($($a)*)))
        }
    };
}

/// Always logs.
#[macro_export]
macro_rules! warn {
    ($($a:tt)*) => {
        $crate::log::write(format_args!("glassd: warning: {}", format_args!($($a)*)))
    };
}
