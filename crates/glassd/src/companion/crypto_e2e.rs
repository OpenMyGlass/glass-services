//! The real ends of the link, `omg-core`'s [`CompanionLink`] on the phone and glassd's [`Link`],
//! talking through the bytes each really produces, with a relay in the middle that sees those
//! bytes and may change them. On a Glass a forged `WifiTearDown` used to be accepted, and a forged
//! `Unpair` used to forget the bond: `plainproof` showed the first in the old link.

use std::collections::VecDeque;

use omg_core::{CompanionLink, Identity as CoreIdentity, LinkEnd, LinkOut, PairingMode};
use omg_protocol::{
    ErrorCode, Frame, FrameDecoder, Hello, PROTOCOL_VERSION, Role, SessionKeyShare,
    WifiBringUpResult, capability, frame_bytes,
};

use super::identity::Identity;
use super::link::{DeviceState, Link, Out};
use super::store::record;

const MARKER: &[u8] = b"MARKER-AFTER-AUTH-hunter2";

fn marker_frame() -> Frame {
    Frame::WifiBringUpResult(WifiBringUpResult {
        ok: true,
        ssid: Some(String::from_utf8_lossy(MARKER).into_owned()),
        ip: None,
        message: String::new(),
    })
}

/// The two ends and what is in flight between them, as the relay holds it.
struct Wire {
    phone: CompanionLink,
    glass: Link,
    to_glass: VecDeque<Frame>,
    to_phone: VecDeque<Frame>,
    /// Every byte either side sent.
    seen: Vec<u8>,
    glass_did: Vec<Out>,
    phone_did: Vec<LinkOut>,
    decoder: FrameDecoder,
}

impl Wire {
    fn phone_carry(&mut self, outs: Vec<LinkOut>) {
        for out in outs {
            match out {
                LinkOut::Send(frame) => {
                    let frame = self.on_wire_from_phone(&frame);
                    self.to_glass.push_back(frame);
                }
                other => self.phone_did.push(other),
            }
        }
    }

    fn glass_carry(&mut self, outs: Vec<Out>) {
        for out in outs {
            match out {
                Out::Send(frame) => {
                    let bytes = self.glass.encode(&frame).unwrap();
                    let frame = self.observe(&bytes);
                    self.to_phone.push_back(frame);
                }
                other => self.glass_did.push(other),
            }
        }
    }

    fn observe(&mut self, bytes: &[u8]) -> Frame {
        self.seen.extend_from_slice(bytes);
        self.decoder.push(bytes);
        self.decoder.next_frame().unwrap().unwrap()
    }

    fn on_wire_from_phone(&mut self, frame: &Frame) -> Frame {
        let bytes = self.phone.encode(frame).unwrap();
        self.observe(&bytes)
    }

    /// The next frame in flight to the Glass reaches it.
    fn deliver_to_glass(&mut self) -> bool {
        let Some(frame) = self.to_glass.pop_front() else {
            return false;
        };
        let outs = self.glass.on_frame(frame);
        self.glass_carry(outs);
        true
    }

    /// The next frame in flight to the phone reaches it.
    fn deliver_to_phone(&mut self) -> bool {
        let Some(frame) = self.to_phone.pop_front() else {
            return false;
        };
        let outs = self.phone.on_frame(frame);
        self.phone_carry(outs);
        true
    }

    /// Everything in flight goes through, with `tamper` on each frame.
    fn settle(&mut self, mut tamper: impl FnMut(Frame) -> Frame) {
        while !(self.to_glass.is_empty() && self.to_phone.is_empty()) {
            for frame in self.to_glass.iter_mut() {
                *frame = tamper(frame.clone());
            }
            for frame in self.to_phone.iter_mut() {
                *frame = tamper(frame.clone());
            }
            self.deliver_to_glass();
            self.deliver_to_phone();
        }
    }

    fn phone_sends(&mut self, frame: &Frame) {
        let frame = self.on_wire_from_phone(frame);
        self.to_glass.push_back(frame);
    }

    fn glass_closed_for(&self) -> Option<&str> {
        self.glass_did.iter().find_map(|o| match o {
            Out::Close(reason) => Some(reason.as_str()),
            _ => None,
        })
    }

    fn phone_ended(&self) -> Option<&LinkEnd> {
        self.phone_did.iter().find_map(|o| match o {
            LinkOut::Closed(Err(end)) => Some(end),
            _ => None,
        })
    }
}

