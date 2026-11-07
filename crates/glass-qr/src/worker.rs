//! The decoder in a process of its own, which the camera process kills when it is late.
//!
//! [`crate::scan`] cannot be interrupted, and a panic in it aborts its process. So the camera
//! process does not decode: it sends each frame to `glass-qr-worker` over a pipe and waits for an
//! answer until a deadline. A worker that is late, dies or answers out of protocol gets `SIGKILL`,
//! and that frame is lost. Only the worker is restarted, never the camera pipeline, and only
//! [`DEFAULT_MAX_FAILURES`] times per scan: after that [`Worker::scan`] gives up, and so should the
//! scan, until the user starts another. A hostile poster can then cost one scan, not a loop of
//! restarts.
//!
//! The protocol, all integers little-endian:
//!
//! ```text
//! request:  "OMGQ" width:u32 height:u32 pixels:width*height bytes
//! response: grids:u8 (at most DECODER_MAX_GRIDS) texts:u8 (at most grids)
//!           capstones:u8 (at most DECODER_MAX_CAPSTONES) regions:u16 (at most DECODER_MAX_REGIONS)
//!           then texts times  length:u16 text:length bytes (at most MAX_TEXT_BYTES)
//! ```
//!
//! The worker only decodes. The camera process checks the answer and validates the texts itself,
//! as [`crate::scan::Scanner::scan`] would.

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

use crate::scan::{
    self, Codes, DECODER_MAX_CAPSTONES, DECODER_MAX_GRIDS, DECODER_MAX_REGIONS, Frame, Scan,
};
use crate::setup::MAX_TEXT_BYTES;

/// What a request starts with.
pub const MAGIC: &[u8; 4] = b"OMGQ";
/// Time for one frame, sent and answered. Normal frames take milliseconds on the host, and
/// estimated about ten to twenty times that on the Glass.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(2);
/// Failed frames per [`Worker`] before it gives up.
pub const DEFAULT_MAX_FAILURES: u32 = 3;

/// Why a frame has no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerError {
    /// Late: killed.
    Deadline,
    /// Could not start, ended, or broke the protocol: killed if still there.
    Failed,
    /// Too many failures already: the scan should end.
    GaveUp,
}

struct Running {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
}

/// The camera process's side of a worker. Make one per scan.
pub struct Worker {
    command: Box<dyn FnMut() -> Command + Send>,
    running: Option<Running>,
    deadline: Duration,
    failures_left: u32,
}

impl Worker {
    /// A worker started, when first needed, by `command`, which gets piped stdin and stdout and a
    /// closed stderr.
    pub fn new(command: impl FnMut() -> Command + Send + 'static) -> Worker {
        Worker {
            command: Box::new(command),
            running: None,
            deadline: DEFAULT_DEADLINE,
            failures_left: DEFAULT_MAX_FAILURES,
        }
    }

    pub fn with_deadline(mut self, deadline: Duration) -> Worker {
        self.deadline = deadline;
        self
    }

    pub fn with_max_failures(mut self, failures: u32) -> Worker {
        self.failures_left = failures;
        self
    }

    /// Decodes a frame in the worker, then decides as [`crate::scan::Scanner::scan`] does.
    pub fn scan(&mut self, frame: Frame<'_>) -> Result<Scan, WorkerError> {
        if self.failures_left == 0 {
            return Err(WorkerError::GaveUp);
        }
        let until = Instant::now() + self.deadline;
        let result = self.exchange(frame, until);
        match result {
            Ok(texts) => Ok(scan::decide(texts)),
            Err(e) => {
                self.kill();
                self.failures_left -= 1;
                Err(if e.kind() == io::ErrorKind::TimedOut {
                    WorkerError::Deadline
                } else {
                    WorkerError::Failed
                })
            }
        }
    }

