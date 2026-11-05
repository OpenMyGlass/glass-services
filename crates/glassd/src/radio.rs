//! Radio control for Model B (plain Linux): Wi-Fi and Bluetooth on/off through the system
//! tools on the Alpine image (docs/api.md "Settings", the `radio` capability).
//!
//! glassd does not own the radio stack; it switches the hardware and the kernel interface and
//! leaves association (Wi-Fi) and pairing (Bluetooth) to the userspace that owns them:
//!
//! - **Wi-Fi:** `rfkill unblock wifi` then `ip link set <iface> up` to enable; `ip link set
//!   <iface> down` then `rfkill block wifi` to disable. With a supplicant config (`wifi.conf` in
//!   the state directory, or from a QR bootstrap) enabling also starts `wpa_supplicant` and
//!   `udhcpc`, which retry by themselves, and disabling stops them. The default is Wi-Fi off,
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
/// The supplicant config's name in the state directory. It holds the network's password.
pub const WIFI_CONF_FILE: &str = "wifi.conf";
const WPA_PID_FILE: &str = "/var/run/glassd-wpa_supplicant.pid";
const DHCP_PID_FILE: &str = "/var/run/glassd-udhcpc.pid";
/// The UART the Bluetooth chip is wired to.
const DEFAULT_BT_UART: &str = "/dev/ttyO1";

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
    /// Still to be kicked once after the chip is powered off (see `kick_bt_uart`).
    bt_uart: Option<String>,
    /// The supplicant config to associate with when Wi-Fi is on.
    wifi_conf: Option<std::path::PathBuf>,
    /// `wpa_supplicant` and `udhcpc` were started and not stopped yet.
    associating: bool,
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