fn wire_between(phone_id: CoreIdentity, glass_id: Identity, bonded: bool) -> Wire {
    let (phone_bond, glass_bond) = if bonded {
        let as_core = omg_core::PairingRecord {
            peer_pub: glass_id.public_b64(),
            peer_name: "glass".into(),
            bonded_at_ms: 0,
        };
        (
            Some(as_core),
            Some(record(phone_id.public_b64(), "phone".into())),
        )
    } else {
        (None, None)
    };
    let mode = if bonded {
        PairingMode::ReuseOnly
    } else {
        PairingMode::AllowNew
    };
    let mut wire = Wire {
        phone: CompanionLink::new(phone_id, phone_bond, mode, "phone", "test", Vec::new()),
        glass: Link::new(glass_id, glass_bond, DeviceState::default()),
        to_glass: VecDeque::new(),
        to_phone: VecDeque::new(),
        seen: Vec::new(),
        glass_did: Vec::new(),
        phone_did: Vec::new(),
        decoder: FrameDecoder::new(),
    };
    let outs = wire.phone.start();
    wire.phone_carry(outs);
    let outs = wire.glass.start();
    wire.glass_carry(outs);
    wire
}

/// A running link between two bonded peers.
fn running() -> Wire {
    let mut wire = wire_between(CoreIdentity::generate(), Identity::generate(), true);
    wire.settle(|f| f);
    assert!(wire.phone.is_running() && wire.glass.is_running());
    wire
}

#[test]
fn the_two_ends_pair_then_reconnect_and_carry_frames_both_ways() {
    let (phone_id, glass_id) = (CoreIdentity::generate(), Identity::generate());
    let mut wire = wire_between(phone_id.clone(), glass_id.clone(), false);
    wire.settle(|f| f);
    assert!(wire.phone.is_running() && wire.glass.is_running());
    assert!(wire.glass_did.iter().any(|o| matches!(o, Out::Persist(_))));

    let mut wire = wire_between(phone_id, glass_id, true);
    wire.settle(|f| f);
    assert!(wire.phone.is_running() && wire.glass.is_running());

    // Phone to Glass.
    wire.phone_sends(&Frame::WifiTearDown);
    wire.settle(|f| f);
    assert!(wire.glass_did.contains(&Out::WifiTearDown));
    // And back: the status answer is one frame the Glass sealed and the phone opened.
    wire.phone_sends(&Frame::StatusRequest(Default::default()));
    wire.settle(|f| f);
    assert!(
        wire.phone_did
            .iter()
            .any(|o| matches!(o, LinkOut::App(Frame::Status(_))))
    );
    // A ping is answered inside the encrypted link, both ways.
    wire.phone_sends(&Frame::Ping(omg_protocol::Ping {
        token: 3,
        unix_ms: 0,
    }));
    wire.settle(|f| f);
    assert!(
        wire.phone_did
            .iter()
            .any(|o| matches!(o, LinkOut::App(Frame::Pong(p)) if p.token == 3))
    );
    assert!(wire.phone.is_running() && wire.glass.is_running());
}

#[test]
fn a_relay_reads_no_marker_after_the_handshake() {
    let mut wire = running();
    assert!(
        wire.seen.windows(5).any(|w| w == b"phone"),
        "the relay does see the handshake"
    );
    let handshake_end = wire.seen.len();
    wire.phone_sends(&marker_frame());
    let outs = wire.glass.battery_changed(50, true);
    wire.glass_carry(outs);
    let outs = wire.glass.send_settings(omg_protocol::Settings {
        radios: Some(omg_protocol::RadioState {
            wifi_on: true,
            bt_on: false,
            airplane_mode: false,
            wifi_ssid: Some(String::from_utf8_lossy(MARKER).into_owned()),
            wifi_ip: None,
        }),
        ..Default::default()
    });
    wire.glass_carry(outs);
    assert!(wire.seen.len() > handshake_end);
    assert!(!wire.seen.windows(MARKER.len()).any(|w| w == MARKER));
    wire.settle(|f| f);
    assert!(
        wire.phone_did
            .iter()
            .any(|o| matches!(o, LinkOut::App(Frame::Settings(_))))
    );
}

#[test]
fn a_forged_frame_in_clear_is_refused_by_the_glass() {
    for forged in [Frame::WifiTearDown, Frame::Unpair] {
        let mut wire = running();
        wire.to_glass.push_back(forged);
        wire.deliver_to_glass();
        assert!(wire.glass.is_closed());
        assert!(!wire.glass_did.contains(&Out::WifiTearDown));
        assert!(!wire.glass_did.contains(&Out::Unpair));
        assert!(wire.glass.bonded().is_some());
    }
}

