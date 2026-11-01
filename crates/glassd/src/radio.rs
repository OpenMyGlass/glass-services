//! Radio control for Model B (plain Linux): Wi-Fi and Bluetooth on/off through the system
//! tools on the Alpine image (docs/api.md "Settings", the `radio` capability).
//!
//! glassd does not own the radio stack; it switches the hardware and the kernel interface and
//! leaves association (Wi-Fi) and pairing (Bluetooth) to the userspace that owns them:
//!
//! - **Wi-Fi:** `rfkill unblock wifi` then `ip link set <iface> up` to enable; `ip link set
//!   <iface> down` then `rfkill block wifi` to disable. Association (wpa_supplicant/iwd +
//!   udhcpc) is the companion/QR-setup flow and is not started here; [`Radio::wifi_supplicant`]
//!   reports which supplicant is installed so that flow can use it. The default is Wi-Fi off,
//!   brought up on demand for big transfers or as a fallback.
//! - **Bluetooth:** `rfkill unblock bluetooth` then bring the HCI controller up (`hciconfig
//!   <hci> up`, else `bluetoothctl power on`); the reverse to disable. Pairing/bonding is
//!   bluez's (`/var/lib/bluetooth`); glassd only switches the radio.
//!
//! The commands are best effort: a missing tool or a non-zero exit is logged, never fatal, so
//! the daemon keeps running on an image that wires the radios differently. The `rfkill` step
//! is the authoritative on/off; the interface steps follow it.
//!
//! The runner is injected so the daemon's tests (and `--mock`) exercise this without touching
//! the host's radios: [`Radio::system`] spawns real processes, [`Radio::recording`] only
//! records the commands.

use crate::{info, warn};
use std::io;
use std::process::Command;

/// Default Wi-Fi interface on the Glass image (BCM4330, `fw_bcmdhd`).
pub const DEFAULT_WIFI_IFACE: &str = "wlan0";
/// Default Bluetooth HCI device (BCM4330 over the HCI UART).
pub const DEFAULT_BT_HCI: &str = "hci0";

/// What one system command produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The process exited 0.
    pub ok: bool,
    /// Its standard output, trimmed (for `rfkill list` and the like).
    pub stdout: String,
}

/// Runs a system command `program args...`. An `Err` means the process could not be run at all
/// (the tool is absent); `Ok` with `ok = false` means it ran and failed.
pub type Runner = Box<dyn FnMut(&str, &[&str]) -> io::Result<Outcome> + Send>;

/// Switches the Wi-Fi and Bluetooth radios.
pub struct Radio {
    run: Runner,
    wifi_iface: String,
    bt_hci: String,
    /// The last states glassd drove the radios to (its view, not a re-query).
    wifi_on: Option<bool>,
    bt_on: Option<bool>,
}

impl std::fmt::Debug for Radio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Radio")
            .field("wifi_iface", &self.wifi_iface)
            .field("bt_hci", &self.bt_hci)
            .field("wifi_on", &self.wifi_on)
            .field("bt_on", &self.bt_on)
            .finish()
    }
}

fn run_system(program: &str, args: &[&str]) -> io::Result<Outcome> {
    let out = Command::new(program).args(args).output()?;
    Ok(Outcome {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
    })
}

impl Radio {
    /// A radio that runs the real system tools, controlling `wifi_iface` (e.g. `wlan0`).
    pub fn system(wifi_iface: impl Into<String>) -> Radio {
        Radio::with_runner(wifi_iface, Box::new(run_system))
    }

