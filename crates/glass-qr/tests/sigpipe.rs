#![cfg(feature = "decoder")]
//! With SIGPIPE at its default, which ends the process, a write to a dead worker must still be
//! only an error. In a test binary of its own, because it changes the disposition process-wide.

use std::process::Command;

use glass_qr::scan::Frame;
use glass_qr::worker::{Worker, WorkerError};

#[test]
fn a_dead_worker_does_not_kill_the_camera_process() {
    // SAFETY: setting a signal's disposition to its default.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    // Larger than a pipe's buffer, so the write is still going when the worker is gone.
    let data = vec![0u8; 1280 * 720];
    let f = Frame::new(&data, 1280, 720, 1280).unwrap();
    let mut w = Worker::new(|| {
        let mut c = Command::new("sh");
        c.args(["-c", "exit 0"]);
        c
    });
    for _ in 0..3 {
        assert_eq!(w.scan(f).err(), Some(WorkerError::Failed));
    }
    assert_eq!(w.scan(f).err(), Some(WorkerError::GaveUp));
}
