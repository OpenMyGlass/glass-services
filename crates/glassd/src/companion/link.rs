//! The Glass-role companion-link session state machine, as a pure, effect-producing state
//! machine (no I/O), like [`crate::companion`]'s `Relay` and `glass-api`'s `Hub`.
//!
//! It is the event-driven twin of `omg-core`'s `session.rs` + `simglass.rs`: where those run
//! the handshake as a straight-line async flow, this feeds on one [`Frame`] at a time and
//! returns the [`Out`]s the daemon must carry out, so it drops into glassd's single-threaded
//! `poll` loop. Each `match` arm matches a `simglass` arm, so both ends are proven against the
//! same contract.
//!
//! Flow (Glass side):
//! 1. [`Link::start`] sends our `Hello`.
//! 2. On the peer `Hello`: protocol-version check, then authorize — already bonded + same key
//!    is a reuse; already bonded + different key is rejected with `Error{UnknownPeer}`;
//!    unbonded runs the one-time pairing (`PairRequest` -> `PairAccept` -> `PairResult`).
//! 3. Mutual auth: each side signs `"openmyglass-companion-auth-v1" || nonce`.
//! 4. Running: `Ping`->`Pong`, `StatusRequest`->`Status`, `SetLocale`->`LocaleChanged`,
//!    battery pushes, notifications in/actions out, radio/Wi-Fi requests surfaced to glassd.

use omg_protocol::{
    AuthChallenge, AuthResponse, AuthResult, BatteryStatus, Bye, ErrorCode, ErrorMsg, Frame,
    GlassStatus, Hello, Locale, LocaleChanged, Notification, NotificationAction,
    NotificationChoice, PROTOCOL_VERSION, PairAccept, PairResult, Ping, Pong, Radio, RadioState,
    Role, SetAirplaneMode, SetRadio, StatusRequest, WifiReason, capability,
};

use super::identity::{Identity, decode_nonce, random_nonce_b64, verify_challenge};
use super::now_ms;
use super::store::{PairingRecord, record};

/// What the device reports in `Status`/`BatteryStatus`. The daemon keeps this in step with the
/// real hardware (battery from `/sys/class/power_supply`, locale from the settings, radios from
/// the radio controller).
#[derive(Debug, Clone)]
pub struct DeviceState {
    /// Friendly device/Bluetooth name.
    pub device_name: String,
    /// glassd/ROM version string.
    pub software_version: String,
    /// Battery charge 0-100, if known.
    pub battery_percent: Option<u8>,
    /// Whether charging, if known.
    pub charging: Option<bool>,
    /// Internal storage total, bytes.
    pub storage_total_bytes: Option<u64>,
    /// Internal storage free, bytes.
    pub storage_free_bytes: Option<u64>,
    /// Prism width in pixels (640 on XE24).
    pub screen_width_px: Option<u32>,
    /// Prism height in pixels (360 on XE24).
    pub screen_height_px: Option<u32>,
    /// Battery/skin temperature, Celsius.
    pub temperature_c: Option<f32>,
    /// Current radio state.
    pub radios: Option<RadioState>,
    /// Active UI locale.
    pub locale: Locale,
}

impl Default for DeviceState {
    fn default() -> Self {
        DeviceState {
            device_name: "OpenMyGlass".into(),
            software_version: concat!("glassd/", env!("CARGO_PKG_VERSION")).into(),
            battery_percent: None,
            charging: None,
            storage_total_bytes: None,
            storage_free_bytes: None,
            screen_width_px: Some(640),
            screen_height_px: Some(360),
            temperature_c: None,
            radios: None,
            locale: Locale::DEFAULT,
        }
    }
}

/// Something the daemon must do in response to a link event.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// Send this frame to the companion.
    Send(Frame),
    /// The handshake finished: the companion is authenticated. `name` and `capabilities` come
    /// from the companion's `Hello`.
    Connected {
        /// Friendly companion name.
        name: String,
        /// Capabilities the companion advertised.
        capabilities: Vec<String>,
    },
    /// A companion was newly bonded; persist it in the store.
    Persist(PairingRecord),
    /// The companion asked to forget the bond (unpair); clear the store.
    Unpair,
    /// Apply this locale to the launcher i18n. The `LocaleChanged` ack is already queued.
    ApplyLocale(Locale),
    /// Show this notification on the launcher timeline.
    Notification(Notification),
    /// Remove the timeline card with this key.
    RemoveNotification(String),
    /// The companion asked to turn a radio on/off (reuse the merged radio control).
    SetRadio {
        /// Which radio.
        radio: Radio,
        /// Desired state.
        on: bool,
    },
    /// The companion asked to toggle airplane mode (all radios off).
    SetAirplane(bool),
    /// The companion asked to bring Wi-Fi up on demand for a bandwidth-heavy job.
    WifiBringUp(WifiReason),
    /// The companion asked to drop Wi-Fi again.
    WifiTearDown,
    /// Close the link; `reason` is for logs. Any `Error` frame is already queued before this.
    Close(String),
}

