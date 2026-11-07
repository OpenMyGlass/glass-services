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
pub mod entropy;
pub mod identity;
pub mod link;
pub mod notes;
pub mod store;
pub mod transport;
pub mod wire;

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use omg_protocol::{
    ErrorCode, ErrorMsg, Frame, Locale, Notification, NotificationChoice, Radio, RadioState,
    WifiReason,
};

use crate::{info, warn};
use entropy::PoolWait;
use link::{DeviceState, Link, Out};
use store::Store;
use transport::{BoxStream, Listeners, Transport};

/// Identifies a companion connection for the lifetime of the service. Never reused.
pub type ConnId = u64;

/// Bytes read from one connection per readable pass.
const READ_BUDGET: usize = 64 * 1024;
/// Most connections in flight at once (a bonded companion plus a few probing strangers).
const MAX_CONNS: usize = 4;
/// With BLE on, connections over anything else stop this far below [`MAX_CONNS`], so that nobody on
/// the network can keep the phone's everyday link out.
const BLE_RESERVED_CONNS: usize = 1;
/// How long a peer has to finish the handshake, about 800 bytes each way, on any transport. A peer
/// that says nothing must not keep a connection: one connected stranger on BLE stops the
/// advertising. Generous for a BLE link that was measured to stall for 3.6 s. The handshake waits
/// for no person at either end.
const HANDSHAKE_MS: u64 = 30_000;

/// How long the pairing mode stays open when no length is asked for, and the longest it can.
pub const PAIRING_WINDOW_DEFAULT_MS: u32 = 120_000;
pub const PAIRING_WINDOW_MAX_MS: u32 = 300_000;

/// The clock of the pairing window and of the handshake deadline. It keeps running through a suspend, so a window cannot outlast its
/// time asleep, and nothing a phone can change moves it, unlike the wall clock.
fn boot_ms() -> u64 {
    crate::daemon::now().boot_us / 1000
}

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
    /// Something happened with a note: for `TOPIC_NOTES`.
    Note(glass_api::proto::NoteEvent),
    /// The bond or the pairing mode changed: the launcher should hear of it.
    PairingChanged,
}

struct Conn {
    stream: BoxStream,
    decoder: wire::FrameDecoder,
    link: Link,
    out: Vec<u8>,
    transport: Transport,
    closing: bool,
    /// A peer that has not finished the handshake goes at this time ([`boot_ms`]).
    handshake_by_ms: Option<u64>,
    /// The owner's QR dial made this connection: it may pair, with the pairing mode closed.
    owner_dial: bool,
}

/// How often the entropy level is checked while a first identity waits for it. The level grows by
/// a few bits a second, so this needs no precision, and it only runs on a first boot.
const ENTROPY_CHECK_MS: u64 = 2_000;

/// Whether the link may use randomness yet (see [`entropy`]). The handshake needs it for a
/// challenge nonce even with a stored identity, so no listener is bound before [`Rng::Ready`].
enum Rng {
    /// The pool is not seeded: the daemon polls this and calls [`Companion::on_rng_event`].
    Waiting(PoolWait),
    /// Seeded, but a first identity needs more entropy than that: checked at this wall-clock
    /// time, on a timer, because `/dev/random` stays readable and polling it would spin.
    Gathering { next_check_ms: u64 },
    /// Seeded: the `getrandom` crate will not block any more.
    Ready,
    /// No randomness to be had, or the identity could not be stored: the link stays down.
    Failed,
}

/// The message of the `ERROR_CODE_UNAVAILABLE` that refuses a pairing mode because the link
/// is down for good (no randomness, or the identity could not be stored). A contract of
/// docs/api.md ("Pairing mode"): the launcher tells it from "already bonded" by this text.
pub const LINK_DISABLED: &str = "the companion link is disabled";

