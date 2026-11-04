//! The Glass side of the OpenMyGlass companion link (docs/api.md, "Companion link").
//!
//! On Model B glassd itself holds the phone link, rather than a separate app: it speaks the
//! shared [`omg_protocol`] wire protocol (the one dependency glassd takes on the companion
//! workspace) over Bluetooth RFCOMM (preferred) or TCP (fallback), runs the Glass-role
//! handshake (one-time pairing, persistent bond, mutual ed25519 auth — rejecting any
//! non-bonded peer), and then serves the backbone: heartbeat, status, battery pushes, locale,
//! and notifications. It surfaces the link to the launcher through the existing `companion`
//! capability.
//!
//! Layering mirrors the rest of glassd: pure, testable pieces ([`link::Link`] — the session
//! state machine; [`store`]/[`identity`] — persistence and keys; [`wire`] — framing) with the
//! I/O wired around them here and in [`crate::daemon`]. [`Companion`] owns the connections and
//! turns each [`link::Out`] into an [`Event`] the daemon carries out.

pub mod battery;
pub mod bootstrap;
pub mod identity;
pub mod link;
pub mod store;
pub mod transport;
pub mod wire;

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use omg_protocol::{Locale, Notification, NotificationChoice, Radio, RadioState, WifiReason};

use crate::{info, warn};
use link::{DeviceState, Link, Out};
use store::Store;
use transport::{BoxStream, Listeners, Transport};

/// Identifies a companion connection for the lifetime of the service. Never reused.
pub type ConnId = u64;

/// Bytes read from one connection per readable pass.
const READ_BUDGET: usize = 64 * 1024;
/// Most connections in flight at once (a bonded companion plus a few probing strangers).
const MAX_CONNS: usize = 4;

/// Current wall-clock in milliseconds since the Unix epoch (matches the companion's `now_ms`).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Errors from the companion subsystem.
#[derive(Debug)]
pub enum CompanionError {
    /// Filesystem I/O error.
    Io(io::Error),
    /// State file could not be read/parsed/written.
    Store(String),
    /// Identity secret was malformed.
    BadIdentity,
    /// A malformed or out-of-sequence frame during the handshake.
    Protocol(String),
}

impl std::fmt::Display for CompanionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompanionError::Io(e) => write!(f, "io: {e}"),
            CompanionError::Store(s) => write!(f, "store: {s}"),
            CompanionError::BadIdentity => write!(f, "invalid identity key"),
            CompanionError::Protocol(s) => write!(f, "protocol: {s}"),
        }
    }
}

impl std::error::Error for CompanionError {}

impl From<io::Error> for CompanionError {
    fn from(e: io::Error) -> Self {
        CompanionError::Io(e)
    }
}

/// Something the daemon must carry out in response to a link event.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A companion finished the handshake over `transport`.
    Connected {
        /// Friendly companion name.
        name: String,
        /// Capabilities the companion advertised.
        capabilities: Vec<String>,
        /// Which radio it arrived over.
        transport: Transport,
    },
    /// The active companion link dropped.
    Disconnected,
    /// Apply this locale to the launcher i18n (then it is acknowledged on the link).
    ApplyLocale(Locale),
    /// The companion asked for the settings.
    SettingsGet,
    /// The companion changed these settings.
    SettingsSet(omg_protocol::Settings),
    /// Show this notification on the launcher timeline.
    Notification(Notification),
    /// Remove the timeline card with this key.
    RemoveNotification(String),
    /// The companion asked to turn a radio on/off.
    SetRadio {
        /// Which radio.
        radio: Radio,
        /// Desired state.
        on: bool,
    },
    /// The companion asked to toggle airplane mode.
    SetAirplane(bool),
    /// The companion asked to bring Wi-Fi up on demand.
    WifiBringUp(WifiReason),
    /// The companion asked to drop Wi-Fi again.
    WifiTearDown,
}

struct Conn {
    stream: BoxStream,
    decoder: wire::FrameDecoder,
    link: Link,
    out: Vec<u8>,
    transport: Transport,
    closing: bool,
}

