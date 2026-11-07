//! The real battery state from `/sys/class/power_supply` (Model B / plain Linux), pushed to
//! the companion as `BatteryStatus` and reported in `Status`.
//!
//! A power supply whose `type` is `Battery` gives the charge (`capacity`, 0-100). The charging
//! flag means a cable is connected: `online` of another supply, else `status` is `Charging`/`Full`. The root is injected so the daemon's tests
//! read a fake sysfs instead of the host's. Best effort: unreadable or absent files yield
//! `None`, never an error.

use std::path::Path;

/// Default sysfs root for power supplies.
pub const DEFAULT_ROOT: &str = "/sys/class/power_supply";

/// The battery charge (0-100) and whether it is charging, read from the first `Battery` supply
/// under `root`. `None` if there is no readable battery.
pub fn read(root: &Path) -> Option<(u8, bool)> {
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let dir = entry.path();
        let kind = read_trim(&dir.join("type"));
        if kind.as_deref() != Some("Battery") {
            continue;
        }
        let Some(percent) = read_trim(&dir.join("capacity")).and_then(|s| s.parse::<u8>().ok())
        else {
            continue;
        };
        // The fuel gauge's status follows the current, so a cable that barely covers the load
        // reads Discharging. The supply says whether a cable is there; the status is the fallback
        // for a system without one.
        let charging = cable_reported(root).unwrap_or_else(|| {
            matches!(
                read_trim(&dir.join("status")).as_deref(),
                Some("Charging") | Some("Full")
            )
        });
        return Some((percent.min(100), charging));
    }
    None
}

/// Whether a cable is connected: a supply other than the battery reports `online`. The battery's
/// `status` cannot tell, since on the fuel gauge it follows the charge and the current, not the
/// cable: a Glass just unplugged at full charge still reads `Full`. If no supply reports `online`
/// the answer is unknown, and this says yes: that is the case where nothing drastic should follow.
pub fn cable_connected(root: &Path) -> bool {
    cable_reported(root).unwrap_or(true)
}

/// Whether a supply other than the battery says a cable is there; `None` if none reports it.
fn cable_reported(root: &Path) -> Option<bool> {
    let online: Vec<String> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|dir| read_trim(&dir.join("type")).as_deref() != Some("Battery"))
        .filter_map(|dir| read_trim(&dir.join("online")))
        .collect();
    (!online.is_empty()).then(|| online.iter().any(|o| o == "1"))
}

fn read_trim(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn the_charging_flag_follows_the_cable_when_a_supply_reports_it() {
        let root = dir("flag");
        std::fs::create_dir_all(&root).unwrap();
        supply(
            &root,
            "bq27520-0",
            "Battery",
            &[("capacity", "96"), ("status", "Discharging")],
        );
        assert_eq!(read(&root), Some((96, false)));
        supply(&root, "twl6030_usb", "USB", &[("online", "1")]);
        assert_eq!(read(&root), Some((96, true)));
        // Just unplugged at full charge: the gauge still says Full.
        supply(
            &root,
            "bq27520-0",
            "Battery",
            &[("capacity", "100"), ("status", "Full")],
        );
        supply(&root, "twl6030_usb", "USB", &[("online", "0")]);
        assert_eq!(read(&root), Some((100, false)));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_cable_is_the_online_of_a_supply_not_the_battery_status() {
        let root = dir("cable");
        std::fs::create_dir_all(&root).unwrap();
        supply(&root, "bq27520-0", "Battery", &[("status", "Full")]);
        // Nothing reports it: unknown counts as connected.
        assert!(cable_connected(&root));
        supply(&root, "twl6030_usb", "USB", &[("online", "0")]);
        assert!(!cable_connected(&root));
        supply(&root, "twl6030_usb", "USB", &[("online", "1")]);
        assert!(cable_connected(&root));
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("glassd-battery-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn supply(root: &Path, name: &str, kind: &str, fields: &[(&str, &str)]) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("type"), format!("{kind}\n")).unwrap();
        for (k, v) in fields {
            std::fs::write(d.join(k), format!("{v}\n")).unwrap();
        }
    }

    #[test]
    fn reads_the_battery_skipping_other_supplies() {
        let root = dir("read");
        std::fs::create_dir_all(&root).unwrap();
        supply(&root, "usb", "USB", &[("online", "0")]);
        supply(
            &root,
            "battery",
            "Battery",
            &[("capacity", "73"), ("status", "Discharging")],
        );
        assert_eq!(read(&root), Some((73, false)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn charging_and_full_are_charging() {
        let root = dir("charging");
        std::fs::create_dir_all(&root).unwrap();
        supply(
            &root,
            "bat",
            "Battery",
            &[("capacity", "100"), ("status", "Full")],
        );
        assert_eq!(read(&root), Some((100, true)));
        std::fs::write(root.join("bat/status"), "Charging\n").unwrap();
        assert_eq!(read(&root), Some((100, true)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_or_empty_is_none() {
        assert_eq!(read(Path::new("/no/such/power_supply")), None);
        let root = dir("empty");
        std::fs::create_dir_all(&root).unwrap();
        supply(&root, "usb", "USB", &[("online", "1")]);
        assert_eq!(read(&root), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
