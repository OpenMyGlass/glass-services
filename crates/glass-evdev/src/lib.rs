//! Minimal evdev access over `libc`: open a `/dev/input/event*` node, read its name and axis
//! ranges, switch its clock to `CLOCK_MONOTONIC`, and read `input_event`s without blocking.
//!
//! The `input_event` layout is the kernel's: `struct timeval` made of two C `long`s, then
//! `u16 type`, `u16 code`, `s32 value`. That is 16 bytes on 32-bit ARM and 24 bytes on 64-bit
//! targets. The layout is spelled out with `libc::c_long` rather than `libc::timeval`, because
//! a C library with a 64-bit `time_t` on 32-bit ARM (musl 1.2) would otherwise disagree with
//! the 3.4 kernel.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// The kernel's `struct input_event`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct RawInputEvent {
    pub tv_sec: libc::c_long,
    pub tv_usec: libc::c_long,
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}

/// Size of one `input_event` on this target.
pub const EVENT_SIZE: usize = std::mem::size_of::<RawInputEvent>();

/// One decoded event, time in µs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    pub time_us: u64,
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}

impl From<RawInputEvent> for Event {
    // `c_long` is i32 on 32-bit ARM and i64 on 64-bit targets: the cast is needed on one of them.
    #[allow(clippy::unnecessary_cast)]
    fn from(r: RawInputEvent) -> Self {
        let sec = (r.tv_sec as i64).max(0) as u64;
        let usec = (r.tv_usec as i64).clamp(0, 999_999) as u64;
        Event {
            time_us: sec * 1_000_000 + usec,
            type_: r.type_,
            code: r.code,
            value: r.value,
        }
    }
}

/// Decodes whole `input_event` records from `buf`; trailing bytes of a partial record are
/// ignored (the kernel never returns one).
pub fn parse_events(buf: &[u8]) -> impl Iterator<Item = Event> + '_ {
    buf.chunks_exact(EVENT_SIZE).map(|c| {
        // SAFETY: `c` has exactly EVENT_SIZE bytes and RawInputEvent is plain old data.
        let raw: RawInputEvent = unsafe { std::ptr::read_unaligned(c.as_ptr().cast()) };
        Event::from(raw)
    })
}

/// The kernel's `struct input_absinfo`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AbsInfo {
    pub value: i32,
    pub minimum: i32,
    pub maximum: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

/// ioctl request numbers (the generic Linux `_IOC` encoding, used by ARM and x86).
pub mod ioctl {
    const READ: u32 = 2;
    const WRITE: u32 = 1;

    const fn ioc(dir: u32, nr: u32, size: u32) -> u32 {
        (dir << 30) | (size << 16) | ((b'E' as u32) << 8) | nr
    }

    /// `EVIOCGNAME(len)`.
    pub const fn gname(len: u32) -> u32 {
        ioc(READ, 0x06, len)
    }

    /// `EVIOCGABS(abs)`.
    pub const fn gabs(abs: u16) -> u32 {
        ioc(READ, 0x40 + abs as u32, 24)
    }

    /// `EVIOCGBIT(ev, len)`.
    pub const fn gbit(ev: u16, len: u32) -> u32 {
        ioc(READ, 0x20 + ev as u32, len)
    }

    /// `EVIOCSCLOCKID`.
    pub const SCLOCKID: u32 = ioc(WRITE, 0xa0, 4);
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

fn monotonic_us() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64) * 1_000_000 + (ts.tv_nsec as u64) / 1_000
}

/// An open evdev device, non-blocking.
#[derive(Debug)]
pub struct Device {
    fd: OwnedFd,
    path: PathBuf,
    monotonic: bool,
}