#[derive(Debug)]
struct Auth {
    our_nonce: Vec<u8>,
    answered_peer: bool,
    got_their_response: bool,
    verify_ok: bool,
    sent_result: bool,
    peer_result: Option<bool>,
}

#[derive(Debug)]
enum Phase {
    Start,
    AwaitingHello,
    AwaitingPairRequest,
    AwaitingPairResult(PairingRecord),
    Authenticating(Auth),
    Running,
    Closed,
}

/// A single companion-link session.
pub struct Link {
    identity: Identity,
    bonded: Option<PairingRecord>,
    device: DeviceState,
    device_name: String,
    app_version: String,
    capabilities: Vec<String>,
    /// Whether an unbonded peer may pair (the owner's one-time setup). Default true.
    pairing_enabled: bool,
    phase: Phase,
    peer_name: Option<String>,
    peer_capabilities: Vec<String>,
}

impl Link {
    /// A Glass-role link with our `identity`, the currently-bonded companion (from the store,
    /// if any), and the device state to report.
    pub fn new(identity: Identity, bonded: Option<PairingRecord>, device: DeviceState) -> Link {
        Link {
            identity,
            bonded,
            device_name: device.device_name.clone(),
            device,
            app_version: concat!("glassd/", env!("CARGO_PKG_VERSION")).into(),
            capabilities: capability::BACKBONE.iter().map(|s| s.to_string()).collect(),
            pairing_enabled: true,
            phase: Phase::Start,
            peer_name: None,
            peer_capabilities: Vec::new(),
        }
    }

    /// Permits or forbids a first pairing with an unbonded peer (the setup window).
    pub fn set_pairing_enabled(&mut self, enabled: bool) {
        self.pairing_enabled = enabled;
    }

    /// Whether the handshake has completed and the link is serving the backbone.
    pub fn is_running(&self) -> bool {
        matches!(self.phase, Phase::Running)
    }

    /// Whether the link has closed.
    pub fn is_closed(&self) -> bool {
        matches!(self.phase, Phase::Closed)
    }

    /// The companion's friendly name once the handshake is far enough along.
    pub fn peer_name(&self) -> Option<&str> {
        self.peer_name.as_deref()
    }

    /// The bonded companion, if any.
    pub fn bonded(&self) -> Option<&PairingRecord> {
        self.bonded.as_ref()
    }

    /// Mutable device state: the daemon updates it as the hardware changes.
    pub fn device_mut(&mut self) -> &mut DeviceState {
        &mut self.device
    }

