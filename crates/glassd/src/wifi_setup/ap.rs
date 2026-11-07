//! The access point's system side: what runs, in what order, with what files. The order and the
//! waits come from the experiments on the Glass (docs/plans/2026-11-07-wifi-setup-ap-design.md).

use crate::radio::Radio;
use crate::warn;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// The driver picks AP firmware mode from this in the firmware's file name (`dhd_get_fw_mode`).
const APSTA_LINK_NAME: &str = "fw_bcmdhd_apsta.bin";
/// `CONFIG_BCMDHD_FW_PATH` in the Glass kernel.
const STOCK_FIRMWARE: &str = "/system/vendor/firmware/fw_bcmdhd.bin";
const FIRMWARE_PARAM: &str = "/sys/module/bcmdhd/parameters/firmware_path";
const RUN_DIR: &str = "/var/run/glassd-ap";
/// `DHD_FLAG_HOSTAP_MODE`, as the driver logs it. In the default mode 5 hostapd logs `AP-ENABLED`
/// and yet nothing could join.
const OP_MODE_AP: u32 = 2;

/// Channel 1 was tested on the Glass, and is allowed everywhere.
const CHANNEL: u32 = 1;
/// The access point's address.
pub const AP_IP: std::net::Ipv4Addr = std::net::Ipv4Addr::new(192, 168, 77, 1);
const AP_PREFIX: u32 = 24;

/// Where the access point's files are.
#[derive(Debug, Clone)]
pub struct ApPaths {
    /// Private directory of the session's files. Removed when the session ends.
    pub run_dir: PathBuf,
    /// The driver's `firmware_path` module parameter.
    pub firmware_param: PathBuf,
    /// The firmware, which the link in `run_dir` points at.
    pub stock_firmware: PathBuf,
}

impl ApPaths {
    /// The Glass's own.
    pub fn system() -> ApPaths {
        ApPaths {
            run_dir: RUN_DIR.into(),
            firmware_param: FIRMWARE_PARAM.into(),
            stock_firmware: STOCK_FIRMWARE.into(),
        }
    }

    fn file(&self, name: &str) -> PathBuf {
        self.run_dir.join(name)
    }

    /// The log hostapd writes at its default level, which has no keys in it.
    pub fn hostapd_log(&self) -> PathBuf {
        self.file("hostapd.log")
    }

    fn hostapd_pid(&self) -> PathBuf {
        self.file("hostapd.pid")
    }
}

/// What the access point runs with.
#[derive(Clone, Copy)]
pub struct ApCredentials<'a> {
    pub ssid: &'a str,
    pub password: &'a str,
    pub entropy: &'a [u8],
}

/// Why the access point could not come up. `code` is what the launcher gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApError {
    /// `hostapd` or `busybox-extras` is not on the image.
    NoTools,
    /// The firmware or the driver's parameter is missing.
    NoFirmware,
    /// A private file could not be written.
    Files,
    /// The interface did not come up, which is where the firmware loads.
    LinkUp,
    /// The firmware came up in a mode other than AP.
    WrongMode(u32),
    /// The kernel log does not say what mode the firmware came up in.
    UnknownMode,
    /// The address could not be set, or the page could not be bound.
    Serve,
}

impl ApError {
    pub fn code(self) -> &'static str {
        match self {
            ApError::NoTools => super::ERR_NO_TOOLS,
            ApError::NoFirmware => super::ERR_NO_FIRMWARE,
            ApError::Files => super::ERR_FILES,
            ApError::LinkUp => super::ERR_LINK_UP,
            ApError::WrongMode(_) | ApError::UnknownMode => super::ERR_FIRMWARE_MODE,
            ApError::Serve => super::ERR_SERVE,
        }
    }
}

/// The hostapd config. Both values are ours (see `super::generate`), from alphabets that cannot
/// end a line. It deliberately sets no country: the Glass does not know where it is.
pub fn hostapd_conf(iface: &str, ssid: &str, password: &str) -> String {
    format!(
        "interface={iface}\n\
         driver=nl80211\n\
         ssid={ssid}\n\
         hw_mode=g\n\
         channel={CHANNEL}\n\
         ieee80211n=1\n\
         wmm_enabled=1\n\
         auth_algs=1\n\
         wpa=2\n\
         wpa_key_mgmt=WPA-PSK\n\
         rsn_pairwise=CCMP\n\
         wpa_passphrase={password}\n"
    )
}

