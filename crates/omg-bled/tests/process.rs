//! The real `omg-bled` process on a pseudo terminal, with the fake controller at the other end:
//! the bring-up, the poll loop and the session socket, which the engine's own tests do not reach.

use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use omg_bled::fake::FakeController;
use omg_bled::hci;

struct Pty {
    master: File,
    _slave: File,
    path: String,
}

fn pty() -> Pty {
    // SAFETY: standard pty setup; every result is checked.
    unsafe {
        let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK);
        assert!(m >= 0);
        let master = OwnedFd::from_raw_fd(m);
        assert_eq!(libc::grantpt(m), 0);
        assert_eq!(libc::unlockpt(m), 0);
        let mut name = [0 as libc::c_char; 128];
        assert_eq!(libc::ptsname_r(m, name.as_mut_ptr(), name.len()), 0);
        let path = std::ffi::CStr::from_ptr(name.as_ptr())
            .to_str()
            .unwrap()
            .to_string();
        // Raw from the start, and held open, so that the master never reads an error.
        let slave = File::options().read(true).write(true).open(&path).unwrap();
        let mut t: libc::termios = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(slave.as_raw_fd(), &mut t), 0);
        libc::cfmakeraw(&mut t);
        assert_eq!(libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &t), 0);
        Pty {
            master: File::from(master),
            _slave: slave,
            path,
        }
    }
}

struct Bench {
    pty: Pty,
    radio: FakeController,
    child: Child,
    stdout: std::process::ChildStdout,
    listener: UnixListener,
    _dir: tempfile::TempDir,
    dir: std::path::PathBuf,
}

fn firmware(path: &Path) {
    // Synthetic: this repository holds no firmware. A Write RAM record and a Launch RAM.
    std::fs::write(path, [0x4C, 0xFC, 2, 1, 2, 0x4E, 0xFC, 0]).unwrap();
}