/// The companion-link service: listeners, the persistent bond, and the live connections.
pub struct Companion {
    /// `None` until a first identity has been generated.
    store: Option<Store>,
    store_path: PathBuf,
    rng: Rng,
    /// Where the input pool's level is read.
    entropy_avail: PathBuf,
    /// Where the seed for the next boot goes.
    seed_path: PathBuf,
    /// A trusted seed went in at this start: a first identity need not wait for the level.
    seed_used: bool,
    /// A dial asked for before the pool was seeded.
    /// With whether it is the owner's dial, that may pair.
    pending_dial: Option<(String, bool)>,
    tcp_port: Option<u16>,
    bt: bool,
    /// Where `omg-bled` connects, bound with the other listeners.
    ble_socket: Option<PathBuf>,
    device: DeviceState,
    listeners: Listeners,
    /// TCP stays closed for now: see [`Companion::set_tcp_closed`].
    tcp_closed: bool,
    conns: BTreeMap<ConnId, Conn>,
    active: Option<ConnId>,
    next_id: ConnId,
    /// A first pairing, over any transport, may happen until this time ([`boot_ms`]). Never stored: a restart
    /// closes it.
    pairing_until_ms: Option<u64>,
    /// The note queue. `None` if it could not be opened: the link works, without notes.
    notes: Option<notes::Notes>,
    /// The live companion advertised note sync.
    notes_link: bool,
}

impl Companion {
    /// Loads the bond under `state_dir` and binds the listeners. `tcp_port` is the TCP accept
    /// port (`None` disables TCP); `bt` enables the RFCOMM server (needs the `linux-bt` feature
    /// to do anything). Never waits for the random pool: if it is not seeded yet, generating a
    /// first identity and binding the listeners wait for [`Companion::rng_fd`].
    pub fn new(
        state_dir: &Path,
        device: DeviceState,
        tcp_port: Option<u16>,
        bt: bool,
    ) -> Result<Companion, CompanionError> {
        Companion::with_rng_device(
            state_dir,
            device,
            tcp_port,
            bt,
            Path::new(entropy::RANDOM_DEVICE),
        )
    }

    /// [`Companion::new`], waiting on `rng_device` instead of `/dev/random`.
    pub fn with_rng_device(
        state_dir: &Path,
        device: DeviceState,
        tcp_port: Option<u16>,
        bt: bool,
        rng_device: &Path,
    ) -> Result<Companion, CompanionError> {
        let store_path = Store::path_in(state_dir);
        let store = Store::load(&store_path)?;
        let seed_path = state_dir.join(entropy::SEED_FILE);
        let seed_used = entropy::use_seed(&seed_path, Path::new(entropy::URANDOM_DEVICE));
        match &store {
            Some(s) => log_bond(s),
            // A state directory that cannot exist disables the link now, not a minute from now.
            None => std::fs::create_dir_all(state_dir)?,
        }
        let notes = match notes::Notes::open(&state_dir.join("notes")) {
            Ok(n) => Some(n),
            Err(e) => {
                warn!("notes: disabled ({e})");
                None
            }
        };
        let mut companion = Companion {
            store,
            store_path,
            rng: Rng::Failed,
            entropy_avail: entropy::ENTROPY_AVAIL.into(),
            seed_path: seed_path.clone(),
            seed_used,
            pending_dial: None,
            tcp_port,
            bt,
            ble_socket: None,
            device,
            listeners: Listeners::bind(None, false),
            tcp_closed: false,
            conns: BTreeMap::new(),
            active: None,
            next_id: 1,
            pairing_until_ms: None,
            notes,
            notes_link: false,
        };
        match PoolWait::open(rng_device) {
            Ok(wait) => match wait.seeded() {
                Ok(true) => companion.pool_seeded(),
                Ok(false) => {
                    info!(
                        "companion: waiting for the kernel's random pool; the link comes up then"
                    );
                    companion.rng = Rng::Waiting(wait);
                }
                Err(e) => warn!("companion: link disabled: {} ({e})", rng_device.display()),
            },
            // No device to wait on. A kernel with getrandom(2) does not need one; on an older
            // one the crate fails at once for the same reason, and pool_seeded says so.
            Err(e) => {
                info!("companion: cannot open {} ({e})", rng_device.display());
                companion.pool_seeded();
            }
        }
        Ok(companion)
    }

    /// Accepts BLE sessions on this Unix socket, once the listeners are up. `omg-bled` drops a
    /// phone that arrives before that, and the phone tries again.
    pub fn set_ble_socket(&mut self, path: PathBuf) {
        if matches!(self.rng, Rng::Ready) {
            self.listeners.bind_ble(&path);
        }
        self.ble_socket = Some(path);
    }

    /// The descriptor to `poll` for `POLLIN` while the link waits for the random pool; then
    /// call [`Companion::on_rng_event`].
    pub fn rng_fd(&self) -> Option<RawFd> {
        match &self.rng {
            Rng::Waiting(wait) => Some(wait.fd()),
            _ => None,
        }
    }

