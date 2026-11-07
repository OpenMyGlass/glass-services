//! The whole BLE path without a radio: the real [`Companion`] on a real Unix socket, the real
//! `omg-bled` engine, a fake controller that holds it to its credits, GATT-sized runs each way,
//! and what the phone apps run at the far end. The encrypted handshake and note sync on top
//! cannot tell this from TCP, which is the point.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use glass_api::proto::{NoteAdd, NoteAttachment, NoteEvent, note_attachment, note_event};
use omg_bled::engine::{Action, Engine};
use omg_bled::fake::{ACL_LEN, CREDITS, FakeController};
use omg_core::notes::{CompanionNotes, LinkProfile, RetryPolicy};
use omg_core::{CompanionLink, Identity, LinkOut, PairingMode, PairingRecord};
use omg_protocol::{Frame, FrameDecoder, capability};

use super::link::DeviceState;
use super::transport::Transport;
use super::{Companion, Event};

/// What the phone apps pull with over BLE.
const PHONE_BLE: LinkProfile = LinkProfile {
    chunk_len: 1024,
    window: 2,
    max_auto_attachment_bytes: Some(256 * 1024),
};
/// A run of bytes in a write or a notification, at the MTU the prototype measured.
const RUN: usize = 244;

/// The engine and the radio, driven as `omg-bled`'s loop drives them.
struct Air {
    engine: Engine,
    radio: FakeController,
    session: Option<UnixStream>,
    socket: PathBuf,
    now: u64,
}

impl Air {
    fn new(socket: PathBuf) -> Air {
        let mut air = Air {
            engine: Engine::new("t", [1, 2, 3, 4, 5, 0xC6], ACL_LEN, CREDITS, 0),
            radio: FakeController::new(),
            session: None,
            socket,
            now: 0,
        };
        air.uart();
        assert!(air.engine.started());
        air
    }

    fn uart(&mut self) {
        loop {
            let out = self.engine.hci_output().to_vec();
            self.engine.hci_written(out.len());
            self.radio.from_host(&out);
            let back = self.radio.take_to_host();
            if !back.is_empty() {
                self.engine.on_hci(&back, self.now);
            }
            for action in self.engine.take_actions() {
                match action {
                    Action::OpenSession => match UnixStream::connect(&self.socket) {
                        Ok(s) => {
                            s.set_nonblocking(true).unwrap();
                            self.session = Some(s);
                            self.engine.session_opened(self.now);
                        }
                        Err(_) => self.engine.session_failed(),
                    },
                    Action::CloseSession => self.session = None,
                }
            }
            assert_eq!(self.engine.fatal(), None);
            if out.is_empty() && back.is_empty() && self.engine.hci_output().is_empty() {
                return;
            }
        }
    }

    /// One pass of the loop: the stream both ways, then a connection event.
    fn step(&mut self, pdus: usize) {
        let mut closed = false;
        if let Some(s) = &mut self.session {
            if !self.engine.to_session().is_empty() {
                match s.write(self.engine.to_session()) {
                    Ok(n) => self.engine.to_session_written(n),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(_) => closed = true,
                }
            }
            let room = self.engine.session_room();
            if room > 0 {
                let mut buf = vec![0u8; room];
                match s.read(&mut buf) {
                    Ok(0) => closed = true,
                    Ok(n) => self.engine.session_data(&buf[..n], self.now),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(_) => closed = true,
                }
            }
        }
        if closed {
            self.session = None;
            self.engine.session_closed(self.now);
        }
        self.uart();
        self.radio.air(pdus);
        self.uart();
    }
}

/// The phone end, as an app runs it: the same core, and bytes to and from a `ByteLink`.
struct Phone {
    decoder: FrameDecoder,
    link: CompanionLink,
    notes: CompanionNotes,
    bond: Option<PairingRecord>,
    profile: LinkProfile,
    notes_up: bool,
    /// What `ByteLink.write` still has to send, paced into runs.
    to_write: Vec<u8>,
    received: u64,
    chunks: u32,
    battery_seen_at: Option<u64>,
    running: bool,
}

