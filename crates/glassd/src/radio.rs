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
//!   chip after. If it dies, or cannot start, the chip is blocked too, so a dead engine cannot hold
//!   the UART awake. See [`BleEngine`].
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
/// How often, and how many times, `wpa_supplicant` is tried again when it would not start.
const ASSOC_RETRY_US: u64 = 2_000_000;
const ASSOC_MAX_FAILURES: u32 = 5;
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
/// How long to wait after killing it. A process still there after this is held by the kernel, in
/// the close of the port (`closing_wait`, 30 s by default, with the chip holding CTS).
const BLE_KILLED_WAIT_MS: u64 = 200;
/// Its output ended: how long it gets to become a process that can be reaped. It usually takes far
/// less, and this is the daemon's loop waiting, so it is short.
const BLE_REAP_SPIN_MS: u64 = 20;
/// Then it is looked at again after this long, and twice as long each time, up to the most.
const BLE_REAP_FIRST_MS: u64 = 100;
const BLE_REAP_MAX_MS: u64 = 30_000;
/// A start that finds the port still held tries again after this long.
const BLE_PORT_BUSY_MS: u64 = 1_000;
/// How long the daemon's loop waits for the kick. It takes a millisecond when nothing is wrong.
const KICK_WAIT_MS: u64 = 200;
/// How long the kick's byte gets to leave before it is thrown away.
const KICK_DRAIN_MS: u64 = 50;

/// A running `omg-bled`.
struct BleChild {
    child: Child,
    /// The leash: its end stops the engine, also when glassd dies.
    _stdin: ChildStdin,
    stdout: ChildStdout,
    said: Vec<u8>,
    started_ms: u64,
}

/// An engine that was told to go, or killed, and has not gone. It may still hold the port.
struct Leaving {
    child: Child,
    check_at_ms: u64,
    wait_ms: u64,
}

impl std::fmt::Debug for Leaving {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Leaving")
            .field("pid", &self.child.id())
            .finish()
    }
}

enum Reap {
    /// It ended, with this exit code if it had one.
    Gone(Option<i32>),
    Stuck,
}