    /// A radio whose commands are only recorded, never run (for `--mock` and tests). Every
    /// command "succeeds" with empty output.
    pub fn recording(
        wifi_iface: impl Into<String>,
        log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> Radio {
        Radio::with_runner(
            wifi_iface,
            Box::new(move |p, a| {
                log.lock().unwrap_or_else(|e| e.into_inner()).push(
                    std::iter::once(p)
                        .chain(a.iter().copied())
                        .collect::<Vec<_>>()
                        .join(" "),
                );
                Ok(Outcome {
                    ok: true,
                    stdout: String::new(),
                })
            }),
        )
    }

    /// A radio with a custom runner.
    pub fn with_runner(wifi_iface: impl Into<String>, run: Runner) -> Radio {
        Radio {
            run,
            wifi_iface: wifi_iface.into(),
            bt_hci: DEFAULT_BT_HCI.into(),
            wifi_on: None,
            bt_on: None,
        }
    }

    /// Runs one command; logs and returns false if the tool is absent or the command failed.
    fn step(&mut self, program: &str, args: &[&str]) -> bool {
        match (self.run)(program, args) {
            Ok(o) if o.ok => true,
            Ok(o) => {
                warn!(
                    "radio: `{program} {}` failed{}",
                    args.join(" "),
                    if o.stdout.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", o.stdout)
                    }
                );
                false
            }
            Err(e) => {
                warn!("radio: cannot run `{program}` ({e})");
                false
            }
        }
    }

    /// Enables or disables Wi-Fi. Returns whether the authoritative rfkill step succeeded.
    pub fn set_wifi(&mut self, on: bool) -> bool {
        let iface = self.wifi_iface.clone();
        let ok = if on {
            let r = self.step("rfkill", &["unblock", "wifi"]);
            // Bring the link up; association is left to the supplicant (companion/QR flow).
            self.step("ip", &["link", "set", &iface, "up"]);
            r
        } else {
            self.step("ip", &["link", "set", &iface, "down"]);
            self.step("rfkill", &["block", "wifi"])
        };
        self.wifi_on = Some(on);
        info!("radio: Wi-Fi {}", if on { "on" } else { "off" });
        ok
    }

    /// Enables or disables Bluetooth. Returns whether the authoritative rfkill step succeeded.
    pub fn set_bluetooth(&mut self, on: bool) -> bool {
        let hci = self.bt_hci.clone();
        let ok = if on {
            let r = self.step("rfkill", &["unblock", "bluetooth"]);
            // Bring the HCI controller up: hciconfig if present, else bluetoothctl.
            if !self.step("hciconfig", &[&hci, "up"]) {
                self.step("bluetoothctl", &["power", "on"]);
            }
            r
        } else {
            if !self.step("hciconfig", &[&hci, "down"]) {
                self.step("bluetoothctl", &["power", "off"]);
            }
            self.step("rfkill", &["block", "bluetooth"])
        };
        self.bt_on = Some(on);
        info!("radio: Bluetooth {}", if on { "on" } else { "off" });
        ok
    }

    /// Brings both radios to the wanted state, writing only what changed since the last call.
    /// `wifi` and `bluetooth` are the effective states (airplane mode already folded in).
    pub fn apply(&mut self, wifi: bool, bluetooth: bool) {
        if self.wifi_on != Some(wifi) {
            self.set_wifi(wifi);
        }
        if self.bt_on != Some(bluetooth) {
            self.set_bluetooth(bluetooth);
        }
    }

    /// Brings Wi-Fi up and joins the network described by a QR bootstrap (docs/api.md, the
    /// companion link). The supplicant config `conf` is [`crate::companion::bootstrap::
    /// wpa_supplicant_conf`]; it is written to `conf_path`, then `wpa_supplicant` is started on
    /// the interface and `udhcpc` asks for a lease. Association is otherwise left to the
    /// supplicant, as [`Radio`] documents. Best effort: a missing tool is logged, not fatal.
    /// Returns whether Wi-Fi was enabled (the rfkill step); association runs regardless.
    pub fn join_wifi(&mut self, conf: &str, conf_path: &std::path::Path) -> bool {
        let enabled = self.set_wifi(true);
        if let Err(e) = std::fs::write(conf_path, conf) {
            warn!("radio: cannot write {} ({e})", conf_path.display());
        }
        let iface = self.wifi_iface.clone();
        let conf_arg = conf_path.to_string_lossy().into_owned();
        // A previous supplicant on this interface would hold the control socket.
        self.step("wpa_supplicant", &["-B", "-i", &iface, "-c", &conf_arg]);
        self.step("udhcpc", &["-i", &iface, "-n"]);
        info!("radio: joining Wi-Fi on {iface}");
        enabled
    }