impl Phone {
    fn new(
        dir: &Path,
        identity: Identity,
        bond: Option<PairingRecord>,
        profile: LinkProfile,
    ) -> Phone {
        let link = CompanionLink::new(
            identity,
            bond.clone(),
            PairingMode::AllowNew,
            "phone",
            "test",
            vec![capability::NOTES_SYNC.to_string()],
        );
        let mut phone = Phone {
            decoder: FrameDecoder::new(),
            link,
            notes: CompanionNotes::open(dir, RetryPolicy::default()).unwrap(),
            bond,
            profile,
            notes_up: false,
            to_write: Vec::new(),
            received: 0,
            chunks: 0,
            battery_seen_at: None,
            running: false,
        };
        let start = phone.link.start();
        phone.carry_out(start);
        phone
    }

    fn send(&mut self, frame: &Frame) {
        let bytes = self.link.encode(frame).unwrap();
        self.to_write.extend_from_slice(&bytes);
    }

    fn carry_out(&mut self, outs: Vec<LinkOut>) {
        for out in outs {
            match out {
                LinkOut::Send(frame) => self.send(&frame),
                LinkOut::Persist(rec) => self.bond = Some(rec),
                LinkOut::Running { capabilities, .. } => {
                    self.running = true;
                    if capabilities.iter().any(|c| c == capability::NOTES_SYNC) {
                        self.notes_up = true;
                        let frames = self.notes.link_up(self.profile, 0).unwrap();
                        frames.iter().for_each(|f| self.send(f));
                    }
                }
                LinkOut::App(Frame::BatteryStatus(_)) => {
                    self.battery_seen_at = Some(self.received);
                }
                LinkOut::App(frame) => {
                    if matches!(frame, Frame::NoteChunk(_)) {
                        self.chunks += 1;
                    }
                    if omg_core::notes::is_note_frame(&frame) {
                        let frames = self.notes.on_frame(frame, omg_core::now_ms()).unwrap();
                        frames.iter().for_each(|f| self.send(f));
                    }
                }
                LinkOut::Closed(end) => assert_eq!(end, Ok(())),
            }
        }
    }

    fn on_air(&mut self, air: &mut Air) {
        // What pacing does for real: never a long queue in the phone's own stack.
        while !self.to_write.is_empty() && air.radio.phone_backlog() < 20 {
            let n = self.to_write.len().min(RUN.min(air.radio.mtu as usize - 3));
            let run: Vec<u8> = self.to_write.drain(..n).collect();
            air.radio.phone_write(&run);
        }
        let stream = air.radio.take_stream();
        self.received += stream.len() as u64;
        self.decoder.push(&stream);
        while let Some(frame) = self.decoder.next_frame().unwrap() {
            let outs = self.link.on_frame(frame);
            self.carry_out(outs);
        }
    }
}

struct Bench {
    glass: Companion,
    air: Air,
    events: Vec<NoteEvent>,
    disconnects: u32,
    connected_over: Option<Transport>,
    _tmp: tempfile::TempDir,
    root: PathBuf,
    identity: Identity,
}

impl Bench {
    fn new() -> Bench {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let socket = root.join("ble.sock");
        let mut glass =
            Companion::new(&root.join("g"), DeviceState::default(), None, false).unwrap();
        glass.set_ble_socket(socket.clone());
        assert_eq!(glass.listener_fds().len(), 1);
        Bench {
            glass,
            air: Air::new(socket),
            events: Vec::new(),
            disconnects: 0,
            connected_over: None,
            _tmp: tmp,
            root,
            identity: Identity::generate(),
        }
    }

    fn take(&mut self, events: Vec<Event>) {
        for e in events {
            match e {
                Event::Note(n) => self.events.push(n),
                Event::Disconnected => self.disconnects += 1,
                Event::Connected { transport, .. } => self.connected_over = Some(transport),
                _ => {}
            }
        }
    }

    fn glass_step(&mut self) {
        let events = self.glass.accept(Transport::Ble);
        self.take(events);
        let ids: Vec<_> = self.glass.conn_fds().iter().map(|c| c.0).collect();
        for id in ids {
            let events = self.glass.on_writable(id);
            self.take(events);
            let events = self.glass.on_readable(id);
            self.take(events);
        }
        let events = self.glass.notes_tick();
        self.take(events);
    }

    fn step(&mut self, phone: &mut Phone) {
        self.glass_step();
        self.air.step(8);
        phone.on_air(&mut self.air);
    }

