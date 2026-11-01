//! End-to-end backbone tests over an in-memory transport, mirroring the companion core's
//! `omg-core/tests/backbone.rs` so both sides are proven against the same contract:
//! pair -> reject unknown -> reconnect reuse bond -> ping -> status/battery -> locale ->
//! notification.
//!
//! The Glass side is the real [`Link`] state machine. The companion side is an independent
//! reimplementation of `omg-core`'s companion role (`session.rs`), so a bug in one side cannot
//! hide behind a matching bug in the other. Frames are pumped directly (the byte framing is
//! covered by [`super::wire`]).

use omg_protocol::{
    AuthChallenge, AuthResponse, AuthResult, BatteryStatus, ErrorCode, ErrorMsg, Frame,
    GlassStatus, Hello, Locale, Notification, NotificationAction, NotificationActionInfo,
    NotificationChoice, NotificationPosted, PROTOCOL_VERSION, PairRequest, PairResult, Ping, Role,
    SetLocale, StatusRequest, capability,
};

use super::identity::{Identity, decode_nonce, random_nonce_b64, verify_challenge};
use super::link::{DeviceState, Link, Out};
use super::store::{PairingRecord, record};

fn glass_device() -> DeviceState {
    DeviceState {
        device_name: "OpenMyGlass-Sim".into(),
        battery_percent: Some(73),
        charging: Some(false),
        ..DeviceState::default()
    }
}

enum CPhase {
    Start,
    AwaitingHello,
    AwaitingPairAccept(String),
    Auth,
    Running,
    Done,
}

/// An independent companion-role peer (the `session.rs`/`drive_companion` logic).
struct TestCompanion {
    id: Identity,
    name: String,
    bonded: Option<PairingRecord>,
    glass_pub: Option<String>,
    phase: CPhase,
    allow_new: bool,
    our_nonce: Vec<u8>,
    answered: bool,
    got_resp: bool,
    verify_ok: bool,
    sent_result: bool,
    peer_result: Option<bool>,
    authenticated: bool,
    statuses: Vec<GlassStatus>,
    batteries: Vec<BatteryStatus>,
    locales: Vec<Locale>,
    actions: Vec<NotificationAction>,
    error: Option<ErrorMsg>,
}

impl TestCompanion {
    fn new(id: Identity, bonded: Option<PairingRecord>, allow_new: bool) -> TestCompanion {
        TestCompanion {
            id,
            name: "giorgio-pc".into(),
            bonded,
            glass_pub: None,
            phase: CPhase::Start,
            allow_new,
            our_nonce: Vec::new(),
            answered: false,
            got_resp: false,
            verify_ok: false,
            sent_result: false,
            peer_result: None,
            authenticated: false,
            statuses: Vec::new(),
            batteries: Vec::new(),
            locales: Vec::new(),
            actions: Vec::new(),
            error: None,
        }
    }

