//! The decoder in its own process: answers, deadlines, kills, and giving up.

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::*;
use glass_qr::scan::{Frame, Scan};
use glass_qr::setup;
use glass_qr::worker::{Worker, WorkerError};
use qrcode::EcLevel;

fn real_worker() -> Worker {
    Worker::new(|| Command::new(env!("CARGO_BIN_EXE_glass-qr-worker")))
}

fn sh(script: &'static str) -> Worker {
    Worker::new(move || {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    })
}

fn code_frame() -> (Vec<u8>, View) {
    let view = View::plain();
    let frame = render(
        &matrix(full_text().as_bytes(), EcLevel::M),
        &view,
        &mut Rng::new(1),
    );
    (frame, view)
}

#[test]
fn the_worker_reads_a_code_and_keeps_going() {
    let (frame, view) = code_frame();
    let mut w = real_worker();
    for _ in 0..3 {
        let f = Frame::new(&frame, view.width, view.height, view.width).unwrap();
        match w.scan(f) {
            Ok(Scan::Setup(text)) => assert!(setup::parse(&text).is_ok()),
            _ => panic!("no setup code"),
        }
    }
    let blank = vec![128u8; 320 * 240];
    let f = Frame::new(&blank, 320, 240, 320).unwrap();
    assert!(matches!(w.scan(f), Ok(Scan::NoCode)));
}

#[test]
fn padded_rows_cross_the_pipe_without_their_padding() {
    let (frame, view) = code_frame();
    let stride = view.width + 7;
    let mut padded = vec![0xee; stride * view.height];
    for y in 0..view.height {
        padded[y * stride..y * stride + view.width]
            .copy_from_slice(&frame[y * view.width..(y + 1) * view.width]);
    }
    let f = Frame::new(&padded, view.width, view.height, stride).unwrap();
    assert!(matches!(real_worker().scan(f), Ok(Scan::Setup(_))));
}

#[test]
fn the_slow_frames_fit_the_deadline() {
    let mut w = real_worker();
    for seed in SLOW_SEEDS {
        let frame = stress_frame(seed, 320, 240);
        let f = Frame::new(&frame, 320, 240, 320).unwrap();
        assert!(w.scan(f).is_ok(), "seed {seed}");
    }
}

/// A worker that never answers is killed at the deadline, and the next frame gets a new one;
/// after the allowed failures the scan is given up, without starting any more.
#[test]
fn a_late_worker_is_killed_and_then_given_up() {
    let (frame, view) = code_frame();
    let f = Frame::new(&frame, view.width, view.height, view.width).unwrap();
    let mut w = sh("exec sleep 30")
        .with_deadline(Duration::from_millis(200))
        .with_max_failures(2);
    for _ in 0..2 {
        let start = Instant::now();
        assert_eq!(w.scan(f).err(), Some(WorkerError::Deadline));
        assert!(start.elapsed() < Duration::from_secs(2));
    }
    let start = Instant::now();
    assert_eq!(w.scan(f).err(), Some(WorkerError::GaveUp));
    assert!(start.elapsed() < Duration::from_millis(50));
}

/// Reads the whole frame, then never answers: the wait for the answer has a deadline too.
#[test]
fn a_worker_that_takes_the_frame_and_hangs_is_killed() {
    let (frame, view) = code_frame();
    let f = Frame::new(&frame, view.width, view.height, view.width).unwrap();
    let mut w = sh("cat >/dev/null").with_deadline(Duration::from_millis(300));
    assert_eq!(w.scan(f).err(), Some(WorkerError::Deadline));
}

#[test]
fn a_worker_out_of_protocol_is_a_failure() {
    let (frame, view) = code_frame();
    let f = Frame::new(&frame, view.width, view.height, view.width).unwrap();
    for script in [
        "exit 0",                                            // dies at once
        "head -c 12 >/dev/null; printf '\\377'",             // more codes than allowed
        "head -c 12 >/dev/null; printf '\\001\\377\\377'",   // a text longer than allowed
        "head -c 12 >/dev/null; printf '\\001\\005\\000ab'", // cut short
    ] {
        let mut w = sh(script).with_deadline(Duration::from_secs(2));
        assert_eq!(w.scan(f).err(), Some(WorkerError::Failed), "{script}");
    }
}

#[test]
fn a_worker_that_cannot_start_is_a_failure() {
    let mut w = Worker::new(|| Command::new("/nonexistent/glass-qr-worker"));
    let (frame, view) = code_frame();
    let f = Frame::new(&frame, view.width, view.height, view.width).unwrap();
    assert_eq!(w.scan(f).err(), Some(WorkerError::Failed));
}

/// The worker binary refuses a frame larger than the decoder takes, before reading its pixels.
#[test]
fn the_worker_refuses_an_oversized_frame() {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new(env!("CARGO_BIN_EXE_glass-qr-worker"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut header = b"OMGQ".to_vec();
    header.extend_from_slice(&100_000u32.to_le_bytes());
    header.extend_from_slice(&100_000u32.to_le_bytes());
    child.stdin.as_mut().unwrap().write_all(&header).unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
}