    fn until(
        &mut self,
        phone: &mut Phone,
        what: &str,
        mut done: impl FnMut(&Bench, &Phone) -> bool,
    ) {
        let start = Instant::now();
        while !done(self, phone) {
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "timed out: {what}"
            );
            self.step(phone);
        }
    }

    /// The phone scans, connects, asks for the MTU and subscribes, in the order `BleLink` does.
    fn phone_connects(&mut self, bond: Option<PairingRecord>, profile: LinkProfile) -> Phone {
        self.air.radio.phone_connects();
        self.air.uart();
        self.air.radio.phone_exchange_mtu(517);
        self.air.radio.phone_subscribe(true);
        for _ in 0..50 {
            self.air.step(8);
        }
        assert_eq!(self.air.radio.mtu, 247);
        Phone::new(&self.root.join("p"), self.identity.clone(), bond, profile)
    }

    fn add_note(&mut self, kinds: &[(note_attachment::Kind, &str, &[u8])]) {
        let attachments = kinds
            .iter()
            .enumerate()
            .map(|(i, (kind, mime, data))| {
                let path = self.root.join(format!("source-{}-{i}", self.events.len()));
                std::fs::write(&path, data).unwrap();
                NoteAttachment {
                    kind: *kind as i32,
                    mime: (*mime).into(),
                    path: path.to_str().unwrap().into(),
                    duration_ms: None,
                }
            })
            .collect();
        let events = self.glass.note_add(NoteAdd {
            request_id: 1,
            text: Some("appunto".into()),
            locale: glass_api::proto::Locale::It as i32,
            attachments,
        });
        self.take(events);
        let start = Instant::now();
        let before = self.count(note_event::Kind::Added);
        while self.count(note_event::Kind::Added) == before
            && self.count(note_event::Kind::Added) == 0
        {
            assert!(start.elapsed() < Duration::from_secs(20), "the seal");
            assert_eq!(self.count(note_event::Kind::AddFailed), 0);
            let events = self.glass.notes_tick();
            self.take(events);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn count(&self, kind: note_event::Kind) -> usize {
        self.events.iter().filter(|e| e.kind == kind as i32).count()
    }

    fn queued_on_glass(&self) -> usize {
        std::fs::read_dir(self.root.join("g/notes"))
            .unwrap()
            .filter(|e| {
                !e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with('.')
            })
            .count()
    }
}

/// Bytes that differ everywhere, so a wrong offset cannot go unnoticed.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31) ^ seed ^ (i >> 8) as u8)
        .collect()
}

fn pair(b: &mut Bench) -> Phone {
    let mut phone = b.phone_connects(None, PHONE_BLE);
    b.until(&mut phone, "the handshake", |b, p| {
        p.running && b.glass.connected()
    });
    assert_eq!(b.glass.transport(), Some(Transport::Ble));
    assert_eq!(b.connected_over, Some(Transport::Ble));
    assert!(phone.bond.is_some());
    assert!(b.glass.paired());
    phone
}

#[test]
fn pairing_runs_over_the_chunked_link_and_a_reconnect_authenticates_on_a_new_session() {
    let mut b = Bench::new();
    let phone = pair(&mut b);
    assert!(b.air.radio.max_outstanding <= CREDITS as usize);

    let first = b.air.session.as_ref().map(|_| ()).is_some();
    assert!(first);
    b.air.radio.disconnect(0x13);
    b.air.step(8);
    assert!(b.air.session.is_none());
    b.glass_step();
    assert_eq!(b.disconnects, 1);
    assert!(!b.glass.connected());
    assert!(b.glass.paired(), "the bond outlives the link");
    assert!(b.air.radio.advertising);

    let mut phone = b.phone_connects(phone.bond, PHONE_BLE);
    b.until(&mut phone, "the second handshake", |b, p| {
        p.running && b.glass.connected()
    });
}

#[test]
fn a_note_with_a_voice_recording_syncs_over_ble_and_leaves_the_glass() {
    let mut b = Bench::new();
    let voice = pattern(60_000, 1);
    b.add_note(&[(note_attachment::Kind::Voice, "audio/wav", &voice)]);
    assert_eq!(b.queued_on_glass(), 1);
    let mut phone = pair(&mut b);
    b.until(&mut phone, "the sync", |b, _| {
        b.count(note_event::Kind::Synced) == 1
    });
    assert_eq!(b.queued_on_glass(), 0);
    let note = phone.notes.machine().notes().next().unwrap().clone();
    assert!(note.complete);
    let on_phone = phone.notes.attachment_path(&note.manifest.id, 0);
    assert_eq!(std::fs::read(on_phone).unwrap(), voice);
    // At 1 KiB a chunk.
    assert!(phone.chunks >= 59, "{}", phone.chunks);
    assert!(b.air.radio.max_outstanding <= CREDITS as usize);
}

