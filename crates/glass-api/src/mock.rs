//! In-process mock of glassd for tests of clients (the launcher, apps, the Android glue).
//!
//! [`MockService::start`] listens on a socket in a fresh temporary directory and serves the
//! session rules of docs/api.md through the same [`Hub`] glassd uses: `Hello`/`Welcome`
//! (capabilities include `mock`), `Subscribe` snapshots, the focus stack, settings. A test
//! publishes service events with [`MockService::publish`] (touchpad frames go to the focus
//! holder, other topics to their subscribers) and sees every frame the clients sent with
//! [`MockService::next_client_frame`].
//!
//! `ScreenRequest` is applied simply: `ACTION_WAKE` and `ACTION_KEEP_ON` turn the screen on,
//! `ACTION_SLEEP` turns it off (reason `"request"`); there are no timers.

use crate::codec::{FrameDecoder, encode_frame};
use crate::hub::{ClientId, Effect, Hub};
use crate::proto::{
    ClientFrame, Screen, ScreenState, ServiceFrame, Wear, WearState, screen_request, service_frame,
};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Capabilities the mock announces.
pub const MOCK_CAPABILITIES: &[&str] = &[
    "mock",
    "touchpad",
    "screen",
    "button-camera",
    "button-power",
    "hinge",
    "head",
    "wear",
    "wink",
];

struct State {
    hub: Hub,
    writers: HashMap<ClientId, UnixStream>,
    received: VecDeque<(ClientId, ClientFrame)>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    stop: AtomicBool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// See the module documentation.
pub struct MockService {
    shared: Arc<Shared>,
    path: PathBuf,
    dir: PathBuf,
    accept: Option<JoinHandle<()>>,
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn now_us() -> u64 {
    crate::monotonic_us()
}

impl MockService {
    /// Starts a mock on a socket in a new temporary directory.
    pub fn start() -> io::Result<MockService> {
        let dir = std::env::temp_dir().join(format!(
            "glass-mock-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("glassd.sock");
        let listener = UnixListener::bind(&path)?;
        let mut hub = Hub::new(
            concat!("mock-", env!("CARGO_PKG_VERSION")),
            MOCK_CAPABILITIES,
        );
        let t = now_us();
        hub.set_screen_source(ScreenState {
            screen: Screen::On as i32,
            reason: "request".into(),
            timestamp_us: t,
            ..Default::default()
        });
        hub.set_wear_source(WearState {
            wear: Wear::OnHead as i32,
            source: crate::proto::WearSource::Forced as i32,
            snapshot: false,
            timestamp_us: t,
        });
        hub.set_settings_source(crate::hub::settings_rules::defaults());
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                hub,
                writers: HashMap::new(),
                received: VecDeque::new(),
            }),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        let s2 = shared.clone();
        let accept = std::thread::spawn(move || accept_loop(listener, s2));
        Ok(MockService {
            shared,
            path,
            dir,
            accept: Some(accept),
        })
    }

    /// The socket path to connect to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Publishes a service event as glassd would (focus holder for touchpad frames,
    /// subscribers for the other topics).
    pub fn publish(&self, frame: ServiceFrame) {
        let mut st = self.shared.lock();
        let fx = st.hub.publish(frame);
        run_effects(&mut st, fx);
    }

    /// Publishes a frame body; see [`publish`](Self::publish).
    pub fn publish_body(&self, body: service_frame::Body) {
        self.publish(ServiceFrame { body: Some(body) });
    }

    /// Sends a frame to one client, bypassing all routing rules.
    pub fn send_to(&self, client: ClientId, frame: ServiceFrame) {
        let mut st = self.shared.lock();
        run_effects(&mut st, vec![Effect::Send(client, frame)]);
    }

