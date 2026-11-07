//! Waking the chip: power cycle, firmware patch, baud rate, sleep mode. Blocks for several
//! seconds at 115200 baud, which is the reason this is its own process and not part of glassd.
//!
//! The sequence is the one libbt uses on Glass (`hardware.c`, android-4.4.4_r2.0.1) and the one
//! measured on this chip with `tools/ble-proto` in OpenMyGlass (out/kmsg_ble1.txt).

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::hci::{self, H4Parser, Packet};

/// `Write Sleep Mode`: UART sleep with BT_WAKE and HOST_WAKE active high, 300 ms idle thresholds.
/// libbt's defaults, which matched how the kernel drives the two lines. Without it HOST_WAKE stays
/// quiet, and the kernel stops the UART once the host goes quiet.
const SLEEP_MODE: [u8; 12] = [1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];

/// What the controller reported, for the [`Engine`](crate::engine::Engine).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Controller {
    pub acl_len: usize,
    pub acl_credits: u16,
}

/// The UART, open and raw.
pub struct Uart {
    file: File,
    parser: H4Parser,
    out: Vec<u8>,
}

impl Uart {
    /// Opens it nonblocking, at 115200 with RTS/CTS.
    pub fn open(path: &Path) -> Result<Uart, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        set_baud(file.as_raw_fd(), libc::B115200)?;
        Ok(Uart {
            file,
            parser: H4Parser::default(),
            out: Vec::new(),
        })
    }

    pub fn fd(&self) -> i32 {
        self.file.as_raw_fd()
    }

    /// One command, with nothing else in flight. Returns the parameters after the status.
    pub fn command(&mut self, op: u16, params: &[u8], timeout_ms: u64) -> Result<Vec<u8>, String> {
        self.out.extend(hci::command(op, params));
        let end = Instant::now() + Duration::from_millis(timeout_ms);
        let mut buf = [0u8; 512];
        while Instant::now() < end {
            self.flush();
            let mut p = libc::pollfd {
                fd: self.fd(),
                events: libc::POLLIN
                    | if self.out.is_empty() {
                        0
                    } else {
                        libc::POLLOUT
                    },
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            unsafe { libc::poll(&mut p, 1, 20) };
            // SAFETY: reading into a buffer of the given length.
            let n = unsafe { libc::read(self.fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                self.parser.push(&buf[..n as usize]);
            }
            while let Some(packet) = self.parser.next_packet() {
                let Packet::Event { code, params: e } = packet else {
                    continue;
                };
                if code == hci::EV_COMMAND_COMPLETE
                    && e.len() >= 4
                    && u16::from_le_bytes([e[1], e[2]]) == op
                {
                    if e[3] != 0 {
                        return Err(format!("command {op:04x}: status {:02x}", e[3]));
                    }
                    return Ok(e[4..].to_vec());
                }
                if code == hci::EV_COMMAND_STATUS
                    && e.len() >= 4
                    && u16::from_le_bytes([e[2], e[3]]) == op
                    && e[0] != 0
                {
                    return Err(format!("command {op:04x}: status {:02x}", e[0]));
                }
            }
        }
        self.out.clear();
        Err(format!("command {op:04x}: timed out"))
    }

    fn flush(&mut self) {
        while !self.out.is_empty() {
            // SAFETY: writing from a buffer of the given length.
            let n = unsafe { libc::write(self.fd(), self.out.as_ptr().cast(), self.out.len()) };
            if n <= 0 {
                return;
            }
            self.out.drain(..n as usize);
        }
    }

    fn reset(&mut self) -> Result<(), String> {
        let mut last = String::new();
        for _ in 0..5 {
            match self.command(hci::OP_RESET, &[], 1000) {
                Ok(_) => return Ok(()),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Past the bring-up: hands over the descriptor. Nothing is in flight by then.
    pub fn into_file(self) -> File {
        self.file
    }
}

fn set_baud(fd: i32, baud: libc::speed_t) -> Result<(), String> {
    // SAFETY: termios is plain data, and it is filled by tcgetattr before use.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) != 0 {
            return Err(format!("tcgetattr: {}", std::io::Error::last_os_error()));
        }
        libc::cfmakeraw(&mut t);
        t.c_cflag |= libc::CRTSCTS | libc::CLOCAL | libc::CREAD;
        t.c_cc[libc::VMIN] = 0;
        t.c_cc[libc::VTIME] = 0;
        libc::cfsetspeed(&mut t, baud);
        if libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
            return Err(format!("tcsetattr: {}", std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

/// Power-cycles the chip, so the patch always meets the ROM, whatever a previous run left. The
/// Bluetooth rfkill's `state` is under `root`.
pub fn power_cycle(root: &Path) -> Result<(), String> {
    for entry in fs::read_dir(root).map_err(|e| format!("{}: {e}", root.display()))? {
        let path = entry.map_err(|e| e.to_string())?.path();
        let kind = fs::read_to_string(path.join("type")).unwrap_or_default();
        if kind.trim() == "bluetooth" {
            let state = path.join("state");
            fs::write(&state, "0").map_err(|e| e.to_string())?;
            std::thread::sleep(Duration::from_millis(200));
            fs::write(&state, "1").map_err(|e| e.to_string())?;
            std::thread::sleep(Duration::from_millis(300));
            return Ok(());
        }
    }
    Err(format!("no Bluetooth rfkill under {}", root.display()))
}

/// Brings the controller from reset to ready for LE. `firmware` is the patch as read from the
/// image; it is a vendor file, and never part of this repository.
pub fn bring_up(uart: &mut Uart, firmware: &[u8], baud: u32) -> Result<Controller, String> {
    let records = hci::parse_hcd(firmware)?;
    uart.reset()?;
    uart.command(hci::OP_VS_MINIDRIVER, &[], 2000)?;
    std::thread::sleep(Duration::from_millis(50));
    for (op, data) in &records {
        match uart.command(*op, data, 2000) {
            Ok(_) => {}
            // The patched firmware may restart before it answers.
            Err(_) if *op == hci::OP_VS_LAUNCH_RAM => {}
            Err(e) => return Err(format!("patch: {e}")),
        }
    }
    std::thread::sleep(Duration::from_millis(250));
    uart.parser.clear();
    uart.reset()?;
    if baud != 115200 {
        let mut p = vec![0, 0];
        p.extend_from_slice(&baud.to_le_bytes());
        uart.command(hci::OP_VS_UPDATE_BAUD, &p, 1000)?;
        set_baud(uart.fd(), speed(baud)?)?;
        std::thread::sleep(Duration::from_millis(100));
        uart.command(hci::OP_READ_LOCAL_NAME, &[], 1000)
            .map_err(|e| format!("after the baud change: {e}"))?;
    }
    uart.command(hci::OP_VS_SLEEP_MODE, &SLEEP_MODE, 1000)?;
    read_buffers(uart)
}

fn speed(baud: u32) -> Result<libc::speed_t, String> {
    match baud {
        115200 => Ok(libc::B115200),
        921600 => Ok(libc::B921600),
        3000000 => Ok(libc::B3000000),
        _ => Err(format!("unsupported baud rate {baud}")),
    }
}

fn read_buffers(uart: &mut Uart) -> Result<Controller, String> {
    let le = uart.command(hci::OP_LE_READ_BUFFER_SIZE, &[], 1000)?;
    let (mut acl_len, mut credits) = if le.len() >= 3 {
        (u16::from_le_bytes([le[0], le[1]]) as usize, le[2] as u16)
    } else {
        (0, 0)
    };
    // Zero means that LE shares the classic ACL buffers.
    if acl_len == 0 || credits == 0 {
        let b = uart.command(hci::OP_READ_BUFFER_SIZE, &[], 1000)?;
        if b.len() >= 5 {
            acl_len = u16::from_le_bytes([b[0], b[1]]) as usize;
            credits = u16::from_le_bytes([b[3], b[4]]);
        }
    }
    if acl_len == 0 || credits == 0 {
        return Err("the controller reported no ACL buffers".into());
    }
    Ok(Controller {
        acl_len,
        acl_credits: credits,
    })
}