    /// Sends our `Hello`. Call once, right after the link is established.
    pub fn start(&mut self) -> Vec<Out> {
        if !matches!(self.phase, Phase::Start) {
            return Vec::new();
        }
        self.phase = Phase::AwaitingHello;
        vec![Out::Send(Frame::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            role: Role::Glass,
            app_version: self.app_version.clone(),
            device_name: self.device_name.clone(),
            identity_pub: self.identity.public_b64(),
            capabilities: self.capabilities.clone(),
        }))]
    }

    /// Handles one decoded frame from the companion.
    pub fn on_frame(&mut self, frame: Frame) -> Vec<Out> {
        let mut out = Vec::new();
        match &mut self.phase {
            Phase::Start | Phase::Closed => {}
            Phase::AwaitingHello => self.on_hello(frame, &mut out),
            Phase::AwaitingPairRequest => self.on_pair_request(frame, &mut out),
            Phase::AwaitingPairResult(_) => self.on_pair_result(frame, &mut out),
            Phase::Authenticating(_) => self.on_auth(frame, &mut out),
            Phase::Running => self.on_running(frame, &mut out),
        }
        out
    }

    /// The battery changed: push a `BatteryStatus` (only once running), like glassd's power
    /// receiver.
    pub fn battery_changed(&mut self, percent: u8, charging: bool) -> Vec<Out> {
        self.device.battery_percent = Some(percent);
        self.device.charging = Some(charging);
        if self.is_running() {
            vec![Out::Send(Frame::BatteryStatus(BatteryStatus {
                percent,
                charging,
            }))]
        } else {
            Vec::new()
        }
    }

    /// The owner picked a notification action (or dismissed the card) on the Glass: relay it.
    pub fn notification_action(&mut self, key: String, choice: NotificationChoice) -> Vec<Out> {
        if self.is_running() {
            vec![Out::Send(Frame::NotificationAction(NotificationAction {
                key,
                choice,
            }))]
        } else {
            Vec::new()
        }
    }

    /// The radio state changed on the Glass: report it to the companion.
    pub fn radio_state_changed(&mut self, radios: RadioState) -> Vec<Out> {
        self.device.radios = Some(radios.clone());
        if self.is_running() {
            vec![Out::Send(Frame::RadioState(radios))]
        } else {
            Vec::new()
        }
    }

    // --------------------------- handshake ---------------------------

    fn error(&mut self, out: &mut Vec<Out>, code: ErrorCode, message: &str) {
        out.push(Out::Send(Frame::Error(ErrorMsg {
            code,
            message: message.into(),
        })));
        self.close(out, message);
    }

    fn close(&mut self, out: &mut Vec<Out>, reason: &str) {
        self.phase = Phase::Closed;
        out.push(Out::Close(reason.into()));
    }

    fn on_hello(&mut self, frame: Frame, out: &mut Vec<Out>) {
        let Frame::Hello(h) = frame else {
            self.error(out, ErrorCode::Protocol, "expected Hello");
            return;
        };
        if h.protocol_version != PROTOCOL_VERSION {
            self.error(
                out,
                ErrorCode::VersionMismatch,
                &format!("peer v{} != v{}", h.protocol_version, PROTOCOL_VERSION),
            );
            return;
        }
        self.peer_name = Some(h.device_name.clone());
        self.peer_capabilities = h.capabilities.clone();
        // Authorize.
        if let Some(rec) = self.bonded.clone() {
            if rec.peer_pub == h.identity_pub {
                self.enter_auth(out);
            } else {
                self.error(
                    out,
                    ErrorCode::UnknownPeer,
                    "already bonded to another companion",
                );
            }
            return;
        }
        if !self.pairing_enabled {
            self.error(
                out,
                ErrorCode::NotPaired,
                "no pairing and new pairing not permitted",
            );
            return;
        }
        // Unbonded: wait for the companion's PairRequest (Glass accepts a one-time pairing).
        self.phase = Phase::AwaitingPairRequest;
    }

    fn on_pair_request(&mut self, frame: Frame, out: &mut Vec<Out>) {
        match frame {
            Frame::PairRequest(req) => {
                out.push(Out::Send(Frame::PairAccept(PairAccept {
                    glass_name: self.device_name.clone(),
                    glass_pub: self.identity.public_b64(),
                    nonce: req.nonce,
                })));
                let rec = record(req.companion_pub, req.companion_name);
                self.phase = Phase::AwaitingPairResult(rec);
            }
            Frame::Error(_) => self.close(out, "companion refused pairing"),
            other => self.error(
                out,
                ErrorCode::Protocol,
                &format!("expected PairRequest, got {}", other.wire_name()),
            ),
        }
    }

    fn on_pair_result(&mut self, frame: Frame, out: &mut Vec<Out>) {
        let Phase::AwaitingPairResult(rec) = &self.phase else {
            return;
        };
        let rec = rec.clone();
        match frame {
            Frame::PairResult(PairResult { ok: true, .. }) => {
                self.peer_name = Some(rec.peer_name.clone());
                self.bonded = Some(rec.clone());
                out.push(Out::Persist(rec));
                self.enter_auth(out);
            }
            Frame::PairResult(_) => self.close(out, "companion refused pairing"),
            other => self.error(
                out,
                ErrorCode::Protocol,
                &format!("expected PairResult, got {}", other.wire_name()),
            ),
        }
    }

    fn enter_auth(&mut self, out: &mut Vec<Out>) {
        let our_nonce_b64 = random_nonce_b64(32);
        // We just generated it, so it decodes.
        let our_nonce = decode_nonce(&our_nonce_b64).unwrap_or_default();
        out.push(Out::Send(Frame::AuthChallenge(AuthChallenge {
            nonce: our_nonce_b64,
        })));
        self.phase = Phase::Authenticating(Auth {
            our_nonce,
            answered_peer: false,
            got_their_response: false,
            verify_ok: false,
            sent_result: false,
            peer_result: None,
        });
    }

    fn on_auth(&mut self, frame: Frame, out: &mut Vec<Out>) {
        let bonded_pub = match self.bonded.as_ref() {
            Some(b) => b.peer_pub.clone(),
            None => {
                self.error(out, ErrorCode::Internal, "auth without a bond");
                return;
            }
        };
        match frame {
            Frame::AuthChallenge(ch) => {
                let Ok(nonce) = decode_nonce(&ch.nonce) else {
                    self.error(out, ErrorCode::Protocol, "bad challenge nonce");
                    return;
                };
                let sig = self.identity.sign_challenge(&nonce);
                out.push(Out::Send(Frame::AuthResponse(AuthResponse {
                    signature: sig,
                })));
                if let Phase::Authenticating(a) = &mut self.phase {
                    a.answered_peer = true;
                }
                self.maybe_finish_auth(out);
            }
            Frame::AuthResponse(resp) => {
                if let Phase::Authenticating(a) = &mut self.phase {
                    a.verify_ok = verify_challenge(&bonded_pub, &a.our_nonce, &resp.signature);
                    a.got_their_response = true;
                }
                self.maybe_finish_auth(out);
            }
            Frame::AuthResult(r) => {
                if let Phase::Authenticating(a) = &mut self.phase {
                    a.peer_result = Some(r.ok);
                }
                self.maybe_finalize_auth(out);
            }
            Frame::Error(e) => self.close(out, &format!("companion auth error: {}", e.message)),
            other => self.error(
                out,
                ErrorCode::Protocol,
                &format!("unexpected {} during auth", other.wire_name()),
            ),
        }
    }

    /// After we have both answered the companion's challenge and verified its response, send
    /// our `AuthResult` (ending the `&mut phase` borrow before any further self calls).
    fn maybe_finish_auth(&mut self, out: &mut Vec<Out>) {
        let verify_ok = match &mut self.phase {
            Phase::Authenticating(a)
                if (a.answered_peer && a.got_their_response) && !a.sent_result =>
            {
                a.sent_result = true;
                a.verify_ok
            }
            _ => return,
        };
        out.push(Out::Send(Frame::AuthResult(AuthResult {
            ok: verify_ok,
            message: if verify_ok {
                "ok".into()
            } else {
                "bad signature".into()
            },
        })));
        if !verify_ok {
            self.close(out, "companion signature did not verify");
        } else {
            self.maybe_finalize_auth(out);
        }
    }

    /// Once we have sent our (ok) `AuthResult` and seen the companion's, go Running.
    fn maybe_finalize_auth(&mut self, out: &mut Vec<Out>) {
        let result = match &self.phase {
            Phase::Authenticating(a) if a.sent_result => a.peer_result,
            _ => return,
        };
        match result {
            None => {}
            Some(true) => self.run(out),
            Some(false) => self.close(out, "companion rejected our signature"),
        }
    }

    fn run(&mut self, out: &mut Vec<Out>) {
        self.phase = Phase::Running;
        out.push(Out::Connected {
            name: self.peer_name.clone().unwrap_or_default(),
            capabilities: self.peer_capabilities.clone(),
        });
        // Push an initial battery status, like glassd's BatteryPowerStateReceiver.
        if let (Some(percent), Some(charging)) = (self.device.battery_percent, self.device.charging)
        {
            out.push(Out::Send(Frame::BatteryStatus(BatteryStatus {
                percent,
                charging,
            })));
        }
    }

    // --------------------------- running backbone ---------------------------

    fn on_running(&mut self, frame: Frame, out: &mut Vec<Out>) {
        match frame {
            Frame::Ping(Ping { token, .. }) => {
                out.push(Out::Send(Frame::Pong(Pong {
                    token,
                    unix_ms: now_ms(),
                })));
            }
            Frame::StatusRequest(StatusRequest { .. }) => {
                out.push(Out::Send(Frame::Status(self.status())));
            }
            Frame::SetLocale(set) => {
                self.device.locale = set.locale;
                out.push(Out::ApplyLocale(set.locale));
                out.push(Out::Send(Frame::LocaleChanged(LocaleChanged {
                    locale: set.locale,
                })));
            }
            Frame::NotificationPosted(n) => out.push(Out::Notification(n.notification)),
            Frame::NotificationRemoved(r) => out.push(Out::RemoveNotification(r.key)),
            Frame::SetRadio(SetRadio { radio, on }) => out.push(Out::SetRadio { radio, on }),
            Frame::SetAirplaneMode(SetAirplaneMode { on }) => out.push(Out::SetAirplane(on)),
            Frame::WifiBringUpRequest(req) => out.push(Out::WifiBringUp(req.reason)),
            Frame::WifiTearDown => out.push(Out::WifiTearDown),
            Frame::Unpair => {
                out.push(Out::Unpair);
                self.bonded = None;
                self.close(out, "companion unpaired");
            }
            Frame::Bye(Bye { reason }) => self.close(out, &format!("companion bye: {reason}")),
            // Ping/Pong the other way, and the many SCAFFOLD frames, are ignored for now.
            _ => {}
        }
    }

    fn status(&self) -> GlassStatus {
        let d = &self.device;
        GlassStatus {
            battery_percent: d.battery_percent,
            charging: d.charging,
            storage_total_bytes: d.storage_total_bytes,
            storage_free_bytes: d.storage_free_bytes,
            device_name: Some(d.device_name.clone()),
            software_version: Some(d.software_version.clone()),
            screen_width_px: d.screen_width_px,
            screen_height_px: d.screen_height_px,
            temperature_c: d.temperature_c,
            radios: d.radios.clone(),
            locale: Some(d.locale),
        }
    }
}