    /// The descriptor from [`Companion::rng_fd`] has an event. Returns the events of a dial that
    /// was waiting for the pool.
    pub fn on_rng_event(&mut self) -> Vec<Event> {
        let Rng::Waiting(wait) = &self.rng else {
            return Vec::new();
        };
        match wait.seeded() {
            Ok(true) => {
                info!(
                    "companion: random pool seeded after {} ms",
                    wait.waited_ms()
                );
                self.pool_seeded();
                return self.dial_pending();
            }
            Ok(false) => {}
            Err(e) => {
                warn!("companion: link disabled: random device failed ({e})");
                self.rng = Rng::Failed;
            }
        }
        Vec::new()
    }

    /// Closes the TCP listener and its connections, or opens it again. While the Glass is its own
    /// access point, TCP would be listening on a network made for something else, where a device
    /// could bond with an unbonded Glass. Connections have nowhere to go then in any case.
    pub fn set_tcp_closed(&mut self, closed: bool) -> Vec<Event> {
        let mut events = Vec::new();
        if self.tcp_closed == closed {
            return events;
        }
        self.tcp_closed = closed;
        if closed {
            let tcp: Vec<ConnId> = self
                .conns
                .iter()
                .filter(|(_, c)| c.transport == Transport::WifiTcp)
                .map(|(id, _)| *id)
                .collect();
            for id in tcp {
                self.drop_conn(id, "closed for Wi-Fi setup", &mut events);
            }
            self.listeners.set_tcp(None);
        } else if matches!(self.rng, Rng::Ready) {
            self.listeners.set_tcp(self.tcp_port);
        }
        events
    }

    /// Whether randomness may be used now: the pool passed the same gate as the identity, and the
    /// `getrandom` crate will not block.
    pub fn rng_ready(&self) -> bool {
        matches!(self.rng, Rng::Ready)
    }

    /// When [`Companion::rng_tick`] should next run (wall clock, ms).
    pub fn rng_next_deadline(&self) -> Option<u64> {
        match self.rng {
            Rng::Gathering { next_check_ms } => Some(next_check_ms),
            _ => None,
        }
    }

    /// When [`Companion::handshake_tick`] should next run ([`crate::daemon::Now::boot_us`], in ms):
    /// the wall clock can be moved, by the phone too, and would keep a silent peer.
    pub fn handshake_next_deadline(&self) -> Option<u64> {
        self.conns.values().filter_map(|c| c.handshake_by_ms).min()
    }

