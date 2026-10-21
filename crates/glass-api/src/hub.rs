//! Session and routing rules of docs/api.md, as a pure state machine.
//!
//! The hub knows the connected clients, their handshake state and topics, the input focus
//! stack, and the last state of the snapshot topics. It does no I/O: every call returns the
//! [`Effect`]s the owner (glassd, or the in-process [`MockService`](crate::MockService)) must
//! carry out. Both servers use it, so they behave the same.

use crate::proto::{
    ClientFrame, ErrorCode, FocusState, Hello, ScreenState, Scroll, ServiceError, ServiceFrame,
    Settings, Topic, WearState, Welcome, client_frame, screen_request, service_frame,
};
use std::collections::BTreeMap;

/// The interface version this crate implements.
pub const API_VERSION: u32 = 1;

/// Identifies a connection for the lifetime of the hub. Never reused.
pub type ClientId = u64;

/// Something the owner of the hub must do.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Queue this frame for the client.
    Send(ClientId, ServiceFrame),
    /// Flush what is queued for the client, then close the connection. The hub has already
    /// forgotten the client; the owner must not call [`Hub::remove_client`] for it again
    /// (it is harmless if it does).
    Close(ClientId),
    /// A valid `ScreenRequest` for the screen policy. `action` is a known, specified action.
    Screen {
        client: ClientId,
        action: screen_request::Action,
        duration_ms: u32,
    },
    /// The settings changed (after merging and clamping); apply them.
    SettingsChanged(Settings),
}

/// Bounds and merge rule for `SetSettings` (docs/api.md, "Settings").
pub mod settings_rules {
    use crate::proto::Settings;

    pub const HEAD_WAKE_ANGLE_MIN: f32 = 10.0;
    pub const HEAD_WAKE_ANGLE_MAX: f32 = 60.0;
    pub const SCREEN_TIMEOUT_MIN_MS: u32 = 3_000;
    pub const SCREEN_TIMEOUT_MAX_MS: u32 = 600_000;

    /// Copies the fields present in `change` into `current`, clamped to their ranges.
    pub fn merge(current: &mut Settings, change: &Settings) {
        if let Some(v) = change.head_wake_enabled {
            current.head_wake_enabled = Some(v);
        }
        if let Some(v) = change.head_wake_angle_deg {
            let v = if v.is_nan() { HEAD_WAKE_ANGLE_MIN } else { v };
            current.head_wake_angle_deg = Some(v.clamp(HEAD_WAKE_ANGLE_MIN, HEAD_WAKE_ANGLE_MAX));
        }
        if let Some(v) = change.wink_enabled {
            current.wink_enabled = Some(v);
        }
        if let Some(v) = change.on_head_detection_enabled {
            current.on_head_detection_enabled = Some(v);
        }
        if let Some(v) = change.screen_timeout_ms {
            current.screen_timeout_ms = Some(v.clamp(SCREEN_TIMEOUT_MIN_MS, SCREEN_TIMEOUT_MAX_MS));
        }
        current.snapshot = false;
    }
}

#[derive(Debug, Default)]
struct ClientState {
    hello: bool,
    name: String,
    topics: Vec<Topic>,
}

impl ClientState {
    fn subscribed(&self, t: Topic) -> bool {
        self.topics.contains(&t)
    }
}

/// Routing of one touch (from the first finger down to the last finger up).
#[derive(Debug, Default)]
struct TouchRoute {
    active: bool,
    /// The client that receives this touch; `None` if nobody did at touch down, or the
    /// focus moved away during the touch.
    owner: Option<ClientId>,
    /// The last `Scroll` sent to the owner during this touch, if it did not end the scroll.
    open_scroll: Option<Scroll>,
}

/// The session and routing state machine.
#[derive(Debug)]
pub struct Hub {
    service_version: String,
    capabilities: Vec<String>,
    next_id: ClientId,
    clients: BTreeMap<ClientId, ClientState>,
    /// Focus stack, top last.
    focus: Vec<ClientId>,
    touch: TouchRoute,
    screen: Option<ScreenState>,
    wear: Option<WearState>,
    settings: Option<Settings>,
}

