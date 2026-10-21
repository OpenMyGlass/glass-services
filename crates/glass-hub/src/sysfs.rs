//! The glasshub kernel driver's sysfs interface (docs/specs/kernel-interfaces.md §2,
//! docs/specs/glasshub.md §4-5, §8).
//!
//! - Presence: the attribute `version` exists in the device directory.
//! - Events: `don_doff` and `wink` are notified with `sysfs_notify`; poll an open descriptor
//!   of each for `POLLPRI | POLLERR` ([`POLL_EVENTS`]), then re-read it from offset 0, which
//!   also re-arms the notification. Reading `wink` acknowledges the interrupt.
//! - Writes are ASCII decimal. Only the attributes in [`WRITABLE`] can be written through
//!   this type: never `calibrate` (it rewrites the LED calibration in the hub's flash),
//!   `update_fw_*` (firmware update), or anything else.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};

/// The glasshub I2C client (bus 4, address 0x35).
pub const DEFAULT_ROOT: &str = "/sys/bus/i2c/devices/4-0035";

/// `poll` events for the notified attributes.
pub const POLL_EVENTS: libc::c_short = libc::POLLPRI | libc::POLLERR;

/// The only attributes this crate writes (docs/specs/glasshub.md §4, §8).
pub const WRITABLE: &[&str] = &[
    "don_doff_enable",
    "wink_enable",
    "wink_flag_enable",
    "pause",
    "don_threshold",
    "doff_threshold",
    "don_max_threshold",
    "detector_gain",
    "wink_min",
    "wink_max",
];

/// Firmware thresholds of the on-head and wink detectors. They are plain register writes,
/// not kept in the hub's flash, so they are written at every start (glasshub.md §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    pub don: u32,
    pub doff: u32,
    pub don_max: u32,
    pub detector_gain: u32,
    /// Wink magnitude window; only written when known (from a calibration).
    pub wink_min: Option<u32>,
    pub wink_max: Option<u32>,
}

impl Default for Thresholds {
    /// The uncalibrated defaults (glasshub.md §6.3): don 450, doff 400, don max 2000, gain 2.
    fn default() -> Self {
        Thresholds {
            don: 450,
            doff: 400,
            don_max: 2000,
            detector_gain: 2,
            wink_min: None,
            wink_max: None,
        }
    }
}

/// Where the stock system keeps a user's calibration (glasshub.md §6.6).
pub const STOCK_PARAMS: &str = "/data/system/gaze/gaze_params.txt";

impl Thresholds {
    /// Reads a stock parameter file (`KEY = VALUE` lines, `#` comments): `GAIN`,
    /// `DON_THRESH`, `DOFF_THRESH`, `DON_MAX_THRESH`, `WINK_MIN`, `WINK_MAX`. Missing or bad
    /// values keep the defaults; values are clamped to the driver's ranges.
    pub fn parse_params(text: &str) -> Thresholds {
        let mut t = Thresholds::default();
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            // Values may be written as floats ("2.000000").
            let Some(v) = v
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v >= 0.0)
            else {
                continue;
            };
            let v = v.round().min(u32::MAX as f64) as u32;
            match k.trim() {
                "GAIN" => t.detector_gain = v.min(2),
                "DON_THRESH" => t.don = v.clamp(1, 10_000),
                "DOFF_THRESH" => t.doff = v.min(10_000),
                "DON_MAX_THRESH" => t.don_max = v.clamp(1, 10_000),
                "WINK_MIN" => t.wink_min = Some(v.clamp(1, 2000)),
                "WINK_MAX" => t.wink_max = Some(v.clamp(1, 2000)),
                _ => {}
            }
        }
        t
    }
}

/// An open glasshub.
#[derive(Debug)]
pub struct GlassHub {
    root: PathBuf,
    don_doff: File,
    wink: File,
}

fn read_from_start(f: &mut File) -> io::Result<String> {
    f.seek(SeekFrom::Start(0))?;
    let mut buf = [0u8; 64];
    let n = f.read(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf[..n]).trim().to_string())
}

