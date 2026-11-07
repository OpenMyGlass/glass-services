//! `omg-bled`: see the crate docs and `docs/ble-link.md`. glassd starts it and stops it.
//!
//! Standard input is the leash: at its end, when glassd stopped it or died, the process ends and
//! the UART closes. Standard output says `ready` once advertising is on.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use omg_bled::bringup::{self, Uart};
use omg_bled::engine::{Action, Engine};

/// Exit code of a failure that a new try cannot mend: glassd does not start it again.
const EXIT_CONFIG: u8 = 78;
const ADDRESS_FILE: &str = "ble-address";

const USAGE: &str = "usage: omg-bled --socket <path> --state-dir <dir> [options]
  --socket <path>      glassd's Unix socket, connected once for each phone session
  --state-dir <dir>    where the BLE address is kept
  --firmware <path>    the firmware patch on the image (default /vendor/firmware/bcm4330.hcd)
  --tty <path>         the Bluetooth UART (default /dev/ttyO1)
  --baud <rate>        115200, 921600 or 3000000 (default 3000000)
  --rfkill <dir|none>  rfkill class directory, for a power cycle (default /sys/class/rfkill)
  --name <text>        the advertised name (default OpenMyGlass)";

struct Args {
    socket: PathBuf,
    state_dir: PathBuf,
    firmware: PathBuf,
    tty: PathBuf,
    baud: u32,
    rfkill: Option<PathBuf>,
    name: String,
}

