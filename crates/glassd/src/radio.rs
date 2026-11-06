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
//! - **Bluetooth, with a BLE engine configured:** `rfkill unblock bluetooth`, then `omg-bled`
//!   starts, and owns the chip and its UART while it runs. Disabling stops it first and blocks the
//!   chip after. If it dies the chip is blocked too, so a dead engine cannot hold the UART awake.
//!   See [`BleEngine`].
//! - **Bluetooth, without one:** `rfkill unblock bluetooth` then bring the HCI controller up
//!   (`hciconfig <hci> up`, else `bluetoothctl power on`); the reverse to disable. Pairing/bonding
//!   is bluez's (`/var/lib/bluetooth`); glassd only switches the radio.
//!
//! The commands are best effort: a missing tool or a non-zero exit is logged, never fatal, so
//! the daemon keeps running on an image that wires the radios differently. The `rfkill` step
//! is the authoritative on/off; the interface steps follow it.
//!
//! The runner is injected so the daemon's tests (and `--mock`) exercise this without touching
//! the host's radios: [`Radio::system`] spawns real processes, [`Radio::recording`] only
//! records the commands.

use crate::{info, warn};
use std::ffi::OsString;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

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

/// The exit code with which `omg-bled` says that trying again is pointless (no firmware).
const BLE_EXIT_CONFIG: i32 = 78;
/// Waits between tries after the engine dies, and with them the most tries. A dead engine keeps
/// the chip off in between, so that it cannot hold the deep sleep back.
const BLE_RESTART_MS: [u64; 3] = [5_000, 30_000, 120_000];
/// An engine that ran this long was fine: its next death starts counting again.
const BLE_STABLE_MS: u64 = 600_000;
/// How long to give the engine after its leash is cut, before killing it. The UART must be closed
/// before the chip goes off.
const BLE_STOP_MS: u64 = 300;

/// A running `omg-bled`.
struct BleChild {
    child: Child,
    /// The leash: its end stops the engine, also when glassd dies.
    _stdin: ChildStdin,
    stdout: ChildStdout,
    said: Vec<u8>,
    started_ms: u64,
}

/// Runs `omg-bled` as a child, whose standard output says when it is ready and whose end shows as
/// the end of that output. So this adds one descriptor to the daemon's poll set, and no timer
/// unless the engine died.
#[derive(Debug)]
pub struct BleEngine {
    command: Vec<OsString>,
    child: Option<BleChild>,
    /// It said it is advertising, and has not ended since.
    running: bool,
    /// When to try again after a death, in the daemon's monotonic milliseconds.
    retry_at_ms: Option<u64>,
    deaths: usize,
    /// It reported something a new try cannot mend.
    given_up: bool,
}

impl std::fmt::Debug for BleChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BleChild")
            .field("pid", &self.child.id())
            .finish()
    }
}

impl BleEngine {
    /// `command` is the program and its arguments.
    pub fn new(command: Vec<OsString>) -> BleEngine {
        BleEngine {
            command,
            child: None,
            running: false,
            retry_at_ms: None,
            deaths: 0,
            given_up: false,
        }
    }