    /// The Wi-Fi supplicant installed on the image, if glassd can see one (`wpa_supplicant` or
    /// `iwd`), for the association flow to use. Best effort; `None` if neither is found.
    pub fn wifi_supplicant(&mut self) -> Option<&'static str> {
        for s in ["wpa_supplicant", "iwd"] {
            // `command -v` via the tool itself: a `--version`/`-h` probe would run it; instead
            // check the usual locations without spawning anything heavy.
            for dir in ["/sbin", "/usr/sbin", "/bin", "/usr/bin"] {
                if std::path::Path::new(dir).join(s).exists() {
                    return Some(s);
                }
            }
        }
        None
    }

    /// glassd's view of the current radio states (what it last drove them to).
    pub fn state(&self) -> (Option<bool>, Option<bool>) {
        (self.wifi_on, self.bt_on)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn recording() -> (Radio, Arc<Mutex<Vec<String>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        (Radio::recording(DEFAULT_WIFI_IFACE, log.clone()), log)
    }

    fn cmds(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        std::mem::take(&mut *log.lock().unwrap())
    }

    #[test]
    fn wifi_on_unblocks_then_brings_the_link_up() {
        let (mut r, log) = recording();
        assert!(r.set_wifi(true));
        assert_eq!(
            cmds(&log),
            vec!["rfkill unblock wifi", "ip link set wlan0 up"]
        );
        assert_eq!(r.state().0, Some(true));
    }

    #[test]
    fn wifi_off_brings_the_link_down_then_blocks() {
        let (mut r, log) = recording();
        r.set_wifi(false);
        assert_eq!(
            cmds(&log),
            vec!["ip link set wlan0 down", "rfkill block wifi"]
        );
    }

    #[test]
    fn bluetooth_on_unblocks_then_raises_the_controller() {
        let (mut r, log) = recording();
        r.set_bluetooth(true);
        // hciconfig "succeeds" in the recording runner, so bluetoothctl is not reached.
        assert_eq!(
            cmds(&log),
            vec!["rfkill unblock bluetooth", "hciconfig hci0 up"]
        );
    }

    #[test]
    fn apply_writes_only_changes() {
        let (mut r, log) = recording();
        r.apply(false, false);
        let first = cmds(&log);
        assert!(first.iter().any(|c| c == "rfkill block wifi"));
        assert!(first.iter().any(|c| c == "rfkill block bluetooth"));
        // Same state again: nothing runs.
        r.apply(false, false);
        assert!(cmds(&log).is_empty());
        // Wi-Fi on only: just the Wi-Fi commands.
        r.apply(true, false);
        let c = cmds(&log);
        assert!(c.iter().any(|c| c == "rfkill unblock wifi"));
        assert!(c.iter().all(|c| !c.contains("bluetooth")));
    }

    #[test]
    fn a_failing_rfkill_is_reported_not_fatal() {
        // A runner where rfkill is absent (Err) and ip fails (ok=false): no panic, apply still
        // records the wanted state.
        let run: Runner = Box::new(|p, _a| {
            if p == "rfkill" {
                Err(io::Error::new(io::ErrorKind::NotFound, "no rfkill"))
            } else {
                Ok(Outcome {
                    ok: false,
                    stdout: "down".into(),
                })
            }
        });
        let mut r = Radio::with_runner(DEFAULT_WIFI_IFACE, run);
        assert!(!r.set_wifi(true));
        assert_eq!(r.state().0, Some(true));
    }
}