fn parse_args() -> Result<Args, String> {
    let (mut socket, mut state_dir) = (None, None);
    let mut a = Args {
        socket: PathBuf::new(),
        state_dir: PathBuf::new(),
        firmware: "/vendor/firmware/bcm4330.hcd".into(),
        tty: "/dev/ttyO1".into(),
        baud: 3_000_000,
        rfkill: Some("/sys/class/rfkill".into()),
        name: "OpenMyGlass".into(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(key) = args.next() {
        let mut value = || args.next().ok_or(format!("{key} needs a value"));
        match key.as_str() {
            "--socket" => socket = Some(PathBuf::from(value()?)),
            "--state-dir" => state_dir = Some(PathBuf::from(value()?)),
            "--firmware" => a.firmware = value()?.into(),
            "--tty" => a.tty = value()?.into(),
            "--baud" => {
                let v = value()?;
                a.baud = v
                    .parse()
                    .map_err(|_| format!("--baud: not a number: {v}"))?;
            }
            "--rfkill" => {
                let v = value()?;
                a.rfkill = (v != "none").then(|| v.into());
            }
            "--name" => a.name = value()?,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    a.socket = socket.ok_or("--socket is required")?;
    a.state_dir = state_dir.ok_or("--state-dir is required")?;
    // A scan response has 29 bytes for the name.
    if a.name.is_empty() || a.name.len() > 29 {
        return Err("--name must be 1 to 29 bytes".into());
    }
    Ok(a)
}

/// A static random address, made once and kept. The phone reconnects to it, and the chip has no
/// public address of its own.
fn address(state_dir: &Path) -> Result<[u8; 6], String> {
    let path = state_dir.join(ADDRESS_FILE);
    if let Ok(saved) = fs::read(&path)
        && let Ok(a) = <[u8; 6]>::try_from(saved.as_slice())
        && a[5] & 0xC0 == 0xC0
    {
        return Ok(a);
    }
    let mut a = [0u8; 6];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut a))
        .map_err(|e| format!("/dev/urandom: {e}"))?;
    // The two top bits mark a static random address. Not a secret, so any random will do.
    a[5] |= 0xC0;
    fs::create_dir_all(state_dir).map_err(|e| format!("{}: {e}", state_dir.display()))?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, a)
        .and_then(|()| fs::rename(&tmp, &path))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(a)
}

fn log(line: &str) {
    eprintln!("omg-bled: {line}");
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    let firmware = match fs::read(&args.firmware) {
        Ok(f) => f,
        Err(e) => {
            log(&format!("firmware {}: {e}", args.firmware.display()));
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    let address = match address(&args.state_dir) {
        Ok(a) => a,
        Err(e) => {
            log(&e);
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    if let Some(root) = &args.rfkill
        && let Err(e) = bringup::power_cycle(root)
    {
        log(&format!("power cycle: {e}"));
        return ExitCode::FAILURE;
    }
    let start = Instant::now();
    let mut uart = match Uart::open(&args.tty) {
        Ok(u) => u,
        Err(e) => {
            log(&e);
            return ExitCode::FAILURE;
        }
    };
    let controller = match bringup::bring_up(&mut uart, &firmware, args.baud) {
        Ok(c) => c,
        Err(e) => {
            log(&format!("bring-up failed: {e}"));
            discard_output(uart.fd());
            return ExitCode::FAILURE;
        }
    };
    drop(firmware);
    log(&format!(
        "controller up in {} ms, ACL {}x{}",
        start.elapsed().as_millis(),
        controller.acl_len,
        controller.acl_credits
    ));
    let mut tty = uart.into_file();
    let engine = Engine::new(
        &args.name,
        address,
        controller.acl_len,
        controller.acl_credits,
        start.elapsed().as_millis() as u64,
    );
    match run(engine, &mut tty, &args.socket, start) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log(&e);
            discard_output(tty.as_raw_fd());
            ExitCode::FAILURE
        }
    }
}

/// The one loop. It sleeps in `poll` until the UART, the session or a one-shot deadline needs it.
fn run(engine: Engine, tty: &mut fs::File, socket: &Path, start: Instant) -> Result<(), String> {
    let mut engine = engine;
    let now_ms = || start.elapsed().as_millis() as u64;
    let mut session: Option<UnixStream> = None;
    // The session's far end closed, seen while there was no room to read what remains.
    let mut session_hup = false;
    let mut ready_said = false;
    let mut buf = [0u8; 2048];
    loop {
        for line in engine.take_logs() {
            log(&line);
        }
        if let Some(e) = engine.fatal() {
            return Err(e.to_string());
        }
        if engine.started() && !ready_said {
            ready_said = true;
            let mut out = std::io::stdout();
            let _ = out.write_all(b"ready\n").and_then(|()| out.flush());
            log("advertising");
        }
        for action in engine.take_actions() {
            match action {
                Action::OpenSession => match connect(socket) {
                    Ok(s) => {
                        session = Some(s);
                        session_hup = false;
                        engine.session_opened(now_ms());
                    }
                    Err(e) => {
                        log(&format!("{}: {e}", socket.display()));
                        engine.session_failed();
                    }
                },
                Action::CloseSession => session = None,
            }
        }

        let timeout = match engine.next_deadline() {
            None => -1,
            Some(d) => d.saturating_sub(now_ms()).min(i32::MAX as u64) as i32,
        };
        let room = engine.session_room();
        let mut fds = [
            libc::pollfd {
                fd: tty.as_raw_fd(),
                events: (if engine.wants_hci_read() {
                    libc::POLLIN
                } else {
                    0
                }) | (if engine.hci_output().is_empty() {
                    0
                } else {
                    libc::POLLOUT
                }),
                revents: 0,
            },
            libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                // A closed peer stays readable or hung up for as long as it is polled, so with no
                // room to read what remains it leaves the set until there is room.
                fd: match &session {
                    Some(s) if room > 0 || !session_hup => s.as_raw_fd(),
                    _ => -1,
                },
                events: (if room > 0 { libc::POLLIN } else { 0 })
                    | (if engine.to_session().is_empty() {
                        0
                    } else {
                        libc::POLLOUT
                    }),
                revents: 0,
            },
        ];
        // SAFETY: a valid array of pollfd of the given length.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("poll: {e}"));
        }
        let now = now_ms();

        if fds[1].revents != 0 {
            let mut b = [0u8; 16];
            // SAFETY: reading into a buffer of the given length, from standard input.
            let got = unsafe { libc::read(0, b.as_mut_ptr().cast(), b.len()) };
            if got <= 0 {
                discard_output(tty.as_raw_fd());
                log("stopping");
                return Ok(());
            }
        }

        if fds[0].revents & libc::POLLOUT != 0 || !engine.hci_output().is_empty() {
            match tty.write(engine.hci_output()) {
                Ok(n) => engine.hci_written(n),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(format!("UART write: {e}")),
            }
        }
        if fds[0].revents & libc::POLLIN != 0 {
            match tty.read(&mut buf) {
                Ok(0) => {}
                Ok(n) => engine.on_hci(&buf[..n], now),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(format!("UART read: {e}")),
            }
        }
        if fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err("the UART went away".into());
        }
        // After the read: an answer that woke the loop is not judged late before it is seen.
        engine.tick(now);

        let mut closed = false;
        if let Some(s) = &mut session {
            if !engine.to_session().is_empty() {
                match s.write(engine.to_session()) {
                    Ok(n) => engine.to_session_written(n),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(_) => closed = true,
                }
            }
            let revents = fds[2].revents;
            if revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                session_hup = true;
            }
            let room = engine.session_room();
            if room > 0 && revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                let want = room.min(buf.len());
                match s.read(&mut buf[..want]) {
                    Ok(0) => closed = true,
                    Ok(n) => engine.session_data(&buf[..n], now),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(_) => closed = true,
                }
            }
        }
        if closed {
            session = None;
            session_hup = false;
            engine.session_closed(now);
        }
    }
}

/// So that closing the port does not wait for a chip that went away: with bytes queued, the kernel
/// holds the close for up to `closing_wait`, 30 s by default.
fn discard_output(fd: std::os::fd::RawFd) {
    // SAFETY: on our own descriptor.
    unsafe { libc::tcflush(fd, libc::TCIOFLUSH) };
}

fn connect(socket: &Path) -> std::io::Result<UnixStream> {
    let s = UnixStream::connect(socket)?;
    s.set_nonblocking(true)?;
    Ok(s)
}
