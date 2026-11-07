use super::*;
use crate::radio::{Outcome, Runner};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::sync::{Arc, Mutex};

/// What the system answers. `lease`: the address `ip` reports once a supplicant started.
#[derive(Default)]
struct World {
    log: Vec<String>,
    no_tools: bool,
    link_up_fails: bool,
    firmware_line: Option<String>,
    lease: Option<String>,
}

type SharedWorld = Arc<Mutex<World>>;

fn runner(world: SharedWorld) -> Runner {
    Box::new(move |p, a| {
        let mut w = world.lock().unwrap();
        let line = std::iter::once(p)
            .chain(a.iter().copied())
            .collect::<Vec<_>>()
            .join(" ");
        w.log.push(line.clone());
        let mut out = Outcome {
            ok: true,
            stdout: String::new(),
        };
        if line.contains("command -v") {
            out.ok = !w.no_tools;
        } else if line.contains("link set wlan0 up") {
            out.ok = !w.link_up_fails;
        } else if line.contains("dmesg") {
            out.stdout = w.firmware_line.clone().unwrap_or_default();
        } else if line.starts_with("ip -4 -o addr show") {
            let started = w.log.iter().any(|c| c.starts_with("wpa_supplicant"));
            if let (true, Some(ip)) = (started, &w.lease) {
                out.stdout = format!("3: wlan0 inet {ip}/24 brd x scope global");
            }
        }
        Ok(out)
    })
}

struct Rig {
    _dir: tempfile::TempDir,
    world: SharedWorld,
    radio: Radio,
    setup: WifiSetup,
    conf: PathBuf,
    now: u64,
}

const AP_MODE: &str = "[ 14.38] Firmware up: op_mode=0x0002, MAC=f8:8f:ca:11:a2:24";

impl Rig {
    fn new() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (param, stock) = (root.join("firmware_path"), root.join("fw_bcmdhd.bin"));
        std::fs::write(&param, "").unwrap();
        std::fs::write(&stock, "firmware").unwrap();
        let world: SharedWorld = Arc::new(Mutex::new(World {
            firmware_line: Some(AP_MODE.into()),
            ..Default::default()
        }));
        let conf = root.join("wifi.conf");
        let radio = Radio::with_runner("wlan0", runner(world.clone())).with_wifi_conf(conf.clone());
        let setup = WifiSetup::new(Config {
            paths: ApPaths {
                run_dir: root.join("ap"),
                firmware_param: param,
                stock_firmware: stock,
            },
            binding: Binding {
                device: "lo".into(),
                ip: Ipv4Addr::LOCALHOST,
                port: 0,
            },
            wifi_conf: conf.clone(),
            timeouts: Timeouts::default(),
            assume_enabled: false,
        });
        Rig {
            _dir: dir,
            world,
            radio,
            setup,
            conf,
            now: 1_000_000,
        }
    }

    fn ctx() -> Context {
        Context {
            enabled: true,
            airplane: false,
            pool_ready: true,
            lang: Lang::En,
        }
    }

    fn start(&mut self) -> Update {
        self.setup.start(&mut self.radio, Self::ctx(), self.now)
    }

    fn cmds(&self) -> Vec<String> {
        self.world.lock().unwrap().log.clone()
    }

    fn hostapd_says(&self, text: &str) {
        let mut f = std::fs::File::options()
            .create(true)
            .append(true)
            .open(self.setup.cfg.paths.hostapd_log())
            .unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    /// Runs the clock to the next deadline, and one tick there.
    fn step(&mut self) -> Update {
        self.now = self
            .setup
            .next_deadline()
            .expect("a deadline")
            .max(self.now);
        self.setup.tick(&mut self.radio, self.now)
    }

    /// Ticks until a state is published with this phase, returning it.
    fn until(&mut self, phase: WifiSetupPhase) -> (WifiSetupState, Option<Finished>) {
        for _ in 0..2000 {
            let u = self.step();
            if let Some(s) = u.state
                && s.phase == phase as i32
            {
                return (s, u.finished);
            }
        }
        panic!("never reached {phase:?}");
    }

    fn serve(&mut self) -> WifiSetupState {
        let u = self.start();
        assert_eq!(u.state.unwrap().phase, WifiSetupPhase::Starting as i32);
        self.step(); // link down
        self.step(); // firmware, hostapd
        self.hostapd_says("wlan0: AP-ENABLED \n");
        self.until(WifiSetupPhase::Serving).0
    }

    /// Sends a form through the real socket.
    fn send_form(&mut self, s: &WifiSetupState, form: impl Fn(&str) -> String) -> Update {
        let host = s
            .url
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
        let token = {
            let Phase::Serving { creds, .. } = &self.setup.phase else {
                panic!("not serving");
            };
            creds.token.clone()
        };
        let body = form(&token);
        let mut c = TcpStream::connect(&host).unwrap();
        write!(
            c,
            "POST /join HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/x-www-form-urlencoded\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut last = Update::default();
        for _ in 0..100 {
            let fds = self.setup.fds();
            let mut pfds: Vec<_> = fds
                .iter()
                .map(|&(fd, events)| libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                })
                .collect();
            // SAFETY: a valid array of pollfd of this length.
            unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, 10) };
            for p in pfds.iter().filter(|p| p.revents != 0) {
                let u = self.setup.on_ready(p.fd, self.now);
                if u.state.is_some() {
                    last = u;
                }
            }
            if last.state.is_some() {
                break;
            }
        }
        let mut reply = String::new();
        c.set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap();
        let _ = c.read_to_string(&mut reply);
        last
    }
}