fn frame(body: service_frame::Body) -> ServiceFrame {
    ServiceFrame { body: Some(body) }
}

fn error(code: ErrorCode, message: &str, seq: u32) -> ServiceFrame {
    frame(service_frame::Body::Error(ServiceError {
        code: code as i32,
        message: message.into(),
        seq,
    }))
}

fn focus_state(focused: bool) -> ServiceFrame {
    frame(service_frame::Body::FocusState(FocusState { focused }))
}

/// The topic a service frame belongs to, or `None` for session frames.
pub fn topic_of(body: &service_frame::Body) -> Option<Topic> {
    use service_frame::Body as B;
    Some(match body {
        B::Gesture(_) | B::Scroll(_) | B::FingerCount(_) => Topic::Gesture,
        B::HeadGesture(_) => Topic::Head,
        B::WearState(_) | B::Wink(_) => Topic::Wear,
        B::ScreenState(_) => Topic::Screen,
        B::Settings(_) => Topic::Settings,
        B::DictationState(_) | B::Transcript(_) => Topic::Dictation,
        B::Button(_) => Topic::Button,
        B::Welcome(_) | B::Error(_) | B::FocusState(_) => return None,
    })
}

impl Hub {
    /// A hub announcing `capabilities` in `Welcome`.
    pub fn new(service_version: impl Into<String>, capabilities: &[&str]) -> Self {
        Hub {
            service_version: service_version.into(),
            capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
            next_id: 1,
            clients: BTreeMap::new(),
            focus: Vec::new(),
            touch: TouchRoute::default(),
            screen: None,
            wear: None,
            settings: None,
        }
    }

    /// Declares that the screen source exists, with its current state (no frame is sent).
    pub fn set_screen_source(&mut self, state: ScreenState) {
        self.screen = Some(state);
    }

    /// Declares that the wear source exists, with its current state (no frame is sent).
    pub fn set_wear_source(&mut self, state: WearState) {
        self.wear = Some(state);
    }

    /// Declares the settings store, with its current values (no frame is sent).
    pub fn set_settings_source(&mut self, settings: Settings) {
        self.settings = Some(settings);
    }

    pub fn settings(&self) -> Option<&Settings> {
        self.settings.as_ref()
    }

    pub fn capabilities(&self) -> &[String] {
        &self.capabilities
    }

    /// Registers a new connection.
    pub fn add_client(&mut self) -> ClientId {
        let id = self.next_id;
        self.next_id += 1;
        self.clients.insert(id, ClientState::default());
        id
    }