#[test]
fn a_lost_link_mid_transfer_resumes_where_it_was() {
    let mut b = Bench::new();
    let voice = pattern(100_000, 2);
    b.add_note(&[(note_attachment::Kind::Voice, "audio/wav", &voice)]);
    let mut phone = pair(&mut b);
    b.until(&mut phone, "part of the transfer", |_, p| p.chunks >= 40);
    let chunks_before = phone.chunks;
    b.air.radio.disconnect(0x08);
    // What the phone's core does at the end of a link: what arrived becomes durable.
    phone.notes.link_down(omg_core::now_ms()).unwrap();
    for _ in 0..3 {
        b.air.step(8);
        b.glass_step();
    }
    assert_eq!(b.disconnects, 1);
    assert_eq!(b.count(note_event::Kind::Synced), 0);
    assert_eq!(b.queued_on_glass(), 1);
    let bond = phone.bond.clone();
    drop(phone);

    let mut phone = b.phone_connects(bond, PHONE_BLE);
    b.until(&mut phone, "the rest", |b, _| {
        b.count(note_event::Kind::Synced) == 1
    });
    assert!(
        (phone.chunks as usize) < 100 - chunks_before as usize + 5,
        "started again: {} chunks after {chunks_before}",
        phone.chunks
    );
    let note = phone.notes.machine().notes().next().unwrap().clone();
    let on_phone = phone.notes.attachment_path(&note.manifest.id, 0);
    assert_eq!(std::fs::read(on_phone).unwrap(), voice);
    assert_eq!(b.queued_on_glass(), 0);
}

/// The rule: a note and what fits the limit go over BLE, and the rest waits on the Glass. A link
/// with no limit picks it up, and a 48 KiB chunk crosses the runs intact, if slowly.
#[test]
fn an_attachment_over_the_limit_waits_and_a_faster_profile_fetches_it_with_large_chunks() {
    let mut b = Bench::new();
    let voice = pattern(10_000, 3);
    let video = pattern(300 * 1024, 4);
    b.add_note(&[
        (note_attachment::Kind::Voice, "audio/wav", &voice),
        (note_attachment::Kind::Video, "video/mp4", &video),
    ]);
    let mut phone = pair(&mut b);
    b.until(&mut phone, "the deferral", |b, _| {
        b.count(note_event::Kind::Deferred) == 1
    });
    assert_eq!(b.count(note_event::Kind::Synced), 0);
    assert_eq!(b.queued_on_glass(), 1);
    let note = phone.notes.machine().notes().next().unwrap().clone();
    assert!(!note.complete);
    let voice_path = phone.notes.attachment_path(&note.manifest.id, 0);
    assert_eq!(std::fs::read(voice_path).unwrap(), voice);
    let bond = phone.bond.clone();
    phone.notes.link_down(omg_core::now_ms()).unwrap();
    drop(phone);
    b.air.radio.disconnect(0x13);
    for _ in 0..3 {
        b.air.step(8);
        b.glass_step();
    }

    // In the field this is the Wi-Fi link. Here the profile alone shows that the choice is the
    // phone's, and the Glass re-offers on any new link.
    let mut phone = b.phone_connects(bond, LinkProfile::fast());
    b.until(&mut phone, "the video", |b, _| {
        b.count(note_event::Kind::Synced) == 1
    });
    let note = phone.notes.machine().notes().next().unwrap().clone();
    let video_path = phone.notes.attachment_path(&note.manifest.id, 1);
    assert_eq!(std::fs::read(video_path).unwrap(), video);
    assert_eq!(b.queued_on_glass(), 0);
}

/// Why chunks are small: a frame sent during a transfer does not wait behind a long queue.
#[test]
fn a_status_frame_overtakes_all_but_a_little_of_a_running_transfer() {
    let mut b = Bench::new();
    let voice = pattern(100_000, 5);
    b.add_note(&[(note_attachment::Kind::Voice, "audio/wav", &voice)]);
    let mut phone = pair(&mut b);
    b.until(&mut phone, "the transfer to run", |_, p| p.chunks >= 10);
    let at = phone.received;
    let events = b.glass.battery_changed(50, false);
    b.take(events);
    b.until(&mut phone, "the status", |_, p| p.battery_seen_at.is_some());
    let behind = phone.battery_seen_at.unwrap() - at;
    // Two chunks, and what the engine and the controller held. At 3 kB/s, about a second.
    assert!(behind <= 4096, "{behind} bytes ahead of it");
}