fn good(token: &str) -> String {
    format!("token={token}&ssid=HomeNet&password=the-real-password")
}

#[test]
fn nothing_starts_unless_it_is_allowed() {
    for (ctx, error) in [
        (
            Context {
                enabled: false,
                ..Rig::ctx()
            },
            ERR_DISABLED,
        ),
        (
            Context {
                airplane: true,
                ..Rig::ctx()
            },
            ERR_AIRPLANE,
        ),
        (
            Context {
                pool_ready: false,
                ..Rig::ctx()
            },
            ERR_NOT_READY,
        ),
    ] {
        let mut r = Rig::new();
        let u = r.setup.start(&mut r.radio, ctx, r.now);
        assert_eq!(u.state.unwrap().error, error);
        assert!(!r.setup.active());
        assert!(r.cmds().is_empty());
    }
}

#[test]
fn missing_tools_or_firmware_change_nothing() {
    let mut r = Rig::new();
    r.world.lock().unwrap().no_tools = true;
    assert_eq!(r.start().state.unwrap().error, ERR_NO_TOOLS);
    assert!(!r.setup.active());

    let mut r = Rig::new();
    std::fs::remove_file(&r.setup.cfg.paths.stock_firmware).unwrap();
    assert_eq!(r.start().state.unwrap().error, ERR_NO_FIRMWARE);
    assert!(r.cmds().is_empty());
}

#[test]
fn the_password_is_random_and_from_the_alphabet() {
    let a = generate(Lang::En).unwrap();
    let b = generate(Lang::En).unwrap();
    assert_ne!(a.password, b.password);
    assert_ne!(a.token, b.token);
    assert_eq!(a.password.len(), PASSWORD_LEN);
    assert!(a.password.bytes().all(|c| ALPHABET.contains(&c)));
    assert!(a.ssid.starts_with("OMG-Glass-") && a.ssid.len() == 14);
    assert_eq!(a.entropy[0], 2);
    assert!(!format!("{a:?}").contains(&a.password));
}

#[test]
fn the_password_draws_each_character_evenly() {
    // Every byte value at most once: each character must come up the same number of times.
    let all: Vec<u8> = (0..=255).collect();
    let mut seen = std::collections::HashMap::new();
    for c in alphabet_chars(&all) {
        *seen.entry(c).or_insert(0) += 1;
    }
    assert_eq!(seen.len(), ALPHABET.len());
    let first = *seen.values().next().unwrap();
    assert!(seen.values().all(|&n| n == first));
}