/// Waits for `child`, this long at the most.
fn reap_within(child: &mut Child, ms: u64) -> Reap {
    let end = Instant::now() + Duration::from_millis(ms);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Reap::Gone(status.code()),
            Err(_) => return Reap::Gone(None),
            Ok(None) if Instant::now() >= end => return Reap::Stuck,
            Ok(None) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

/// Runs `omg-bled` as a child, whose standard output says when it is ready and whose end shows as
/// the end of that output. So this adds one descriptor to the daemon's poll set, and no timer
/// unless the engine died.
#[derive(Debug)]
pub struct BleEngine {
    command: Vec<OsString>,
    child: Option<BleChild>,
    /// Nothing starts while this is here: the port may be held.
    leaving: Option<Leaving>,
    /// It said it is advertising, and has not ended since.
    running: bool,
    /// When to try again after a death, in the daemon's monotonic milliseconds.
    retry_at_ms: Option<u64>,
    deaths: usize,
    /// It reported something a new try cannot mend.
    given_up: bool,
    /// Stands for a process the kernel holds, which a host test cannot make.
    #[cfg(test)]
    unkillable: bool,
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
            leaving: None,
            running: false,
            retry_at_ms: None,
            deaths: 0,
            given_up: false,
            #[cfg(test)]
            unkillable: false,
        }
    }

    fn kill(&self, child: &mut Child) {
        #[cfg(test)]
        if self.unkillable {
            return;
        }
        let _ = child.kill();
    }

    /// To be reaped later, by [`Radio::ble_tick`].
    fn leave(&mut self, child: Child, now_ms: u64) {
        self.leaving = Some(Leaving {
            child,
            check_at_ms: now_ms + BLE_REAP_FIRST_MS,
            wait_ms: BLE_REAP_FIRST_MS,
        });
    }

    /// Returns false if it could not be started: the chip then has nobody at the port.
    fn start(&mut self, now_ms: u64) -> bool {
        self.retry_at_ms = None;
        if self.given_up || self.child.is_some() {
            return true;
        }
        let Some((program, args)) = self.command.split_first() else {
            return false;
        };
        let spawned = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn();
        match spawned {
            Ok(mut child) => {
                let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
                    self.kill(&mut child);
                    self.leave(child, now_ms);
                    return false;
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
                true
            }
            Err(e) => {
                warn!("radio: cannot start the BLE engine ({e})");
                false
            }
        }
    }

    /// Ends it, and returns once the UART is closed, or as good as: a killed process that does not
    /// go is stuck in the kernel, and waiting longer would stop the daemon's loop. It is then left
    /// to [`Radio::ble_tick`], and nothing starts or kicks the port until it has gone.
    fn stop(&mut self, now_ms: u64) {
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
        if let Reap::Stuck = reap_within(&mut child, BLE_STOP_MS) {
            warn!("radio: the BLE engine did not stop: killing it");
            self.kill(&mut child);
            if let Reap::Stuck = reap_within(&mut child, BLE_KILLED_WAIT_MS) {
                warn!("radio: the BLE engine is stuck in the kernel: not waiting for it");
                self.leave(child, now_ms);
                return;
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

/// The command line of a running process, by pid, with its arguments joined by spaces; `None` if
/// there is no such process. Injected so the tests can say what runs.
pub type Processes = Box<dyn Fn(&str) -> Option<String> + Send>;

fn system_processes(pid: &str) -> Option<String> {
    let raw = std::fs::read(std::path::Path::new("/proc").join(pid).join("cmdline")).ok()?;
    Some(String::from_utf8_lossy(&raw).replace('\0', " "))
}

/// The whole numbers in a pid file: nothing else may become an argument of `kill`.
pub(crate) fn pids_in(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter(|p| p.bytes().all(|b| b.is_ascii_digit()))
        .map(str::to_string)
        .collect()
}

/// Runs a system command `program args...`. An `Err` means the process could not be run at all
/// (the tool is absent); `Ok` with `ok = false` means it ran and failed.
pub type Runner = Box<dyn FnMut(&str, &[&str]) -> io::Result<Outcome> + Send>;

/// Switches the Wi-Fi and Bluetooth radios.
pub struct Radio {
    run: Runner,
    procs: Processes,
    /// Where `wpa_supplicant` and `udhcpc` write their pids.
    wpa_pid: std::path::PathBuf,
    dhcp_pid: std::path::PathBuf,
    wifi_iface: String,
    bt_hci: String,
    /// Still to be kicked once after the chip is powered off (see `kick_bt_uart`).
    bt_uart: Option<String>,
    /// What the kick does, on its own thread.
    kick_port: fn(&str),
    /// A kick that is taking long. Until it ends it may still be at the port.
    kicking: Option<std::thread::JoinHandle<()>>,
    /// The supplicant config to associate with when Wi-Fi is on.
    wifi_conf: Option<std::path::PathBuf>,
    /// `wpa_supplicant` and `udhcpc` were started and not stopped yet.
    associating: bool,
    /// Owns Bluetooth when set: see the module docs.
    ble: Option<BleEngine>,
    /// A Wi-Fi setup session owns the interface: the settings do not drive it for now.
    wifi_held: bool,
    /// `wpa_supplicant` refused to start this many times in a row, and the next try's time.
    assoc_failures: u32,
    assoc_retry_at_us: Option<u64>,
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

/// The kick itself: see [`Radio::kick_bt_uart`]. Safe with the port in any state: no flow control,
/// so that CTS cannot hold the byte, and nothing left queued, so that the close has nothing to
/// wait for. What it leaves behind is the flow control off, and the engine sets its own.
fn kick_port(path: &str) {
    match send_one_byte(path) {
        Ok(true) => info!("radio: one byte sent on {path} so that the idle port can sleep"),
        Ok(false) => warn!("radio: the byte on {path} did not leave: deep sleep may be blocked"),
        Err(e) => warn!("radio: cannot write to {path} ({e}): deep sleep may be blocked"),
    }
}

/// Returns whether the byte left. Not a terminal (a test's file) is fine.
fn send_one_byte(path: &str) -> io::Result<bool> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut port = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
        .open(path)?;
    let fd = port.as_raw_fd();
    // SAFETY: termios is plain data, filled by tcgetattr before use, on a descriptor we hold.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if tty::get_attr(fd, &mut t) == 0 {
            t.c_cflag &= !libc::CRTSCTS;
            t.c_cflag |= libc::CLOCAL;
            tty::set_attr_now(fd, &t);
        }
    }
    port.write_all(&[0])?;
    let end = Instant::now() + Duration::from_millis(KICK_DRAIN_MS);
    loop {
        let mut queued: libc::c_int = 0;
        // SAFETY: TIOCOUTQ fills one int.
        let r = unsafe { libc::ioctl(fd, libc::TIOCOUTQ as _, &mut queued) };
        if r != 0 || queued == 0 {
            return Ok(true);
        }
        if Instant::now() >= end {
            // SAFETY: on a descriptor we hold.
            unsafe { tty::flush(fd, libc::TCOFLUSH) };
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The termios calls glassd needs. Bionic before API 21 has them only as inline functions in its
/// headers, with no symbol to link against, so on Android they are the ioctls those functions wrap.
mod tty {
    use std::os::fd::RawFd;

    /// # Safety
    /// `fd` must be an open descriptor.
    pub unsafe fn get_attr(fd: RawFd, t: &mut libc::termios) -> libc::c_int {
        #[cfg(target_os = "android")]
        // SAFETY: as the caller promises.
        return unsafe { libc::ioctl(fd, libc::TCGETS as _, t as *mut libc::termios) };
        #[cfg(not(target_os = "android"))]
        // SAFETY: as the caller promises.
        return unsafe { libc::tcgetattr(fd, t) };
    }

    /// # Safety
    /// `fd` must be an open descriptor.
    pub unsafe fn set_attr_now(fd: RawFd, t: &libc::termios) -> libc::c_int {
        #[cfg(target_os = "android")]
        // SAFETY: as the caller promises.
        return unsafe { libc::ioctl(fd, libc::TCSETS as _, t as *const libc::termios) };
        #[cfg(not(target_os = "android"))]
        // SAFETY: as the caller promises.
        return unsafe { libc::tcsetattr(fd, libc::TCSANOW, t) };
    }

    /// # Safety
    /// `fd` must be an open descriptor.
    pub unsafe fn flush(fd: RawFd, queue: libc::c_int) -> libc::c_int {
        #[cfg(target_os = "android")]
        // SAFETY: as the caller promises.
        return unsafe { libc::ioctl(fd, libc::TCFLSH as _, queue as libc::c_ulong) };
        #[cfg(not(target_os = "android"))]
        // SAFETY: as the caller promises.
        return unsafe { libc::tcflush(fd, queue) };
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

    /// Reads what runs through `procs` instead of `/proc`, and keeps the pid files in `dir`.
    pub fn with_processes(mut self, procs: Processes, dir: &std::path::Path) -> Radio {
        self.procs = procs;
        self.wpa_pid = dir.join("glassd-wpa_supplicant.pid");
        self.dhcp_pid = dir.join("glassd-udhcpc.pid");
        self
    }

    /// The pids in `file` of processes still running with `marker` in their command line. A pid
    /// file outlives its process, and the pid can be another process's by now.
    pub(crate) fn live_pids(&self, file: &std::path::Path, marker: &str) -> Vec<String> {
        let Ok(text) = std::fs::read_to_string(file) else {
            return Vec::new();
        };
        pids_in(&text)
            .into_iter()
            .filter(|p| (self.procs)(p).is_some_and(|c| c.contains(marker)))
            .collect()
    }

    /// Sends `signal` to the live processes of these pid files. Returns whether there were any.
    pub(crate) fn signal_pid_files(
        &mut self,
        signal: &str,
        files: &[(&std::path::Path, &str)],
    ) -> bool {
        let pids: Vec<String> = files
            .iter()
            .flat_map(|(f, marker)| self.live_pids(f, marker))
            .collect();
        if pids.is_empty() {
            return false;
        }
        let mut args = vec![signal];
        args.extend(pids.iter().map(String::as_str));
        self.step("kill", &args);
        true
    }

    /// A radio with a custom runner.
    pub fn with_runner(wifi_iface: impl Into<String>, run: Runner) -> Radio {
        Radio {
            run,
            procs: Box::new(system_processes),
            wpa_pid: WPA_PID_FILE.into(),
            dhcp_pid: DHCP_PID_FILE.into(),
            wifi_iface: wifi_iface.into(),
            bt_hci: DEFAULT_BT_HCI.into(),
            bt_uart: None,
            kick_port,
            kicking: None,
            wifi_conf: None,
            associating: false,
            ble: None,
            wifi_held: false,
            assoc_failures: 0,
            assoc_retry_at_us: None,
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
        let Some(c) = b.child.take() else { return };
        let BleChild {
            mut child,
            started_ms,
            ..
        } = c;
        // Its output closes before its other descriptors. The port is one of them, and its close
        // can last as long as the kernel's closing_wait: the daemon's loop does not wait for that.
        let code = match reap_within(&mut child, BLE_REAP_SPIN_MS) {
            Reap::Gone(code) => code,
            Reap::Stuck => {
                warn!("radio: the BLE engine is slow to end: killing it");
                b.kill(&mut child);
                b.leave(child, now_ms);
                None
            }
        };
        if now_ms.saturating_sub(started_ms) >= BLE_STABLE_MS {
            b.deaths = 0;
        }
        self.ble_failed(now_ms, code);
    }

    /// There is no engine, and there should be: what its death or a failed start lead to.
    fn ble_failed(&mut self, now_ms: u64, code: Option<i32>) {
        let Some(b) = self.ble.as_mut() else { return };
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
        let b = self.ble.as_ref()?;
        let reap = b.leaving.as_ref().map(|l| l.check_at_ms);
        [b.retry_at_ms, reap].into_iter().flatten().min()
    }

    /// Reaps an engine that was slow to go, and starts it again when its wait is over.
    pub fn ble_tick(&mut self, now_ms: u64) {
        self.ble_reap(now_ms);
        if self
            .ble
            .as_ref()
            .and_then(|b| b.retry_at_ms)
            .is_none_or(|t| now_ms < t)
        {
            return;
        }
        self.ble_bring_up(now_ms);
    }

    fn ble_reap(&mut self, now_ms: u64) {
        let Some(b) = self.ble.as_mut() else { return };
        let Some(l) = b.leaving.as_mut() else { return };
        if now_ms < l.check_at_ms {
            return;
        }
        match l.child.try_wait() {
            Ok(None) => {
                l.wait_ms = (l.wait_ms * 2).min(BLE_REAP_MAX_MS);
                l.check_at_ms = now_ms + l.wait_ms;
                return;
            }
            Ok(Some(status)) if status.code() == Some(BLE_EXIT_CONFIG) => {
                warn!("radio: the BLE engine cannot run: BLE stays off");
                b.given_up = true;
                b.retry_at_ms = None;
            }
            _ => {}
        }
        b.leaving = None;
        info!("radio: the BLE engine has gone");
        // The chip is off whenever no engine runs, and the port is free now.
        if b.child.is_none() {
            self.kick_bt_uart();
        }
    }

    /// Whether something may still hold the Bluetooth port.
    fn ble_port_busy(&self) -> bool {
        self.ble.as_ref().is_some_and(|b| b.leaving.is_some())
            || self.kicking.as_ref().is_some_and(|k| !k.is_finished())
    }

    /// Powers the chip and starts the engine. With the port still held neither happens yet, so that
    /// the chip is not on with nobody at the port. Returns whether the rfkill step succeeded.
    fn ble_bring_up(&mut self, now_ms: u64) -> bool {
        if self.ble_port_busy() {
            if let Some(b) = self.ble.as_mut() {
                b.retry_at_ms = Some(now_ms + BLE_PORT_BUSY_MS);
            }
            return true;
        }
        let r = self.step("rfkill", &["unblock", "bluetooth"]);
        if !self.ble.as_mut().is_some_and(|b| b.start(now_ms)) {
            self.ble_failed(now_ms, None);
        }
        r
    }

    /// The Wi-Fi interface's name.
    pub(crate) fn wifi_iface(&self) -> &str {
        &self.wifi_iface
    }

    /// Runs one command for its output; `None` if the tool is absent or the command failed.
    pub(crate) fn query(&mut self, program: &str, args: &[&str]) -> Option<String> {
        match (self.run)(program, args) {
            Ok(o) if o.ok => Some(o.stdout),
            _ => None,
        }
    }

    /// Runs one command; logs and returns false if the tool is absent or the command failed. The
    /// arguments are logged then, so a secret never goes in them.
    pub(crate) fn step(&mut self, program: &str, args: &[&str]) -> bool {
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
            self.forget_association_failures();
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
                // The owner asked: whatever made the engine give up gets another chance.
                b.given_up = false;
                b.deaths = 0;
                self.ble_bring_up(mono_ms())
            } else {
                // The UART closes before the chip goes off, and only then is it free to kick.
                b.stop(mono_ms());
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
    ///
    /// An engine that failed its bring-up has left its settings on the port, and it is not known
    /// what CTS does with the chip off. So [`kick_port`] drops the flow control, and it runs on its
    /// own thread, so that whatever the kernel does with this port cannot stop the daemon's loop.
    fn kick_bt_uart(&mut self) {
        // It stays due: an engine on its way out still owns the port.
        if self.ble_port_busy() {
            return;
        }
        let Some(path) = self.bt_uart.take() else {
            return;
        };
        let kick = self.kick_port;
        let thread = std::thread::Builder::new()
            .name("bt-kick".into())
            .spawn(move || kick(&path));
        let thread = match thread {
            Ok(t) => t,
            Err(e) => {
                warn!("radio: cannot kick the Bluetooth port ({e}): deep sleep may be blocked");
                return;
            }
        };
        let end = Instant::now() + Duration::from_millis(KICK_WAIT_MS);
        while !thread.is_finished() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(1));
        }
        if !thread.is_finished() {
            warn!("radio: the Bluetooth port is slow to take a byte: not waiting for it");
            self.kicking = Some(thread);
        }
    }

    /// Brings both radios to the wanted state, writing only what changed since the last call.
    /// `wifi` and `bluetooth` are the effective states (airplane mode already folded in).
    pub fn apply(&mut self, wifi: bool, bluetooth: bool) {
        if !self.wifi_held && self.wifi_on != Some(wifi) {
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
        let pid_arg = self.wpa_pid.to_string_lossy().into_owned();
        let started = self.step(
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
                &pid_arg,
            ],
        );
        // An old one that is still going holds the control socket, and this one exits at once.
        // One that an earlier run left (a crash, an update, an exit) is not ours to know of but
        // by its pid file: it goes, and the next try comes after it has.
        if !started {
            if self.kill_association_by_files() {
                warn!("radio: stopped a supplicant left by an earlier run");
            }
            self.assoc_failures += 1;
            return;
        }
        self.assoc_failures = 0;
        self.assoc_retry_at_us = None;
        // Through a shell, with the output closed: the runner waits for its pipe to close, and
        // udhcpc stays in the foreground until it has a lease.
        let dhcp = format!(
            "udhcpc -b -i {iface} -p {} >/dev/null 2>&1 &",
            self.dhcp_pid.display()
        );
        self.step("sh", &["-c", &dhcp]);
        self.associating = true;
        info!("radio: joining Wi-Fi on {iface}");
    }

    /// A Wi-Fi setup session takes the interface: [`Radio::apply`] leaves Wi-Fi alone for now.
    pub(crate) fn hold_wifi(&mut self) {
        self.wifi_held = true;
        self.forget_association_failures();
    }

    fn forget_association_failures(&mut self) {
        self.assoc_failures = 0;
        self.assoc_retry_at_us = None;
    }

    /// When [`Radio::association_tick`] next has work (`CLOCK_MONOTONIC` µs).
    pub fn association_next_deadline(&self) -> Option<u64> {
        self.assoc_retry_at_us
    }

    /// Tries `wpa_supplicant` again if it refused to start, a few times.
    pub fn association_tick(&mut self, now_us: u64) {
        if self.assoc_failures == 0 {
            return;
        }
        if self.assoc_failures > ASSOC_MAX_FAILURES {
            warn!("radio: wpa_supplicant will not start: giving up");
            self.forget_association_failures();
            return;
        }
        match self.assoc_retry_at_us {
            None => self.assoc_retry_at_us = Some(now_us + ASSOC_RETRY_US),
            Some(at) if now_us >= at => {
                self.assoc_retry_at_us = None;
                self.start_association();
                if self.assoc_failures > 0 {
                    self.assoc_retry_at_us = Some(now_us + ASSOC_RETRY_US);
                }
            }
            Some(_) => {}
        }
    }

    /// The session gave the interface back, in an unknown state: the next [`Radio::apply`] drives
    /// it, whatever it was before.
    pub(crate) fn release_wifi(&mut self) {
        self.wifi_held = false;
        self.wifi_on = None;
    }

    /// The network tried by [`Radio::join_wifi`] works, and is now stored as `conf`.
    pub(crate) fn finish_join(&mut self, conf: std::path::PathBuf) {
        self.wifi_conf = Some(conf);
        self.wifi_held = false;
    }

    /// The network tried by [`Radio::join_wifi`] did not work: back to `conf`. The radio stays
    /// held: the supplicant has only been told to go, and a new one started before it has gone
    /// cannot get the control socket. Wait, then [`Radio::release_wifi`].
    pub(crate) fn abandon_join(&mut self, conf: std::path::PathBuf) {
        self.stop_association();
        self.forget_association_failures();
        self.wifi_conf = Some(conf);
    }

    /// Stops the supplicant and the DHCP client, by their pid files: also those of an earlier run,
    /// which this one did not start and would otherwise run beside.
    pub(crate) fn stop_association(&mut self) {
        self.associating = false;
        self.kill_association_by_files();
    }

    fn kill_association_by_files(&mut self) -> bool {
        let (wpa, dhcp) = (self.wpa_pid.clone(), self.dhcp_pid.clone());
        let (wpa_s, dhcp_s) = (wpa.to_string_lossy(), dhcp.to_string_lossy());
        self.signal_pid_files("-TERM", &[(&wpa, &wpa_s), (&dhcp, &dhcp_s)])
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
        // The supplicant runs once started: its pid file then names it.
        let pid_dir = d.path().to_path_buf();
        let mut r = r.with_wifi_conf(&conf).with_processes(
            Box::new(move |pid| {
                (pid == "4242").then(|| {
                    pid_dir
                        .join("glassd-wpa_supplicant.pid")
                        .display()
                        .to_string()
                })
            }),
            d.path(),
        );
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

        std::fs::write(d.path().join("glassd-wpa_supplicant.pid"), "4242\n").unwrap();
        log.lock().unwrap().clear();
        r.apply(false, false);
        let stopped = log.lock().unwrap().clone();
        assert!(
            stopped.contains(&"kill -TERM 4242".to_string()),
            "{stopped:?}"
        );
        let kill = stopped.iter().position(|c| c.contains("kill")).unwrap();
        let down = stopped
            .iter()
            .position(|c| c.contains("link set wlan0 down"))
            .unwrap();
        assert!(kill < down);
    }

    #[test]
    fn a_supplicant_that_will_not_start_is_tried_again_and_then_given_up() {
        let d = tempfile::tempdir().unwrap();
        let conf = d.path().join(WIFI_CONF_FILE);
        std::fs::write(&conf, "network={}").unwrap();
        let refusals = Arc::new(Mutex::new(2));
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let (r2, s2) = (refusals.clone(), seen.clone());
        let mut r = Radio::with_runner(
            "wlan0",
            Box::new(move |p, _| {
                s2.lock().unwrap().push(p.to_string());
                let mut left = r2.lock().unwrap();
                let ok = p != "wpa_supplicant" || *left == 0;
                if p == "wpa_supplicant" && *left > 0 {
                    *left -= 1;
                }
                Ok(Outcome {
                    ok,
                    stdout: String::new(),
                })
            }),
        )
        .with_wifi_conf(&conf);
        let starts = |s: &Arc<Mutex<Vec<String>>>| {
            s.lock()
                .unwrap()
                .iter()
                .filter(|p| *p == "wpa_supplicant")
                .count()
        };
        r.set_wifi(true);
        assert_eq!(starts(&seen), 1);
        assert!(
            !r.associating,
            "no DHCP client for a supplicant that is not there"
        );
        r.association_tick(0);
        let at = r.association_next_deadline().unwrap();
        r.association_tick(at - 1);
        assert_eq!(starts(&seen), 1);
        r.association_tick(at);
        assert_eq!(starts(&seen), 2);
        r.association_tick(at + ASSOC_RETRY_US);
        assert_eq!(starts(&seen), 3);
        assert!(r.associating);
        assert_eq!(r.association_next_deadline(), None);
        assert!(
            seen.lock().unwrap().iter().any(|p| p == "sh"),
            "the DHCP client"
        );

        // It never starts: a limited number of tries, and no more deadlines.
        *refusals.lock().unwrap() = 1000;
        r.set_wifi(false);
        r.set_wifi(true);
        let mut now = 0;
        for _ in 0..50 {
            r.association_tick(now);
            now += ASSOC_RETRY_US;
        }
        assert_eq!(r.association_next_deadline(), None);
        assert_eq!(starts(&seen), 4 + ASSOC_MAX_FAILURES as usize);
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

    /// Ticks at each deadline until the slow engine has been reaped.
    fn until_reaped(r: &mut Radio) {
        let end = Instant::now() + Duration::from_secs(10);
        while r.ble_port_busy() {
            assert!(Instant::now() < end, "timed out");
            let at = r
                .ble
                .as_ref()
                .unwrap()
                .leaving
                .as_ref()
                .unwrap()
                .check_at_ms;
            r.ble_reap(at);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn a_spawn_that_fails_turns_the_chip_off_kicks_the_port_and_is_retried() {
        let d = tempfile::tempdir().unwrap();
        let uart = d.path().join("uart");
        std::fs::write(&uart, "").unwrap();
        let (r, log) = recording();
        let mut r = r.with_ble_engine(vec![d.path().join("absent").into()]);
        r.bt_uart = Some(uart.to_string_lossy().into_owned());
        r.set_bluetooth(true);
        assert_eq!(
            cmds(&log),
            vec!["rfkill unblock bluetooth", "rfkill block bluetooth"]
        );
        assert_eq!(std::fs::read(&uart).unwrap(), [0], "no kick");
        let mut tries = 1;
        while let Some(at) = r.ble_next_deadline() {
            r.ble_tick(at);
            assert_eq!(
                cmds(&log),
                vec!["rfkill unblock bluetooth", "rfkill block bluetooth"]
            );
            tries += 1;
        }
        assert_eq!(tries, BLE_RESTART_MS.len() + 1);
        assert!(!r.ble_running());
    }

    #[test]
    fn an_engine_that_is_slow_to_end_is_killed_without_stopping_the_loop() {
        // Its output closes and it stays, as when the close of the port waits for the chip.
        let (mut r, log, _d) = with_engine("exec >&-; exec sleep 30");
        r.set_bluetooth(true);
        cmds(&log);
        let t = Instant::now();
        wait_engine(&mut r, |r| r.ble_fd().is_none());
        assert!(t.elapsed() < Duration::from_secs(5), "the loop waited");
        assert!(cmds(&log).iter().any(|c| c == "rfkill block bluetooth"));
        until_reaped(&mut r);
    }

    #[test]
    fn an_engine_the_kernel_holds_stops_nothing_and_blocks_the_next_start() {
        let d = tempfile::tempdir().unwrap();
        let uart = d.path().join("uart");
        std::fs::write(&uart, "").unwrap();
        let (r, log) = recording();
        let script = "exec >&-; while [ ! -e go ]; do sleep 0.01; done";
        let mut r = r.with_ble_engine(engine_script(d.path(), script));
        r.bt_uart = Some(uart.to_string_lossy().into_owned());
        r.ble.as_mut().unwrap().unkillable = true;
        r.set_bluetooth(true);
        cmds(&log);

        let t = Instant::now();
        wait_engine(&mut r, |r| r.ble_fd().is_none());
        assert!(t.elapsed() < Duration::from_secs(5), "the loop waited");
        assert_eq!(
            cmds(&log),
            vec!["rfkill block bluetooth"],
            "the chip stayed on"
        );
        assert_eq!(std::fs::read(&uart).unwrap(), b"", "kicked a port in use");

        // The retry comes, and finds the port held: the chip stays off, and it waits.
        let at = r.ble.as_ref().unwrap().retry_at_ms.unwrap();
        r.ble_tick(at);
        assert!(cmds(&log).is_empty(), "powered with the port held");
        assert!(r.ble_next_deadline().is_some());
        assert_eq!(r.ble_fd(), None);

        // The same when the owner asks for it.
        r.set_bluetooth(false);
        cmds(&log);
        r.set_bluetooth(true);
        assert!(cmds(&log).is_empty(), "powered with the port held");
        assert_eq!(r.ble_fd(), None);

        std::fs::write(d.path().join("go"), "").unwrap();
        until_reaped(&mut r);
        assert_eq!(std::fs::read(&uart).unwrap(), [0], "the kick stayed due");
        r.ble.as_mut().unwrap().unkillable = false;
        let at = r.ble.as_ref().unwrap().retry_at_ms.unwrap();
        r.ble_tick(at);
        assert_eq!(cmds(&log), vec!["rfkill unblock bluetooth"]);
        // Not left behind for the temporary directory to go from under it.
        r.set_bluetooth(false);
    }

    #[test]
    fn stopping_an_engine_the_kernel_holds_is_bounded_and_it_is_reaped_later() {
        let (mut r, log, d) = with_engine("trap '' TERM; while [ ! -e go ]; do sleep 0.01; done");
        r.ble.as_mut().unwrap().unkillable = true;
        r.set_bluetooth(true);
        cmds(&log);
        let t = Instant::now();
        r.set_bluetooth(false);
        assert!(t.elapsed() < Duration::from_secs(2), "the loop waited");
        assert_eq!(cmds(&log), vec!["rfkill block bluetooth"]);
        assert!(r.ble_next_deadline().is_some(), "nobody will reap it");
        std::fs::write(d.path().join("go"), "").unwrap();
        until_reaped(&mut r);
        assert_eq!(r.ble_next_deadline(), None, "a timer stayed");
    }

    #[test]
    fn the_kick_turns_off_the_flow_control_a_failed_bring_up_left_and_the_byte_leaves() {
        use std::os::fd::FromRawFd;
        // SAFETY: plain calls on the descriptor just opened.
        let (master, slave) = unsafe {
            let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK);
            assert!(m >= 0);
            assert_eq!(libc::grantpt(m), 0);
            assert_eq!(libc::unlockpt(m), 0);
            let mut name = [0 as libc::c_char; 128];
            assert_eq!(libc::ptsname_r(m, name.as_mut_ptr(), name.len()), 0);
            let path = std::ffi::CStr::from_ptr(name.as_ptr())
                .to_str()
                .unwrap()
                .to_string();
            (std::fs::File::from_raw_fd(m), path)
        };
        // Held open, so that the terminal's settings last from one open to the next.
        let holder = std::fs::File::options()
            .read(true)
            .write(true)
            .open(&slave)
            .unwrap();
        // SAFETY: termios is plain data, filled by tcgetattr before use.
        let flags = |set: bool| unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(holder.as_raw_fd(), &mut t), 0);
            if set {
                libc::cfmakeraw(&mut t);
                t.c_cflag |= libc::CRTSCTS;
                assert_eq!(libc::tcsetattr(holder.as_raw_fd(), libc::TCSANOW, &t), 0);
            }
            t.c_cflag
        };
        assert_ne!(flags(true) & libc::CRTSCTS, 0);

        assert!(send_one_byte(&slave).unwrap());

        assert_eq!(flags(false) & libc::CRTSCTS, 0, "flow control left on");
        let mut got = [9u8; 4];
        assert_eq!((&master).read(&mut got).unwrap(), 1);
        assert_eq!(got[0], 0);
    }

    #[test]
    fn a_kick_the_kernel_holds_stops_neither_the_loop_nor_the_order_of_things() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static LET_GO: AtomicBool = AtomicBool::new(false);
        let (r, log, _d) = {
            let d = tempfile::tempdir().unwrap();
            let (r, log) = recording();
            let command = engine_script(d.path(), "cat >/dev/null");
            (r.with_ble_engine(command), log, d)
        };
        let mut r = r;
        r.bt_uart = Some("unused".into());
        r.kick_port = |_| {
            while !LET_GO.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        let t = Instant::now();
        r.set_bluetooth(false);
        assert!(t.elapsed() < Duration::from_secs(2), "the loop waited");
        cmds(&log);

        // The engine must not meet a kick at the port: the chip waits with it.
        r.set_bluetooth(true);
        assert!(cmds(&log).is_empty(), "started with a kick at the port");
        assert_eq!(r.ble_fd(), None);
        let at = r.ble_next_deadline().unwrap();
        r.ble_tick(at);
        assert!(cmds(&log).is_empty());

        LET_GO.store(true, Ordering::SeqCst);
        let end = Instant::now() + Duration::from_secs(10);
        while r.ble_port_busy() {
            assert!(Instant::now() < end, "timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
        let at = r.ble_next_deadline().unwrap();
        r.ble_tick(at);
        assert_eq!(cmds(&log), vec!["rfkill unblock bluetooth"]);
        r.set_bluetooth(false);
    }

    #[test]
    fn a_late_exit_that_says_it_cannot_run_cancels_the_retry() {
        let (mut r, log, d) =
            with_engine("exec >&-; while [ ! -e go ]; do sleep 0.01; done; exit 78");
        r.ble.as_mut().unwrap().unkillable = true;
        r.set_bluetooth(true);
        wait_engine(&mut r, |r| r.ble_fd().is_none());
        assert!(r.ble.as_ref().unwrap().retry_at_ms.is_some());
        cmds(&log);
        std::fs::write(d.path().join("go"), "").unwrap();
        until_reaped(&mut r);
        assert_eq!(r.ble_next_deadline(), None);
        assert!(cmds(&log).is_empty());
    }
}