/// The companion-link service: listeners, the persistent bond, and the live connections.
pub struct Companion {
    store: Store,
    device: DeviceState,
    listeners: Listeners,
    conns: BTreeMap<ConnId, Conn>,
    active: Option<ConnId>,
    next_id: ConnId,
    pairing_enabled: bool,
}

impl Companion {
    /// Loads (or creates) the bond under `state_dir` and binds the listeners. `tcp_port` is the
    /// TCP accept port (`None` disables TCP); `bt` enables the RFCOMM server (needs the
    /// `linux-bt` feature to do anything).
    pub fn new(
        state_dir: &Path,
        device: DeviceState,
        tcp_port: Option<u16>,
        bt: bool,
    ) -> Result<Companion, CompanionError> {
        let store = Store::load_or_create(Store::path_in(state_dir))?;
        if let Some(p) = store.paired() {
            info!("companion: bonded to {:?}", p.peer_name);
        } else {
            info!("companion: not yet paired");
        }
        let listeners = Listeners::bind(tcp_port, bt);
        Ok(Companion {
            store,
            device,
            listeners,
            conns: BTreeMap::new(),
            active: None,
            next_id: 1,
            pairing_enabled: true,
        })
    }

    /// Whether a companion is bonded (persisted).
    pub fn paired(&self) -> bool {
        self.store.paired().is_some()
    }

    /// The bonded companion's name, if any.
    pub fn paired_name(&self) -> Option<&str> {
        self.store.paired().map(|p| p.peer_name.as_str())
    }

    /// The transport of the live link, if a companion is connected.
    pub fn transport(&self) -> Option<Transport> {
        self.active
            .and_then(|id| self.conns.get(&id))
            .map(|c| c.transport)
    }

    /// Whether a companion is connected and authenticated right now.
    pub fn connected(&self) -> bool {
        self.active.is_some()
    }

    /// Allows or forbids a first pairing (the owner's setup window). Applies to future and
    /// in-flight handshakes.
    pub fn set_pairing_enabled(&mut self, enabled: bool) {
        self.pairing_enabled = enabled;
        for c in self.conns.values_mut() {
            c.link.set_pairing_enabled(enabled);
        }
    }

    /// Forgets the bonded companion and drops the live link.
    pub fn unpair(&mut self) -> Vec<Event> {
        if let Err(e) = self.store.clear_paired() {
            warn!("companion: cannot clear the bond ({e})");
        }
        let ids: Vec<ConnId> = self.conns.keys().copied().collect();
        let mut events = Vec::new();
        for id in ids {
            self.drop_conn(id, "unpaired", &mut events);
        }
        events
    }

    /// The listener fds to add to the poll set, each tagged with its transport.
    pub fn listener_fds(&self) -> Vec<(RawFd, Transport)> {
        self.listeners.fds()
    }

    /// The connection fds to poll: `(id, fd, want_write)`.
    pub fn conn_fds(&self) -> Vec<(ConnId, RawFd, bool)> {
        self.conns
            .iter()
            .map(|(&id, c)| (id, c.stream.as_raw_fd(), !c.out.is_empty()))
            .collect()
    }

    /// Accepts the pending connections on the listener for `transport`.
    pub fn accept(&mut self, transport: Transport) -> Vec<Event> {
        let mut events = Vec::new();
        for stream in self.listeners.accept(transport) {
            if self.conns.len() >= MAX_CONNS {
                warn!(
                    "companion: too many connections; dropping a new {}",
                    transport.as_str()
                );
                continue;
            }
            self.add_conn(stream, transport, &mut events);
        }
        events
    }

    /// Dials a companion over TCP (the QR bootstrap connect path), then starts the handshake.
    pub fn connect_tcp(&mut self, addr: &str) -> io::Result<Vec<Event>> {
        let stream = TcpStream::connect(addr)?;
        stream.set_nonblocking(true)?;
        let _ = stream.set_nodelay(true);
        info!("companion: dialed {addr}");
        let mut events = Vec::new();
        self.add_conn(
            Box::new(stream) as BoxStream,
            Transport::WifiTcp,
            &mut events,
        );
        Ok(events)
    }

