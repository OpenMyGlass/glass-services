//! Spawns `glassd --mock` on a temporary socket and drives it through standard input.

use glass_api::Client;
use glass_api::proto::{
    ButtonAction, GestureKind, HeadGestureKind, HeadRequest, Screen, ServiceFrame, SetSettings,
    Settings, Topic, Wear, WearSource, client_frame::Body as Body2, service_frame::Body,
};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

const T: Duration = Duration::from_secs(5);
/// Keeps a non-mock daemon away from the host's devices.
const NO_HARDWARE: &[&str] = &[
    "--display",
    "log",
    "--glasshub",
    "none",
    "--camera-device",
    "none",
    "--power-device",
    "none",
    "--touchpad-sysfs",
    "none",
    "--iio",
    "none",
];
/// Handshake timeout for the integration tests (slow CI runners).
const HANDSHAKE: Duration = Duration::from_secs(20);

/// Kills and reaps the daemon when dropped, also when a test panics.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(cmd: &mut Command) -> ChildGuard {
    ChildGuard(cmd.spawn().expect("spawn glassd"))
}

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("glassd-it-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Mock {
    child: ChildGuard,
    stdin: ChildStdin,
    dir: PathBuf,
    socket: PathBuf,
}

impl Mock {
    fn start(name: &str) -> Mock {
        Mock::start_in(&test_dir(name), &[])
    }

    /// Starts in `dir` (kept when the mock is dropped and started again).
    fn start_in(dir: &std::path::Path, args: &[&str]) -> Mock {
        let dir = dir.to_path_buf();
        let socket = dir.join("glassd.sock");
        let mut child = spawn(
            Command::new(env!("CARGO_BIN_EXE_glassd"))
                .args(["--mock", "--socket"])
                .arg(&socket)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit()),
        );
        let stdin = child.0.stdin.take().unwrap();
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
        let mut c = Client::connect_with_timeout(&self.socket, "it", HANDSHAKE).unwrap();
        c.subscribe(topics).unwrap();
        // Wait for the answer to a GetState, so the subscription is in place before a mock
        // command (read from another descriptor) is sent.
        c.send(Body2::GetState(Default::default())).unwrap();
        c.recv_until(T, |f| matches!(f.body, Some(Body::FocusState(_))))
            .unwrap()
            .expect("GetState answer");
        c
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        let _ = self.child.0.kill();
        let _ = self.child.0.wait();
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
    let _child = spawn(
        Command::new(env!("CARGO_BIN_EXE_glassd"))
            .arg("--socket")
            .arg(&socket)
            .arg("--device")
            .arg(&fifo)
            .args(NO_HARDWARE)
            .arg("--state-dir")
            .arg(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit()),
    );
    let deadline = Instant::now() + T;
    while !socket.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut c = Client::connect_with_timeout(&socket, "it", HANDSHAKE).unwrap();
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
    let _ = std::fs::remove_dir_all(&dir);
}

fn wear(f: &ServiceFrame) -> Option<(Wear, WearSource)> {
    match &f.body {
        Some(Body::WearState(w)) if !w.snapshot => Some((
            Wear::try_from(w.wear).unwrap(),
            WearSource::try_from(w.source).unwrap(),
        )),
        _ => None,
    }
}

fn set_settings(c: &mut Client, s: Settings) -> Settings {
    c.send(Body2::SetSettings(SetSettings { settings: Some(s) }))
        .unwrap();
    let f = c
        .recv_until(T, |f| matches!(f.body, Some(Body::Settings(_))))
        .unwrap()
        .unwrap();
    let Some(Body::Settings(s)) = f.body else {
        unreachable!()
    };
    s
}