fn spawn(pty: &Pty, dir: &Path, fw: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_omg-bled"))
        .args(["--tty", &pty.path, "--rfkill", "none", "--baud", "921600"])
        .arg("--firmware")
        .arg(fw)
        .arg("--socket")
        .arg(dir.join("s.sock"))
        .arg("--state-dir")
        .arg(dir.join("state"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap()
}

impl Bench {
    fn new() -> Bench {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().to_path_buf();
        firmware(&dir.join("fw.hcd"));
        let listener = UnixListener::bind(dir.join("s.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let pty = pty();
        let mut child = spawn(&pty, &dir, &dir.join("fw.hcd"));
        let stdout = child.stdout.take().unwrap();
        // SAFETY: on the pipe we hold.
        unsafe {
            libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
        }
        Bench {
            pty,
            radio: FakeController::new(),
            child,
            stdout,
            listener,
            _dir: d,
            dir,
        }
    }

    /// Carries the UART both ways once.
    fn uart(&mut self) {
        let mut buf = [0u8; 4096];
        loop {
            match self.pty.master.read(&mut buf) {
                Ok(n) if n > 0 => self.radio.from_host(&buf[..n]),
                Ok(_) => break,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
        }
        let out = self.radio.take_to_host();
        if !out.is_empty() {
            self.pty.master.write_all(&out).unwrap();
        }
    }

    fn run_until(&mut self, what: &str, mut done: impl FnMut(&mut Bench) -> bool) {
        let end = Instant::now() + Duration::from_secs(20);
        while !done(self) {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            self.uart();
            self.radio.air(4);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn wait_ready(&mut self) {
        let mut said = Vec::new();
        self.run_until("ready", |b| {
            let mut buf = [0u8; 16];
            if let Ok(n) = b.stdout.read(&mut buf) {
                said.extend_from_slice(&buf[..n]);
            }
            said == b"ready\n"
        });
    }

    fn accept_session(&mut self) -> UnixStream {
        let mut session = None;
        self.run_until("the session", |b| {
            session = b.listener.accept().ok().map(|(s, _)| s);
            session.is_some()
        });
        let s = session.unwrap();
        s.set_nonblocking(true).unwrap();
        s
    }

    fn voluntary_switches(&self) -> u64 {
        let status = std::fs::read_to_string(format!("/proc/{}/status", self.child.id())).unwrap();
        status
            .lines()
            .find_map(|l| l.strip_prefix("voluntary_ctxt_switches:"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }
}

impl Drop for Bench {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_process_brings_the_chip_up_serves_a_session_and_stops_with_its_parent() {
    let mut b = Bench::new();
    b.wait_ready();
    // The patch went in order, then the baud, then the sleep mode the deep sleep depends on.
    let ops: Vec<u16> = b.radio.commands.iter().map(|(op, _)| *op).collect();
    let at = |op: u16| ops.iter().position(|o| *o == op).unwrap();
    assert!(at(hci::OP_VS_MINIDRIVER) < at(0xFC4C));
    assert!(at(0xFC4C) < at(hci::OP_VS_LAUNCH_RAM));
    assert!(at(hci::OP_VS_LAUNCH_RAM) < at(hci::OP_VS_UPDATE_BAUD));
    assert!(at(hci::OP_VS_UPDATE_BAUD) < at(hci::OP_VS_SLEEP_MODE));
    assert!(at(hci::OP_VS_SLEEP_MODE) < at(hci::OP_LE_SET_ADV_ENABLE));
    assert!(b.radio.advertising);
    // A static random address, kept for the next start.
    let address = std::fs::read(b.dir.join("state/ble-address")).unwrap();
    assert_eq!(address.len(), 6);
    assert_eq!(address[5] & 0xC0, 0xC0);
    let set = b
        .radio
        .commands
        .iter()
        .find(|(op, _)| *op == hci::OP_LE_SET_RANDOM_ADDRESS)
        .unwrap();
    assert_eq!(set.1, address);

    // Nobody around: this process does not run at all, once the fast advertising is left to run.
    b.run_until("settling", |_| true);
    std::thread::sleep(Duration::from_millis(100));
    let before = b.voluntary_switches();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(b.voluntary_switches() - before, 0, "woke up while idle");

    b.radio.phone_connects();
    b.uart();
    b.radio.phone_exchange_mtu(517);
    b.radio.phone_subscribe(true);
    let mut session = b.accept_session();

    // Toward glassd.
    b.radio.phone_write(b"hello glass");
    let mut got = Vec::new();
    b.run_until("the phone's bytes", |b2| {
        let _ = b2;
        let mut buf = [0u8; 64];
        if let Ok(n) = session.read(&mut buf) {
            got.extend_from_slice(&buf[..n]);
        }
        got == b"hello glass"
    });

    // Toward the phone, more than any single notification or buffer holds.
    let stream: Vec<u8> = (0..50_000).map(|i| (i % 251) as u8).collect();
    let mut sent = 0;
    let mut got = Vec::new();
    b.run_until("the whole stream", |b2| {
        if sent < stream.len()
            && let Ok(n) = session.write(&stream[sent..])
        {
            sent += n;
        }
        got.extend(b2.radio.take_stream());
        got.len() == stream.len()
    });
    assert_eq!(got, stream);
    assert!(b.radio.max_outstanding <= 15);

    // A link that sits with nothing to say keeps the process asleep too.
    b.run_until("settling", |_| true);
    std::thread::sleep(Duration::from_millis(100));
    let before = b.voluntary_switches();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        b.voluntary_switches() - before,
        0,
        "woke up on a quiet link"
    );

    // The phone goes: the session ends, and the next one is a new stream.
    b.radio.disconnect(0x13);
    b.uart();
    let mut ended = false;
    b.run_until("the end of the session", |_| {
        let mut buf = [0u8; 16];
        ended = matches!(session.read(&mut buf), Ok(0));
        ended
    });
    b.run_until("advertising again", |b2| b2.radio.advertising);
    b.radio.phone_connects();
    b.uart();
    b.radio.phone_exchange_mtu(517);
    b.radio.phone_subscribe(true);
    let mut session = b.accept_session();

    // glassd drops the session: the link goes after the last bytes.
    session.write_all(b"bye").unwrap();
    drop(session);
    b.run_until("the link to be cut", |b2| !b2.radio.connected());
    assert_eq!(b.radio.take_stream(), b"bye");

    // The end of standard input ends the process, whatever it was doing.
    drop(b.child.stdin.take());
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(code) = b.child.try_wait().unwrap() {
            assert!(code.success(), "{code:?}");
            break;
        }
        assert!(Instant::now() < end, "still running");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_missing_firmware_says_so_and_leaves_the_chip_alone() {
    let d = tempfile::tempdir().unwrap();
    let pty = pty();
    let mut child = spawn(&pty, d.path(), &d.path().join("absent.hcd"));
    let code = child.wait().unwrap();
    assert_eq!(code.code(), Some(78));
    let mut master = &pty.master;
    let mut buf = [0u8; 8];
    assert!(master.read(&mut buf).is_err(), "something reached the chip");
}

#[test]
fn nobody_listening_drops_the_phone_and_the_next_try_works() {
    let mut b = Bench::new();
    b.wait_ready();
    let path = b.dir.join("s.sock");
    std::fs::remove_file(&path).unwrap();

    b.radio.phone_connects();
    b.uart();
    b.radio.phone_subscribe(true);
    b.run_until("the phone to be dropped", |b2| !b2.radio.connected());
    b.run_until("advertising again", |b2| b2.radio.advertising);

    b.listener = UnixListener::bind(&path).unwrap();
    b.listener.set_nonblocking(true).unwrap();
    b.radio.phone_connects();
    b.uart();
    b.radio.phone_subscribe(true);
    let _session = b.accept_session();
}