impl GlassHub {
    /// Opens the glasshub under `root`; `Ok(None)` if there is none (no `version`).
    pub fn open(root: impl AsRef<Path>) -> io::Result<Option<GlassHub>> {
        let root = root.as_ref();
        if !root.join("version").exists() {
            return Ok(None);
        }
        let mut hub = GlassHub {
            root: root.to_path_buf(),
            don_doff: File::open(root.join("don_doff"))?,
            wink: File::open(root.join("wink"))?,
        };
        // The first read arms the notifications.
        hub.ack_wink()?;
        hub.read_don_doff()?;
        Ok(Some(hub))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The MCU firmware version (`"major.minor"`), for the log.
    pub fn version(&self) -> io::Result<String> {
        Ok(std::fs::read_to_string(self.root.join("version"))?
            .trim()
            .to_string())
    }

    /// Writes one attribute of [`WRITABLE`]; any other name is refused.
    pub fn write_attr(&self, name: &str, value: u32) -> io::Result<()> {
        if !WRITABLE.contains(&name) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("glasshub attribute {name:?} is not written by glassd"),
            ));
        }
        let mut f = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(self.root.join(name))?;
        f.write_all(format!("{value}\n").as_bytes())
    }

    /// Arms or disarms the firmware's on-head detection.
    pub fn set_on_head_detection(&self, on: bool) -> io::Result<()> {
        self.write_attr("don_doff_enable", on as u32)
    }

    /// Arms or disarms the low-power wink interrupt (`wink_enable` and `wink_flag_enable`).
    pub fn set_wink(&self, on: bool) -> io::Result<()> {
        self.write_attr("wink_enable", on as u32)?;
        self.write_attr("wink_flag_enable", on as u32)
    }

    /// Pauses the hub (while the Glass is forced off the head).
    pub fn set_pause(&self, on: bool) -> io::Result<()> {
        self.write_attr("pause", on as u32)
    }

    /// Writes the detector thresholds. Call with on-head detection disabled.
    pub fn apply_thresholds(&self, t: &Thresholds) -> io::Result<()> {
        self.write_attr("don_threshold", t.don)?;
        self.write_attr("doff_threshold", t.doff)?;
        self.write_attr("don_max_threshold", t.don_max)?;
        self.write_attr("detector_gain", t.detector_gain)?;
        if let Some(v) = t.wink_min {
            self.write_attr("wink_min", v)?;
        }
        if let Some(v) = t.wink_max {
            self.write_attr("wink_max", v)?;
        }
        Ok(())
    }

    /// Re-reads `don_doff`: `Some(true)` on the head (1), `Some(false)` off (0), `None` for
    /// any other value (65535 is an I2C failure).
    pub fn read_don_doff(&mut self) -> io::Result<Option<bool>> {
        Ok(match read_from_start(&mut self.don_doff)?.as_str() {
            "1" => Some(true),
            "0" => Some(false),
            _ => None,
        })
    }

    /// Re-reads `wink`, which acknowledges the interrupt; returns its value (0 if unreadable).
    pub fn ack_wink(&mut self) -> io::Result<u32> {
        Ok(read_from_start(&mut self.wink)?.parse().unwrap_or(0))
    }

    pub fn don_doff_fd(&self) -> RawFd {
        self.don_doff.as_raw_fd()
    }

    pub fn wink_fd(&self) -> RawFd {
        self.wink.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("glass-hub-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for (f, v) in [("version", "1.14\n"), ("don_doff", "1\n"), ("wink", "0\n")] {
            std::fs::write(d.join(f), v).unwrap();
        }
        for f in WRITABLE.iter().chain(&["calibrate", "update_fw_enable"]) {
            std::fs::write(d.join(f), "").unwrap();
        }
        d
    }

    fn read(d: &Path, f: &str) -> String {
        std::fs::read_to_string(d.join(f)).unwrap()
    }

    #[test]
    fn absent_without_version() {
        let d = fake("absent");
        std::fs::remove_file(d.join("version")).unwrap();
        assert!(GlassHub::open(&d).unwrap().is_none());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn reads_don_doff_again_after_each_change() {
        let d = fake("dondoff");
        let mut h = GlassHub::open(&d).unwrap().unwrap();
        assert_eq!(h.version().unwrap(), "1.14");
        assert_eq!(h.read_don_doff().unwrap(), Some(true));
        std::fs::write(d.join("don_doff"), "0\n").unwrap();
        assert_eq!(h.read_don_doff().unwrap(), Some(false));
        std::fs::write(d.join("don_doff"), "65535\n").unwrap();
        assert_eq!(h.read_don_doff().unwrap(), None);
        std::fs::write(d.join("wink"), "2\n").unwrap();
        assert_eq!(h.ack_wink().unwrap(), 2);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn writes_only_allowed_attributes() {
        let d = fake("writes");
        let h = GlassHub::open(&d).unwrap().unwrap();
        h.set_on_head_detection(true).unwrap();
        h.set_wink(true).unwrap();
        h.set_pause(false).unwrap();
        assert_eq!(read(&d, "don_doff_enable"), "1\n");
        assert_eq!(read(&d, "wink_enable"), "1\n");
        assert_eq!(read(&d, "wink_flag_enable"), "1\n");
        assert_eq!(read(&d, "pause"), "0\n");
        for bad in [
            "calibrate",
            "update_fw_enable",
            "update_fw_data",
            "passthru_enable",
            "led_drive",
            "../x",
        ] {
            let e = h.write_attr(bad, 1).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{bad}");
        }
        assert_eq!(read(&d, "calibrate"), "");
        assert_eq!(read(&d, "update_fw_enable"), "");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn thresholds_defaults_and_params_file() {
        let d = fake("thresholds");
        let h = GlassHub::open(&d).unwrap().unwrap();
        h.apply_thresholds(&Thresholds::default()).unwrap();
        assert_eq!(read(&d, "don_threshold"), "450\n");
        assert_eq!(read(&d, "doff_threshold"), "400\n");
        assert_eq!(read(&d, "don_max_threshold"), "2000\n");
        assert_eq!(read(&d, "detector_gain"), "2\n");
        assert_eq!(read(&d, "wink_min"), "");
        let t = Thresholds::parse_params(
            "# gaze parameters\nVERSION = 3.0\nGAIN = 1\nDON_THRESH = 520\nDOFF_THRESH=380\n\
             DON_MAX_THRESH = 99999\nWINK_MIN = 30.4\nWINK_MAX = x\nOTHER = 5\n",
        );
        assert_eq!(
            t,
            Thresholds {
                don: 520,
                doff: 380,
                don_max: 10_000,
                detector_gain: 1,
                wink_min: Some(30),
                wink_max: None,
            }
        );
        h.apply_thresholds(&t).unwrap();
        assert_eq!(read(&d, "wink_min"), "30\n");
        assert_eq!(read(&d, "wink_max"), "");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