#[test]
fn off_head_camera_is_delivered_without_waking() {
    let mut m = Mock::start("offhead");
    let mut c = m.client(&[Topic::Screen, Topic::Button, Topic::Wear]);
    // A short power press forces off the head.
    m.send("power");
    let f = c.recv_until(T, |f| wear(f).is_some()).unwrap().unwrap();
    assert_eq!(wear(&f), Some((Wear::OffHead, WearSource::PowerKey)));
    let f = c.recv_until(T, |f| screen(f).is_some()).unwrap().unwrap();
    assert_eq!(screen(&f), Some((Screen::Off, "off-head".into())));
    m.send("camera");
    let f = c
        .recv_until(T, |f| {
            screen(f).is_some() || matches!(f.body, Some(Body::Button(_)))
        })
        .unwrap()
        .unwrap();
    let Some(Body::Button(b)) = f.body else {
        panic!("{f:?}")
    };
    assert!(b.screen_off);
    // A wake request is refused off the head too.
    c.send(glass_api::proto::client_frame::Body::Screen(
        glass_api::proto::ScreenRequest {
            action: glass_api::proto::screen_request::Action::Wake as i32,
            duration_ms: 0,
        },
    ))
    .unwrap();
    assert!(
        c.recv_until(Duration::from_millis(300), |f| screen(f).is_some())
            .unwrap()
            .is_none()
    );
    // The next power press puts it back on the head and wakes the screen (quick timeout).
    m.send("power");
    let f = c.recv_until(T, |f| wear(f).is_some()).unwrap().unwrap();
    assert_eq!(wear(&f), Some((Wear::OnHead, WearSource::PowerKey)));
    let f = c.recv_until(T, |f| screen(f).is_some()).unwrap().unwrap();
    assert_eq!(screen(&f), Some((Screen::On, "don".into())));
}

#[test]
fn wear_sensor_needs_on_head_detection() {
    let mut m = Mock::start("sensor");
    let mut c = m.client(&[Topic::Screen, Topic::Wear, Topic::Settings]);
    m.send("wear off");
    assert!(
        c.recv_until(Duration::from_millis(300), |f| wear(f).is_some())
            .unwrap()
            .is_none()
    );
    // Turning detection on defers to the sensor at once: the stored reading applies.
    let s = set_settings(
        &mut c,
        Settings {
            on_head_detection_enabled: Some(true),
            ..Default::default()
        },
    );
    assert_eq!(s.on_head_detection_enabled, Some(true));
    let f = c.recv_until(T, |f| wear(f).is_some()).unwrap().unwrap();
    assert_eq!(wear(&f), Some((Wear::OffHead, WearSource::Sensor)));
    m.send("wear on");
    let f = c.recv_until(T, |f| wear(f).is_some()).unwrap().unwrap();
    assert_eq!(wear(&f), Some((Wear::OnHead, WearSource::Sensor)));
    let f = c.recv_until(T, |f| screen(f).is_some()).unwrap().unwrap();
    assert_eq!(screen(&f), Some((Screen::On, "don".into())));
    // The hinge forces off the head.
    m.send("hinge closed");
    let f = c.recv_until(T, |f| wear(f).is_some()).unwrap().unwrap();
    assert_eq!(wear(&f), Some((Wear::OffHead, WearSource::Hinge)));
}

#[test]
fn wink_and_long_press() {
    let mut m = Mock::start("wink");
    let mut c = m.client(&[Topic::Wear, Topic::Button]);
    m.send("wink");
    assert!(
        c.recv_until(Duration::from_millis(300), |f| matches!(
            f.body,
            Some(Body::Wink(_))
        ))
        .unwrap()
        .is_none()
    );
    set_settings(
        &mut c,
        Settings {
            wink_enabled: Some(true),
            ..Default::default()
        },
    );
    m.send("wink");
    c.recv_until(T, |f| matches!(f.body, Some(Body::Wink(_))))
        .unwrap()
        .expect("wink");
    m.send("camera long");
    let mut actions = Vec::new();
    while actions.len() < 2 {
        let f = c
            .recv_until(T, |f| matches!(f.body, Some(Body::Button(_))))
            .unwrap()
            .unwrap();
        let Some(Body::Button(b)) = f.body else {
            unreachable!()
        };
        actions.push(ButtonAction::try_from(b.action).unwrap());
    }
    assert_eq!(
        actions,
        vec![ButtonAction::LongPress, ButtonAction::Release]
    );
}