    /// Drops the peers that did not finish the handshake in time.
    pub fn handshake_tick(&mut self) -> Vec<Event> {
        let now = boot_ms();
        let late: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, c)| c.handshake_by_ms.is_some_and(|t| now >= t))
            .map(|(&id, _)| id)
            .collect();
        let mut events = Vec::new();
        for id in late {
            self.drop_conn(id, "no handshake in time", &mut events);
        }
        events
    }

    /// Checks again whether a first identity may be generated. Returns the events of a dial
    /// that was waiting for it.
    pub fn rng_tick(&mut self) -> Vec<Event> {
        let Rng::Gathering { next_check_ms } = self.rng else {
            return Vec::new();
        };
        if now_ms() < next_check_ms {
            return Vec::new();
        }
        self.pool_seeded_step();
        if matches!(self.rng, Rng::Ready) {
            return self.dial_pending();
        }
        Vec::new()
    }

    /// What had to wait for the pool: a first identity, and the listeners.
    fn pool_seeded(&mut self) {
        self.rng = Rng::Failed;
        // After this the crate has /dev/urandom open and never blocks a caller again.
        if let Err(e) = getrandom::getrandom(&mut [0u8; 1]) {
            warn!("companion: link disabled: no randomness ({e})");
            return;
        }
        self.pool_seeded_step();
    }

    /// Brings the link up, unless a first identity needs more entropy.
    fn pool_seeded_step(&mut self) {
        let created = self.store.is_none();
        if self.store.is_none() {
            // A kernel that does not say cannot be waited on; Model B always mounts /proc.
            let bits = entropy::entropy_avail(&self.entropy_avail);
            if !self.seed_used && bits.is_some_and(|b| b < entropy::MIN_BITS_FOR_IDENTITY) {
                if !matches!(self.rng, Rng::Gathering { .. }) {
                    info!(
                        "companion: waiting for {} bits of entropy before generating the identity",
                        entropy::MIN_BITS_FOR_IDENTITY
                    );
                }
                self.rng = Rng::Gathering {
                    next_check_ms: now_ms() + ENTROPY_CHECK_MS,
                };
                return;
            }
            if !self.seed_used && bits.is_none() {
                warn!("companion: cannot read the entropy level; generating the identity anyway");
            }
            self.rng = Rng::Failed;
            match Store::create(&self.store_path) {
                Ok(s) => {
                    info!("companion: generated a new identity");
                    log_bond(&s);
                    self.store = Some(s);
                }
                Err(e) => {
                    warn!("companion: link disabled ({e})");
                    return;
                }
            }
        }
        self.save_next_seed(created);
        let tcp = self.tcp_port.filter(|_| !self.tcp_closed);
        self.listeners = Listeners::bind(tcp, self.bt);
        if let Some(path) = &self.ble_socket {
            self.listeners.bind_ble(path);
        }
        self.rng = Rng::Ready;
    }

    /// Leaves a seed for the next boot, when the pool has passed one of the tests: a seed from a
    /// weak pool would be trusted next time. `created`: the entropy gate just opened.
    fn save_next_seed(&self, created: bool) {
        if !(self.seed_used || created) {
            return;
        }
        if let Err(e) = entropy::save_seed(&self.seed_path) {
            warn!("companion: cannot save the random seed ({e})");
        }
    }

    /// The dial asked for while waiting for the pool.
    fn dial_pending(&mut self) -> Vec<Event> {
        let Some((addr, owner_dial)) = self.pending_dial.take() else {
            return Vec::new();
        };
        self.dial(&addr, owner_dial).unwrap_or_else(|e| {
            warn!("companion: cannot dial {addr} ({e})");
            Vec::new()
        })
    }

    /// Whether a companion is bonded (persisted).
    pub fn paired(&self) -> bool {
        self.bond().is_some()
    }

    /// The bonded companion's name, if any.
    pub fn paired_name(&self) -> Option<&str> {
        self.bond().map(|p| p.peer_name.as_str())
    }

    fn bond(&self) -> Option<&store::PairingRecord> {
        self.store.as_ref().and_then(Store::paired)
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

    /// How much is left of the pairing window, if it is open.
    pub fn pairing_left_ms(&self) -> Option<u64> {
        let left = self.pairing_until_ms?.saturating_sub(boot_ms());
        (left > 0).then_some(left)
    }

    /// When [`Companion::pairing_tick`] should next run ([`crate::daemon::Now::boot_us`], in ms).
    pub fn pairing_next_deadline(&self) -> Option<u64> {
        self.pairing_until_ms
    }

    /// Closes the pairing window if its time is over.
    pub fn pairing_tick(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        self.expire_pairing_window(&mut events);
        events
    }

    fn expire_pairing_window(&mut self, events: &mut Vec<Event>) {
        if self.pairing_until_ms.is_some_and(|t| boot_ms() >= t) {
            info!("companion: the pairing mode timed out");
            self.set_pairing_window(None, events);
        }
    }

    /// The owner entered the pairing mode on the Glass. `duration_ms`: 0 is the default, and it is
    /// limited. Opening it again starts the time again. The error says why it stays closed.
    pub fn open_pairing_window(&mut self, duration_ms: u32) -> Result<Vec<Event>, String> {
        if matches!(self.rng, Rng::Failed) {
            return Err(LINK_DISABLED.into());
        }
        // A bonded Glass refuses any other phone: there is nothing the window could do.
        if self.paired() {
            return Err("already bonded: forget the bond first".into());
        }
        let ms = match duration_ms {
            0 => PAIRING_WINDOW_DEFAULT_MS,
            d => d.min(PAIRING_WINDOW_MAX_MS),
        };
        info!("companion: pairing mode open for {ms} ms");
        let mut events = Vec::new();
        self.set_pairing_window(Some(boot_ms() + ms as u64), &mut events);
        Ok(events)
    }

    /// The owner left the pairing mode.
    pub fn close_pairing_window(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        self.set_pairing_window(None, &mut events);
        events
    }

    fn set_pairing_window(&mut self, until_ms: Option<u64>, events: &mut Vec<Event>) {
        if self.pairing_until_ms == until_ms {
            return;
        }
        self.pairing_until_ms = until_ms;
        // Also for handshakes under way.
        let open = until_ms.is_some();
        for c in self.conns.values_mut() {
            c.link.set_pairing_enabled(open || c.owner_dial);
        }
        events.push(Event::PairingChanged);
    }

    /// Forgets the bonded companion, ends every link, and closes the pairing mode: pairing again
    /// takes the owner opening it. Returns the events, and whether the bond is gone from the disk.
    pub fn unpair(&mut self) -> (Vec<Event>, Result<(), CompanionError>) {
        let had_bond = self.paired();
        // No store yet means no identity, and so no bond to forget.
        let cleared = match &mut self.store {
            Some(store) => store.clear_paired(),
            None => Ok(()),
        };
        if let Err(e) = &cleared {
            warn!("companion: cannot clear the bond ({e})");
        }
        let ids: Vec<ConnId> = self.conns.keys().copied().collect();
        let mut events = Vec::new();
        for id in ids {
            self.drop_conn(id, "unpaired", &mut events);
        }
        self.set_pairing_window(None, &mut events);
        if had_bond {
            info!("companion: the bond was forgotten");
            events.push(Event::PairingChanged);
        }
        (events, cleared)
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
        self.expire_pairing_window(&mut events);
        for stream in self.listeners.accept(transport) {
            if self.conns_full_for(transport) {
                warn!(
                    "companion: too many connections; dropping a new {}",
                    transport.as_str()
                );
                continue;
            }
            self.add_conn(stream, transport, false, &mut events);
        }
        events
    }

    fn conns_full_for(&self, transport: Transport) -> bool {
        if transport == Transport::Ble || self.ble_socket.is_none() {
            return self.conns.len() >= MAX_CONNS;
        }
        let others = self
            .conns
            .values()
            .filter(|c| c.transport != Transport::Ble)
            .count();
        others >= MAX_CONNS - BLE_RESERVED_CONNS
    }

    /// Dials a companion over TCP (the QR bootstrap connect path), then starts the handshake.
    /// The handshake needs randomness, so before the pool is seeded the dial waits for it.
    /// Pairing on it takes the pairing mode, as on any other connection.
    pub fn connect_tcp(&mut self, addr: &str) -> io::Result<Vec<Event>> {
        self.dial(addr, false)
    }

    /// [`Companion::connect_tcp`] for the owner's QR bootstrap: the connection it makes may pair,
    /// and no other, so the pairing mode stays closed for everyone else. A dial that waits for the
    /// random pool keeps this, however long the wait.
    pub fn dial_to_pair(&mut self, addr: &str) -> io::Result<Vec<Event>> {
        self.dial(addr, true)
    }

    fn dial(&mut self, addr: &str, owner_dial: bool) -> io::Result<Vec<Event>> {
        match self.rng {
            Rng::Waiting(_) | Rng::Gathering { .. } => {
                info!("companion: will dial {addr} once the random pool is seeded");
                self.pending_dial = Some((addr.to_owned(), owner_dial));
                return Ok(Vec::new());
            }
            Rng::Failed => return Err(io::Error::other("the link is disabled")),
            Rng::Ready => {}
        }
        let stream = TcpStream::connect(addr)?;
        stream.set_nonblocking(true)?;
        let _ = stream.set_nodelay(true);
        info!("companion: dialed {addr}");
        let mut events = Vec::new();
        self.add_conn(
            Box::new(stream) as BoxStream,
            Transport::WifiTcp,
            owner_dial,
            &mut events,
        );
        Ok(events)
    }

    fn add_conn(
        &mut self,
        stream: BoxStream,
        transport: Transport,
        owner_dial: bool,
        events: &mut Vec<Event>,
    ) {
        // Not reachable: a connection needs Rng::Ready, and that needs a store.
        let Some(store) = &self.store else {
            return;
        };
        let identity = store.identity().clone();
        let bond = store.paired().cloned();
        let id = self.next_id;
        self.next_id += 1;
        let mut link = Link::new(identity, bond, self.device.clone());
        link.set_pairing_enabled(self.pairing_until_ms.is_some() || owner_dial);
        if self.notes.is_some() {
            link.advertise(omg_protocol::capability::NOTES_SYNC);
        }
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
                handshake_by_ms: Some(boot_ms() + HANDSHAKE_MS),
                owner_dial,
            },
        );
        // Drive the Hello that start() queued against the now-inserted connection.
        self.drive(id, outs, events);
    }

    /// A connection is readable: read, decode, and drive the link.
    pub fn on_readable(&mut self, id: ConnId) -> Vec<Event> {
        let mut events = Vec::new();
        // The timer may not have had its turn yet, right after a resume.
        self.expire_pairing_window(&mut events);
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

    /// Whether notes can be taken here.
    pub fn notes_available(&self) -> bool {
        self.notes.is_some()
    }

    /// A client added a note. It stays on the Glass until a companion has a verified copy, with
    /// or without a link right now.
    pub fn note_add(&mut self, add: glass_api::proto::NoteAdd) -> Vec<Event> {
        let locale = self.device.locale;
        let mut events = Vec::new();
        if let Some(n) = &mut self.notes {
            let refused = n.add(add, locale);
            events.extend(refused.into_iter().map(Event::Note));
            events.extend(n.take_events().into_iter().map(Event::Note));
        }
        events
    }

    /// A client asked the companion to read a note aloud.
    pub fn note_speak(&mut self, speak: glass_api::proto::NoteSpeak) -> Vec<Event> {
        let locale = self.device.locale;
        let Some(n) = &mut self.notes else {
            return Vec::new();
        };
        let linked = self.notes_link;
        let result = n.speak(speak, locale, linked, now_ms());
        self.note_frames(result)
    }

    /// When note sync next needs [`notes_tick`](Companion::notes_tick), in wall-clock ms.
    pub fn notes_next_deadline(&self) -> Option<u64> {
        // A seal cannot end before the pool is seeded, so there is no point in checking on it.
        let seals_can_end = !matches!(self.rng, Rng::Waiting(_));
        self.notes
            .as_ref()
            .and_then(|n| n.next_deadline(now_ms(), seals_can_end))
    }

    /// Time passed, or a seal finished on its thread.
    pub fn notes_tick(&mut self) -> Vec<Event> {
        let Some(n) = &mut self.notes else {
            return Vec::new();
        };
        let result = n.tick(now_ms());
        self.note_frames(result)
    }

    /// Queues what the note machine produced on the live link and reports its events. A store
    /// failure counts as a lost link, as the driver asks: the state on disk is what counts, and
    /// the next link starts from it.
    fn note_frames(
        &mut self,
        result: Result<Vec<omg_protocol::Frame>, omg_core::CoreError>,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        let active = self.active;
        match result {
            Ok(frames) => {
                if let Some(id) = active {
                    self.queue_frames(id, frames);
                    self.flush(id, &mut events);
                }
            }
            Err(e) => {
                warn!("notes: {e}");
                if let Some(id) = active {
                    self.drop_conn(id, "note store failed", &mut events);
                }
            }
        }
        if let Some(n) = &mut self.notes {
            events.extend(n.take_events().into_iter().map(Event::Note));
        }
        events
    }

    fn queue_frames(&mut self, id: ConnId, frames: Vec<omg_protocol::Frame>) {
        let Some(c) = self.conns.get_mut(&id) else {
            return;
        };
        for frame in frames {
            match c.link.encode(&frame) {
                Ok(bytes) => c.out.extend_from_slice(&bytes),
                Err(e) => warn!("companion {id}: cannot encode {}: {e}", frame.wire_name()),
            }
        }
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
                Out::Send(frame) => {
                    if let Some(c) = self.conns.get_mut(&id) {
                        match c.link.encode(&frame) {
                            Ok(bytes) => c.out.extend_from_slice(&bytes),
                            Err(e) => {
                                warn!("companion {id}: cannot encode {}: {e}", frame.wire_name())
                            }
                        }
                    }
                }
                Out::Connected { name, capabilities } => {
                    let transport = self
                        .conns
                        .get(&id)
                        .map(|c| c.transport)
                        .unwrap_or(Transport::WifiTcp);
                    if let Some(c) = self.conns.get_mut(&id) {
                        c.handshake_by_ms = None;
                    }
                    // Only one active companion: close any other live link.
                    let others: Vec<ConnId> =
                        self.conns.keys().copied().filter(|&o| o != id).collect();
                    for o in others {
                        self.drop_conn(o, "another companion connected", events);
                    }
                    self.active = Some(id);
                    info!("companion: {name:?} connected over {}", transport.as_str());
                    self.notes_link = capabilities
                        .iter()
                        .any(|c| c == omg_protocol::capability::NOTES_SYNC);
                    if self.notes_link
                        && let Some(n) = &mut self.notes
                    {
                        let result = n.link_up(notes::profile_for(transport), now_ms());
                        events.extend(self.note_frames(result));
                    }
                    events.push(Event::Connected {
                        name,
                        capabilities,
                        transport,
                    });
                }
                Out::Persist(rec) => {
                    // This link was made before the bond was. The first one stays.
                    if self.bond().is_some_and(|b| b.peer_pub != rec.peer_pub) {
                        warn!("companion {id}: refusing a second bond");
                        let refusal = Frame::Error(ErrorMsg {
                            code: ErrorCode::UnknownPeer,
                            message: "already bonded to another companion".into(),
                        });
                        self.queue_frames(id, vec![refusal]);
                        self.flush(id, events);
                        self.drop_conn(id, "already bonded", events);
                        continue;
                    }
                    let saved = match &mut self.store {
                        Some(store) => store.set_paired(rec.clone()),
                        None => Err(CompanionError::Store("no store".into())),
                    };
                    if let Err(e) = saved {
                        // A bond that would not outlive a restart is no bond: the phone would keep
                        // one the Glass forgets, and the window would stay open behind it.
                        warn!("companion: cannot persist the bond ({e})");
                        let refusal = Frame::Error(ErrorMsg {
                            code: ErrorCode::Internal,
                            message: "the Glass cannot store the bond".into(),
                        });
                        self.queue_frames(id, vec![refusal]);
                        self.flush(id, events);
                        self.drop_conn(id, "the bond could not be stored", events);
                        self.set_pairing_window(None, events);
                        continue;
                    } else {
                        info!("companion: paired with {:?}", rec.peer_name);
                        // One phone per opening.
                        self.set_pairing_window(None, events);
                        events.push(Event::PairingChanged);
                    }
                }
                Out::Unpair => {
                    if let Some(store) = &mut self.store
                        && let Err(e) = store.clear_paired()
                    {
                        warn!("companion: cannot clear the bond ({e})");
                    }
                    events.push(Event::PairingChanged);
                }
                Out::ApplyLocale(l) => events.push(Event::ApplyLocale(l)),
                Out::SettingsGet => events.push(Event::SettingsGet),
                Out::SettingsSet(s) => events.push(Event::SettingsSet(s)),
                Out::Note(frame) => {
                    let backlog = self.conns.get(&id).map_or(0, |c| c.out.len());
                    if notes::over_backlog(&frame, backlog) {
                        continue;
                    }
                    if self.active == Some(id)
                        && self.notes_link
                        && let Some(n) = &mut self.notes
                    {
                        let result = n.on_frame(frame, now_ms());
                        events.extend(self.note_frames(result));
                    }
                }
                Out::Notification(n) => events.push(Event::Notification(n)),
                Out::RemoveNotification(k) => events.push(Event::RemoveNotification(k)),
                Out::SetRadio { radio, on } => events.push(Event::SetRadio { radio, on }),
                Out::SetAirplane(on) => events.push(Event::SetAirplane(on)),
                Out::WifiBringUp(r) => events.push(Event::WifiBringUp(r)),
                Out::WifiTearDown => events.push(Event::WifiTearDown),
                Out::Close(reason) => {
                    // What the link queued first is the reason it closes: an Error frame.
                    self.flush(id, events);
                    self.drop_conn(id, &reason, events);
                }
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
            self.notes_link = false;
            if let Some(n) = &mut self.notes {
                n.link_down();
                events.extend(n.take_events().into_iter().map(Event::Note));
            }
            events.push(Event::Disconnected);
        }
    }
}

fn log_bond(store: &Store) {
    match store.paired() {
        Some(p) => info!("companion: bonded to {:?}", p.peer_name),
        None => info!("companion: not yet paired"),
    }
}

/// The companion state directory file (for the daemon to locate the bond).
pub fn state_file(dir: &Path) -> PathBuf {
    Store::path_in(dir)
}

#[cfg(test)]
mod ble_e2e;
#[cfg(test)]
mod crypto_e2e;
#[cfg(test)]
mod e2e;
#[cfg(test)]
mod entropy_tests;
#[cfg(test)]
mod notes_e2e;