    /// Ids of the connected clients.
    pub fn clients(&self) -> impl Iterator<Item = ClientId> + '_ {
        self.clients.keys().copied()
    }

    /// The name the client gave in `Hello`, if it sent one.
    pub fn client_name(&self, id: ClientId) -> Option<&str> {
        self.clients
            .get(&id)
            .filter(|c| c.hello)
            .map(|c| c.name.as_str())
    }

    /// The current focus holder.
    pub fn focus_holder(&self) -> Option<ClientId> {
        self.focus.last().copied()
    }

    /// The focus stack, bottom first.
    pub fn focus_stack(&self) -> &[ClientId] {
        &self.focus
    }

    /// The client disconnected (or the owner closed it).
    pub fn remove_client(&mut self, id: ClientId) -> Vec<Effect> {
        let mut fx = Vec::new();
        if self.clients.remove(&id).is_none() {
            return fx;
        }
        self.drop_from_focus(id, &mut fx);
        if self.touch.owner == Some(id) {
            self.touch.owner = None;
            self.touch.open_scroll = None;
        }
        fx
    }

    /// The client sent bytes that are not a valid frame: report and close.
    pub fn bad_frame(&mut self, id: ClientId, message: &str) -> Vec<Effect> {
        self.fail(id, message, 0)
    }

    fn fail(&mut self, id: ClientId, message: &str, seq: u32) -> Vec<Effect> {
        let mut fx = vec![Effect::Send(id, error(ErrorCode::BadFrame, message, seq))];
        fx.extend(self.remove_client(id));
        fx.push(Effect::Close(id));
        fx
    }

    /// Handles one decoded frame from a client.
    pub fn on_frame(&mut self, id: ClientId, f: ClientFrame) -> Vec<Effect> {
        use client_frame::Body as B;
        let Some(state) = self.clients.get_mut(&id) else {
            return Vec::new();
        };
        let seq = f.seq;
        if !state.hello && !matches!(f.body, Some(B::Hello(_))) {
            return self.fail(id, "Hello must be the first frame", seq);
        }
        let mut fx = Vec::new();
        match f.body {
            None => fx.push(Effect::Send(
                id,
                error(ErrorCode::Unsupported, "unknown request", seq),
            )),
            Some(B::Hello(h)) => return self.hello(id, h, seq),
            Some(B::Subscribe(s)) => {
                let mut topics = Vec::new();
                for t in s.topics {
                    match Topic::try_from(t) {
                        Ok(Topic::Unspecified) | Err(_) => {}
                        Ok(t) if !topics.contains(&t) => topics.push(t),
                        Ok(_) => {}
                    }
                }
                state.topics = topics;
                self.snapshots(id, &mut fx);
            }
            Some(B::AcquireFocus(_)) => self.acquire_focus(id, &mut fx),
            Some(B::ReleaseFocus(_)) => self.drop_from_focus(id, &mut fx),
            Some(B::Screen(r)) => match screen_request::Action::try_from(r.action) {
                Ok(screen_request::Action::Unspecified) | Err(_) => fx.push(Effect::Send(
                    id,
                    error(ErrorCode::InvalidArgument, "invalid screen action", seq),
                )),
                Ok(action) => fx.push(Effect::Screen {
                    client: id,
                    action,
                    duration_ms: r.duration_ms,
                }),
            },
            Some(B::SetSettings(s)) => match self.settings.as_mut() {
                None => fx.push(Effect::Send(
                    id,
                    error(ErrorCode::Unsupported, "no settings store", seq),
                )),
                Some(current) => {
                    if let Some(change) = s.settings {
                        settings_rules::merge(current, &change);
                    }
                    let applied = *current;
                    fx.push(Effect::SettingsChanged(applied));
                    fx.push(Effect::Send(
                        id,
                        frame(service_frame::Body::Settings(applied)),
                    ));
                    for (&other, c) in &self.clients {
                        if other != id && c.hello && c.subscribed(Topic::Settings) {
                            fx.push(Effect::Send(
                                other,
                                frame(service_frame::Body::Settings(applied)),
                            ));
                        }
                    }
                }
            },
            Some(B::GetState(_)) => {
                self.snapshots(id, &mut fx);
                let focused = self.focus_holder() == Some(id);
                fx.push(Effect::Send(id, focus_state(focused)));
            }
            Some(B::DictationStart(d)) => {
                let valid = match d.purpose.as_str() {
                    "free" => true,
                    "notification-reply" => !d.notification_key.is_empty(),
                    _ => false,
                };
                let e = if valid {
                    error(ErrorCode::NoCompanion, "no companion link", seq)
                } else {
                    error(ErrorCode::InvalidArgument, "invalid dictation request", seq)
                };
                fx.push(Effect::Send(id, e));
            }
            // Nothing to cancel or confirm without a companion link.
            Some(B::DictationCancel(_)) | Some(B::DictationConfirm(_)) => {}
        }
        fx
    }

    fn hello(&mut self, id: ClientId, h: Hello, seq: u32) -> Vec<Effect> {
        if h.api_version == 0 {
            return self.fail(id, "api_version 0 is invalid", seq);
        }
        let state = self.clients.get_mut(&id).expect("checked by caller");
        if !state.hello {
            state.hello = true;
            state.name = h.client_name;
        }
        vec![Effect::Send(
            id,
            frame(service_frame::Body::Welcome(Welcome {
                api_version: h.api_version.min(API_VERSION),
                service_version: self.service_version.clone(),
                capabilities: self.capabilities.clone(),
            })),
        )]
    }

    fn snapshots(&self, id: ClientId, fx: &mut Vec<Effect>) {
        let Some(c) = self.clients.get(&id) else {
            return;
        };
        for t in &c.topics {
            let body = match t {
                Topic::Screen => self.screen.as_ref().map(|s| {
                    service_frame::Body::ScreenState(ScreenState {
                        reason: "snapshot".into(),
                        ..s.clone()
                    })
                }),
                Topic::Wear => self.wear.as_ref().map(|w| {
                    service_frame::Body::WearState(WearState {
                        snapshot: true,
                        ..*w
                    })
                }),
                Topic::Settings => self.settings.as_ref().map(|s| {
                    service_frame::Body::Settings(Settings {
                        snapshot: true,
                        ..*s
                    })
                }),
                _ => None,
            };
            if let Some(b) = body {
                fx.push(Effect::Send(id, frame(b)));
            }
        }
    }

    fn acquire_focus(&mut self, id: ClientId, fx: &mut Vec<Effect>) {
        let old = self.focus_holder();
        self.focus.retain(|&c| c != id);
        self.focus.push(id);
        if let Some(old) = old.filter(|&o| o != id) {
            fx.push(Effect::Send(old, focus_state(false)));
        }
        fx.push(Effect::Send(id, focus_state(true)));
        self.focus_changed(fx);
    }

    fn drop_from_focus(&mut self, id: ClientId, fx: &mut Vec<Effect>) {
        let was_top = self.focus_holder() == Some(id);
        self.focus.retain(|&c| c != id);
        if was_top {
            if self.clients.contains_key(&id) {
                fx.push(Effect::Send(id, focus_state(false)));
            }
            if let Some(next) = self.focus_holder() {
                fx.push(Effect::Send(next, focus_state(true)));
            }
            self.focus_changed(fx);
        }
    }

    /// The focus holder changed: a touch in progress is taken away from its owner.
    fn focus_changed(&mut self, fx: &mut Vec<Effect>) {
        if !self.touch.active
            || self.touch.owner.is_none()
            || self.touch.owner == self.focus_holder()
        {
            return;
        }
        let owner = self.touch.owner.take().expect("checked");
        if let Some(last) = self.touch.open_scroll.take()
            && self.clients.contains_key(&owner)
        {
            fx.push(Effect::Send(
                owner,
                frame(service_frame::Body::Scroll(Scroll {
                    delta: 0.0,
                    velocity: 0.0,
                    ended: true,
                    cancelled: true,
                    ..last
                })),
            ));
        }
    }

    /// The current screen state, if the screen source exists.
    pub fn screen(&self) -> Option<&ScreenState> {
        self.screen.as_ref()
    }

    /// Publishes an event from a source: touchpad frames go to the focus holder, everything
    /// else to the subscribers of its topic. `ScreenState`, `WearState` and `Settings` also
    /// update the snapshot state.
    pub fn publish(&mut self, f: ServiceFrame) -> Vec<Effect> {
        let mut fx = Vec::new();
        let Some(body) = f.body.as_ref() else {
            return fx;
        };
        let Some(topic) = topic_of(body) else {
            return fx;
        };
        match body {
            service_frame::Body::ScreenState(s) => self.screen = Some(s.clone()),
            service_frame::Body::WearState(w) => self.wear = Some(*w),
            service_frame::Body::Settings(s) => self.settings = Some(*s),
            _ => {}
        }
        if topic == Topic::Gesture {
            self.route_touch(f, &mut fx);
            return fx;
        }
        for (&id, c) in &self.clients {
            if c.hello && c.subscribed(topic) {
                fx.push(Effect::Send(id, f.clone()));
            }
        }
        fx
    }

    fn route_touch(&mut self, f: ServiceFrame, fx: &mut Vec<Effect>) {
        let body = f.body.as_ref().expect("checked by caller");
        let mut ends = false;
        if let service_frame::Body::FingerCount(fc) = body {
            if fc.previous == 0 && fc.current > 0 {
                self.touch = TouchRoute {
                    active: true,
                    owner: self.focus_holder(),
                    open_scroll: None,
                };
            }
            ends = fc.current == 0;
        }
        let target = if self.touch.active {
            self.touch.owner
        } else {
            self.focus_holder()
        };
        if let Some(id) = target
            && self
                .clients
                .get(&id)
                .is_some_and(|c| c.hello && c.subscribed(Topic::Gesture))
        {
            if let service_frame::Body::Scroll(s) = body
                && self.touch.active
            {
                self.touch.open_scroll = (!s.ended).then_some(*s);
            }
            fx.push(Effect::Send(id, f));
        }
        if ends {
            self.touch = TouchRoute::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{
        FingerCount, Gesture, GestureKind, Screen, ScreenRequest, SetSettings, Subscribe, Wear,
    };
    use client_frame::Body as C;
    use service_frame::Body as S;

    fn cf(body: C) -> ClientFrame {
        ClientFrame {
            seq: 0,
            body: Some(body),
        }
    }

    fn hello_frame(v: u32) -> ClientFrame {
        cf(C::Hello(Hello {
            api_version: v,
            client_name: "t".into(),
        }))
    }

    fn sub(topics: &[Topic]) -> ClientFrame {
        cf(C::Subscribe(Subscribe {
            topics: topics.iter().map(|&t| t as i32).collect(),
        }))
    }

    fn connect(hub: &mut Hub, topics: &[Topic]) -> ClientId {
        let id = hub.add_client();
        hub.on_frame(id, hello_frame(1));
        hub.on_frame(id, sub(topics));
        id
    }

    fn sends(fx: &[Effect]) -> Vec<(ClientId, S)> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::Send(id, f) => Some((*id, f.body.clone().unwrap())),
                _ => None,
            })
            .collect()
    }

    fn fc(previous: u32, current: u32) -> ServiceFrame {
        frame(S::FingerCount(FingerCount {
            previous,
            current,
            timestamp_us: 0,
        }))
    }

    fn scroll(d: f32, ended: bool) -> ServiceFrame {
        frame(S::Scroll(Scroll {
            displacement: d,
            delta: d,
            velocity: 1.0,
            fingers: 1,
            ended,
            cancelled: false,
            timestamp_us: 5,
        }))
    }

    fn tap() -> ServiceFrame {
        frame(S::Gesture(Gesture {
            kind: GestureKind::Tap as i32,
            fingers: 1,
            velocity: 0.0,
            timestamp_us: 0,
        }))
    }

    fn error_code(body: &S) -> Option<(ErrorCode, u32)> {
        match body {
            S::Error(e) => Some((ErrorCode::try_from(e.code).unwrap(), e.seq)),
            _ => None,
        }
    }

    #[test]
    fn hello_gets_welcome_with_min_version() {
        let mut hub = Hub::new("9.9", &["mock"]);
        let id = hub.add_client();
        let fx = hub.on_frame(id, hello_frame(7));
        let s = sends(&fx);
        let [(to, S::Welcome(w))] = &s[..] else {
            panic!("{fx:?}")
        };
        let to = *to;
        assert_eq!(to, id);
        assert_eq!(w.api_version, 1);
        assert_eq!(w.service_version, "9.9");
        assert_eq!(w.capabilities, vec!["mock".to_string()]);
        // A second Hello: Welcome again.
        let fx = hub.on_frame(id, hello_frame(1));
        assert!(matches!(sends(&fx)[..], [(_, S::Welcome(_))]));
    }

    #[test]
    fn frame_before_hello_is_bad_frame_and_closes() {
        let mut hub = Hub::new("1", &[]);
        let id = hub.add_client();
        let mut f = sub(&[Topic::Gesture]);
        f.seq = 42;
        let fx = hub.on_frame(id, f);
        assert_eq!(
            error_code(&sends(&fx)[0].1),
            Some((ErrorCode::BadFrame, 42))
        );
        assert_eq!(fx.last(), Some(&Effect::Close(id)));
        assert_eq!(hub.clients().count(), 0);
    }

    #[test]
    fn api_version_zero_is_bad_frame() {
        let mut hub = Hub::new("1", &[]);
        let id = hub.add_client();
        let fx = hub.on_frame(id, hello_frame(0));
        assert_eq!(error_code(&sends(&fx)[0].1).unwrap().0, ErrorCode::BadFrame);
        assert_eq!(fx.last(), Some(&Effect::Close(id)));
    }

    #[test]
    fn empty_frame_is_unsupported_with_seq() {
        let mut hub = Hub::new("1", &[]);
        let id = connect(&mut hub, &[]);
        let fx = hub.on_frame(id, ClientFrame { seq: 3, body: None });
        assert_eq!(
            error_code(&sends(&fx)[0].1),
            Some((ErrorCode::Unsupported, 3))
        );
        assert!(!fx.contains(&Effect::Close(id)));
    }

    #[test]
    fn subscribe_sends_snapshots_for_existing_sources_only() {
        let mut hub = Hub::new("1", &[]);
        hub.set_screen_source(ScreenState {
            screen: Screen::On as i32,
            reason: "touch".into(),
            timestamp_us: 77,
        });
        hub.set_settings_source(Settings::default());
        let id = hub.add_client();
        hub.on_frame(id, hello_frame(1));
        let fx = hub.on_frame(
            id,
            sub(&[
                Topic::Screen,
                Topic::Wear,
                Topic::Settings,
                Topic::Unspecified,
            ]),
        );
        let s = sends(&fx);
        assert_eq!(s.len(), 2, "{s:?}");
        let S::ScreenState(ss) = &s[0].1 else {
            panic!()
        };
        assert_eq!(ss.reason, "snapshot");
        assert_eq!(ss.timestamp_us, 77);
        let S::Settings(st) = &s[1].1 else { panic!() };
        assert!(st.snapshot);
        // Wear exists now: a new Subscribe sends it too, and replaces the set.
        hub.set_wear_source(WearState {
            wear: Wear::OnHead as i32,
            snapshot: false,
            timestamp_us: 9,
        });
        let s = sends(&hub.on_frame(id, sub(&[Topic::Wear, Topic::Wear, Topic::Gesture])));
        assert_eq!(s.len(), 1);
        let S::WearState(w) = &s[0].1 else { panic!() };
        assert!(w.snapshot);
        // Screen no longer subscribed: changes are not delivered.
        let fx = hub.publish(frame(S::ScreenState(ScreenState::default())));
        assert!(sends(&fx).is_empty());
    }

    #[test]
    fn get_state_sends_snapshots_and_focus() {
        let mut hub = Hub::new("1", &[]);
        hub.set_screen_source(ScreenState::default());
        let id = connect(&mut hub, &[Topic::Screen]);
        let s = sends(&hub.on_frame(id, cf(C::GetState(Default::default()))));
        assert!(matches!(s[0].1, S::ScreenState(_)));
        assert_eq!(s[1].1, S::FocusState(FocusState { focused: false }));
        hub.on_frame(id, cf(C::AcquireFocus(Default::default())));
        let s = sends(&hub.on_frame(id, cf(C::GetState(Default::default()))));
        assert_eq!(s[1].1, S::FocusState(FocusState { focused: true }));
    }

    #[test]
    fn focus_stack_acquire_release_and_disconnect() {
        let mut hub = Hub::new("1", &[]);
        let a = connect(&mut hub, &[Topic::Gesture]);
        let b = connect(&mut hub, &[Topic::Gesture]);
        let c = connect(&mut hub, &[Topic::Gesture]);
        let acq = || cf(C::AcquireFocus(Default::default()));
        let f = |v| S::FocusState(FocusState { focused: v });

        assert_eq!(sends(&hub.on_frame(a, acq())), vec![(a, f(true))]);
        assert_eq!(
            sends(&hub.on_frame(b, acq())),
            vec![(a, f(false)), (b, f(true))]
        );
        assert_eq!(
            sends(&hub.on_frame(c, acq())),
            vec![(b, f(false)), (c, f(true))]
        );
        assert_eq!(hub.focus_stack(), &[a, b, c]);
        // a moves to the top.
        assert_eq!(
            sends(&hub.on_frame(a, acq())),
            vec![(c, f(false)), (a, f(true))]
        );
        assert_eq!(hub.focus_stack(), &[b, c, a]);
        // Removing c from the middle tells nobody.
        assert!(sends(&hub.on_frame(c, cf(C::ReleaseFocus(Default::default())))).is_empty());
        assert_eq!(hub.focus_stack(), &[b, a]);
        // Disconnect of the top: b gets the focus.
        assert_eq!(sends(&hub.remove_client(a)), vec![(b, f(true))]);
        // Release by the top: told false, nobody left.
        assert_eq!(
            sends(&hub.on_frame(b, cf(C::ReleaseFocus(Default::default())))),
            vec![(b, f(false))]
        );
        assert_eq!(hub.focus_holder(), None);
    }

    #[test]
    fn touch_events_go_to_focus_holder_only() {
        let mut hub = Hub::new("1", &[]);
        let a = connect(&mut hub, &[Topic::Gesture]);
        let b = connect(&mut hub, &[Topic::Gesture]);
        // Nobody holds the focus: dropped.
        assert!(hub.publish(tap()).is_empty());
        hub.on_frame(a, cf(C::AcquireFocus(Default::default())));
        let fx = hub.publish(tap());
        assert_eq!(sends(&fx).iter().map(|s| s.0).collect::<Vec<_>>(), vec![a]);
        let _ = b;
    }

    #[test]
    fn unsubscribed_focus_holder_gets_nothing_and_events_are_not_passed_down() {
        let mut hub = Hub::new("1", &[]);
        let a = connect(&mut hub, &[Topic::Gesture]);
        let b = connect(&mut hub, &[Topic::Screen]);
        hub.on_frame(a, cf(C::AcquireFocus(Default::default())));
        hub.on_frame(b, cf(C::AcquireFocus(Default::default())));
        assert!(hub.publish(tap()).is_empty());
    }

    #[test]
    fn focus_change_mid_touch_cancels_scroll_and_withholds_rest_of_touch() {
        let mut hub = Hub::new("1", &[]);
        let a = connect(&mut hub, &[Topic::Gesture]);
        let b = connect(&mut hub, &[Topic::Gesture]);
        hub.on_frame(a, cf(C::AcquireFocus(Default::default())));
        assert_eq!(sends(&hub.publish(fc(0, 1)))[0].0, a);
        assert_eq!(sends(&hub.publish(scroll(0.2, false)))[0].0, a);
        let s = sends(&hub.on_frame(b, cf(C::AcquireFocus(Default::default()))));
        let cancel = s
            .iter()
            .find_map(|(id, body)| match body {
                S::Scroll(sc) if *id == a => Some(*sc),
                _ => None,
            })
            .expect("cancel scroll to a");
        assert!(cancel.ended && cancel.cancelled);
        assert_eq!(cancel.displacement, 0.2);
        // Rest of this touch goes to nobody.
        assert!(hub.publish(scroll(0.3, true)).is_empty());
        assert!(hub.publish(tap()).is_empty());
        assert!(hub.publish(fc(1, 0)).is_empty());
        // Next touch goes to b.
        assert_eq!(sends(&hub.publish(fc(0, 1)))[0].0, b);
        assert_eq!(sends(&hub.publish(tap()))[0].0, b);
    }

    #[test]
    fn focus_change_mid_touch_without_scroll_sends_no_cancel() {
        let mut hub = Hub::new("1", &[]);
        let a = connect(&mut hub, &[Topic::Gesture]);
        let b = connect(&mut hub, &[Topic::Gesture]);
        hub.on_frame(a, cf(C::AcquireFocus(Default::default())));
        hub.publish(fc(0, 1));
        let s = sends(&hub.on_frame(b, cf(C::AcquireFocus(Default::default()))));
        assert!(s.iter().all(|(_, b)| matches!(b, S::FocusState(_))));
        assert!(hub.publish(tap()).is_empty());
    }

    #[test]
    fn ended_scroll_closes_the_scroll_so_no_cancel_follows() {
        let mut hub = Hub::new("1", &[]);
        let a = connect(&mut hub, &[Topic::Gesture]);
        let b = connect(&mut hub, &[Topic::Gesture]);
        hub.on_frame(a, cf(C::AcquireFocus(Default::default())));
        hub.publish(fc(0, 1));
        hub.publish(scroll(0.2, false));
        hub.publish(scroll(0.3, true));
        let s = sends(&hub.on_frame(b, cf(C::AcquireFocus(Default::default()))));
        assert!(s.iter().all(|(_, b)| matches!(b, S::FocusState(_))));
    }

    #[test]
    fn broadcast_topics_go_to_all_subscribers() {
        let mut hub = Hub::new("1", &[]);
        let a = connect(&mut hub, &[Topic::Screen]);
        let b = connect(&mut hub, &[Topic::Screen, Topic::Gesture]);
        let _c = connect(&mut hub, &[Topic::Gesture]);
        let fx = hub.publish(frame(S::ScreenState(ScreenState {
            screen: Screen::Off as i32,
            reason: "timeout".into(),
            timestamp_us: 1,
        })));
        assert_eq!(
            sends(&fx).iter().map(|s| s.0).collect::<Vec<_>>(),
            vec![a, b]
        );
        assert_eq!(hub.screen().unwrap().screen, Screen::Off as i32);
    }

    #[test]
    fn set_settings_merges_clamps_and_replies_once() {
        let mut hub = Hub::new("1", &[]);
        hub.set_settings_source(Settings {
            head_wake_enabled: Some(true),
            head_wake_angle_deg: Some(30.0),
            wink_enabled: Some(false),
            on_head_detection_enabled: Some(true),
            screen_timeout_ms: Some(10_000),
            snapshot: false,
        });
        let a = connect(&mut hub, &[Topic::Settings]);
        let b = connect(&mut hub, &[Topic::Settings]);
        let fx = hub.on_frame(
            a,
            cf(C::SetSettings(SetSettings {
                settings: Some(Settings {
                    head_wake_angle_deg: Some(90.0),
                    screen_timeout_ms: Some(1),
                    ..Default::default()
                }),
            })),
        );
        let s = sends(&fx);
        assert_eq!(s.iter().map(|x| x.0).collect::<Vec<_>>(), vec![a, b]);
        let S::Settings(applied) = &s[0].1 else {
            panic!()
        };
        assert_eq!(applied.head_wake_angle_deg, Some(60.0));
        assert_eq!(applied.screen_timeout_ms, Some(3000));
        assert_eq!(applied.head_wake_enabled, Some(true));
        assert!(!applied.snapshot);
        assert!(fx.iter().any(|e| matches!(e, Effect::SettingsChanged(_))));
    }

    #[test]
    fn screen_request_validation() {
        let mut hub = Hub::new("1", &[]);
        let a = connect(&mut hub, &[]);
        let req = |action| {
            cf(C::Screen(ScreenRequest {
                action,
                duration_ms: 5,
            }))
        };
        let fx = hub.on_frame(a, req(0));
        assert_eq!(
            error_code(&sends(&fx)[0].1).unwrap().0,
            ErrorCode::InvalidArgument
        );
        let fx = hub.on_frame(a, req(99));
        assert_eq!(
            error_code(&sends(&fx)[0].1).unwrap().0,
            ErrorCode::InvalidArgument
        );
        let fx = hub.on_frame(a, req(screen_request::Action::KeepOn as i32));
        assert_eq!(
            fx,
            vec![Effect::Screen {
                client: a,
                action: screen_request::Action::KeepOn,
                duration_ms: 5
            }]
        );
    }

    #[test]
    fn dictation_without_companion() {
        use crate::proto::DictationStart;
        let mut hub = Hub::new("1", &[]);
        let a = connect(&mut hub, &[]);
        let start = |purpose: &str, key: &str| {
            cf(C::DictationStart(DictationStart {
                request_id: 1,
                purpose: purpose.into(),
                notification_key: key.into(),
                action_index: 0,
            }))
        };
        let code = |hub: &mut Hub, f| error_code(&sends(&hub.on_frame(a, f))[0].1).unwrap().0;
        assert_eq!(code(&mut hub, start("free", "")), ErrorCode::NoCompanion);
        assert_eq!(
            code(&mut hub, start("notification-reply", "")),
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            code(&mut hub, start("notification-reply", "k")),
            ErrorCode::NoCompanion
        );
        assert_eq!(
            code(&mut hub, start("poem", "")),
            ErrorCode::InvalidArgument
        );
    }
}