#[test]
fn a_forged_frame_in_clear_is_refused_by_the_phone() {
    let mut wire = running();
    wire.to_phone
        .push_back(Frame::BatteryStatus(omg_protocol::BatteryStatus {
            percent: 1,
            charging: false,
        }));
    wire.deliver_to_phone();
    assert_eq!(wire.phone_ended(), Some(&LinkEnd::Tampered));
    assert!(!wire.phone_did.iter().any(|o| matches!(o, LinkOut::App(_))));
}

#[test]
fn a_replayed_frame_is_refused_by_the_glass() {
    let mut wire = running();
    wire.phone_sends(&Frame::WifiTearDown);
    let copy = wire.to_glass[0].clone();
    wire.deliver_to_glass();
    assert!(wire.glass_did.contains(&Out::WifiTearDown));
    wire.glass_did.clear();
    wire.to_glass.push_back(copy);
    wire.deliver_to_glass();
    assert!(!wire.glass_did.contains(&Out::WifiTearDown));
    assert!(wire.glass_closed_for().is_some());
}

#[test]
fn a_replayed_frame_is_refused_by_the_phone() {
    let mut wire = running();
    let outs = wire.glass.battery_changed(50, true);
    wire.glass_carry(outs);
    let copy = wire.to_phone[0].clone();
    wire.deliver_to_phone();
    wire.to_phone.push_back(copy);
    wire.deliver_to_phone();
    assert_eq!(wire.phone_ended(), Some(&LinkEnd::Tampered));
}

#[test]
fn reordered_and_dropped_frames_are_refused() {
    let mut wire = running();
    wire.phone_sends(&Frame::WifiTearDown);
    wire.phone_sends(&Frame::SettingsGet);
    wire.to_glass.swap(0, 1);
    wire.deliver_to_glass();
    assert!(wire.glass.is_closed());
    assert!(!wire.glass_did.contains(&Out::SettingsGet));

    let mut wire = running();
    wire.phone_sends(&Frame::WifiTearDown);
    wire.phone_sends(&Frame::SettingsGet);
    wire.to_glass.pop_front();
    wire.deliver_to_glass();
    assert!(wire.glass.is_closed());
    assert!(!wire.glass_did.contains(&Out::SettingsGet));
}

#[test]
fn a_changed_ciphertext_is_refused_and_appended_bytes_do_not_decode() {
    let mut wire = running();
    wire.phone_sends(&Frame::WifiTearDown);
    let Frame::Sealed(mut sealed) = wire.to_glass.pop_front().unwrap() else {
        panic!("not sealed");
    };
    sealed.ciphertext[0] ^= 1;
    wire.to_glass.push_back(Frame::Sealed(sealed.clone()));
    wire.deliver_to_glass();
    assert!(!wire.glass_did.contains(&Out::WifiTearDown));
    assert!(wire.glass.is_closed());

    let mut bytes = frame_bytes(&Frame::Sealed(sealed)).unwrap();
    bytes.push(0);
    let len = (bytes.len() - 4) as u32;
    bytes[..4].copy_from_slice(&len.to_be_bytes());
    let mut decoder = super::wire::FrameDecoder::new();
    decoder.push(&bytes);
    assert!(decoder.next_frame().is_err());
}

#[test]
fn frames_from_another_session_are_refused() {
    let (phone_id, glass_id) = (CoreIdentity::generate(), Identity::generate());
    let mut old = wire_between(phone_id.clone(), glass_id.clone(), true);
    old.settle(|f| f);
    old.phone_sends(&Frame::WifiTearDown);
    let captured = old.to_glass.pop_front().unwrap();

    let mut wire = wire_between(phone_id, glass_id, true);
    wire.settle(|f| f);
    wire.to_glass.push_back(captured);
    wire.deliver_to_glass();
    assert!(!wire.glass_did.contains(&Out::WifiTearDown));
    assert!(wire.glass.is_closed());
}

#[test]
fn a_bonded_pair_resists_a_relay_replacing_the_key_shares() {
    let mut wire = wire_between(CoreIdentity::generate(), Identity::generate(), true);
    wire.settle(|f| match f {
        Frame::SessionKeyShare(_) => Frame::SessionKeyShare(SessionKeyShare {
            ephemeral_pub: vec![9; 32],
        }),
        other => other,
    });
    assert!(!wire.phone.is_running());
    assert!(!wire.glass.is_running());
    assert_eq!(wire.phone_ended(), Some(&LinkEnd::AuthFailed));
    assert!(wire.glass.is_closed());
}