fn udhcpd_conf(iface: &str, leases: &Path) -> String {
    format!(
        "start 192.168.77.10\n\
         end 192.168.77.20\n\
         interface {iface}\n\
         lease_file {}\n\
         opt subnet 255.255.255.0\n\
         opt router {AP_IP}\n\
         opt dns {AP_IP}\n\
         opt lease 3600\n",
        leases.display()
    )
}

fn write_private(path: &Path, content: &[u8]) -> io::Result<()> {
    fs::File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(content)
}

/// The driver's mode from its log line `Firmware up: op_mode=0x0002, MAC=...`.
fn parse_op_mode(line: &str) -> Option<u32> {
    let hex = line.split("op_mode=0x").nth(1)?;
    let end = hex
        .find(|c: char| !c.is_ascii_hexdigit())
        .unwrap_or(hex.len());
    u32::from_str_radix(&hex[..end], 16).ok()
}

impl Radio {
    /// Stops whatever holds the Wi-Fi interface. Then wait, then [`Radio::ap_link_down`].
    pub fn ap_prepare(&mut self) {
        self.stop_association();
    }

    /// Takes the firmware down. The next link-up loads it again, in the mode the name says.
    pub fn ap_link_down(&mut self) {
        let iface = self.wifi_iface().to_string();
        self.step("ip", &["link", "set", &iface, "down"]);
    }

    /// Whether the access point can run here. Nothing is changed.
    pub fn ap_preflight(&mut self, paths: &ApPaths) -> Result<(), ApError> {
        if !paths.stock_firmware.exists() || !paths.firmware_param.exists() {
            return Err(ApError::NoFirmware);
        }
        // Busybox's `timeout`, which becomes the program: `$!` is then the program's pid. A GNU one
        // stays as its parent, and the pid file would name it.
        let tools = "command -v hostapd >/dev/null && command -v busybox-extras >/dev/null \
                     && t=$(command -v timeout) && b=$(command -v busybox) \
                     && [ \"$(readlink -f \"$t\")\" = \"$(readlink -f \"$b\")\" ]";
        if !self.step("sh", &["-c", tools]) {
            return Err(ApError::NoTools);
        }
        Ok(())
    }

    /// Writes the session's files, loads the firmware in AP mode, and starts `hostapd`, which
    /// goes on by itself: watch [`ApPaths::hostapd_log`] for `AP-ENABLED`.
    ///
    /// The password is in a file in a private directory and never on a command line, where any
    /// process could read it and [`Radio::step`] would log it on a failure.
    ///
    /// `limit_s` bounds the daemons by themselves, for a glassd that dies without telling them.
    /// `check_mode`: the firmware's mode must be in the kernel log, and be AP.
    pub fn ap_start(
        &mut self,
        paths: &ApPaths,
        creds: ApCredentials<'_>,
        limit_s: u64,
        check_mode: bool,
    ) -> Result<(), ApError> {
        let ApCredentials {
            ssid,
            password,
            entropy,
        } = creds;
        let iface = self.wifi_iface().to_string();
        let files = |paths: &ApPaths| -> io::Result<()> {
            remove_run_dir(&paths.run_dir)?;
            fs::DirBuilder::new().mode(0o700).create(&paths.run_dir)?;
            write_private(
                &paths.file("hostapd.conf"),
                hostapd_conf(&iface, ssid, password).as_bytes(),
            )?;
            write_private(&paths.file("hostapd.entropy"), entropy)?;
            let leases = paths.file("udhcpd.leases");
            write_private(&leases, b"")?;
            write_private(
                &paths.file("udhcpd.conf"),
                udhcpd_conf(&iface, &leases).as_bytes(),
            )?;
            std::os::unix::fs::symlink(&paths.stock_firmware, paths.file(APSTA_LINK_NAME))
        };
        files(paths).map_err(|e| {
            warn!("wifi-setup: cannot write the session's files ({e})");
            ApError::Files
        })?;
        let link = paths.file(APSTA_LINK_NAME);
        if let Err(e) = fs::write(&paths.firmware_param, format!("{}\n", link.display())) {
            warn!("wifi-setup: cannot set the firmware path ({e})");
            return Err(ApError::NoFirmware);
        }
        self.step("rfkill", &["unblock", "wifi"]);
        if !self.step("ip", &["link", "set", &iface, "up"]) {
            return Err(ApError::LinkUp);
        }
        if check_mode {
            match self.firmware_mode() {
                Some(OP_MODE_AP) => {}
                Some(mode) => return Err(ApError::WrongMode(mode)),
                // The line was written less than a second ago. Going on without it would be to
                // trust an AP-ENABLED that the default mode also logs.
                None => return Err(ApError::UnknownMode),
            }
        }
        // Through a shell for the pid and the log, with the output closed: the runner waits for
        // its pipe to close. Not `-d`, which logs handshake details. Busybox's `timeout` becomes
        // the program it runs, so the pid is hostapd's.
        let run = format!(
            "timeout -s TERM {limit_s} hostapd -e {entropy} {conf} >>{log} 2>&1 & echo $! >{pid}",
            entropy = paths.file("hostapd.entropy").display(),
            conf = paths.file("hostapd.conf").display(),
            log = paths.hostapd_log().display(),
            pid = paths.hostapd_pid().display(),
        );
        self.step("sh", &["-c", &run]);
        Ok(())
    }

