//! Finding out when the kernel's random pool is seeded, without waiting for it.
//!
//! Kernels before 3.17 (Model B runs 3.4) have no `getrandom(2)`, so the `getrandom` crate
//! blocks its first caller in `poll(/dev/random)` before it trusts `/dev/urandom`. That poll is
//! right: the companion identity is a long-lived secret and must not come from an unseeded pool.
//! But on the Glass it takes a minute or two after boot, because an idle device collects only a
//! few bits a second, and the daemon must not stand still for it: nothing else in glassd needs
//! randomness.
//!
//! So the same condition goes into the daemon's `poll` loop: [`PoolWait`] polls the same device
//! the crate does, and [`crate::companion::Companion`] touches the crate only once it is ready.
//! Afterwards the crate keeps `/dev/urandom` open and never waits again, and keys, challenge
//! nonces and note ids come out of it at once.
//!
//! Polling reads nothing, so it costs no entropy. Readiness also lasts on 3.4: `/dev/urandom`
//! leaves at least 128 bits in the input pool, above the 64 that make `/dev/random` readable, and
//! nothing in the image reads `/dev/random` itself.
//!
//! Readable is not enough for a long-term key, though. On 3.4 interrupts credit the nonblocking
//! pool first, but input and disk events credit the input pool directly, so it can reach 64 bits
//! with `/dev/urandom` barely seeded, and `/dev/urandom` pulls from it only at 192 bits or more.
//! So a first identity also waits for proof that `/dev/urandom` got real entropy; see
//! [`PullMeter`]. Later boots reuse the stored identity and skip this.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Instant;

/// What the `getrandom` crate polls before it trusts `/dev/urandom`.
pub const RANDOM_DEVICE: &str = "/dev/random";

/// The input pool's credited bits. Unlike `/dev/random` it cannot be polled for a higher level.
pub const ENTROPY_AVAIL: &str = "/proc/sys/kernel/random/entropy_avail";
/// With this level in the input pool, the 32-byte read that makes a key pulls at least
/// [`MIN_BITS_MOVED`] credited bits by itself.
pub const ENOUGH_BITS_IN_POOL: u32 = 256;
/// Bits seen leaving the input pool that make `/dev/urandom` fit for a long-term key.
pub const MIN_BITS_MOVED: u32 = 128;
/// From this level on a read of `/dev/urandom` pulls from the input pool, leaving 128 bits there
/// (`account` and `xfer_secondary_pool` in 3.4's `drivers/char/random.c`).
pub const PULL_LEVEL: u32 = 192;

/// Tells when `/dev/urandom` holds enough real entropy for a long-term key, from the input pool's
/// level. A level of [`ENOUGH_BITS_IN_POOL`] alone is not a usable test: from [`PULL_LEVEL`] on
/// any reader of `/dev/urandom` drains the pool, and the kernel itself does when Wi-Fi scans, so
/// the level can stay under it for good (25 minutes on the Glass, at 130-182 bits). But each of
/// those drops is entropy moved into `/dev/urandom`, since nothing here reads `/dev/random`.
#[derive(Debug, Default)]
pub struct PullMeter {
    last: Option<u32>,
    moved: u32,
}

impl PullMeter {
    /// Takes a new reading; returns whether the pool has enough now.
    pub fn update(&mut self, level: u32) -> bool {
        if let Some(last) = self.last {
            self.moved = self.moved.saturating_add(last.saturating_sub(level));
        }
        self.last = Some(level);
        level >= ENOUGH_BITS_IN_POOL || self.moved >= MIN_BITS_MOVED
    }
}

/// The credited bits now; `None` if the kernel does not say.
pub fn entropy_avail(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// A wait for the random pool, for a place in a `poll` set (`POLLIN`).
pub struct PoolWait {
    device: File,
    since: Instant,
}

impl PoolWait {
    /// Opens `device` to wait on. Never blocks.
    pub fn open(device: &Path) -> io::Result<PoolWait> {
        let device = File::options()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(device)?;
        Ok(PoolWait {
            device,
            since: Instant::now(),
        })
    }

    /// The descriptor for the caller's `poll` set.
    pub fn fd(&self) -> RawFd {
        self.device.as_raw_fd()
    }

    /// Whether the pool is seeded now. Never blocks; an error means it will never tell.
    pub fn seeded(&self) -> io::Result<bool> {
        let mut p = libc::pollfd {
            fd: self.fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            // SAFETY: one valid pollfd, and a timeout of zero.
            if unsafe { libc::poll(&mut p, 1, 0) } >= 0 {
                break;
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
        if p.revents & libc::POLLIN != 0 {
            Ok(true)
        } else if p.revents != 0 {
            Err(io::Error::other(format!("poll revents {:#x}", p.revents)))
        } else {
            Ok(false)
        }
    }

    /// How long this wait has lasted, in milliseconds.
    pub fn waited_ms(&self) -> u128 {
        self.since.elapsed().as_millis()
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::ffi::CString;
    use std::fs::File;
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};

    /// Stands in for `/dev/random`: a FIFO stays unreadable until the test seeds it.
    pub struct FakePool {
        pub path: PathBuf,
        writer: File,
    }

    impl FakePool {
        pub fn new(dir: &Path) -> FakePool {
            let path = dir.join("fake-random");
            let c = CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: a valid NUL-terminated path.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            // Read and write, so that opening does not wait for a reader. Holding it keeps the
            // FIFO from reporting a hangup.
            let writer = File::options()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path)
                .unwrap();
            FakePool { path, writer }
        }

        pub fn seed(&mut self) {
            self.writer.write_all(b"x").unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakePool;
    use super::*;

    #[test]
    fn tells_when_the_pool_is_seeded_without_blocking() {
        let d = tempfile::tempdir().unwrap();
        let mut pool = FakePool::new(d.path());
        let wait = PoolWait::open(&pool.path).unwrap();
        assert!(!wait.seeded().unwrap());
        assert!(!wait.seeded().unwrap());
        pool.seed();
        assert!(wait.seeded().unwrap());
    }

    #[test]
    fn a_full_pool_is_enough() {
        let mut m = PullMeter::default();
        assert!(!m.update(255));
        assert!(m.update(256));
    }

    #[test]
    fn entropy_moved_out_of_the_pool_is_enough_without_ever_filling_it() {
        let mut m = PullMeter::default();
        // Rising is credit, not a move; each fall from 192 to 128 moves 64 bits.
        assert!(!m.update(64));
        assert!(!m.update(192));
        assert!(!m.update(128));
        assert!(!m.update(191));
        assert!(!m.update(190));
        assert!(!m.update(192));
        assert!(m.update(128));
    }

    #[test]
    fn reads_the_pool_level() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("entropy_avail");
        assert_eq!(entropy_avail(&path), None);
        std::fs::write(&path, "191\n").unwrap();
        assert_eq!(entropy_avail(&path), Some(191));
    }

    #[test]
    fn a_missing_device_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        assert!(PoolWait::open(&d.path().join("none")).is_err());
    }

    #[test]
    fn a_hung_up_device_is_an_error_not_a_busy_loop() {
        let d = tempfile::tempdir().unwrap();
        let pool = FakePool::new(d.path());
        let wait = PoolWait::open(&pool.path).unwrap();
        drop(pool);
        assert!(wait.seeded().is_err());
    }
}