#[test]
fn the_firmware_is_named_for_ap_mode_and_the_password_stays_off_the_command_line() {
    let mut r = Rig::new();
    let s = r.serve();
    assert_eq!(s.phase, WifiSetupPhase::Serving as i32);
    assert_eq!(s.password.len(), PASSWORD_LEN);
    assert_eq!(s.seconds_left, 600);

    let run = r.setup.cfg.paths.run_dir.clone();
    let param = std::fs::read_to_string(&r.setup.cfg.paths.firmware_param).unwrap();
    assert_eq!(
        param.trim_end(),
        run.join("fw_bcmdhd_apsta.bin").display().to_string()
    );
    assert!(param.ends_with('\n'));
    assert!(!run.to_string_lossy().contains("_p2p"));

    let c = r.cmds();
    let at = |needle: &str| c.iter().position(|l| l.contains(needle)).expect(needle);
    assert!(at("link set wlan0 down") < at("link set wlan0 up"));
    assert!(at("link set wlan0 up") < at("dmesg"));
    assert!(at("dmesg") < at("hostapd -e"));
    assert!(at("hostapd -e") < at("addr add 192.168.77.1/24"));
    assert!(at("addr add") < at("udhcpd"));
    for line in &c {
        assert!(!line.contains(&s.password), "password in `{line}`");
        assert!(!line.contains(" -d"), "debug level in `{line}`");
    }

    use std::os::unix::fs::PermissionsExt;
    let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(run.clone()), 0o700);
    assert_eq!(mode(run.join("hostapd.conf")), 0o600);
    assert_eq!(mode(run.join("hostapd.entropy")), 0o600);
    let conf = std::fs::read_to_string(run.join("hostapd.conf")).unwrap();
    assert!(conf.contains(&format!("wpa_passphrase={}\n", s.password)));
    assert_eq!(
        std::fs::read(run.join("hostapd.entropy")).unwrap().len(),
        21
    );
}

#[test]
fn a_firmware_in_the_wrong_mode_is_not_believed() {
    let mut r = Rig::new();
    r.world.lock().unwrap().firmware_line = Some("Firmware up: op_mode=0x0005, MAC=f8".into());
    r.start();
    r.step();
    let u = r.step();
    assert_eq!(u.state.unwrap().phase, WifiSetupPhase::Stopping as i32);
    assert!(!r.cmds().iter().any(|l| l.contains("hostapd -e")));
    let (s, finished) = r.until(WifiSetupPhase::Failed);
    assert_eq!(s.error, ERR_FIRMWARE_MODE);
    assert_eq!(finished, Some(Finished::Restore));
    assert!(!r.setup.cfg.paths.run_dir.exists());
    assert_eq!(
        std::fs::read_to_string(&r.setup.cfg.paths.firmware_param).unwrap(),
        "\n"
    );
}

#[test]
fn a_link_that_will_not_come_up_ends_the_session() {
    let mut r = Rig::new();
    r.world.lock().unwrap().link_up_fails = true;
    r.start();
    let (s, _) = r.until(WifiSetupPhase::Failed);
    assert_eq!(s.error, ERR_LINK_UP);
}

#[test]
fn an_access_point_that_never_comes_up_ends_the_session() {
    let mut r = Rig::new();
    r.start();
    let (s, finished) = r.until(WifiSetupPhase::Failed);
    assert_eq!(s.error, ERR_AP);
    assert_eq!(finished, Some(Finished::Restore));
    assert!(r.now > 30_000_000, "gave it the 30 s");
    assert!(!r.setup.active());
}

#[test]
fn the_session_times_out_and_leaves_nothing_behind() {
    let mut r = Rig::new();
    let s = r.serve();
    let run = r.setup.cfg.paths.run_dir.clone();
    assert!(run.exists());
    let started = r.now;
    let (end, finished) = r.until(WifiSetupPhase::Failed);
    assert_eq!(end.error, ERR_TIMEOUT);
    assert_eq!(finished, Some(Finished::Restore));
    assert!(r.now >= started + 600_000_000);
    assert!(end.password.is_empty() && end.ssid.is_empty());
    assert!(!run.exists(), "the password's file is gone");
    assert!(r.setup.fds().is_empty());
    assert!(TcpStream::connect(s.url.trim_start_matches("http://").trim_end_matches('/')).is_err());
    let c = r.cmds();
    let last = |needle: &str| c.iter().rposition(|l| l.contains(needle)).expect(needle);
    assert!(last("addr flush") < last("link set wlan0 down"));
    assert_eq!(
        std::fs::read_to_string(&r.setup.cfg.paths.firmware_param).unwrap(),
        "\n"
    );
}