    fn exchange(&mut self, frame: Frame<'_>, until: Instant) -> io::Result<Codes> {
        if self.running.is_none() {
            self.running = Some(self.start()?);
        }
        let w = self.running.as_mut().expect("started above");
        let mut header = [0u8; 12];
        header[..4].copy_from_slice(MAGIC);
        header[4..8].copy_from_slice(&(frame.width as u32).to_le_bytes());
        header[8..].copy_from_slice(&(frame.height as u32).to_le_bytes());
        write_until(&mut w.stdin, &header, until)?;
        for row in frame.rows() {
            write_until(&mut w.stdin, row, until)?;
        }
        let mut counts = [0u8; 5];
        read_until(&mut w.stdout, &mut counts, until)?;
        let (grids, count, capstones) = (counts[0].into(), counts[1].into(), counts[2].into());
        let regions = usize::from(u16::from_le_bytes([counts[3], counts[4]]));
        // Each grid is made of 3 finder patterns, so fewer than that is no answer quircs gives.
        if grids > DECODER_MAX_GRIDS
            || count > grids
            || capstones > DECODER_MAX_CAPSTONES
            || capstones < 3 * grids
            || regions > DECODER_MAX_REGIONS
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut texts = Vec::with_capacity(count);
        for _ in 0..count {
            let mut len = [0u8; 2];
            read_until(&mut w.stdout, &mut len, until)?;
            let len = usize::from(u16::from_le_bytes(len));
            if len > MAX_TEXT_BYTES {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let mut text = Zeroizing::new(vec![0u8; len]);
            read_until(&mut w.stdout, &mut text, until)?;
            texts.push(text);
        }
        Ok(Codes {
            grids,
            capstones,
            regions,
            texts,
        })
    }

    fn start(&mut self) -> io::Result<Running> {
        let mut child = (self.command)()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let (stdin, stdout) = (child.stdin.take(), child.stdout.take());
        let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::ErrorKind::BrokenPipe.into());
        };
        set_nonblocking(&stdin)?;
        set_nonblocking(&stdout)?;
        Ok(Running {
            child,
            stdin,
            stdout,
        })
    }

    /// `SIGKILL`, and the zombie reaped.
    fn kill(&mut self) {
        if let Some(mut w) = self.running.take() {
            let _ = w.child.kill();
            let _ = w.child.wait();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.kill();
    }
}

fn set_nonblocking(fd: &impl AsRawFd) -> io::Result<()> {
    let fd = fd.as_raw_fd();
    // SAFETY: fcntl on a descriptor this process owns, with no pointer arguments.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Waits until `fd` is ready for `events`, or fails with `TimedOut` at `until`.
fn wait(fd: &impl AsRawFd, events: libc::c_short, until: Instant) -> io::Result<()> {
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let mut p = libc::pollfd {
            fd: fd.as_raw_fd(),
            events,
            revents: 0,
        };
        let ms = left.as_millis().clamp(1, i32::MAX as u128) as libc::c_int;
        // SAFETY: one valid pollfd, and its count.
        match unsafe { libc::poll(&mut p, 1, ms) } {
            n if n > 0 => return Ok(()),
            0 => {}
            _ => {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
        }
    }
}

/// SIGPIPE blocked for this thread while it lives. A write to a pipe whose reader is gone raises
/// SIGPIPE, which by default ends the process. Rust's runtime ignores it at start, but the camera
/// process need not be a Rust program, or may restore the default. So each write blocks it, and
/// after a write that failed with `EPIPE`, [`SigpipeBlocked::take_ours`] takes that SIGPIPE back
/// before the mask is restored: a dead worker is then only an error.
///
/// Which SIGPIPE is taken: the kernel sends the write's SIGPIPE to this thread, and Linux takes a
/// thread's own pending signals before the process's (`dequeue_signal`). So a SIGPIPE pending for
/// the whole process from before is left for whoever it was meant for. One pending for this thread
/// from before can exist only if the caller already blocked SIGPIPE here; standard signals do not
/// queue, so it and ours are one, and it is taken.
struct SigpipeBlocked {
    old: libc::sigset_t,
    set: libc::sigset_t,
}

impl SigpipeBlocked {
    fn new() -> io::Result<SigpipeBlocked> {
        // SAFETY: sigset_t is plain data, filled in by sigemptyset before any use.
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        let mut old: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: valid pointers to local sigsets.
        let r = unsafe {
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGPIPE);
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old)
        };
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r));
        }
        Ok(SigpipeBlocked { old, set })
    }

    /// Takes the SIGPIPE a failed write raised, without running any handler.
    fn take_ours(&self) {
        let zero = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: valid set and timeout.
        unsafe { libc::sigtimedwait(&self.set, std::ptr::null_mut(), &zero) };
    }
}

impl Drop for SigpipeBlocked {
    fn drop(&mut self) {
        // SAFETY: restores the mask saved in new().
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &self.old, std::ptr::null_mut()) };
    }
}

fn write_until(out: &mut ChildStdin, mut buf: &[u8], until: Instant) -> io::Result<()> {
    while !buf.is_empty() {
        let written = {
            let blocked = SigpipeBlocked::new()?;
            let r = out.write(buf);
            if matches!(&r, Err(e) if e.raw_os_error() == Some(libc::EPIPE)) {
                blocked.take_ours();
            }
            r
        };
        match written {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => wait(out, libc::POLLOUT, until)?,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn read_until(input: &mut ChildStdout, mut buf: &mut [u8], until: Instant) -> io::Result<()> {
    while !buf.is_empty() {
        match input.read(buf) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => buf = &mut buf[n..],
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => wait(input, libc::POLLIN, until)?,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