impl Device {
    /// Opens `path` read-only, non-blocking, close-on-exec.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Device> {
        let path = path.as_ref();
        let c = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: `c` is a valid NUL-terminated path.
        let fd = cvt(unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        })?;
        Ok(Device {
            // SAFETY: `fd` was just opened and is owned by nobody else.
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            path: path.to_path_buf(),
            monotonic: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The device name (`EVIOCGNAME`).
    pub fn name(&self) -> io::Result<String> {
        let mut buf = [0u8; 256];
        // SAFETY: the kernel writes at most `buf.len()` bytes into `buf`.
        let n = cvt(unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                ioctl::gname(buf.len() as u32) as _,
                buf.as_mut_ptr(),
            )
        })? as usize;
        let s = &buf[..n.min(buf.len())];
        let end = s.iter().position(|&b| b == 0).unwrap_or(s.len());
        Ok(String::from_utf8_lossy(&s[..end]).into_owned())
    }

    /// The range of an absolute axis (`EVIOCGABS`).
    pub fn abs_info(&self, code: u16) -> io::Result<AbsInfo> {
        let mut info = AbsInfo::default();
        // SAFETY: `info` matches `struct input_absinfo` (24 bytes).
        cvt(unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                ioctl::gabs(code) as _,
                &mut info as *mut AbsInfo,
            )
        })?;
        Ok(info)
    }

    /// Whether the device reports event type `ev` and code `code` (`EVIOCGBIT`).
    pub fn has_code(&self, ev: u16, code: u16) -> io::Result<bool> {
        let mut bits = [0u8; 96];
        // SAFETY: the kernel writes at most `bits.len()` bytes.
        cvt(unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                ioctl::gbit(ev, bits.len() as u32) as _,
                bits.as_mut_ptr(),
            )
        })?;
        let i = code as usize / 8;
        Ok(i < bits.len() && bits[i] & (1 << (code % 8)) != 0)
    }

    /// Asks the kernel to stamp events with `CLOCK_MONOTONIC` (`EVIOCSCLOCKID`, Linux 3.4+).
    /// If that fails, events are stamped with `CLOCK_MONOTONIC` when they are read instead,
    /// and the error is returned for logging.
    pub fn set_clock_monotonic(&mut self) -> io::Result<()> {
        let id: libc::c_int = libc::CLOCK_MONOTONIC as libc::c_int;
        // SAFETY: the ioctl reads one int.
        let r = cvt(unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                ioctl::SCLOCKID as _,
                &id as *const libc::c_int,
            )
        });
        self.monotonic = r.is_ok();
        r.map(|_| ())
    }

    /// Whether event times come from the kernel in `CLOCK_MONOTONIC`.
    pub fn kernel_monotonic(&self) -> bool {
        self.monotonic
    }

    /// Reads the events available now, appending them to `out`; returns how many were read.
    /// Returns `Ok(0)` when nothing is pending. A removed device gives an error (`ENODEV`).
    pub fn read_events(&mut self, out: &mut Vec<Event>) -> io::Result<usize> {
        let mut buf = [0u8; EVENT_SIZE * 64];
        let mut total = 0;
        loop {
            // SAFETY: `buf` is valid for `buf.len()` bytes.
            let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                let e = io::Error::last_os_error();
                return match e.kind() {
                    io::ErrorKind::WouldBlock => Ok(total),
                    io::ErrorKind::Interrupted => continue,
                    _ => Err(e),
                };
            }
            if n == 0 {
                return if total == 0 {
                    Err(io::Error::from_raw_os_error(libc::ENODEV))
                } else {
                    Ok(total)
                };
            }
            let now = (!self.monotonic).then(monotonic_us);
            let before = out.len();
            out.extend(parse_events(&buf[..n as usize]).map(|mut e| {
                if let Some(t) = now {
                    e.time_us = t;
                }
                e
            }));
            total += out.len() - before;
            if (n as usize) < buf.len() {
                return Ok(total);
            }
        }
    }
}

impl AsRawFd for Device {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// Finds the first `event*` node in `dir` whose name is `name`. Nodes that cannot be opened
/// or are not evdev devices are skipped.
pub fn find_by_name_in(dir: impl AsRef<Path>, name: &str) -> io::Result<Option<PathBuf>> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("event"))
        })
        .collect();
    // event2 before event10.
    paths.sort_by_key(|p| {
        let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        (n.len(), n.to_string())
    });
    for p in paths {
        if let Ok(d) = Device::open(&p)
            && d.name().is_ok_and(|n| n == name)
        {
            return Ok(Some(p));
        }
    }
    Ok(None)
}