#[test]
fn a_cancel_ends_the_session_at_any_stage() {
    let mut r = Rig::new();
    r.serve();
    let u = r.setup.stop(&mut r.radio, r.now);
    assert_eq!(u.state.unwrap().phase, WifiSetupPhase::Stopping as i32);
    let (_, finished) = r.until(WifiSetupPhase::Off);
    assert_eq!(finished, Some(Finished::Restore));
    assert!(!r.setup.active());

    let mut r = Rig::new();
    r.start();
    r.setup.stop(&mut r.radio, r.now);
    r.until(WifiSetupPhase::Off);
    assert!(!r.setup.active());

    // A second stop while stopping changes nothing.
    let mut r = Rig::new();
    r.serve();
    r.setup.stop(&mut r.radio, r.now);
    r.setup.stop(&mut r.radio, r.now);
    r.until(WifiSetupPhase::Off);
}

#[test]
fn a_second_start_keeps_the_session_and_its_password() {
    let mut r = Rig::new();
    let s = r.serve();
    let again = r.start().state.unwrap();
    assert_eq!(again.password, s.password);
}

#[test]
fn clients_on_the_access_point_are_reported() {
    let mut r = Rig::new();
    r.serve();
    r.hostapd_says("wlan0: AP-STA-CONNECTED aa:bb\n");
    let mut seen = None;
    for _ in 0..3 {
        if let Some(s) = r.step().state {
            seen = Some(s);
            break;
        }
    }
    assert_eq!(seen.unwrap().clients, 1);
}

#[test]
fn hostapd_going_away_ends_the_session() {
    let mut r = Rig::new();
    r.serve();
    r.hostapd_says("wlan0: AP-DISABLED \n");
    let (s, _) = r.until(WifiSetupPhase::Failed);
    assert_eq!(s.error, ERR_AP);
}

#[test]
fn a_network_that_works_is_kept_and_the_access_point_is_gone_first() {
    let mut r = Rig::new();
    std::fs::write(&r.conf, "old").unwrap();
    let s = r.serve();
    r.world.lock().unwrap().lease = Some("192.168.1.50".into());
    // The recording runner starts nothing, so what the shell would have left is put here. The
    // pid of a process that exists, since the session checks; the kill is only recorded.
    let pid = r.setup.cfg.paths.run_dir.join("hostapd.pid");
    std::fs::write(pid, format!("{}\n", std::process::id())).unwrap();

    let u = r.send_form(&s, good);
    let joining = u.state.unwrap();
    assert_eq!(joining.phase, WifiSetupPhase::Joining as i32);
    assert_eq!(joining.network, "HomeNet");
    assert!(joining.password.is_empty(), "off the prism at once");
    assert_eq!(std::fs::read_to_string(&r.conf).unwrap(), "old");

    let (end, finished) = r.until(WifiSetupPhase::Joined);
    assert_eq!(finished, Some(Finished::Joined));
    assert_eq!(end.network, "HomeNet");
    let conf = std::fs::read_to_string(&r.conf).unwrap();
    assert!(conf.contains("the-real-password"));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&r.conf).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!r.setup.tried_conf().exists());
    assert!(!r.setup.cfg.paths.run_dir.exists());

    let c = r.cmds();
    let first = |n: &str| c.iter().position(|l| l.contains(n)).expect(n);
    assert!(first("kill -TERM") < first("wpa_supplicant"));
    assert!(c.iter().rposition(|l| l.contains("addr flush")).unwrap() < first("wpa_supplicant"));
    for line in &c {
        assert!(!line.contains("the-real-password"), "password in `{line}`");
    }
    assert_eq!(r.radio.state().0, Some(true));
}