#[test]
fn settings_are_stored_across_restarts() {
    // The state directory is outside the mock's directory, which is removed with the mock.
    let state = test_dir("store-state");
    let state_s = state.to_str().unwrap();
    {
        let m = Mock::start_in(&test_dir("store-1"), &["--state-dir", state_s]);
        let mut c = m.client(&[]);
        set_settings(
            &mut c,
            Settings {
                screen_timeout_ms: Some(42_000),
                head_wake_angle_deg: Some(20.0),
                ..Default::default()
            },
        );
        // Dropping the mock kills it.
        drop(m);
    }
    assert!(state.join("settings").exists());
    let m = Mock::start_in(&test_dir("store-2"), &["--state-dir", state_s]);
    let mut c = m.client(&[Topic::Settings]);
    c.send(Body2::GetState(Default::default())).unwrap();
    let f = c
        .recv_until(T, |f| matches!(f.body, Some(Body::Settings(_))))
        .unwrap()
        .unwrap();
    let Some(Body::Settings(s)) = f.body else {
        unreachable!()
    };
    assert!(s.snapshot);
    assert_eq!(s.screen_timeout_ms, Some(42_000));
    assert_eq!(s.head_wake_angle_deg, Some(20.0));
    drop(m);
    let _ = std::fs::remove_dir_all(&state);
}