    fn start(&mut self) -> Vec<Frame> {
        self.phase = CPhase::AwaitingHello;
        vec![Frame::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            role: Role::Companion,
            app_version: "omg-test/0.1".into(),
            device_name: self.name.clone(),
            identity_pub: self.id.public_b64(),
            capabilities: capability::BACKBONE.iter().map(|s| s.to_string()).collect(),
        })]
    }

    fn enter_auth(&mut self) -> Vec<Frame> {
        let b64 = random_nonce_b64(32);
        self.our_nonce = decode_nonce(&b64).unwrap();
        self.phase = CPhase::Auth;
        vec![Frame::AuthChallenge(AuthChallenge { nonce: b64 })]
    }

    fn maybe_finish(&mut self) -> Vec<Frame> {
        if !(self.answered && self.got_resp) || self.sent_result {
            return Vec::new();
        }
        self.sent_result = true;
        let mut out = vec![Frame::AuthResult(AuthResult {
            ok: self.verify_ok,
            message: String::new(),
        })];
        if self.verify_ok && self.peer_result == Some(true) {
            self.authenticated = true;
            self.phase = CPhase::Running;
        }
        out.extend(self.finalize());
        out
    }

    fn finalize(&mut self) -> Vec<Frame> {
        if self.sent_result && self.verify_ok && self.peer_result == Some(true) {
            self.authenticated = true;
            self.phase = CPhase::Running;
        }
        Vec::new()
    }

    fn on_frame(&mut self, f: Frame) -> Vec<Frame> {
        match (&self.phase, f) {
            (CPhase::AwaitingHello, Frame::Hello(h)) => {
                assert_eq!(h.protocol_version, PROTOCOL_VERSION);
                self.glass_pub = Some(h.identity_pub.clone());
                if let Some(b) = &self.bonded
                    && b.peer_pub == h.identity_pub
                {
                    return self.enter_auth();
                }
                assert!(self.allow_new, "companion would have nothing to do");
                let nonce = random_nonce_b64(16);
                self.phase = CPhase::AwaitingPairAccept(nonce.clone());
                vec![Frame::PairRequest(PairRequest {
                    companion_name: self.name.clone(),
                    companion_pub: self.id.public_b64(),
                    nonce,
                    bootstrap_token: None,
                })]
            }
            (CPhase::AwaitingPairAccept(nonce), Frame::PairAccept(a)) => {
                assert_eq!(&a.nonce, nonce);
                assert_eq!(Some(&a.glass_pub), self.glass_pub.as_ref());
                self.bonded = Some(record(a.glass_pub, a.glass_name));
                let mut out = vec![Frame::PairResult(PairResult {
                    ok: true,
                    message: "bonded".into(),
                })];
                out.extend(self.enter_auth());
                out
            }
            (CPhase::Auth, Frame::AuthChallenge(ch)) => {
                let nonce = decode_nonce(&ch.nonce).unwrap();
                let sig = self.id.sign_challenge(&nonce);
                self.answered = true;
                let mut out = vec![Frame::AuthResponse(AuthResponse { signature: sig })];
                out.extend(self.maybe_finish());
                out
            }
            (CPhase::Auth, Frame::AuthResponse(resp)) => {
                let glass_pub = self.bonded.as_ref().unwrap().peer_pub.clone();
                self.verify_ok = verify_challenge(&glass_pub, &self.our_nonce, &resp.signature);
                self.got_resp = true;
                self.maybe_finish()
            }
            (CPhase::Auth, Frame::AuthResult(r)) => {
                self.peer_result = Some(r.ok);
                self.finalize()
            }
            (_, Frame::Error(e)) => {
                self.error = Some(e);
                self.phase = CPhase::Done;
                Vec::new()
            }
            (CPhase::Running, Frame::Status(s)) => {
                self.statuses.push(s);
                Vec::new()
            }
            (CPhase::Running, Frame::BatteryStatus(b)) => {
                self.batteries.push(b);
                Vec::new()
            }
            (CPhase::Running, Frame::LocaleChanged(l)) => {
                self.locales.push(l.locale);
                Vec::new()
            }
            (CPhase::Running, Frame::NotificationAction(a)) => {
                self.actions.push(a);
                Vec::new()
            }
            (CPhase::Running, Frame::Pong(_)) => Vec::new(),
            // Ignore anything else (unused scaffold frames).
            _ => Vec::new(),
        }
    }
}

struct Harness {
    glass: Link,
    comp: TestCompanion,
    events: Vec<Out>,
}

impl Harness {
    fn glass_frames(&mut self, outs: Vec<Out>) -> Vec<Frame> {
        let mut frames = Vec::new();
        for o in outs {
            match o {
                Out::Send(f) => frames.push(f),
                other => self.events.push(other),
            }
        }
        frames
    }

    /// Delivers queued frames both ways until the link is quiescent.
    fn pump(&mut self, to_glass: Vec<Frame>, to_comp: Vec<Frame>) {
        let mut gq = to_glass;
        let mut cq = to_comp;
        let mut guard = 0;
        while !gq.is_empty() || !cq.is_empty() {
            guard += 1;
            assert!(guard < 1000, "pump did not settle");
            let mut next_c = Vec::new();
            let mut next_g = Vec::new();
            for f in std::mem::take(&mut gq) {
                let outs = self.glass.on_frame(f);
                next_c.extend(self.glass_frames(outs));
            }
            for f in std::mem::take(&mut cq) {
                next_g.extend(self.comp.on_frame(f));
            }
            gq = next_g;
            cq = next_c;
        }
    }

    fn connected_event(&self) -> Option<(&str, &[String])> {
        self.events.iter().find_map(|e| match e {
            Out::Connected { name, capabilities } => Some((name.as_str(), capabilities.as_slice())),
            _ => None,
        })
    }
}

fn handshake(glass: Link, comp: TestCompanion) -> Harness {
    let mut h = Harness {
        glass,
        comp,
        events: Vec::new(),
    };
    let g = h.glass.start();
    let to_comp = h.glass_frames(g);
    let to_glass = h.comp.start();
    h.pump(to_glass, to_comp);
    h
}

