//! Minimal logging to standard error, and on Android also to the log buffer (`logcat`, tag
//! `glassd`), since init sends a service's standard error to `/dev/null`.

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
    #[cfg(target_os = "android")]
    android::write(&args.to_string());
}

#[cfg(target_os = "android")]
mod android {
    use std::ffi::{CString, c_char, c_int};

    #[link(name = "log")]
    unsafe extern "C" {
        fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }

    /// `ANDROID_LOG_INFO`, or `ANDROID_LOG_WARN` for warnings.
    pub fn write(line: &str) {
        let prio = if line.contains("warning:") { 5 } else { 4 };
        let text = line.strip_prefix("glassd: ").unwrap_or(line);
        let Ok(text) = CString::new(text.replace('\0', " ")) else {
            return;
        };
        // SAFETY: both strings are valid and NUL-terminated for the duration of the call.
        unsafe { __android_log_write(prio, c"glassd".as_ptr(), text.as_ptr()) };
    }
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
