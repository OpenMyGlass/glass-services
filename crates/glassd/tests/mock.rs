//! Spawns `glassd --mock` on a temporary socket and drives it through standard input.

use glass_api::Client;
use glass_api::proto::{GestureKind, Screen, ServiceFrame, Topic, service_frame::Body};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

const T: Duration = Duration::from_secs(5);

struct Mock {
    child: Child,
    stdin: ChildStdin,
    dir: PathBuf,
    socket: PathBuf,
}

impl Mock {
    fn start(name: &str) -> Mock {
        let dir = std::env::temp_dir().join(format!("glassd-it-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("glassd.sock");
        let mut child = Command::new(env!("CARGO_BIN_EXE_glassd"))
            .args(["--mock", "--socket"])
            .arg(&socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn glassd");
        let stdin = child.stdin.take().unwrap();
        let deadline = Instant::now() + T;
        while !socket.exists() {
            assert!(
                Instant::now() < deadline,
                "glassd did not create its socket"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        Mock {
            child,
            stdin,
            dir,
            socket,
        }
    }

    fn send(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn client(&self, topics: &[Topic]) -> Client {
        let mut c = Client::connect_as(&self.socket, "it").unwrap();
        c.subscribe(topics).unwrap();
        c
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn focused(c: &mut Client) {
    c.acquire_focus().unwrap();
    c.recv_until(
        T,
        |f| matches!(f.body, Some(Body::FocusState(ref s)) if s.focused),
    )
    .unwrap()
    .expect("focus");
}

fn gesture(f: &ServiceFrame) -> Option<(GestureKind, u32)> {
    match &f.body {
        Some(Body::Gesture(g)) => Some((GestureKind::try_from(g.kind).unwrap(), g.fingers)),
        _ => None,
    }
}

fn screen(f: &ServiceFrame) -> Option<(Screen, String)> {
    match &f.body {
        Some(Body::ScreenState(s)) if s.reason != "snapshot" => {
            Some((Screen::try_from(s.screen).unwrap(), s.reason.clone()))
        }
        _ => None,
    }
}

#[test]
fn tap_on_stdin_reaches_the_focus_holder() {
    let mut m = Mock::start("tap");
    let mut c = m.client(&[Topic::Gesture]);
    assert!(c.has_capability("mock"));
    assert!(c.has_capability("touchpad"));
    focused(&mut c);
    m.send("tap");
    let g = c.recv_until(T, |f| gesture(f).is_some()).unwrap().unwrap();
    assert_eq!(gesture(&g), Some((GestureKind::Tap, 1)));
    m.send("swipe forward 2");
    let g = c.recv_until(T, |f| gesture(f).is_some()).unwrap().unwrap();
    assert_eq!(gesture(&g), Some((GestureKind::SwipeForward, 2)));
}

#[test]
fn scroll_then_swipe_ordering_over_the_socket() {
    let mut m = Mock::start("order");
    let mut c = m.client(&[Topic::Gesture]);
    focused(&mut c);
    m.send("swipe backward");
    let mut kinds = Vec::new();
    loop {
        let f = c.recv(Some(T)).unwrap().expect("frame");
        let k = match &f.body {
            Some(Body::FingerCount(fc)) => format!("count {}", fc.current),
            Some(Body::Scroll(s)) if s.ended => "scroll-ended".to_string(),
            Some(Body::Scroll(_)) => "scroll".to_string(),
            Some(Body::Gesture(g)) => format!("gesture {}", g.kind),
            _ => continue,
        };
        let done = k == "count 0";
        if kinds.last() != Some(&k) {
            kinds.push(k);
        }
        if done {
            break;
        }
    }
    assert_eq!(
        kinds,
        vec![
            "count 1".to_string(),
            "scroll".into(),
            "scroll-ended".into(),
            format!("gesture {}", GestureKind::SwipeBackward as i32),
            "count 0".into()
        ]
    );
}

#[test]
fn two_finger_swipe_down_sleeps_and_the_waking_tap_is_consumed() {
    let mut m = Mock::start("screen");
    let mut c = m.client(&[Topic::Gesture, Topic::Screen]);
    // Snapshot first.
    let snap = c.recv(Some(T)).unwrap().unwrap();
    assert!(matches!(snap.body, Some(Body::ScreenState(ref s)) if s.reason == "snapshot"));
    focused(&mut c);

    m.send("swipe down 2");
    let f = c
        .recv_until(T, |f| screen(f).is_some() || gesture(f).is_some())
        .unwrap()
        .unwrap();
    assert_eq!(
        screen(&f),
        Some((Screen::Off, "two-finger-swipe-down".into()))
    );

    m.send("tap");
    let f = c
        .recv_until(T, |f| screen(f).is_some() || gesture(f).is_some())
        .unwrap()
        .unwrap();
    assert_eq!(screen(&f), Some((Screen::On, "tap".into())));
    let Some(Body::ScreenState(s)) = &f.body else {
        panic!()
    };
    assert_eq!(s.last_sleep_reason, "two-finger-swipe-down");
    // The tap was consumed: the next frame is the gesture of the next tap.
    m.send("tap 2");
    let f = c
        .recv_until(T, |f| screen(f).is_some() || gesture(f).is_some())
        .unwrap()
        .unwrap();
    assert_eq!(gesture(&f), Some((GestureKind::Tap, 2)));
}

#[test]
fn wait_delays_the_next_command_and_bad_frames_are_rejected() {
    let mut m = Mock::start("wait");
    let mut c = m.client(&[Topic::Gesture]);
    focused(&mut c);
    let t0 = Instant::now();
    m.send("wait 300");
    m.send("tap");
    c.recv_until(T, |f| gesture(f).is_some()).unwrap().unwrap();
    assert!(t0.elapsed() >= Duration::from_millis(290));

    // A raw connection that skips Hello is told BAD_FRAME and closed.
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    let mut s = UnixStream::connect(&m.socket).unwrap();
    let frame = glass_api::proto::ClientFrame {
        seq: 5,
        body: Some(glass_api::proto::client_frame::Body::GetState(
            Default::default(),
        )),
    };
    let mut buf = Vec::new();
    glass_api::encode_frame(&frame, &mut buf).unwrap();
    s.write_all(&buf).unwrap();
    s.set_read_timeout(Some(T)).unwrap();
    let mut got = Vec::new();
    s.read_to_end(&mut got).unwrap();
    let mut d = glass_api::FrameDecoder::new();
    d.push(&got);
    let f: ServiceFrame = d.next_message().unwrap().unwrap();
    let Some(Body::Error(e)) = f.body else {
        panic!("{f:?}")
    };
    assert_eq!(e.seq, 5);
}

/// The real device path, fed through a FIFO: `--device` with raw `input_event` records in the
/// `sensor00fn11` format (the ioctls fail on a FIFO, so the defaults and the read-time clock
/// are used).
#[test]
fn touchpad_events_from_a_fake_device() {
    use glass_evdev::{EVENT_SIZE, RawInputEvent};
    use glass_gesture::Config;
    use glass_gesture::synth::Synth;
    use std::ffi::CString;
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;

    let dir = std::env::temp_dir().join(format!("glassd-it-{}-fifo", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fifo = dir.join("event0");
    let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: plain libc calls with a valid path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    // Hold a read-write end so the daemon never sees the writer side closed.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK) };
    assert!(fd >= 0);
    let mut writer = unsafe { std::fs::File::from_raw_fd(fd) };

    let socket = dir.join("glassd.sock");
    let mut child = Command::new(env!("CARGO_BIN_EXE_glassd"))
        .arg("--socket")
        .arg(&socket)
        .arg("--device")
        .arg(&fifo)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + T;
    while !socket.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut c = Client::connect_as(&socket, "it").unwrap();
    assert!(c.has_capability("touchpad"));
    assert!(!c.has_capability("mock"));
    c.subscribe(&[Topic::Gesture]).unwrap();
    focused(&mut c);

    let mut s = Synth::new(&Config::default(), 0);
    s.tap(2, 60);
    let mut bytes = Vec::new();
    for e in s.take() {
        let raw = RawInputEvent {
            tv_sec: (e.time_us / 1_000_000) as _,
            tv_usec: (e.time_us % 1_000_000) as _,
            type_: e.type_,
            code: e.code,
            value: e.value,
        };
        // SAFETY: RawInputEvent is plain old data.
        let b: &[u8] = unsafe {
            std::slice::from_raw_parts((&raw as *const RawInputEvent).cast(), EVENT_SIZE)
        };
        bytes.extend_from_slice(b);
    }
    writer.write_all(&bytes).unwrap();
    let g = c.recv_until(T, |f| gesture(f).is_some()).unwrap().unwrap();
    assert_eq!(gesture(&g), Some((GestureKind::Tap, 2)));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
