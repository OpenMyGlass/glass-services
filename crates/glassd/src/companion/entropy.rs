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

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Instant;

/// What the `getrandom` crate polls before it trusts `/dev/urandom`.
pub const RANDOM_DEVICE: &str = "/dev/random";

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