/// Writes a supplicant config, which holds the network's password, readable only by its owner.
/// The file goes first, because opening an existing one would keep its old mode.
fn write_private(path: &std::path::Path, content: &str) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    std::fs::File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(content.as_bytes())
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
        let mut r = Radio::with_runner(wifi_iface, Box::new(run_system));
        r.bt_uart = Some(DEFAULT_BT_UART.into());
        r
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

    /// Associates with this supplicant config whenever Wi-Fi is on, if the file exists then.
    pub fn with_wifi_conf(mut self, conf: impl Into<std::path::PathBuf>) -> Radio {
        self.wifi_conf = Some(conf.into());
        self
    }

    /// A radio with a custom runner.
    pub fn with_runner(wifi_iface: impl Into<String>, run: Runner) -> Radio {
        Radio {
            run,
            wifi_iface: wifi_iface.into(),
            bt_hci: DEFAULT_BT_HCI.into(),
            bt_uart: None,
            wifi_conf: None,
            associating: false,
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
            self.step("ip", &["link", "set", &iface, "up"]);
            self.start_association();
            r
        } else {
            self.stop_association();
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
            let r = self.step("rfkill", &["block", "bluetooth"]);
            self.kick_bt_uart();
            r
        };
        self.bt_on = Some(on);
        info!("radio: Bluetooth {}", if on { "on" } else { "off" });
        ok
    }

    /// Writes one byte to the Bluetooth UART, once per run.
    ///
    /// Powering the chip off changes CTS, which wakes the port. A port that has never transmitted
    /// then idles with `CM_L4PER_UART2_CLKCTRL` IDLEST stuck at 1 (seen 3 -> 1 on a Glass, 3
    /// boots), and that keeps the CORE power domain out of deep sleep. After one transmitted byte
    /// it went back to 3 and stayed there through later block/unblock cycles. The chip is off,
    /// so nothing receives the byte.
    fn kick_bt_uart(&mut self) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let Some(path) = self.bt_uart.take() else {
            return;
        };
        let r = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
            .open(&path)
            .and_then(|mut f| f.write_all(&[0]));
        match r {
            Ok(()) => info!("radio: one byte sent on {path} so that the idle port can sleep"),
            Err(e) => warn!("radio: cannot write to {path} ({e}): deep sleep may be blocked"),
        }
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
        if let Err(e) = write_private(conf_path, conf) {
            warn!("radio: cannot write {} ({e})", conf_path.display());
        }
        // A previous supplicant on this interface would hold the control socket.
        self.stop_association();
        self.wifi_conf = Some(conf_path.to_path_buf());
        self.set_wifi(true)
    }

    /// Starts the supplicant and the DHCP client on the configured network. Both run on their
    /// own and retry, so this does not wait for the network: the daemon's loop must not stop.
    fn start_association(&mut self) {
        let Some(conf) = self.wifi_conf.clone().filter(|c| c.exists()) else {
            return;
        };
        if self.associating {
            return;
        }
        let iface = self.wifi_iface.clone();
        let conf_arg = conf.to_string_lossy().into_owned();
        self.step(
            "wpa_supplicant",
            &[
                "-B",
                "-i",
                &iface,
                "-D",
                "nl80211,wext",
                "-c",
                &conf_arg,
                "-P",
                WPA_PID_FILE,
            ],
        );
        // Through a shell, with the output closed: the runner waits for its pipe to close, and
        // udhcpc stays in the foreground until it has a lease.
        let dhcp = format!("udhcpc -b -i {iface} -p {DHCP_PID_FILE} >/dev/null 2>&1 &");
        self.step("sh", &["-c", &dhcp]);
        self.associating = true;
        info!("radio: joining Wi-Fi on {iface}");
    }

    fn stop_association(&mut self) {
        if !std::mem::take(&mut self.associating) {
            return;
        }
        let kill = format!("kill $(cat {WPA_PID_FILE} {DHCP_PID_FILE}) 2>/dev/null");
        self.step("sh", &["-c", &kill]);
    }

    /// The Wi-Fi interface's IPv4 address, if it has one.
    pub fn wifi_ip(&mut self) -> Option<String> {
        let iface = self.wifi_iface.clone();
        let out = (self.run)("ip", &["-4", "-o", "addr", "show", &iface]).ok()?;
        let after = out.stdout.split("inet ").nth(1)?;
        Some(after.split(['/', ' ']).next()?.to_string()).filter(|ip| !ip.is_empty())
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
    #[test]
    fn a_joined_network_password_is_private_even_over_an_old_file() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let conf = d.path().join("wpa.conf");
        std::fs::write(&conf, "old").unwrap();
        std::fs::set_permissions(&conf, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (mut r, _log) = with_log();
        r.join_wifi("network={}", &conf);
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), "network={}");
        let mode = std::fs::metadata(&conf).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    fn with_log() -> (Radio, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (Radio::recording("wlan0", log.clone()), log)
    }

    #[test]
    fn wifi_associates_only_with_a_config_and_stops_with_the_radio() {
        let d = tempfile::tempdir().unwrap();
        let conf = d.path().join(WIFI_CONF_FILE);
        let (r, log) = with_log();
        let mut r = r.with_wifi_conf(&conf);
        r.apply(true, false);
        assert!(
            !log.lock()
                .unwrap()
                .iter()
                .any(|c| c.contains("wpa_supplicant"))
        );
        r.apply(false, false);

        std::fs::write(&conf, "network={}").unwrap();
        log.lock().unwrap().clear();
        r.apply(true, false);
        r.apply(true, false);
        let started = log.lock().unwrap().clone();
        assert_eq!(
            started
                .iter()
                .filter(|c| c.starts_with("wpa_supplicant"))
                .count(),
            1
        );
        assert!(
            started
                .iter()
                .any(|c| c.contains("udhcpc -b -i wlan0") && c.ends_with('&'))
        );

        log.lock().unwrap().clear();
        r.apply(false, false);
        let stopped = log.lock().unwrap().clone();
        let kill = stopped.iter().position(|c| c.contains("kill")).unwrap();
        let down = stopped
            .iter()
            .position(|c| c.contains("link set wlan0 down"))
            .unwrap();
        assert!(kill < down);
    }

    #[test]
    fn reads_the_address_of_the_interface() {
        let mut r = Radio::with_runner(
            "wlan0",
            Box::new(|_, _| {
                Ok(Outcome {
                    ok: true,
                    stdout: "3: wlan0    inet 192.168.99.40/24 brd 192.168.99.255 scope global"
                        .into(),
                })
            }),
        );
        assert_eq!(r.wifi_ip().as_deref(), Some("192.168.99.40"));
        let mut none = Radio::with_runner(
            "wlan0",
            Box::new(|_, _| {
                Ok(Outcome {
                    ok: true,
                    stdout: String::new(),
                })
            }),
        );
        assert_eq!(none.wifi_ip(), None);
    }

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