    /// The firmware's mode, from the last line the driver logged about it.
    pub fn firmware_mode(&mut self) -> Option<u32> {
        let out = self.query(
            "sh",
            &["-c", "dmesg | grep 'Firmware up: op_mode=' | tail -n 1"],
        )?;
        parse_op_mode(&out)
    }

    /// `hostapd` has the access point up: gives it its address and a DHCP server.
    pub fn ap_serve(&mut self, paths: &ApPaths, limit_s: u64) -> Result<(), ApError> {
        let iface = self.wifi_iface().to_string();
        self.step("ip", &["addr", "flush", "dev", &iface]);
        let address = format!("{AP_IP}/{AP_PREFIX}");
        if !self.step("ip", &["addr", "add", &address, "dev", &iface]) {
            return Err(ApError::Serve);
        }
        // Alpine's busybox has no udhcpd: it is in busybox-extras.
        let run = format!(
            "timeout -s TERM {limit_s} busybox-extras udhcpd -f {conf} >/dev/null 2>&1 & echo $! >{pid}",
            conf = paths.file("udhcpd.conf").display(),
            pid = paths.file("udhcpd.pid").display(),
        );
        self.step("sh", &["-c", &run]);
        Ok(())
    }

    /// Whether `hostapd` still runs. Without its pid file (not started yet) it counts as running;
    /// with one, the pid must be a process with this session's config in its command line.
    pub fn ap_alive(&self, paths: &ApPaths) -> bool {
        if !paths.hostapd_pid().exists() {
            return true;
        }
        let conf = paths.file("hostapd.conf");
        !self
            .live_pids(&paths.hostapd_pid(), &conf.to_string_lossy())
            .is_empty()
    }

    /// The session's daemons by pid file, each with what its command line must have: a pid file
    /// can outlive its process (`timeout` ended it, or a reboot), and the pid be another's.
    fn ap_signal(&mut self, paths: &ApPaths, signal: &str) {
        let (hpid, hconf) = (paths.hostapd_pid(), paths.file("hostapd.conf"));
        let (dpid, dconf) = (paths.file("udhcpd.pid"), paths.file("udhcpd.conf"));
        let (h, d) = (hconf.to_string_lossy(), dconf.to_string_lossy());
        self.signal_pid_files(signal, &[(&hpid, &h), (&dpid, &d)]);
    }

    /// Stops the daemons, so that `hostapd` can tell the clients and leave the firmware clean.
    /// Then wait, then [`Radio::ap_teardown`].
    pub fn ap_stop_daemons(&mut self, paths: &ApPaths) {
        self.ap_signal(paths, "-TERM");
    }

    /// Takes the access point down for good and deletes its files: the address (a join would
    /// otherwise seem to have an address at once), the firmware, the name in the driver.
    pub fn ap_teardown(&mut self, paths: &ApPaths) {
        let iface = self.wifi_iface().to_string();
        // What did not take the hint has had its time.
        self.ap_signal(paths, "-KILL");
        self.step("ip", &["addr", "flush", "dev", &iface]);
        self.step("ip", &["link", "set", &iface, "down"]);
        // As `echo "" > firmware_path` in the experiments. A lone newline, not nothing: on the
        // Glass the next link-up after this came back in the normal mode.
        if let Err(e) = fs::write(&paths.firmware_param, "\n") {
            warn!("wifi-setup: cannot restore the firmware path ({e})");
        }
        if let Err(e) = remove_run_dir(&paths.run_dir) {
            warn!("wifi-setup: cannot remove the session's files ({e})");
        }
    }
}