    fn start(&mut self, now_ms: u64) {
        self.retry_at_ms = None;
        if self.given_up || self.child.is_some() {
            return;
        }
        let Some((program, args)) = self.command.split_first() else {
            return;
        };
        let spawned = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn();
        match spawned {
            Ok(mut child) => {
                let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
                    return;
                };
                // SAFETY: fcntl on a pipe we hold.
                unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
                info!("radio: BLE engine started");
                self.child = Some(BleChild {
                    child,
                    _stdin: stdin,
                    stdout,
                    said: Vec::new(),
                    started_ms: now_ms,
                });
            }
            Err(e) => {
                warn!("radio: cannot start the BLE engine ({e})");
                self.given_up = true;
            }
        }
    }

    /// Ends it, and returns once the UART is closed, or as good as: a killed process that does not
    /// go is stuck in the kernel, and waiting longer would stop the daemon's loop.
    fn stop(&mut self) {
        self.retry_at_ms = None;
        self.running = false;
        let Some(c) = self.child.take() else {
            return;
        };
        // Dropping standard input is what tells it to stop.
        let BleChild {
            mut child, _stdin, ..
        } = c;
        drop(_stdin);
        let end = Instant::now() + Duration::from_millis(BLE_STOP_MS);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(5)),
                _ => {
                    warn!("radio: the BLE engine did not stop: killing it");
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
        info!("radio: BLE engine stopped");
    }
}

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
    /// Owns Bluetooth when set: see the module docs.
    ble: Option<BleEngine>,
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
            ble: None,
            wifi_on: None,
            bt_on: None,
        }
    }

    /// Bluetooth is `omg-bled`'s from now on, run with this command line.
    pub fn with_ble_engine(mut self, command: Vec<OsString>) -> Radio {
        self.ble = Some(BleEngine::new(command));
        self
    }

    /// The engine's output, for the poll set: readable when it said something or ended.
    pub fn ble_fd(&self) -> Option<RawFd> {
        let child = self.ble.as_ref()?.child.as_ref()?;
        Some(child.stdout.as_raw_fd())
    }

    /// Whether the engine is advertising or holding a link. A capability follows this.
    pub fn ble_running(&self) -> bool {
        self.ble.as_ref().is_some_and(|b| b.running)
    }

    /// The descriptor from [`Radio::ble_fd`] has an event.
    pub fn on_ble_event(&mut self, now_ms: u64) {
        let Some(child) = self.ble.as_mut().and_then(|b| b.child.as_mut()) else {
            return;
        };
        let mut buf = [0u8; 64];
        let ended = match child.stdout.read(&mut buf) {
            Ok(0) => true,
            Ok(n) => {
                child.said.extend_from_slice(&buf[..n]);
                false
            }
            Err(e) => !matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ),
        };
        if ended {
            self.ble_ended(now_ms);
            return;
        }
        if child.said.ends_with(b"ready\n") {
            child.said.clear();
            if let Some(b) = self.ble.as_mut() {
                b.running = true;
            }
            // It has written to the UART, and the kick is for a port that never did. And it must
            // not happen now in any case: one byte in the middle of its HCI stream.
            self.bt_uart = None;
            info!("radio: BLE engine ready");
        }
    }

    /// The engine ended by itself. The chip goes off, because an owned chip with nobody at the
    /// port keeps the UART from idling, which keeps the system out of deep sleep.
    fn ble_ended(&mut self, now_ms: u64) {
        let Some(b) = self.ble.as_mut() else { return };
        b.running = false;
        let Some(mut c) = b.child.take() else { return };
        let code = c.child.wait().ok().and_then(|s| s.code());
        if now_ms.saturating_sub(c.started_ms) >= BLE_STABLE_MS {
            b.deaths = 0;
        }
        if code == Some(BLE_EXIT_CONFIG) {
            warn!("radio: the BLE engine cannot run: BLE stays off");
            b.given_up = true;
        } else if let Some(wait) = BLE_RESTART_MS.get(b.deaths) {
            warn!("radio: the BLE engine ended ({code:?}); trying again in {wait} ms");
            b.retry_at_ms = Some(now_ms + wait);
            b.deaths += 1;
        } else {
            warn!("radio: the BLE engine keeps ending ({code:?}): BLE stays off");
            b.given_up = true;
        }
        self.step("rfkill", &["block", "bluetooth"]);
        self.kick_bt_uart();
    }

    /// When [`Radio::ble_tick`] should next run, in the daemon's monotonic milliseconds.
    pub fn ble_next_deadline(&self) -> Option<u64> {
        self.ble.as_ref()?.retry_at_ms
    }

    /// Starts the engine again when its wait is over.
    pub fn ble_tick(&mut self, now_ms: u64) {
        if self
            .ble
            .as_ref()
            .and_then(|b| b.retry_at_ms)
            .is_none_or(|t| now_ms < t)
        {
            return;
        }
        self.step("rfkill", &["unblock", "bluetooth"]);
        if let Some(b) = self.ble.as_mut() {
            b.start(now_ms);
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
        let ok = if let Some(b) = self.ble.as_mut() {
            if on {
                let r = self.step("rfkill", &["unblock", "bluetooth"]);
                // The owner asked: whatever made the engine give up gets another chance.
                if let Some(b) = self.ble.as_mut() {
                    b.given_up = false;
                    b.deaths = 0;
                    b.start(mono_ms());
                }
                r
            } else {
                // The UART closes before the chip goes off, and only then is it free to kick.
                b.stop();
                let r = self.step("rfkill", &["block", "bluetooth"]);
                self.kick_bt_uart();
                r
            }
        } else if on {
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

/// The clock [`Radio::on_ble_event`] and friends use: the daemon's monotonic base.
pub fn mono_ms() -> u64 {
    glass_api::monotonic_us() / 1000
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

    /// A shell script in place of `omg-bled`, for what glassd does around any engine.
    fn engine_script(dir: &std::path::Path, body: &str) -> Vec<OsString> {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("cd {}; {body}", dir.display()).into(),
        ]
    }

    /// Reads the engine's output until `want` holds.
    fn wait_engine(r: &mut Radio, want: impl Fn(&Radio) -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !want(r) {
            assert!(Instant::now() < end, "timed out");
            if let Some(fd) = r.ble_fd() {
                let mut p = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one valid pollfd.
                unsafe { libc::poll(&mut p, 1, 100) };
                if p.revents != 0 {
                    r.on_ble_event(mono_ms());
                }
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    fn with_engine(body: &str) -> (Radio, Arc<Mutex<Vec<String>>>, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        let (r, log) = recording();
        let command = engine_script(d.path(), body);
        (r.with_ble_engine(command), log, d)
    }

    #[test]
    fn the_engine_owns_bluetooth_and_is_stopped_before_the_chip_goes_off() {
        // It reports its end in a file, then waits for the leash like the real one.
        let (mut r, log, d) = with_engine("echo ready; cat >/dev/null; echo stopped > stopped");
        r.set_bluetooth(true);
        assert_eq!(cmds(&log), vec!["rfkill unblock bluetooth"], "no hciconfig");
        assert!(!r.ble_running(), "says nothing before it is up");
        wait_engine(&mut r, Radio::ble_running);

        r.set_bluetooth(false);
        assert!(!r.ble_running());
        assert!(d.path().join("stopped").exists(), "the chip went off first");
        assert_eq!(cmds(&log), vec!["rfkill block bluetooth"]);
        assert_eq!(r.ble_fd(), None);
    }

    #[test]
    fn an_engine_that_ignores_its_leash_is_killed() {
        let (mut r, _log, _d) = with_engine("trap '' TERM; echo ready; exec sleep 30");
        r.set_bluetooth(true);
        wait_engine(&mut r, Radio::ble_running);
        let t = Instant::now();
        r.set_bluetooth(false);
        assert!(t.elapsed() < Duration::from_secs(5));
        assert_eq!(r.ble_fd(), None);
    }

    #[test]
    fn the_kick_never_lands_on_a_port_the_engine_owned() {
        let d = tempfile::tempdir().unwrap();
        let uart = d.path().join("uart");
        std::fs::write(&uart, "").unwrap();
        let (r, _log) = recording();
        let mut r = r.with_ble_engine(engine_script(d.path(), "echo ready; cat >/dev/null"));
        r.bt_uart = Some(uart.to_string_lossy().into_owned());
        r.set_bluetooth(true);
        wait_engine(&mut r, Radio::ble_running);
        r.set_bluetooth(false);
        assert_eq!(
            std::fs::read(&uart).unwrap(),
            b"",
            "written after the engine"
        );
    }

    #[test]
    fn bluetooth_off_from_the_start_kicks_the_port_once_as_before() {
        let d = tempfile::tempdir().unwrap();
        let uart = d.path().join("uart");
        std::fs::write(&uart, "").unwrap();
        let (r, _log) = recording();
        let mut r = r.with_ble_engine(engine_script(d.path(), "echo ready; cat >/dev/null"));
        r.bt_uart = Some(uart.to_string_lossy().into_owned());
        r.set_bluetooth(false);
        r.set_bluetooth(false);
        assert_eq!(std::fs::read(&uart).unwrap(), [0]);
    }

    #[test]
    fn a_dead_engine_turns_the_chip_off_and_is_retried_a_few_times() {
        let (mut r, log, _d) = with_engine("exit 3");
        r.set_bluetooth(true);
        cmds(&log);
        let mut tries = 1;
        let mut now = mono_ms();
        loop {
            wait_engine(&mut r, |r| r.ble_fd().is_none());
            assert!(!r.ble_running());
            assert!(cmds(&log).iter().any(|c| c == "rfkill block bluetooth"));
            let Some(at) = r.ble_next_deadline() else {
                break;
            };
            assert!(at > now);
            r.ble_tick(at - 1);
            assert!(cmds(&log).is_empty(), "before its time");
            r.ble_tick(at);
            assert_eq!(cmds(&log), vec!["rfkill unblock bluetooth"]);
            tries += 1;
            now = at;
        }
        assert_eq!(tries, BLE_RESTART_MS.len() + 1);
        // No timer left: a given-up engine does not wake the system.
        assert_eq!(r.ble_next_deadline(), None);
        // The owner turning it on again is a new start.
        r.set_bluetooth(false);
        r.set_bluetooth(true);
        assert!(r.ble_fd().is_some());
    }

    #[test]
    fn a_missing_firmware_is_not_retried() {
        let (mut r, log, _d) = with_engine("exit 78");
        r.set_bluetooth(true);
        wait_engine(&mut r, |r| r.ble_fd().is_none());
        assert_eq!(r.ble_next_deadline(), None);
        assert!(cmds(&log).iter().any(|c| c == "rfkill block bluetooth"));
        assert!(!r.ble_running());
    }

    #[test]
    fn the_running_state_follows_a_crash_while_up() {
        let (mut r, _log, d) =
            with_engine("echo ready; while [ ! -e die ]; do sleep 0.01; done; exit 1");
        r.set_bluetooth(true);
        wait_engine(&mut r, Radio::ble_running);
        std::fs::write(d.path().join("die"), "").unwrap();
        wait_engine(&mut r, |r| !r.ble_running());
        assert!(r.ble_next_deadline().is_some());
    }
}