#[test]
fn another_key_than_the_bonded_one_is_refused() {
    // The Glass is bonded to one phone and another connects, encryption and all.
    let phone = CoreIdentity::generate();
    let glass_id = Identity::generate();
    let glass = Link::new(
        glass_id.clone(),
        Some(record(
            CoreIdentity::generate().public_b64(),
            "other".into(),
        )),
        DeviceState::default(),
    );
    let mut wire = wire_between(phone, glass_id, false);
    wire.glass = glass;
    let outs = wire.glass.start();
    wire.glass_carry(outs);
    wire.settle(|f| f);
    assert!(!wire.glass.is_running());
    assert!(!wire.phone.is_running());
    assert!(wire.glass.is_closed());
}

#[test]
fn a_companion_that_does_not_encrypt_is_refused_with_a_reason_and_no_bond() {
    let mut glass = Link::new(Identity::generate(), None, DeviceState::default());
    glass.start();
    let outs = glass.on_frame(Frame::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        role: Role::Companion,
        app_version: "old".into(),
        device_name: "old phone".into(),
        identity_pub: Identity::generate().public_b64(),
        capabilities: vec![capability::NOTIFICATIONS.to_string()],
    }));
    assert!(matches!(
        &outs[0],
        Out::Send(Frame::Error(e)) if e.code == ErrorCode::Unsupported
    ));
    assert!(matches!(outs[1], Out::Close(_)));
    assert!(glass.is_closed());
    assert!(glass.bonded().is_none());
}

#[test]
fn stripping_the_capability_cannot_downgrade_either_side() {
    let mut wire = wire_between(CoreIdentity::generate(), Identity::generate(), true);
    wire.settle(|f| match f {
        Frame::Hello(mut h) => {
            h.capabilities.retain(|c| c != capability::SESSION_CRYPTO);
            Frame::Hello(h)
        }
        other => other,
    });
    assert!(!wire.phone.is_running() && !wire.glass.is_running());
    assert_eq!(wire.phone_ended(), Some(&LinkEnd::PeerNotEncrypted));
}

#[test]
fn a_pair_request_for_another_key_than_the_hello_is_refused() {
    let id = Identity::generate();
    let mut glass = Link::new(Identity::generate(), None, DeviceState::default());
    glass.start();
    glass.on_frame(Frame::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        role: Role::Companion,
        app_version: "x".into(),
        device_name: "x".into(),
        identity_pub: id.public_b64(),
        capabilities: vec![capability::SESSION_CRYPTO.to_string()],
    }));
    glass.on_frame(Frame::PairRequest(omg_protocol::PairRequest {
        companion_name: "x".into(),
        companion_pub: Identity::generate().public_b64(),
        nonce: "AAAA".into(),
        bootstrap_token: None,
    }));
    assert!(glass.is_closed());
}

#[test]
fn nothing_but_the_handshake_leaves_the_glass_before_the_keys_exist() {
    let mut glass = Link::new(Identity::generate(), None, DeviceState::default());
    glass.start();
    assert!(glass.encode(&Frame::WifiTearDown).is_err());
    assert!(
        glass
            .encode(&Frame::Status(omg_protocol::GlassStatus::default()))
            .is_err()
    );
}

#[test]
fn a_companion_that_does_not_encrypt_hears_why_over_a_real_socket() {
    use std::io::{Read, Write};
    let tmp = tempfile::tempdir().unwrap();
    let mut glass = super::Companion::new(tmp.path(), DeviceState::default(), None, false).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    glass
        .connect_tcp(&listener.local_addr().unwrap().to_string())
        .unwrap();
    let (mut old, _) = listener.accept().unwrap();
    old.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    old.write_all(
        &frame_bytes(&Frame::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            role: Role::Companion,
            app_version: "old".into(),
            device_name: "old phone".into(),
            identity_pub: Identity::generate().public_b64(),
            capabilities: vec![capability::NOTIFICATIONS.to_string()],
        }))
        .unwrap(),
    )
    .unwrap();
    let start = std::time::Instant::now();
    while glass.connected() || !glass.conn_fds().is_empty() {
        assert!(start.elapsed().as_secs() < 10);
        for (id, _, _) in glass.conn_fds() {
            glass.on_readable(id);
        }
    }
    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 4096];
    let mut frames = Vec::new();
    while frames.len() < 2 {
        let n = old.read(&mut buf).unwrap();
        assert!(n > 0, "closed after {} frames", frames.len());
        decoder.push(&buf[..n]);
        while let Some(frame) = decoder.next_frame().unwrap() {
            frames.push(frame);
        }
    }
    assert!(matches!(frames[0], Frame::Hello(_)));
    assert!(matches!(
        &frames[1],
        Frame::Error(e) if e.code == ErrorCode::Unsupported
    ));
    assert!(!glass.paired());
}