fn remove_run_dir(dir: &Path) -> io::Result<()> {
    match fs::remove_dir_all(dir) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Puts a joined network's config in its final place, with the password still private: the file
/// keeps its mode through the rename. Survives a power cut.
pub fn install_conf(tried: &Path, conf: &Path) -> io::Result<()> {
    fs::File::open(tried)?.sync_all()?;
    fs::rename(tried, conf)?;
    if let Some(dir) = conf.parent() {
        fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// What the hostapd log says so far.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LogView {
    pub enabled: bool,
    pub disabled: bool,
    /// Stations in, less stations out. `iw station dump` lists none on this driver.
    pub clients: u32,
}

impl LogView {
    fn feed(&mut self, text: &str) {
        for line in text.lines() {
            if line.contains("AP-ENABLED") {
                self.enabled = true;
            } else if line.contains("AP-DISABLED") {
                self.disabled = true;
            } else if line.contains("AP-STA-CONNECTED") {
                self.clients += 1;
            } else if line.contains("AP-STA-DISCONNECTED") {
                self.clients = self.clients.saturating_sub(1);
            }
        }
    }
}

/// The log, read only from where it had got to. It is never cut while hostapd writes it, which
/// could lose a line; it is in RAM, so one past [`LOG_LIMIT`] ends the session instead. A session
/// adds a line or two per device going on or off the access point.
#[derive(Debug, Default)]
pub struct LogReader {
    offset: u64,
    view: LogView,
}

/// Read at most this much at a time.
const LOG_KEEP: u64 = 64 * 1024;
/// A log this long is no session's: the access point is ended.
const LOG_LIMIT: u64 = 4 * 1024 * 1024;

impl LogReader {
    /// What the log says, with what was added since the last call.
    pub fn poll(&mut self, path: &Path) -> LogView {
        use std::io::{Read, Seek, SeekFrom};
        let Ok(mut file) = fs::File::open(path) else {
            return self.view;
        };
        // Its real size, whatever has been read: also a log with no line end in it.
        if file.metadata().is_ok_and(|m| m.len() >= LOG_LIMIT) {
            self.too_long();
            return self.view;
        }
        let mut new = Vec::new();
        let read = file
            .seek(SeekFrom::Start(self.offset))
            .and_then(|_| (&mut file).take(LOG_KEEP).read_to_end(&mut new));
        if read.is_err() {
            return self.view;
        }
        // A line still being written waits for the next call.
        let Some(whole) = new.iter().rposition(|&b| b == b'\n').map(|i| i + 1) else {
            return self.view;
        };
        self.view.feed(&String::from_utf8_lossy(&new[..whole]));
        self.offset += whole as u64;
        if self.offset >= LOG_LIMIT {
            self.too_long();
        }
        self.view
    }

    fn too_long(&mut self) {
        if !self.view.disabled {
            crate::warn!("wifi-setup: the hostapd log is too long: ending the access point");
        }
        self.view.disabled = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_mode_from_the_driver_line() {
        let l = "[   14.382965] Firmware up: op_mode=0x0002, MAC=f8:8f:ca:11:a2:24";
        assert_eq!(parse_op_mode(l), Some(2));
        assert_eq!(parse_op_mode("Firmware up: op_mode=0x0005, MAC=x"), Some(5));
        assert_eq!(parse_op_mode(""), None);
    }

    #[test]
    fn the_conf_is_wpa2_only_with_no_country() {
        let c = hostapd_conf("wlan0", "OMG-Glass-ab12", "pw");
        assert!(c.contains("\nwpa=2\n") && c.contains("\nrsn_pairwise=CCMP\n"));
        assert!(c.contains("\nssid=OMG-Glass-ab12\n") && c.ends_with("wpa_passphrase=pw\n"));
        assert!(!c.contains("country_code"));
        assert!(!c.contains("TKIP"));
    }

    fn read_log(text: &str) -> LogView {
        let mut v = LogView::default();
        v.feed(text);
        v
    }

    #[test]
    fn the_log_is_read_from_where_it_got_to_and_nothing_is_lost() {
        use std::io::Write;
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("log");
        let mut w = fs::File::options()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let mut r = LogReader::default();
        assert_eq!(r.poll(&d.path().join("absent")), LogView::default());
        write!(w, "wlan0: AP-ENABLED \nwlan0: AP-STA-CONN").unwrap();
        let v = r.poll(&path);
        assert!(v.enabled);
        assert_eq!(v.clients, 0, "half a line waits");
        writeln!(w, "ECTED aa").unwrap();
        assert_eq!(r.poll(&path).clients, 1);
        assert_eq!(r.poll(&path).clients, 1, "a line counts once");

        // More than one read's worth between two polls: every line counts.
        for i in 0..20_000 {
            let e = if i % 2 == 0 { "DIS" } else { "" };
            writeln!(w, "wlan0: AP-STA-{e}CONNECTED aa:bb:cc:dd:ee:ff").unwrap();
        }
        for _ in 0..50 {
            r.poll(&path);
        }
        assert_eq!(r.poll(&path).clients, 1);
        assert!(!r.poll(&path).disabled);
        writeln!(w, "wlan0: AP-DISABLED ").unwrap();
        assert!(r.poll(&path).disabled);
    }

    #[test]
    fn a_log_that_is_long_ends_the_access_point_at_once() {
        // Read from the start, the position is still small: the size says it.
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("log");
        let f = fs::File::create(&path).unwrap();
        f.set_len(LOG_LIMIT + 1).unwrap();
        let mut r = LogReader::default();
        assert!(r.poll(&path).disabled);
    }

    #[test]
    fn a_log_past_the_limit_ends_the_access_point() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("log");
        let line = "wlan0: AP-STA-CONNECTED aa:bb:cc:dd:ee:ff\n";
        fs::write(&path, line.repeat((LOG_LIMIT as usize) / line.len() + 1)).unwrap();
        let mut r = LogReader::default();
        for _ in 0..(LOG_LIMIT / LOG_KEEP + 2) {
            r.poll(&path);
        }
        assert!(r.poll(&path).disabled);
    }

    #[test]
    fn a_pid_file_names_a_daemon_only_with_its_command_line() {
        let d = tempfile::tempdir().unwrap();
        let paths = ApPaths {
            run_dir: d.path().to_path_buf(),
            firmware_param: d.path().join("p"),
            stock_firmware: d.path().join("f"),
        };
        let conf = paths.file("hostapd.conf").display().to_string();
        let ours = format!("hostapd -e x {conf}");
        let r = Radio::recording("wlan0", Default::default()).with_processes(
            Box::new(move |pid| match pid {
                "10" => Some(ours.clone()),
                "11" => Some("/sbin/syslogd -n".into()),
                _ => None,
            }),
            d.path(),
        );
        assert!(r.ap_alive(&paths), "not started yet");
        for dead in ["", "\n", "x\n", "99\n", "11\n"] {
            fs::write(paths.hostapd_pid(), dead).unwrap();
            assert!(!r.ap_alive(&paths), "{dead:?}");
        }
        fs::write(paths.hostapd_pid(), "10\n").unwrap();
        assert!(r.ap_alive(&paths));
    }

    #[test]
    fn a_stale_pid_that_is_another_process_now_gets_no_signal() {
        let d = tempfile::tempdir().unwrap();
        let paths = ApPaths {
            run_dir: d.path().join("ap"),
            firmware_param: d.path().join("p"),
            stock_firmware: d.path().join("f"),
        };
        fs::create_dir(&paths.run_dir).unwrap();
        fs::write(paths.hostapd_pid(), "11\n").unwrap();
        fs::write(paths.file("udhcpd.pid"), "12\n").unwrap();
        let udhcpd = format!(
            "busybox-extras udhcpd -f {}",
            paths.file("udhcpd.conf").display()
        );
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut r = Radio::recording("wlan0", log.clone()).with_processes(
            Box::new(move |pid| match pid {
                "11" => Some("/usr/sbin/sshd -D".into()),
                "12" => Some(udhcpd.clone()),
                _ => None,
            }),
            d.path(),
        );
        r.ap_stop_daemons(&paths);
        r.ap_teardown(&paths);
        let log = log.lock().unwrap().clone();
        assert!(log.contains(&"kill -TERM 12".to_string()), "{log:?}");
        assert!(log.contains(&"kill -KILL 12".to_string()), "{log:?}");
        assert!(!log.iter().any(|l| l.contains("11")), "{log:?}");
    }

    #[test]
    fn reads_the_events_in_the_log() {
        let v = read_log(
            "wlan0: interface state UNINITIALIZED->ENABLED\nwlan0: AP-ENABLED \n\
             wlan0: AP-STA-CONNECTED aa\nwlan0: AP-STA-CONNECTED bb\nwlan0: AP-STA-DISCONNECTED aa\n",
        );
        assert_eq!(
            v,
            LogView {
                enabled: true,
                disabled: false,
                clients: 1
            }
        );
        assert!(read_log("wlan0: AP-DISABLED \n").disabled);
        assert_eq!(read_log("wlan0: AP-STA-DISCONNECTED aa\n").clients, 0);
    }

    #[test]
    fn the_install_keeps_the_password_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let (tried, conf) = (d.path().join("t"), d.path().join("c"));
        write_private(&tried, b"new").unwrap();
        fs::write(&conf, "old").unwrap();
        install_conf(&tried, &conf).unwrap();
        assert_eq!(fs::read_to_string(&conf).unwrap(), "new");
        assert_eq!(
            fs::metadata(&conf).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!tried.exists());
    }
}