#[test]
fn a_network_that_fails_changes_nothing() {
    let mut r = Rig::new();
    std::fs::write(&r.conf, "old").unwrap();
    let s = r.serve();
    r.send_form(&s, good);
    let (end, finished) = r.until(WifiSetupPhase::Failed);
    assert_eq!(end.error, ERR_JOIN);
    assert_eq!(end.network, "HomeNet");
    assert_eq!(finished, Some(Finished::Restore));
    assert_eq!(std::fs::read_to_string(&r.conf).unwrap(), "old");
    assert!(!r.setup.tried_conf().exists(), "the typed password is gone");
    assert!(!r.setup.active());
}

#[test]
fn a_cancel_while_joining_changes_nothing() {
    let mut r = Rig::new();
    std::fs::write(&r.conf, "old").unwrap();
    let s = r.serve();
    r.send_form(&s, good);
    for _ in 0..50 {
        if matches!(r.setup.phase, Phase::Joining { .. }) {
            break;
        }
        r.step();
    }
    assert!(matches!(r.setup.phase, Phase::Joining { .. }));
    let u = r.setup.stop(&mut r.radio, r.now);
    assert_eq!(u.finished, Some(Finished::Restore));
    assert_eq!(std::fs::read_to_string(&r.conf).unwrap(), "old");
    assert!(!r.setup.tried_conf().exists());
    assert!(!r.setup.active());
}

#[test]
fn a_bad_form_keeps_the_session() {
    let mut r = Rig::new();
    let s = r.serve();
    let u = r.send_form(&s, |t| format!("token={t}&ssid=&password=password1"));
    assert!(u.state.is_none());
    assert!(matches!(r.setup.phase, Phase::Serving { .. }));
    let u = r.send_form(&s, good);
    assert_eq!(u.state.unwrap().phase, WifiSetupPhase::Joining as i32);
}

#[test]
fn the_radio_stays_with_the_session_whatever_the_settings_say() {
    let mut r = Rig::new();
    r.serve();
    let before = r.cmds().len();
    r.radio.apply(false, false);
    r.radio.apply(true, false);
    let after = r.cmds();
    assert!(
        after[before..].iter().all(|l| !l.contains("wlan0")
            && !l.contains("rfkill block wifi")
            && !l.contains("unblock wifi")),
        "{:?}",
        &after[before..]
    );
    r.setup.stop(&mut r.radio, r.now);
    r.until(WifiSetupPhase::Off);
    r.world.lock().unwrap().log.clear();
    r.radio.apply(false, false);
    assert!(
        r.cmds().iter().any(|l| l == "rfkill block wifi"),
        "released and driven again"
    );
}

#[test]
fn a_session_left_by_a_dead_daemon_is_ended_at_start() {
    let mut r = Rig::new();
    r.serve();
    let run = r.setup.cfg.paths.run_dir.clone();
    std::fs::write(run.join("hostapd.pid"), "4242\n").unwrap();
    std::fs::write(r.setup.tried_conf(), "secret").unwrap();
    r.world.lock().unwrap().log.clear();

    let (param, tried) = (
        r.setup.cfg.paths.firmware_param.clone(),
        r.setup.tried_conf(),
    );
    r.setup.cleanup_stale(&mut r.radio);
    assert!(!run.exists());
    assert!(!tried.exists());
    assert_eq!(std::fs::read_to_string(param).unwrap(), "\n");
    assert!(r.cmds().iter().any(|l| l == "kill -TERM 4242"));

    // Nothing left: nothing done.
    r.world.lock().unwrap().log.clear();
    r.setup.cleanup_stale(&mut r.radio);
    assert!(r.cmds().is_empty());
}

#[test]
fn the_pid_files_cannot_be_turned_into_arguments() {
    let mut r = Rig::new();
    r.serve();
    let run = r.setup.cfg.paths.run_dir.clone();
    std::fs::write(run.join("hostapd.pid"), "12 -9 ; x\n").unwrap();
    std::fs::write(run.join("udhcpd.pid"), "34\n").unwrap();
    r.radio.ap_stop_daemons(&r.setup.cfg.paths);
    assert_eq!(r.cmds().last().unwrap(), "kill -TERM 12 34");
}