/// [`find_by_name_in`] over `/dev/input`.
pub fn find_by_name(name: &str) -> io::Result<Option<PathBuf>> {
    find_by_name_in("/dev/input", name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_of(evs: &[RawInputEvent]) -> Vec<u8> {
        let mut v = Vec::new();
        for e in evs {
            // SAFETY: RawInputEvent is plain old data.
            let b: &[u8] = unsafe {
                std::slice::from_raw_parts((e as *const RawInputEvent).cast(), EVENT_SIZE)
            };
            v.extend_from_slice(b);
        }
        v
    }

    #[test]
    fn layout_matches_the_kernel() {
        let long = std::mem::size_of::<libc::c_long>();
        assert_eq!(EVENT_SIZE, 2 * long + 8);
        if cfg!(target_pointer_width = "32") {
            assert_eq!(EVENT_SIZE, 16);
        } else {
            assert_eq!(EVENT_SIZE, 24);
        }
        assert_eq!(std::mem::size_of::<AbsInfo>(), 24);
    }

    #[test]
    fn parses_events_and_times() {
        let raw = [
            RawInputEvent {
                tv_sec: 12,
                tv_usec: 345_678,
                type_: 3,
                code: 0x35,
                value: 512,
            },
            RawInputEvent {
                tv_sec: 12,
                tv_usec: 345_679,
                type_: 0,
                code: 0,
                value: -1,
            },
        ];
        let mut b = bytes_of(&raw);
        b.extend_from_slice(&[1, 2, 3]); // partial record ignored
        let evs: Vec<Event> = parse_events(&b).collect();
        assert_eq!(
            evs,
            vec![
                Event {
                    time_us: 12_345_678,
                    type_: 3,
                    code: 0x35,
                    value: 512
                },
                Event {
                    time_us: 12_345_679,
                    type_: 0,
                    code: 0,
                    value: -1
                },
            ]
        );
    }

    #[test]
    fn parses_unaligned_buffers() {
        let raw = [RawInputEvent {
            tv_sec: 1,
            tv_usec: 2,
            type_: 3,
            code: 4,
            value: 5,
        }];
        let mut b = vec![0u8];
        b.extend(bytes_of(&raw));
        let e = parse_events(&b[1..]).next().unwrap();
        assert_eq!((e.time_us, e.type_, e.code, e.value), (1_000_002, 3, 4, 5));
    }

    #[test]
    fn negative_times_clamp_to_zero() {
        let raw = [RawInputEvent {
            tv_sec: -1,
            tv_usec: -5,
            ..Default::default()
        }];
        assert_eq!(parse_events(&bytes_of(&raw)).next().unwrap().time_us, 0);
    }

    #[test]
    fn ioctl_numbers() {
        // Values from linux/input.h on ARM and x86.
        assert_eq!(ioctl::gname(256), 0x8100_4506);
        assert_eq!(ioctl::gabs(0x35), 0x8018_4575);
        assert_eq!(ioctl::gabs(0), 0x8018_4540);
        assert_eq!(ioctl::gbit(3, 8), 0x8008_4523);
        assert_eq!(ioctl::SCLOCKID, 0x4004_45a0);
    }

    #[test]
    fn find_by_name_skips_non_devices() {
        let dir = std::env::temp_dir().join(format!("glass-evdev-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("event0"), b"not a device").unwrap();
        fs::write(dir.join("mice"), b"").unwrap();
        assert_eq!(find_by_name_in(&dir, "sensor00fn11").unwrap(), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_missing_device_fails() {
        assert!(Device::open("/nonexistent/event0").is_err());
    }

    /// Needs a real input device: `cargo test -p glass-evdev -- --ignored`.
    #[test]
    #[ignore]
    fn device_event0() {
        let mut d = Device::open("/dev/input/event0").expect("open /dev/input/event0");
        println!("name: {}", d.name().unwrap());
        println!("clock: {:?}", d.set_clock_monotonic());
        let mut out = Vec::new();
        println!("pending events: {}", d.read_events(&mut out).unwrap());
    }

    /// On a Glass: finds the touchpad and prints its axis ranges.
    #[test]
    #[ignore]
    fn device_touchpad() {
        let p = find_by_name("sensor00fn11").unwrap().expect("sensor00fn11");
        let d = Device::open(&p).unwrap();
        println!("{}: X {:?}", p.display(), d.abs_info(0x35).unwrap());
        println!("{}: Y {:?}", p.display(), d.abs_info(0x36).unwrap());
    }
}