    fn add_conn(&mut self, stream: BoxStream, transport: Transport, events: &mut Vec<Event>) {
        let id = self.next_id;
        self.next_id += 1;
        let mut link = Link::new(
            self.store.identity().clone(),
            self.store.paired().cloned(),
            self.device.clone(),
        );
        link.set_pairing_enabled(self.pairing_enabled);
        let outs = link.start();
        self.conns.insert(
            id,
            Conn {
                stream,
                decoder: wire::FrameDecoder::new(),
                link,
                out: Vec::new(),
                transport,
                closing: false,
            },
        );
        // Drive the Hello that start() queued against the now-inserted connection.
        self.drive(id, outs, events);
    }

    /// A connection is readable: read, decode, and drive the link.
    pub fn on_readable(&mut self, id: ConnId) -> Vec<Event> {
        let mut events = Vec::new();
        let mut buf = [0u8; 8192];
        let mut budget = READ_BUDGET;
        let mut frames = Vec::new();
        let mut gone = false;
        let mut bad = None;
        if let Some(c) = self.conns.get_mut(&id) {
            'read: while budget > 0 {
                let want = buf.len().min(budget);
                match c.stream.read(&mut buf[..want]) {
                    Ok(0) => {
                        gone = true;
                        break;
                    }
                    Ok(n) => {
                        budget -= n;
                        c.decoder.push(&buf[..n]);
                        loop {
                            match c.decoder.next_frame() {
                                Ok(Some(f)) => frames.push(f),
                                Ok(None) => break,
                                Err(e) => {
                                    bad = Some(e.to_string());
                                    break 'read;
                                }
                            }
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        gone = true;
                        break;
                    }
                }
            }
        }
        for f in frames {
            if !self.conns.contains_key(&id) || self.conns[&id].closing {
                break;
            }
            let outs = self
                .conns
                .get_mut(&id)
                .map(|c| c.link.on_frame(f))
                .unwrap_or_default();
            self.drive(id, outs, &mut events);
        }
        if let Some(e) = bad {
            warn!("companion {id}: {e}");
            self.drop_conn(id, "bad frame", &mut events);
        } else if gone {
            self.drop_conn(id, "closed by peer", &mut events);
        }
        events
    }

    /// A connection is writable: flush its queued bytes.
    pub fn on_writable(&mut self, id: ConnId) -> Vec<Event> {
        let mut events = Vec::new();
        self.flush(id, &mut events);
        events
    }

    /// The battery changed (from `/sys/class/power_supply`): push it on the live link.
    pub fn battery_changed(&mut self, percent: u8, charging: bool) -> Vec<Event> {
        self.device.battery_percent = Some(percent);
        self.device.charging = Some(charging);
        self.on_active(|l| l.battery_changed(percent, charging))
    }

    /// The radio state changed: report it on the live link.
    pub fn radio_state_changed(&mut self, radios: RadioState) -> Vec<Event> {
        self.device.radios = Some(radios.clone());
        self.on_active(move |l| l.radio_state_changed(radios.clone()))
    }

    /// The UI language changed on the Glass: keep the reported device state in step and tell the
    /// live companion (a no-op on the active link when it was the companion's own `SetLocale`).
    pub fn locale_changed(&mut self, locale: Locale) -> Vec<Event> {
        self.device.locale = locale;
        self.on_active(move |l| l.locale_changed(locale))
    }

    /// Sends the settings on the live link (an answer, or a change on the Glass).
    pub fn send_settings(&mut self, settings: omg_protocol::Settings) -> Vec<Event> {
        self.on_active(move |l| l.send_settings(settings.clone()))
    }

    /// The owner picked a notification action (or dismissed it) on the Glass: relay it.
    pub fn notification_action(&mut self, key: String, choice: NotificationChoice) -> Vec<Event> {
        self.on_active(move |l| l.notification_action(key.clone(), choice))
    }

    /// Decodes a Wi-Fi QR bootstrap (`OMG1:`) so the daemon can join and connect (item 6). The
    /// camera-side scan is a later phase; this is the decode entry point.
    pub fn decode_bootstrap(
        &self,
        s: &str,
    ) -> Result<bootstrap::WifiBootstrap, bootstrap::BootstrapError> {
        bootstrap::decode(s)
    }