#[test]
fn a_peer_that_says_nothing_is_dropped_and_advertising_comes_back() {
    let mut b = Bench::new();
    assert_eq!(b.glass.handshake_next_deadline(), None);
    b.air.radio.phone_connects();
    b.air.uart();
    b.air.radio.phone_subscribe(true);
    for _ in 0..50 {
        b.air.step(8);
    }
    b.glass_step();
    let at = b.glass.handshake_next_deadline().unwrap();
    assert!(at > super::now_ms());
    assert!(b.glass.handshake_tick().is_empty());
    for c in b.glass.conns.values_mut() {
        c.handshake_by_ms = Some(0);
    }
    assert!(b.glass.handshake_tick().is_empty(), "it was no live link");
    assert_eq!(b.glass.conn_fds().len(), 0);
    assert_eq!(b.glass.handshake_next_deadline(), None);
    // The engine sees the socket close and lets the phone go.
    for _ in 0..50 {
        b.air.step(8);
    }
    assert!(!b.air.radio.connected());
    assert!(b.air.radio.advertising);
}

#[test]
fn a_finished_handshake_has_no_deadline_left() {
    let mut b = Bench::new();
    let _phone = pair(&mut b);
    assert_eq!(b.glass.handshake_next_deadline(), None);
}

/// A Glass with TCP and BLE, and how to reach each.
fn glass_with_tcp_and_ble(root: &Path) -> (Companion, u16, PathBuf) {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let socket = root.join("ble.sock");
    let mut glass =
        Companion::new(&root.join("g"), DeviceState::default(), Some(port), false).unwrap();
    glass.set_ble_socket(socket.clone());
    (glass, port, socket)
}

/// Accepts until `want` connections are there, or says what it got.
fn accept_until(glass: &mut Companion, transport: Transport, want: usize) {
    let start = Instant::now();
    while glass.conn_fds().len() < want && start.elapsed() < Duration::from_secs(5) {
        glass.accept(transport);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(glass.conn_fds().len(), want);
}

#[test]
fn idle_tcp_peers_cannot_lock_the_phone_out_of_ble() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut glass, port, socket) = glass_with_tcp_and_ble(tmp.path());
    let _peers: Vec<_> = (0..5)
        .map(|_| std::net::TcpStream::connect(("127.0.0.1", port)).unwrap())
        .collect();
    accept_until(&mut glass, Transport::WifiTcp, 3);
    // The other two knocked and were turned away.
    for _ in 0..50 {
        glass.accept(Transport::WifiTcp);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(glass.conn_fds().len(), 3);

    let _phone = UnixStream::connect(&socket).unwrap();
    accept_until(&mut glass, Transport::Ble, 4);
}

#[test]
fn without_ble_tcp_keeps_all_its_connections() {
    let tmp = tempfile::tempdir().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut glass = Companion::new(
        &tmp.path().join("g"),
        DeviceState::default(),
        Some(port),
        false,
    )
    .unwrap();
    let _peers: Vec<_> = (0..4)
        .map(|_| std::net::TcpStream::connect(("127.0.0.1", port)).unwrap())
        .collect();
    accept_until(&mut glass, Transport::WifiTcp, 4);
}

#[test]
fn a_tcp_peer_that_says_nothing_is_dropped_too() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut glass, port, _socket) = glass_with_tcp_and_ble(tmp.path());
    let _peer = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    accept_until(&mut glass, Transport::WifiTcp, 1);
    assert!(glass.handshake_next_deadline().unwrap() > super::now_ms());
    assert!(glass.handshake_tick().is_empty());
    assert_eq!(glass.conn_fds().len(), 1, "before its time");
    for c in glass.conns.values_mut() {
        c.handshake_by_ms = Some(0);
    }
    assert!(glass.handshake_tick().is_empty());
    assert_eq!(glass.conn_fds().len(), 0);
    assert_eq!(glass.handshake_next_deadline(), None);
}

#[test]
fn a_dial_has_the_same_deadline() {
    let tmp = tempfile::tempdir().unwrap();
    let mut glass =
        Companion::new(&tmp.path().join("g"), DeviceState::default(), None, false).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    glass
        .connect_tcp(&listener.local_addr().unwrap().to_string())
        .unwrap();
    assert!(glass.handshake_next_deadline().is_some());
}