#[test]
fn full_backbone_pairs_then_serves_the_backbone() {
    let glass = Link::new(Identity::generate(), None, glass_device());
    let comp = TestCompanion::new(Identity::generate(), None, true);
    let mut h = handshake(glass, comp);

    // Handshake done: the glass is running, bonded, and pushed an initial battery status.
    assert!(h.glass.is_running());
    assert!(h.glass.bonded().is_some());
    assert!(h.comp.authenticated);
    let (name, caps) = h.connected_event().expect("Connected event");
    assert_eq!(name, "giorgio-pc");
    assert!(caps.contains(&capability::NOTIFICATIONS.to_string()));
    assert!(h.events.iter().any(|e| matches!(e, Out::Persist(_))));
    assert_eq!(h.comp.batteries.first().map(|b| b.percent), Some(73));

    // Heartbeat: Ping -> Pong.
    let outs = h.glass.on_frame(Frame::Ping(Ping {
        token: 99,
        unix_ms: 1,
    }));
    let frames = h.glass_frames(outs);
    assert!(matches!(frames.as_slice(), [Frame::Pong(p)] if p.token == 99));

    // Status round-trip.
    let outs = h
        .glass
        .on_frame(Frame::StatusRequest(StatusRequest::default()));
    let f = h.glass_frames(outs);
    for fr in f {
        h.comp.on_frame(fr);
    }
    let st = h.comp.statuses.last().expect("Status");
    assert_eq!(st.battery_percent, Some(73));
    assert_eq!(st.screen_width_px, Some(640));

    // Locale push: SetLocale -> ApplyLocale + LocaleChanged.
    let outs = h
        .glass
        .on_frame(Frame::SetLocale(SetLocale { locale: Locale::Fr }));
    let f = h.glass_frames(outs);
    for fr in f {
        h.comp.on_frame(fr);
    }
    assert_eq!(h.comp.locales.last(), Some(&Locale::Fr));
    assert!(
        h.events
            .iter()
            .any(|e| matches!(e, Out::ApplyLocale(Locale::Fr)))
    );

    // Notification posted -> surfaced; the owner's action is relayed back.
    let outs = h
        .glass
        .on_frame(Frame::NotificationPosted(NotificationPosted {
            notification: Notification {
                key: "msg-1".into(),
                app_name: "Signal".into(),
                title: "Giorgio".into(),
                text: "ci vediamo alle 18".into(),
                posted_at_ms: 1,
                icon_sha256: None,
                actions: vec![NotificationActionInfo {
                    index: 0,
                    title: "Reply".into(),
                    reply: true,
                }],
                wake: true,
            },
        }));
    let _ = h.glass_frames(outs);
    let surfaced = h
        .events
        .iter()
        .any(|e| matches!(e, Out::Notification(n) if n.key == "msg-1"));
    assert!(surfaced, "notification surfaced to the launcher");
    let outs = h
        .glass
        .notification_action("msg-1".into(), NotificationChoice::Dismiss);
    let f = h.glass_frames(outs);
    for fr in f {
        h.comp.on_frame(fr);
    }
    assert_eq!(h.comp.actions.last().map(|a| a.key.as_str()), Some("msg-1"));
}

#[test]
fn rejects_an_unknown_peer() {
    // The glass is bonded to companion A; companion B (a different key) connects.
    let companion_a = Identity::generate();
    let bond = record(companion_a.public_b64(), "companion-A".into());
    let glass = Link::new(Identity::generate(), Some(bond), glass_device());
    let comp_b = TestCompanion::new(Identity::generate(), None, true);
    let h = handshake(glass, comp_b);

    assert!(h.glass.is_closed());
    assert!(!h.glass.is_running());
    let e = h.comp.error.as_ref().expect("the companion saw an Error");
    assert_eq!(e.code, ErrorCode::UnknownPeer);
}

#[test]
fn reconnect_reuses_the_bond_and_authenticates() {
    // Both sides already bonded to each other: no pairing, straight to auth.
    let glass_id = Identity::generate();
    let companion_id = Identity::generate();
    let glass_bond = record(companion_id.public_b64(), "giorgio-pc".into());
    let companion_bond = record(glass_id.public_b64(), "OpenMyGlass-Sim".into());

    let glass = Link::new(glass_id, Some(glass_bond), glass_device());
    // allow_new = false: the companion must reuse the stored key, not pair anew.
    let comp = TestCompanion::new(companion_id, Some(companion_bond), false);
    let mut h = handshake(glass, comp);

    assert!(h.glass.is_running());
    assert!(h.comp.authenticated);
    // No new bond was persisted (already bonded).
    assert!(!h.events.iter().any(|e| matches!(e, Out::Persist(_))));
    assert_eq!(h.glass.peer_name(), Some("giorgio-pc"));

    // A ping still works after a bare reconnect.
    let outs = h.glass.on_frame(Frame::Ping(Ping {
        token: 7,
        unix_ms: 0,
    }));
    let frames = h.glass_frames(outs);
    assert!(matches!(frames.as_slice(), [Frame::Pong(p)] if p.token == 7));
}