    fn on_active(&mut self, f: impl FnOnce(&mut Link) -> Vec<Out>) -> Vec<Event> {
        let Some(id) = self.active else {
            return Vec::new();
        };
        let outs = self
            .conns
            .get_mut(&id)
            .map(|c| f(&mut c.link))
            .unwrap_or_default();
        let mut events = Vec::new();
        self.drive(id, outs, &mut events);
        events
    }

    /// Turns a link's outputs into queued bytes, store writes, and daemon events.
    fn drive(&mut self, id: ConnId, outs: Vec<Out>, events: &mut Vec<Event>) {
        for out in outs {
            match out {
                Out::Send(frame) => match wire::frame_bytes(&frame) {
                    Ok(bytes) => {
                        if let Some(c) = self.conns.get_mut(&id) {
                            c.out.extend_from_slice(&bytes);
                        }
                    }
                    Err(e) => warn!("companion {id}: cannot encode {}: {e}", frame.wire_name()),
                },
                Out::Connected { name, capabilities } => {
                    let transport = self
                        .conns
                        .get(&id)
                        .map(|c| c.transport)
                        .unwrap_or(Transport::WifiTcp);
                    // Only one active companion: close any other live link.
                    let others: Vec<ConnId> =
                        self.conns.keys().copied().filter(|&o| o != id).collect();
                    for o in others {
                        self.drop_conn(o, "another companion connected", events);
                    }
                    self.active = Some(id);
                    info!("companion: {name:?} connected over {}", transport.as_str());
                    events.push(Event::Connected {
                        name,
                        capabilities,
                        transport,
                    });
                }
                Out::Persist(rec) => {
                    if let Err(e) = self.store.set_paired(rec.clone()) {
                        warn!("companion: cannot persist the bond ({e})");
                    } else {
                        info!("companion: paired with {:?}", rec.peer_name);
                    }
                }
                Out::Unpair => {
                    if let Err(e) = self.store.clear_paired() {
                        warn!("companion: cannot clear the bond ({e})");
                    }
                }
                Out::ApplyLocale(l) => events.push(Event::ApplyLocale(l)),
                Out::SettingsGet => events.push(Event::SettingsGet),
                Out::SettingsSet(s) => events.push(Event::SettingsSet(s)),
                Out::Notification(n) => events.push(Event::Notification(n)),
                Out::RemoveNotification(k) => events.push(Event::RemoveNotification(k)),
                Out::SetRadio { radio, on } => events.push(Event::SetRadio { radio, on }),
                Out::SetAirplane(on) => events.push(Event::SetAirplane(on)),
                Out::WifiBringUp(r) => events.push(Event::WifiBringUp(r)),
                Out::WifiTearDown => events.push(Event::WifiTearDown),
                Out::Close(reason) => self.drop_conn(id, &reason, events),
            }
        }
        // Best-effort immediate flush; the rest waits for POLLOUT.
        self.flush(id, events);
    }

    fn flush(&mut self, id: ConnId, events: &mut Vec<Event>) {
        let mut err = false;
        let mut done = false;
        if let Some(c) = self.conns.get_mut(&id) {
            while !c.out.is_empty() {
                match c.stream.write(&c.out) {
                    Ok(0) => {
                        err = true;
                        break;
                    }
                    Ok(n) => {
                        c.out.drain(..n);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        err = true;
                        break;
                    }
                }
            }
            done = c.closing && c.out.is_empty();
        }
        if err {
            self.drop_conn(id, "write failed", events);
        } else if done {
            self.drop_conn(id, "closed", events);
        }
    }

    fn drop_conn(&mut self, id: ConnId, reason: &str, events: &mut Vec<Event>) {
        if self.conns.remove(&id).is_some() {
            info!("companion {id}: {reason}");
        }
        if self.active == Some(id) {
            self.active = None;
            events.push(Event::Disconnected);
        }
    }
}

/// The companion state directory file (for the daemon to locate the bond).
pub fn state_file(dir: &Path) -> PathBuf {
    Store::path_in(dir)
}

#[cfg(test)]
mod e2e;
