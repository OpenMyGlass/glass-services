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
//! nonces, session keys and note ids come out of it at once.
//!
//! Polling reads nothing, so it costs no entropy. Readiness also lasts on 3.4: `/dev/urandom`
//! leaves at least 128 bits in the input pool, above the 64 that make `/dev/random` readable, and
//! nothing in the image reads `/dev/random` itself.
//!
//! Readable is not enough for a long-term key, though. On 3.4 interrupts credit the nonblocking
//! pool first, but input and disk events credit the input pool directly, so it can reach 64 bits
//! with `/dev/urandom` barely seeded, and `/dev/urandom` pulls from it only at 192 bits or more.
//! So a first identity needs more. Normally that is a seed file ([`SEED_FILE`]) put there by the
//! installer, from a machine with good randomness: it goes into `/dev/urandom` at start, and is
//! replaced with a fresh one so that it is never used twice. Without one the identity waits for
//! [`MIN_BITS_FOR_IDENTITY`], so that the 32-byte read that makes the key pulls at least 128
//! credited bits, whatever the order. On an idle Glass that takes about 8 minutes, and with Wi-Fi
//! scanning it may never happen, because from 192 bits on the kernel drains the pool (130-182 bits
//! for 25 minutes on the Glass). Later boots reuse the stored identity and skip this.
//!
//! A later boot with no seed file, after a power cut between using one seed and saving the next,
//! has only the readable `/dev/random` behind it. So `omg_core::secure` mixes the identity's secret
//! into challenge nonces and session keys, and a thin pool alone cannot make them predictable.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Instant;

use crate::{info, warn};

/// What the `getrandom` crate polls before it trusts `/dev/urandom`.
pub const RANDOM_DEVICE: &str = "/dev/random";

/// The input pool's credited bits. Unlike `/dev/random` it cannot be polled for a higher level.
pub const ENTROPY_AVAIL: &str = "/proc/sys/kernel/random/entropy_avail";
/// The input pool's level at which a first identity may be generated. See the module doc.
pub const MIN_BITS_FOR_IDENTITY: u32 = 256;

/// The credited bits now; `None` if the kernel does not say.
pub fn entropy_avail(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Whether a secret may be drawn now with no companion to say so: `/dev/random` readable, so the
/// `getrandom` crate will not block, and as many credited bits as a first identity needs.
pub fn ready_for_a_secret() -> bool {
    let readable =
        PoolWait::open(Path::new(RANDOM_DEVICE)).is_ok_and(|w| w.seeded().unwrap_or(false));
    readable && entropy_avail(Path::new(ENTROPY_AVAIL)).is_some_and(|b| b >= MIN_BITS_FOR_IDENTITY)
}

/// The seed's name in the state directory.
pub const SEED_FILE: &str = "random-seed";
/// Where a seed goes in. Writing mixes it into the pool without crediting it.
pub const URANDOM_DEVICE: &str = "/dev/urandom";
/// The size of a seed: the input pool's size in 3.4.
const SEED_BYTES: usize = 512;
/// A shorter seed than this is not trusted to stand in for the wait: 256 bits.
const MIN_SEED_BYTES: usize = 32;

/// Feeds the seed at `path` into `urandom` and removes it, so that it is never used twice.
/// Returns whether a first identity may rely on it. A seed that could not be removed is still
/// mixed in, since that can only help, but it is not trusted: it may have been used already.
pub fn use_seed(path: &Path, urandom: &Path) -> bool {
    let seed = match std::fs::read(path) {
        Ok(seed) => seed,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return false,
        Err(e) => {
            warn!("companion: cannot read {} ({e})", path.display());
            return false;
        }
    };
    if let Err(e) = std::fs::write(urandom, &seed) {
        warn!("companion: cannot feed {} ({e})", urandom.display());
        return false;
    }
    if let Err(e) = std::fs::remove_file(path) {
        warn!("companion: cannot remove {} ({e})", path.display());
        return false;
    }
    if seed.len() < MIN_SEED_BYTES {
        warn!("companion: {} is too short to rely on", path.display());
        return false;
    }
    info!("companion: random seed used");
    true
}

/// Stores a fresh seed for the next boot. Call it only when the pool is trusted: the next boot
/// relies on this seed without waiting. It holds no key, but it is private like one.
pub fn save_seed(path: &Path) -> io::Result<()> {
    let mut seed = [0u8; SEED_BYTES];
    getrandom::getrandom(&mut seed).map_err(|e| io::Error::other(e.to_string()))?;
    let tmp = path.with_extension("tmp");
    match std::fs::remove_file(&tmp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(&seed)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
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
    fn a_seed_goes_in_once() {
        let d = tempfile::tempdir().unwrap();
        let (seed, urandom) = (d.path().join(SEED_FILE), d.path().join("urandom"));
        assert!(!use_seed(&seed, &urandom), "no seed");
        std::fs::write(&seed, [7u8; 64]).unwrap();
        assert!(use_seed(&seed, &urandom));
        assert_eq!(std::fs::read(&urandom).unwrap(), [7u8; 64]);
        assert!(!seed.exists());
        assert!(!use_seed(&seed, &urandom), "never twice");
    }

    #[test]
    fn a_short_seed_is_mixed_in_but_not_trusted() {
        let d = tempfile::tempdir().unwrap();
        let (seed, urandom) = (d.path().join(SEED_FILE), d.path().join("urandom"));
        std::fs::write(&seed, [7u8; 31]).unwrap();
        assert!(!use_seed(&seed, &urandom));
        assert_eq!(std::fs::read(&urandom).unwrap().len(), 31);
    }

    #[test]
    fn a_seed_that_cannot_go_in_is_kept_and_not_trusted() {
        let d = tempfile::tempdir().unwrap();
        let seed = d.path().join(SEED_FILE);
        std::fs::write(&seed, [7u8; 64]).unwrap();
        assert!(!use_seed(&seed, &d.path().join("no/such/device")));
        assert!(seed.exists());
    }

    #[test]
    fn a_saved_seed_is_private_and_replaces_the_old_one() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let seed = d.path().join(SEED_FILE);
        save_seed(&seed).unwrap();
        let first = std::fs::read(&seed).unwrap();
        assert_eq!(first.len(), SEED_BYTES);
        let mode = std::fs::metadata(&seed).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        save_seed(&seed).unwrap();
        assert_ne!(std::fs::read(&seed).unwrap(), first);
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