/// A non-mock daemon on fake hardware: a glasshub sysfs directory, a camera button FIFO and
/// a touchpad sysfs directory.
#[test]
fn fake_glasshub_camera_and_touchpad_sysfs() {
    use glass_evdev::{EVENT_SIZE, RawInputEvent};
    use std::ffi::CString;
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;

    let dir = test_dir("hw");
    let hub = dir.join("glasshub");
    std::fs::create_dir_all(&hub).unwrap();
    for (f, v) in [("version", "1.14\n"), ("don_doff", "1\n"), ("wink", "0\n")] {
        std::fs::write(hub.join(f), v).unwrap();
    }
    for f in glass_hub::sysfs::WRITABLE {
        std::fs::write(hub.join(f), "").unwrap();
    }
    let tp = dir.join("touchpad");
    std::fs::create_dir_all(&tp).unwrap();
    std::fs::write(tp.join("sleepmode"), "").unwrap();
    let fifo = dir.join("camera");
    let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: plain libc calls with a valid path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK) };
    assert!(fd >= 0);
    let mut writer = unsafe { std::fs::File::from_raw_fd(fd) };

    let socket = dir.join("glassd.sock");
    let _child = spawn(
        Command::new(env!("CARGO_BIN_EXE_glassd"))
            .arg("--socket")
            .arg(&socket)
            .args(NO_HARDWARE)
            .arg("--glasshub")
            .arg(&hub)
            .arg("--gaze-params")
            .arg(dir.join("no-such-file"))
            .arg("--camera-device")
            .arg(&fifo)
            .arg("--touchpad-sysfs")
            .arg(&tp)
            .arg("--state-dir")
            .arg(dir.join("state"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit()),
    );
    let deadline = Instant::now() + T;
    while !socket.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut cl = Client::connect_with_timeout(&socket, "it", HANDSHAKE).unwrap();
    for cap in ["wear", "wink", "button-camera", "screen"] {
        assert!(cl.has_capability(cap), "{cap}");
    }
    for cap in ["mock", "touchpad", "button-power", "hinge"] {
        assert!(!cl.has_capability(cap), "{cap}");
    }
    let read = |f: &str| std::fs::read_to_string(hub.join(f)).unwrap();
    // Thresholds at start (defaults), detection and wink off, not paused.
    assert_eq!(read("don_threshold"), "450\n");
    assert_eq!(read("doff_threshold"), "400\n");
    assert_eq!(read("don_max_threshold"), "2000\n");
    assert_eq!(read("detector_gain"), "2\n");
    assert_eq!(read("don_doff_enable"), "0\n");
    assert_eq!(read("wink_enable"), "0\n");
    assert_eq!(read("pause"), "0\n");
    assert_eq!(
        std::fs::read_to_string(tp.join("sleepmode")).unwrap(),
        "0\n"
    );

    cl.subscribe(&[Topic::Button, Topic::Wear]).unwrap();
    set_settings(
        &mut cl,
        Settings {
            on_head_detection_enabled: Some(true),
            wink_enabled: Some(true),
            ..Default::default()
        },
    );
    assert_eq!(read("don_doff_enable"), "1\n");
    assert_eq!(read("wink_enable"), "1\n");
    assert_eq!(read("wink_flag_enable"), "1\n");

    let ev = |value: i32, ms: u64| RawInputEvent {
        tv_sec: (ms / 1000) as _,
        tv_usec: ((ms % 1000) * 1000) as _,
        type_: 1,
        code: 212,
        value,
    };
    let mut bytes = Vec::new();
    for raw in [ev(1, 1000), ev(0, 1100)] {
        // SAFETY: RawInputEvent is plain old data.
        let b: &[u8] = unsafe {
            std::slice::from_raw_parts((&raw as *const RawInputEvent).cast(), EVENT_SIZE)
        };
        bytes.extend_from_slice(b);
    }
    writer.write_all(&bytes).unwrap();
    let f = cl
        .recv_until(T, |f| matches!(f.body, Some(Body::Button(_))))
        .unwrap()
        .unwrap();
    let Some(Body::Button(b)) = f.body else {
        unreachable!()
    };
    assert_eq!(b.button, glass_api::proto::ButtonId::Camera as i32);
    assert_eq!(b.action, ButtonAction::Press as i32);
    // The settings file was written.
    assert!(dir.join("state/settings").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

fn head(f: &ServiceFrame) -> Option<HeadGestureKind> {
    match &f.body {
        Some(Body::HeadGesture(g)) => Some(HeadGestureKind::try_from(g.kind).unwrap()),
        _ => None,
    }
}

fn sleep_request(c: &mut Client) {
    c.send(Body2::Screen(glass_api::proto::ScreenRequest {
        action: glass_api::proto::screen_request::Action::Sleep as i32,
        duration_ms: 0,
    }))
    .unwrap();
    let f = c.recv_until(T, |f| screen(f).is_some()).unwrap().unwrap();
    assert_eq!(screen(&f), Some((Screen::Off, "request".into())));
}

const SHORT: Duration = Duration::from_millis(300);

/// A GetState round trip: every frame sent before it has been handled.
fn sync(c: &mut Client) {
    c.send(Body2::GetState(Default::default())).unwrap();
    c.recv_until(T, |f| matches!(f.body, Some(Body::FocusState(_))))
        .unwrap()
        .expect("GetState answer");
}

#[test]
fn head_gestures_follow_the_rules_in_the_mock() {
    let mut m = Mock::start("head");
    let mut c = m.client(&[Topic::Head, Topic::Screen]);
    // Screen on: no look-up detector runs.
    m.send("lookup");
    assert!(
        c.recv_until(SHORT, |f| head(f).is_some())
            .unwrap()
            .is_none()
    );
    sleep_request(&mut c);
    m.send("lookup");
    let f = c.recv_until(T, |f| head(f).is_some()).unwrap().unwrap();
    assert_eq!(head(&f), Some(HeadGestureKind::LookUp));
    let f = c.recv_until(T, |f| screen(f).is_some()).unwrap().unwrap();
    assert_eq!(screen(&f), Some((Screen::On, "look-up".into())));
    // The detector runs until the look-down, with the screen on.
    m.send("lookdown");
    let f = c.recv_until(T, |f| head(f).is_some()).unwrap().unwrap();
    assert_eq!(head(&f), Some(HeadGestureKind::LookDown));
    // Back to the normal timeout, and past the nudge inhibition that the look-up started.
    c.send(Body2::Screen(glass_api::proto::ScreenRequest {
        action: glass_api::proto::screen_request::Action::Wake as i32,
        duration_ms: 0,
    }))
    .unwrap();
    sync(&mut c);
    std::thread::sleep(Duration::from_millis(1100));
    // Nudge: not wanted by default.
    m.send("nudge");
    assert!(
        c.recv_until(SHORT, |f| head(f).is_some())
            .unwrap()
            .is_none()
    );
    // Asked for: delivered, and the screen stays on (nudge_sleep_enabled is false).
    c.send(Body2::Head(HeadRequest {
        nudge: true,
        ..Default::default()
    }))
    .unwrap();
    sync(&mut c);
    m.send("nudge");
    let f = c
        .recv_until(T, |f| head(f).is_some() || screen(f).is_some())
        .unwrap()
        .unwrap();
    assert_eq!(head(&f), Some(HeadGestureKind::Nudge));
    // Relative look-up needs its own request.
    m.send("relative-lookup");
    assert!(
        c.recv_until(SHORT, |f| head(f).is_some())
            .unwrap()
            .is_none()
    );
    c.send(Body2::Head(HeadRequest {
        relative_look_up: true,
        ..Default::default()
    }))
    .unwrap();
    sync(&mut c);
    m.send("relative-lookup");
    let f = c.recv_until(T, |f| head(f).is_some()).unwrap().unwrap();
    assert_eq!(head(&f), Some(HeadGestureKind::RelativeLookUp));
}

#[test]
fn nudge_sleep_turns_the_screen_off() {
    let mut m = Mock::start("nudge");
    let mut c = m.client(&[Topic::Head, Topic::Screen, Topic::Settings]);
    set_settings(
        &mut c,
        Settings {
            nudge_sleep_enabled: Some(true),
            ..Default::default()
        },
    );
    m.send("nudge");
    let f = c.recv_until(T, |f| head(f).is_some()).unwrap().unwrap();
    assert_eq!(head(&f), Some(HeadGestureKind::Nudge));
    let f = c.recv_until(T, |f| screen(f).is_some()).unwrap().unwrap();
    assert_eq!(screen(&f), Some((Screen::Off, "nudge".into())));
}

/// A non-mock daemon on a fake IIO device: the accelerometer stream is started when the
/// screen goes off, and a look-up in the samples wakes the screen.
#[test]
fn look_up_from_a_fake_imu() {
    use std::ffi::CString;
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;

    let dir = test_dir("imu");
    let iio = dir.join("iio");
    let d = iio.join("iio:device0");
    std::fs::create_dir_all(d.join("scan_elements")).unwrap();
    std::fs::create_dir_all(d.join("buffer")).unwrap();
    std::fs::write(d.join("name"), "mpu9150\n").unwrap();
    for f in [
        "power_state",
        "sampling_frequency",
        "gyro_enable",
        "accl_enable",
        "buffer/enable",
    ] {
        std::fs::write(d.join(f), "").unwrap();
    }
    let scan = [
        ("in_anglvel_x", 4),
        ("in_anglvel_y", 5),
        ("in_anglvel_z", 6),
        ("in_accel_x", 7),
        ("in_accel_y", 8),
        ("in_accel_z", 9),
        ("in_timestamp", 13),
    ];
    for (n, i) in scan {
        let ty = if n == "in_timestamp" {
            "le:s64/64>>0"
        } else {
            "le:s16/16>>0"
        };
        std::fs::write(d.join(format!("scan_elements/{n}_en")), "0\n").unwrap();
        std::fs::write(d.join(format!("scan_elements/{n}_index")), format!("{i}\n")).unwrap();
        std::fs::write(d.join(format!("scan_elements/{n}_type")), format!("{ty}\n")).unwrap();
    }
    let fifo = dir.join("iio-node");
    let cpath = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: plain libc calls with a valid path.
    assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK) };
    assert!(fd >= 0);
    let mut writer = unsafe { std::fs::File::from_raw_fd(fd) };

    let socket = dir.join("glassd.sock");
    let _child = spawn(
        Command::new(env!("CARGO_BIN_EXE_glassd"))
            .arg("--socket")
            .arg(&socket)
            .args(NO_HARDWARE)
            .arg("--iio")
            .arg(&iio)
            .arg("--iio-device")
            .arg(&fifo)
            .arg("--state-dir")
            .arg(dir.join("state"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit()),
    );
    let deadline = Instant::now() + T;
    while !socket.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut c = Client::connect_with_timeout(&socket, "it", HANDSHAKE).unwrap();
    assert!(c.has_capability("head"));
    c.subscribe(&[Topic::Head, Topic::Screen]).unwrap();
    let rd = |f: &str| std::fs::read_to_string(d.join(f)).unwrap_or_default();
    // Screen on, nothing wanted: the IMU is not started.
    assert_ne!(rd("buffer/enable"), "1\n");
    sleep_request(&mut c);
    let deadline = Instant::now() + T;
    while rd("buffer/enable") != "1\n" {
        assert!(
            Instant::now() < deadline,
            "the accelerometer stream did not start"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(rd("accl_enable"), "1\n");
    assert_eq!(rd("gyro_enable"), "0\n");
    assert_eq!(rd("scan_elements/in_accel_x_en"), "1\n");
    assert_eq!(rd("scan_elements/in_anglvel_x_en"), "0\n");
    // Accel only: 6 data bytes, the format byte at 7, the timestamp at 8 (16-byte records).
    // Board matrix: out = (chip.y, chip.z, chip.x), so chip = (-sin p, 0, cos p) for a pitch p.
    let mut bytes = Vec::new();
    for i in 0..100i64 {
        let pitch: f32 = if i < 50 { 0.0 } else { 40.0 };
        let p = pitch.to_radians();
        let chip = [
            (-p.sin() * 16384.0) as i16,
            0i16,
            (p.cos() * 16384.0) as i16,
        ];
        for v in chip {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        bytes.extend_from_slice(&[0, 0x04]);
        bytes.extend_from_slice(&(i * 20_000_000).to_le_bytes());
    }
    writer.write_all(&bytes).unwrap();
    let f = c.recv_until(T, |f| head(f).is_some()).unwrap().unwrap();
    assert_eq!(head(&f), Some(HeadGestureKind::LookUp));
    let f = c.recv_until(T, |f| screen(f).is_some()).unwrap().unwrap();
    assert_eq!(screen(&f), Some((Screen::On, "look-up".into())));
    let _ = std::fs::remove_dir_all(&dir);
}

fn companion(f: &ServiceFrame) -> Option<glass_api::proto::CompanionState> {
    match &f.body {
        Some(Body::CompanionState(c)) => Some(c.clone()),
        _ => None,
    }
}

fn dictation(f: &ServiceFrame) -> Option<(u32, glass_api::proto::dictation_state::State, String)> {
    match &f.body {
        Some(Body::DictationState(d)) => Some((
            d.request_id,
            glass_api::proto::dictation_state::State::try_from(d.state).unwrap(),
            d.error.clone(),
        )),
        _ => None,
    }
}

fn free_start(rid: u32) -> Body2 {
    Body2::DictationStart(glass_api::proto::DictationStart {
        request_id: rid,
        purpose: "free".into(),
        ..Default::default()
    })
}

#[test]
fn mock_companion_relays_a_free_dictation() {
    use glass_api::proto::dictation_state::State as St;
    let mut m = Mock::start("companion");
    let mut c = m.client(&[Topic::Companion]);
    assert!(c.has_capability("dictation") && c.has_capability("companion"));
    // No provider yet: refused.
    c.send(free_start(1)).unwrap();
    let e = c
        .recv_until(T, |f| matches!(f.body, Some(Body::Error(_))))
        .unwrap()
        .unwrap();
    match e.body {
        Some(Body::Error(e)) => {
            assert_eq!(e.code, glass_api::proto::ErrorCode::NoCompanion as i32)
        }
        _ => unreachable!(),
    }
    m.send("companion on");
    let st = c
        .recv_until(T, |f| companion(f).is_some_and(|s| s.phone_connected))
        .unwrap()
        .and_then(|f| companion(&f))
        .expect("companion state");
    assert!(st.provider);
    assert_eq!(st.phone_name, "Mock phone");
    assert_eq!(st.phone_capabilities, vec!["dictation-free".to_string()]);
    c.send(free_start(7)).unwrap();
    let f = c
        .recv_until(T, |f| dictation(f).is_some())
        .unwrap()
        .unwrap();
    assert_eq!(dictation(&f), Some((7, St::Listening, String::new())));
    m.send("transcript buy");
    m.send("transcript-final buy milk");
    let f = c
        .recv_until(
            T,
            |f| matches!(&f.body, Some(Body::Transcript(t)) if t.r#final),
        )
        .unwrap()
        .unwrap();
    match f.body {
        Some(Body::Transcript(t)) => {
            assert_eq!((t.request_id, t.text.as_str()), (7, "buy milk"))
        }
        _ => unreachable!(),
    }
    let f = c
        .recv_until(T, |f| dictation(f).is_some())
        .unwrap()
        .unwrap();
    assert_eq!(dictation(&f), Some((7, St::Done, String::new())));
    // An error, then the phone goes away mid-request.
    c.send(free_start(8)).unwrap();
    c.recv_until(T, |f| dictation(f).is_some())
        .unwrap()
        .unwrap();
    m.send("dictation-error no-speech");
    let f = c
        .recv_until(T, |f| dictation(f).is_some())
        .unwrap()
        .unwrap();
    assert_eq!(dictation(&f), Some((8, St::Failed, "no-speech".into())));
    c.send(free_start(9)).unwrap();
    c.recv_until(T, |f| dictation(f).is_some())
        .unwrap()
        .unwrap();
    m.send("companion off");
    let f = c
        .recv_until(T, |f| dictation(f).is_some())
        .unwrap()
        .unwrap();
    assert_eq!(dictation(&f), Some((9, St::Failed, "link-lost".into())));
    m.send("companion gone");
    c.recv_until(T, |f| companion(f).is_some_and(|s| !s.provider))
        .unwrap()
        .expect("provider gone");
}

#[test]
fn a_provider_client_relays_over_the_socket() {
    use glass_api::proto::{
        CompanionRegister, CompanionStatus, DictationState, Transcript,
        dictation_state::State as St,
    };
    let m = Mock::start("provider");
    let mut provider = m.client(&[]);
    provider
        .send(Body2::CompanionRegister(CompanionRegister {
            name: "it-provider".into(),
        }))
        .unwrap();
    provider
        .send(Body2::CompanionStatus(CompanionStatus {
            phone_connected: true,
            phone_capabilities: vec!["dictation-free".into()],
            phone_name: "Phone".into(),
        }))
        .unwrap();
    let mut app = m.client(&[]);
    // The snapshot on Subscribe tells the state.
    app.subscribe(&[Topic::Companion]).unwrap();
    app.recv_until(T, |f| {
        companion(f).is_some_and(|s| s.snapshot && s.phone_connected)
    })
    .unwrap()
    .expect("phone connected");
    app.send(free_start(3)).unwrap();
    let f = provider
        .recv_until(T, |f| {
            matches!(f.body, Some(Body::ProviderDictationStart(_)))
        })
        .unwrap()
        .unwrap();
    let link = match f.body {
        Some(Body::ProviderDictationStart(d)) => d.request_id,
        _ => unreachable!(),
    };
    provider
        .send(Body2::ProviderDictationState(DictationState {
            request_id: link,
            state: St::Listening as i32,
            error: String::new(),
        }))
        .unwrap();
    provider
        .send(Body2::ProviderTranscript(Transcript {
            request_id: link,
            text: "hello".into(),
            r#final: true,
        }))
        .unwrap();
    let f = app
        .recv_until(T, |f| dictation(f).is_some())
        .unwrap()
        .unwrap();
    assert_eq!(dictation(&f), Some((3, St::Listening, String::new())));
    let f = app
        .recv_until(T, |f| dictation(f).is_some_and(|d| d.1 == St::Done))
        .unwrap();
    assert!(f.is_some());
    // The mock's own provider cannot register while a real one is.
    drop(provider);
    app.recv_until(T, |f| companion(f).is_some_and(|s| !s.provider))
        .unwrap()
        .expect("provider gone");
}

#[test]
fn peer_check_refuses_unknown_uids() {
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    if glassd::peers::ALWAYS.contains(&uid) {
        eprintln!("skipped: the tests run as uid {uid}, which is always allowed");
        return;
    }
    let dir = test_dir("peers");
    {
        let m = Mock::start_in(&dir, &["--peer-check", "on", "--allow-uid", "4242"]);
        let r = Client::connect_with_timeout(&m.socket, "it", HANDSHAKE);
        assert!(r.is_err(), "connection should be refused");
    }
    let own = uid.to_string();
    let m = Mock::start_in(&dir, &["--allow-uid", own.as_str()]);
    m.client(&[]);
}

#[test]
fn listens_on_a_socket_passed_by_init() {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    let dir = test_dir("init-socket");
    // As Android's init does: a socket that is bound but not listening.
    let path = dir.join("glassd");
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    assert!(fd >= 0);
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as _;
    let bytes = path.as_os_str().as_encoded_bytes();
    for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
        *d = *s as _;
    }
    // SAFETY: a valid sockaddr_un of the given size.
    let r = unsafe {
        libc::bind(
            fd,
            (&addr as *const libc::sockaddr_un).cast(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    assert_eq!(r, 0, "bind: {}", std::io::Error::last_os_error());
    // SAFETY: we own fd.
    let listener = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    let _ = listener.as_raw_fd();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_glassd"));
    cmd.args(["--mock", "--socket"])
        .arg(&path)
        .env("ANDROID_SOCKET_glassd", fd.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    // SAFETY: only fcntl on a descriptor of this process, between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            Ok(())
        });
    }
    let child = spawn(&mut cmd);
    drop(listener);
    // Until glassd calls listen(), connecting is refused.
    let deadline = Instant::now() + HANDSHAKE;
    let mut c = loop {
        match Client::connect_with_timeout(&path, "it", HANDSHAKE) {
            Ok(c) => break c,
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("connect: {e}"),
        }
    };
    assert!(c.has_capability("mock"));
    c.subscribe(&[Topic::Screen]).unwrap();
    drop(c);
    drop(child);
    // glassd did not own the file, so it left it in place.
    assert!(path.exists());
    let _ = std::fs::remove_dir_all(&dir);
}
