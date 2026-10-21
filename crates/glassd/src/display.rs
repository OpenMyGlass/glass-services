//! Display and touchpad power back ends (docs/specs/kernel-interfaces.md §1.6, §4.3).
//!
//! - [`FbDisplay`]: `FBIOBLANK` on the panel's framebuffer: `FB_BLANK_UNBLANK` for on and
//!   dim, `FB_BLANK_POWERDOWN` for off. The dim step does not change the panel: brightness is
//!   left alone (brightness, `brightness_limit` and `brightness_multiplier` are never
//!   written by glassd).
//! - [`LoggingDisplay`]: logs only (the mock, tests, `--display log`).
//! - [`TouchpadSleep`]: the touchpad's `sleepmode` (1 off the head, 0 on the head).

use glass_api::proto::Screen;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

/// Turns the panel on, dims it, or turns it off.
pub trait Display {
    fn set(&mut self, screen: Screen);
}

/// Does nothing but log the requested state.
#[derive(Debug, Default)]
pub struct LoggingDisplay;

impl Display for LoggingDisplay {
    fn set(&mut self, screen: Screen) {
        crate::info!("display -> {screen:?} (logging back end, nothing changed)");
    }
}

/// `FBIOBLANK` (linux/fb.h).
pub const FBIOBLANK: u32 = 0x4611;
pub const FB_BLANK_UNBLANK: libc::c_int = 0;
pub const FB_BLANK_POWERDOWN: libc::c_int = 4;

/// The panel framebuffer: `/dev/graphics/fb0` on Android, `/dev/fb0` elsewhere.
pub fn default_fb() -> PathBuf {
    if cfg!(target_os = "android") {
        "/dev/graphics/fb0".into()
    } else {
        "/dev/fb0".into()
    }
}

/// The `FBIOBLANK` argument for a screen state (`None`: leave the panel as it is).
pub fn blank_arg(screen: Screen) -> Option<libc::c_int> {
    match screen {
        Screen::On | Screen::Dim => Some(FB_BLANK_UNBLANK),
        Screen::Off => Some(FB_BLANK_POWERDOWN),
        Screen::Unknown => None,
    }
}

/// Blanks and unblanks a framebuffer.
#[derive(Debug)]
pub struct FbDisplay {
    file: File,
    path: PathBuf,
    current: Option<libc::c_int>,
}

impl FbDisplay {
    pub fn open(path: impl AsRef<Path>) -> io::Result<FbDisplay> {
        let path = path.as_ref();
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(FbDisplay {
            file,
            path: path.to_path_buf(),
            current: None,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn blank(&mut self, arg: libc::c_int) -> io::Result<()> {
        // SAFETY: FBIOBLANK takes its argument by value; the descriptor is open.
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), FBIOBLANK as _, arg as libc::c_long) };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Display for FbDisplay {
    fn set(&mut self, screen: Screen) {
        let Some(arg) = blank_arg(screen) else { return };
        if self.current == Some(arg) {
            return;
        }
        match self.blank(arg) {
            Ok(()) => {
                self.current = Some(arg);
                crate::info!("display -> {screen:?} (FBIOBLANK {arg})");
            }
            Err(e) => crate::warn!("FBIOBLANK {arg} on {}: {e}", self.path.display()),
        }
    }
}

/// The touchpad's F11 function in sysfs.
pub const TOUCHPAD_SYSFS: &str = "/sys/bus/rmi/devices/touchpad.fn11";

/// Writes the touchpad's `sleepmode`. Without a directory it only logs.
#[derive(Debug, Default)]
pub struct TouchpadSleep {
    dir: Option<PathBuf>,
    current: Option<bool>,
}

impl TouchpadSleep {
    pub fn new(dir: Option<PathBuf>) -> Self {
        TouchpadSleep { dir, current: None }
    }

    /// `true` puts the touchpad to sleep (off the head), `false` wakes it.
    pub fn set(&mut self, sleep: bool) {
        if self.current == Some(sleep) {
            return;
        }
        self.current = Some(sleep);
        let Some(dir) = &self.dir else {
            crate::info!("touchpad sleepmode -> {} (not written)", sleep as u8);
            return;
        };
        let p = dir.join("sleepmode");
        let r = OpenOptions::new()
            .write(true)
            .open(&p)
            .and_then(|mut f| f.write_all(if sleep { b"1\n" } else { b"0\n" }));
        match r {
            Ok(()) => crate::info!("touchpad sleepmode -> {}", sleep as u8),
            Err(e) => crate::warn!("cannot write {}: {e}", p.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_arguments() {
        assert_eq!(blank_arg(Screen::On), Some(0));
        assert_eq!(blank_arg(Screen::Dim), Some(0));
        assert_eq!(blank_arg(Screen::Off), Some(4));
        assert_eq!(blank_arg(Screen::Unknown), None);
    }

    #[test]
    fn fb_on_a_regular_file_fails_quietly() {
        let p = std::env::temp_dir().join(format!("glassd-fb-{}", std::process::id()));
        std::fs::write(&p, b"").unwrap();
        let mut d = FbDisplay::open(&p).unwrap();
        d.set(Screen::Off); // ENOTTY: logged, no panic
        assert_eq!(d.current, None);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn touchpad_sleepmode_is_written_on_change() {
        let d = std::env::temp_dir().join(format!("glassd-tp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("sleepmode"), b"").unwrap();
        let mut t = TouchpadSleep::new(Some(d.clone()));
        t.set(true);
        assert_eq!(std::fs::read(d.join("sleepmode")).unwrap(), b"1\n");
        t.set(false);
        assert_eq!(std::fs::read(d.join("sleepmode")).unwrap(), b"0\n");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