    /// The next frame any client sent (Hello included), waiting up to `timeout`.
    pub fn next_client_frame(&self, timeout: Duration) -> Option<(ClientId, ClientFrame)> {
        let deadline = Instant::now() + timeout;
        let mut st = self.shared.lock();
        loop {
            if let Some(f) = st.received.pop_front() {
                return Some(f);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            st = self
                .shared
                .changed
                .wait_timeout(st, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Waits until `pred` holds for the hub (for example a client holds the focus).
    pub fn wait_until(&self, timeout: Duration, pred: impl Fn(&Hub) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut st = self.shared.lock();
        loop {
            if pred(&st.hub) {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            st = self
                .shared
                .changed
                .wait_timeout(st, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Runs `f` on the hub (read its state: clients, focus stack, settings).
    pub fn with_hub<R>(&self, f: impl FnOnce(&Hub) -> R) -> R {
        f(&self.shared.lock().hub)
    }

    /// The current focus holder.
    pub fn focus_holder(&self) -> Option<ClientId> {
        self.with_hub(|h| h.focus_holder())
    }
}

impl Drop for MockService {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.path); // wakes the accept loop
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
        let st = self.shared.lock();
        for s in st.writers.values() {
            let _ = s.shutdown(Shutdown::Both);
        }
        drop(st);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn accept_loop(listener: UnixListener, shared: Arc<Shared>) {
    for conn in listener.incoming() {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = conn else { continue };
        let Ok(writer) = stream.try_clone() else {
            continue;
        };
        let _ = writer.set_write_timeout(Some(Duration::from_secs(2)));
        let id = {
            let mut st = shared.lock();
            let id = st.hub.add_client();
            st.writers.insert(id, writer);
            id
        };
        shared.changed.notify_all();
        let s2 = shared.clone();
        std::thread::spawn(move || client_loop(id, stream, s2));
    }
}

fn client_loop(id: ClientId, mut stream: UnixStream, shared: Arc<Shared>) {
    let mut dec = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        dec.push(&buf[..n]);
        loop {
            match dec.next_message::<ClientFrame>() {
                Ok(None) => break,
                Ok(Some(f)) => {
                    let mut st = shared.lock();
                    st.received.push_back((id, f.clone()));
                    let fx = st.hub.on_frame(id, f);
                    run_effects(&mut st, fx);
                    drop(st);
                    shared.changed.notify_all();
                }
                Err(e) => {
                    let mut st = shared.lock();
                    let fx = st.hub.bad_frame(id, &e.to_string());
                    run_effects(&mut st, fx);
                    drop(st);
                    shared.changed.notify_all();
                    return;
                }
            }
        }
    }
    let mut st = shared.lock();
    st.writers.remove(&id);
    let fx = st.hub.remove_client(id);
    run_effects(&mut st, fx);
    drop(st);
    shared.changed.notify_all();
}

fn run_effects(st: &mut State, fx: Vec<Effect>) {
    let mut pending: VecDeque<Effect> = fx.into();
    while let Some(e) = pending.pop_front() {
        match e {
            Effect::Send(id, frame) => {
                let mut buf = Vec::new();
                if encode_frame(&frame, &mut buf).is_ok()
                    && let Some(w) = st.writers.get_mut(&id)
                    && w.write_all(&buf).is_err()
                {
                    let _ = w.shutdown(Shutdown::Both);
                }
            }
            Effect::Close(id) => {
                if let Some(w) = st.writers.remove(&id) {
                    let _ = w.shutdown(Shutdown::Both);
                }
            }
            Effect::Screen { action, .. } => {
                use screen_request::Action as A;
                let target = match action {
                    A::Wake | A::KeepOn => Some(Screen::On),
                    A::Sleep => Some(Screen::Off),
                    _ => None,
                };
                let current = st.hub.screen().map(|s| s.screen);
                if let Some(t) = target
                    && current != Some(t as i32)
                {
                    pending.extend(st.hub.publish(ServiceFrame {
                        body: Some(service_frame::Body::ScreenState(ScreenState {
                            screen: t as i32,
                            reason: "request".into(),
                            timestamp_us: now_us(),
                            ..Default::default()
                        })),
                    }));
                }
            }
            Effect::SettingsChanged(_) | Effect::Head { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Client, ClientError};
    use crate::codec::put_varint32;
    use crate::proto::{
        ClientFrame, ErrorCode, FocusState, Gesture, GestureKind, ScreenRequest, Topic,
        client_frame,
    };
    use service_frame::Body as S;

    const T: Duration = Duration::from_secs(5);

    fn tap() -> ServiceFrame {
        ServiceFrame {
            body: Some(S::Gesture(Gesture {
                kind: GestureKind::Tap as i32,
                fingers: 1,
                velocity: 0.0,
                timestamp_us: 1,
            })),
        }
    }

    #[test]
    fn handshake_announces_mock() {
        let m = MockService::start().unwrap();
        let c = Client::connect_as(m.path(), "unit").unwrap();
        assert!(c.has_capability("mock"));
        assert_eq!(c.welcome().api_version, 1);
        let (_, f) = m.next_client_frame(T).unwrap();
        let Some(client_frame::Body::Hello(h)) = f.body else {
            panic!()
        };
        assert_eq!(h.client_name, "unit");
    }

    #[test]
    fn snapshot_on_subscribe() {
        let m = MockService::start().unwrap();
        let mut c = Client::connect(m.path()).unwrap();
        c.subscribe(&[Topic::Screen, Topic::Wear, Topic::Settings])
            .unwrap();
        let a = c.recv(Some(T)).unwrap().unwrap().body.unwrap();
        let b = c.recv(Some(T)).unwrap().unwrap().body.unwrap();
        let d = c.recv(Some(T)).unwrap().unwrap().body.unwrap();
        assert!(matches!(a, S::ScreenState(ref s) if s.reason == "snapshot"));
        assert!(matches!(b, S::WearState(ref w) if w.snapshot));
        assert!(matches!(d, S::Settings(ref s) if s.snapshot));
    }

    #[test]
    fn gesture_goes_to_focus_holder_and_focus_moves_back_on_disconnect() {
        let m = MockService::start().unwrap();
        let mut a = Client::connect(m.path()).unwrap();
        a.subscribe(&[Topic::Gesture]).unwrap();
        a.acquire_focus().unwrap();
        assert_eq!(
            a.recv(Some(T)).unwrap().unwrap().body,
            Some(S::FocusState(FocusState { focused: true }))
        );
        m.publish(tap());
        assert!(matches!(
            a.recv(Some(T)).unwrap().unwrap().body,
            Some(S::Gesture(_))
        ));

        let mut b = Client::connect(m.path()).unwrap();
        b.subscribe(&[Topic::Gesture]).unwrap();
        b.acquire_focus().unwrap();
        assert_eq!(
            a.recv(Some(T)).unwrap().unwrap().body,
            Some(S::FocusState(FocusState { focused: false }))
        );
        assert_eq!(
            b.recv(Some(T)).unwrap().unwrap().body,
            Some(S::FocusState(FocusState { focused: true }))
        );
        m.publish(tap());
        assert!(matches!(
            b.recv(Some(T)).unwrap().unwrap().body,
            Some(S::Gesture(_))
        ));
        assert_eq!(a.recv(Some(Duration::from_millis(100))).unwrap(), None);

        drop(b);
        assert_eq!(
            a.recv(Some(T)).unwrap().unwrap().body,
            Some(S::FocusState(FocusState { focused: true }))
        );
    }

    #[test]
    fn frame_before_hello_is_rejected_and_closed() {
        let m = MockService::start().unwrap();
        let mut s = UnixStream::connect(m.path()).unwrap();
        let mut buf = Vec::new();
        encode_frame(
            &ClientFrame {
                seq: 9,
                body: Some(client_frame::Body::GetState(Default::default())),
            },
            &mut buf,
        )
        .unwrap();
        s.write_all(&buf).unwrap();
        let mut got = Vec::new();
        s.set_read_timeout(Some(T)).unwrap();
        s.read_to_end(&mut got).unwrap();
        let mut d = FrameDecoder::new();
        d.push(&got);
        let f: ServiceFrame = d.next_message().unwrap().unwrap();
        let Some(S::Error(e)) = f.body else { panic!() };
        assert_eq!(e.code, ErrorCode::BadFrame as i32);
        assert_eq!(e.seq, 9);
    }

    #[test]
    fn oversize_frame_is_rejected_and_closed() {
        let m = MockService::start().unwrap();
        let mut s = UnixStream::connect(m.path()).unwrap();
        let mut buf = Vec::new();
        put_varint32(70_000, &mut buf);
        s.write_all(&buf).unwrap();
        let mut got = Vec::new();
        s.set_read_timeout(Some(T)).unwrap();
        s.read_to_end(&mut got).unwrap();
        let mut d = FrameDecoder::new();
        d.push(&got);
        let f: ServiceFrame = d.next_message().unwrap().unwrap();
        let Some(S::Error(e)) = f.body else { panic!() };
        assert_eq!(e.code, ErrorCode::BadFrame as i32);
    }

    #[test]
    fn screen_request_is_applied_and_broadcast() {
        let m = MockService::start().unwrap();
        let mut c = Client::connect(m.path()).unwrap();
        c.subscribe(&[Topic::Screen]).unwrap();
        let _snapshot = c.recv(Some(T)).unwrap();
        c.send(client_frame::Body::Screen(ScreenRequest {
            action: screen_request::Action::Sleep as i32,
            duration_ms: 0,
        }))
        .unwrap();
        let Some(S::ScreenState(s)) = c.recv(Some(T)).unwrap().unwrap().body else {
            panic!()
        };
        assert_eq!(s.screen, Screen::Off as i32);
        assert_eq!(s.reason, "request");
    }

    #[test]
    fn mock_drop_closes_clients() {
        let m = MockService::start().unwrap();
        let mut c = Client::connect(m.path()).unwrap();
        drop(m);
        assert!(matches!(
            c.recv(Some(T)),
            Err(ClientError::Closed) | Err(ClientError::Io(_))
        ));
    }
}
