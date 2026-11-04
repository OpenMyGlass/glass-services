//! Note sync through the real [`Companion`] service over real TCP. The other side is what the
//! phone apps run: `omg-core`'s synchronous handshake and note driver on a plain socket.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use glass_api::proto::{
    NoteAdd, NoteAttachment, NoteEvent, NoteSpeak, note_attachment, note_event,
};
use omg_core::notes::{CompanionNotes, LinkProfile, RetryPolicy, SpeechJob, SpeechOutcome};
use omg_core::{CompanionLink, Identity, LinkOut, PairingMode, PairingRecord};
use omg_protocol::{Frame, FrameDecoder, capability, frame_bytes};

use super::link::DeviceState;
use super::{Companion, ConnId, Event};

fn glass(dir: &Path) -> Companion {
    Companion::new(dir, DeviceState::default(), None, false).unwrap()
}

/// The phone: handshake and note driver on a socket it owns, as an app would.
struct Phone {
    socket: TcpStream,
    decoder: FrameDecoder,
    link: CompanionLink,
    notes: CompanionNotes,
    bond: Option<PairingRecord>,
    capabilities: Vec<String>,
    settings_seen: bool,
    jobs: Vec<SpeechJob>,
    notes_up: bool,
}

impl Phone {
    fn new(
        socket: TcpStream,
        dir: &Path,
        identity: Identity,
        bond: Option<PairingRecord>,
        capabilities: Vec<String>,
    ) -> Phone {
        socket.set_nonblocking(true).unwrap();
        let link = CompanionLink::new(
            identity,
            bond.clone(),
            PairingMode::AllowNew,
            "phone",
            "test",
            capabilities.clone(),
        );
        let mut phone = Phone {
            socket,
            decoder: FrameDecoder::new(),
            link,
            notes: CompanionNotes::open(dir, RetryPolicy::default()).unwrap(),
            bond,
            capabilities,
            settings_seen: false,
            jobs: Vec::new(),
            notes_up: false,
        };
        let start = phone.link.start();
        phone.carry_out(start);
        phone
    }

    fn send(&mut self, frame: &Frame) {
        let bytes = frame_bytes(frame).unwrap();
        let mut written = 0;
        // Nonblocking, but the kernel buffers hold all these small test frames.
        while written < bytes.len() {
            match self.socket.write(&bytes[written..]) {
                Ok(n) => written += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                Err(_) => return,
            }
        }
    }

    fn carry_out(&mut self, outs: Vec<LinkOut>) {
        for out in outs {
            match out {
                LinkOut::Send(frame) => self.send(&frame),
                LinkOut::Persist(rec) => self.bond = Some(rec),
                LinkOut::Running { capabilities, .. } => {
                    if capabilities.iter().any(|c| c == capability::NOTES_SYNC)
                        && self
                            .capabilities
                            .iter()
                            .any(|c| c == capability::NOTES_SYNC)
                    {
                        self.notes_up = true;
                        let frames = self.notes.link_up(LinkProfile::fast(), 0).unwrap();
                        frames.iter().for_each(|f| self.send(f));
                    }
                }
                LinkOut::App(Frame::Settings(_)) => self.settings_seen = true,
                LinkOut::App(frame) => {
                    if omg_core::notes::is_note_frame(&frame) {
                        assert!(self.notes_up, "a note frame reached a phone without notes");
                        let frames = self.notes.on_frame(frame, omg_core::now_ms()).unwrap();
                        frames.iter().for_each(|f| self.send(f));
                        self.jobs.extend(self.notes.take_speech_jobs());
                    }
                }
                LinkOut::Closed(end) => assert_eq!(end, Ok(())),
            }
        }
    }

    fn pump(&mut self) {
        let mut buf = [0u8; 65536];
        loop {
            match self.socket.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => self.decoder.push(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => return,
            }
        }
        while let Some(frame) = self.decoder.next_frame().unwrap() {
            let outs = self.link.on_frame(frame);
            self.carry_out(outs);
        }
    }

    /// Runs the speech jobs with canned engines.
    fn run_speech(&mut self) {
        for job in std::mem::take(&mut self.jobs) {
            let outcome = match job {
                SpeechJob::Transcribe(r) => SpeechOutcome::Transcribed {
                    note_id: r.note_id,
                    attachment_id: r.attachment_id,
                    result: Ok("trascritto".to_string()),
                },
                SpeechJob::Synthesize {
                    request_id,
                    request,
                } => SpeechOutcome::Synthesized {
                    request_id,
                    note_id: request.note_id,
                    result: Ok(omg_core::notes::SynthesizedAudio {
                        sample_rate: 16_000,
                        pcm: vec![7; 100_000],
                    }),
                },
            };
            let frames = self
                .notes
                .complete_speech(outcome, omg_core::now_ms())
                .unwrap();
            frames.iter().for_each(|f| self.send(f));
        }
    }
}

/// A Glass dialing a phone, both pumped from this one thread.
struct Bench {
    glass: Companion,
    listener: TcpListener,
    events: Vec<NoteEvent>,
    disconnected: bool,
}

impl Bench {
    fn new(dir: &Path) -> Bench {
        Bench {
            glass: glass(dir),
            listener: TcpListener::bind("127.0.0.1:0").unwrap(),
            events: Vec::new(),
            disconnected: false,
        }
    }

    fn take(&mut self, events: Vec<Event>) {
        for e in events {
            match e {
                Event::Note(n) => self.events.push(n),
                Event::Disconnected => self.disconnected = true,
                _ => {}
            }
        }
    }

    fn connect(
        &mut self,
        dir: &Path,
        identity: Identity,
        bond: Option<PairingRecord>,
        capabilities: Vec<String>,
    ) -> Phone {
        let addr = self.listener.local_addr().unwrap().to_string();
        let events = self.glass.connect_tcp(&addr).unwrap();
        self.take(events);
        let (socket, _) = self.listener.accept().unwrap();
        Phone::new(socket, dir, identity, bond, capabilities)
    }

    fn step(&mut self, phone: &mut Phone) {
        let ids: Vec<ConnId> = self.glass.conn_fds().iter().map(|c| c.0).collect();
        for id in ids {
            let events = self.glass.on_writable(id);
            self.take(events);
            let events = self.glass.on_readable(id);
            self.take(events);
        }
        let events = self.glass.notes_tick();
        self.take(events);
        phone.pump();
        phone.run_speech();
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
                start.elapsed() < Duration::from_secs(20),
                "timed out: {what}"
            );
            self.step(phone);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn wait_seal(&mut self) {
        let start = Instant::now();
        while !self.has(note_event::Kind::Added) {
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "the seal timed out"
            );
            assert!(!self.has(note_event::Kind::AddFailed), "{:?}", self.events);
            let events = self.glass.notes_tick();
            self.take(events);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn has(&self, kind: note_event::Kind) -> bool {
        self.events.iter().any(|e| e.kind == kind as i32)
    }
}

fn file(dir: &Path, name: &str, data: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, data).unwrap();
    path
}

fn attachment(kind: note_attachment::Kind, mime: &str, path: &Path) -> NoteAttachment {
    NoteAttachment {
        kind: kind as i32,
        mime: mime.into(),
        path: path.to_str().unwrap().into(),
        duration_ms: None,
    }
}

fn notes_caps() -> Vec<String> {
    vec![capability::NOTES_SYNC.to_string()]
}

/// Bytes that differ everywhere, so a wrong offset cannot go unnoticed.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31) ^ seed ^ (i >> 8) as u8)
        .collect()
}

fn note_dirs(root: &Path) -> usize {
    std::fs::read_dir(root.join("notes"))
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

#[test]
fn a_note_taken_offline_syncs_on_the_next_link_and_only_then_leaves_the_glass() {
    let tmp = tempfile::tempdir().unwrap();
    let (glass_dir, phone_dir, src) = (
        tmp.path().join("g"),
        tmp.path().join("p"),
        tmp.path().join("s"),
    );
    std::fs::create_dir_all(&src).unwrap();
    let voice = pattern(300_000, 1);
    let photo = pattern(70_000, 2);
    let video = pattern(1_000_000, 3);
    let (v, p, m) = (
        file(&src, "v.wav", &voice),
        file(&src, "p.jpg", &photo),
        file(&src, "m.mp4", &video),
    );

    let mut b = Bench::new(&glass_dir);
    assert!(b.glass.notes_available());
    // No companion anywhere: the note goes in the queue all the same.
    let events = b.glass.note_add(NoteAdd {
        request_id: 42,
        text: Some("appunto".into()),
        locale: glass_api::proto::Locale::It as i32,
        attachments: vec![
            attachment(note_attachment::Kind::Voice, "audio/wav", &v),
            attachment(note_attachment::Kind::Photo, "image/jpeg", &p),
            attachment(note_attachment::Kind::Video, "video/mp4", &m),
        ],
    });
    b.take(events);
    b.wait_seal();
    assert_eq!(
        b.events
            .iter()
            .find(|e| e.kind == note_event::Kind::Added as i32)
            .unwrap()
            .request_id,
        42
    );
    assert_eq!(note_dirs(&glass_dir), 1);

    // It survives a restart of glassd before any link.
    drop(b);
    let mut b = Bench::new(&glass_dir);

    let mut phone = b.connect(&phone_dir, Identity::generate(), None, notes_caps());
    b.until(&mut phone, "the note to sync", |b, _| {
        b.has(note_event::Kind::Synced)
    });

    let synced = phone.notes.machine().notes().next().unwrap().clone();
    assert!(synced.complete);
    assert_eq!(synced.manifest.text.as_deref(), Some("appunto"));
    assert_eq!(synced.manifest.locale, omg_protocol::Locale::It);
    for (i, original) in [&voice, &photo, &video].into_iter().enumerate() {
        let path = phone.notes.attachment_path(&synced.manifest.id, i as u16);
        assert_eq!(&std::fs::read(path).unwrap(), original, "attachment {i}");
    }
    // Verified on the phone, so gone from the Glass, by the default policy. The sources stay.
    assert_eq!(note_dirs(&glass_dir), 0);
    assert!(v.exists() && p.exists() && m.exists());
}

#[test]
fn a_link_cut_mid_transfer_resumes_and_keeps_the_note_until_verified() {
    let tmp = tempfile::tempdir().unwrap();
    let (glass_dir, phone_dir) = (tmp.path().join("g"), tmp.path().join("p"));
    let data = pattern(3_000_000, 9);
    let src = file(tmp.path(), "m.mp4", &data);

    let mut b = Bench::new(&glass_dir);
    let events = b.glass.note_add(NoteAdd {
        request_id: 1,
        attachments: vec![attachment(note_attachment::Kind::Video, "video/mp4", &src)],
        ..Default::default()
    });
    b.take(events);
    b.wait_seal();

    let id = Identity::generate();
    let mut phone = b.connect(&phone_dir, id.clone(), None, notes_caps());
    // Until some, but not all, of the file has arrived.
    b.until(&mut phone, "part of the file", |_, p| {
        p.notes.machine().notes().next().is_some_and(|n| {
            n.attachments
                .get(&0)
                .is_some_and(|a| a.committed_offset > 0)
        }) || std::fs::read_dir(phone_dir.join({
            p.notes
                .machine()
                .notes()
                .next()
                .map_or(String::from("."), |n| n.manifest.id.to_hex())
        }))
        .is_ok_and(|d| {
            d.flatten().any(|e| {
                e.file_name().to_string_lossy().ends_with(".part")
                    && e.metadata().unwrap().len() > 0
            })
        })
    });
    let bond = phone.bond.clone();
    let _ = phone.notes.link_down(omg_core::now_ms());
    drop(phone); // The socket closes without a goodbye.
    b.until_gone();
    assert!(!b.has(note_event::Kind::Synced));
    assert_eq!(
        note_dirs(&glass_dir),
        1,
        "not verified, so still on the Glass"
    );

    let mut phone = b.connect(&phone_dir, id, bond, notes_caps());
    b.until(&mut phone, "the resumed sync", |b, _| {
        b.has(note_event::Kind::Synced)
    });
    let note = phone.notes.machine().notes().next().unwrap().clone();
    let path = phone.notes.attachment_path(&note.manifest.id, 0);
    assert_eq!(std::fs::read(path).unwrap(), data);
    assert_eq!(note_dirs(&glass_dir), 0);
}

impl Bench {
    fn until_gone(&mut self) {
        let start = Instant::now();
        while !self.disconnected {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "the link stayed up"
            );
            let ids: Vec<ConnId> = self.glass.conn_fds().iter().map(|c| c.0).collect();
            for id in ids {
                let events = self.glass.on_readable(id);
                self.take(events);
            }
        }
    }
}

#[test]
fn a_companion_without_notes_sees_no_note_frames_and_the_note_waits() {
    let tmp = tempfile::tempdir().unwrap();
    let (glass_dir, phone_dir) = (tmp.path().join("g"), tmp.path().join("p"));
    let mut b = Bench::new(&glass_dir);
    let events = b.glass.note_add(NoteAdd {
        request_id: 1,
        text: Some("solo testo".into()),
        ..Default::default()
    });
    b.take(events);
    b.wait_seal();
    // Phone::carry_out fails the test if a note frame arrives.
    let mut phone = b.connect(&phone_dir, Identity::generate(), None, Vec::new());
    b.until(&mut phone, "the handshake", |_, p| p.link.is_running());
    phone.send(&Frame::SettingsGet);
    for _ in 0..200 {
        b.step(&mut phone);
    }
    assert_eq!(note_dirs(&glass_dir), 1);
    assert!(!b.has(note_event::Kind::Synced));
}

#[test]
fn speech_is_made_on_the_companion_and_needs_the_link() {
    let tmp = tempfile::tempdir().unwrap();
    let (glass_dir, phone_dir) = (tmp.path().join("g"), tmp.path().join("p"));
    let mut b = Bench::new(&glass_dir);
    let speak = |request_id| NoteSpeak {
        request_id,
        note_id: "ab".repeat(16),
        text: Some("ciao".into()),
        locale: glass_api::proto::Locale::It as i32,
    };

    // No link: an answer right away, and no engine on the Glass to fall back on.
    let events = b.glass.note_speak(speak(5));
    b.take(events);
    let failed = b.events.pop().unwrap();
    assert_eq!(failed.kind, note_event::Kind::SpeechFinished as i32);
    assert_eq!(
        (failed.request_id, failed.detail.as_str()),
        (5, "no companion")
    );

    let mut phone = b.connect(&phone_dir, Identity::generate(), None, notes_caps());
    b.until(&mut phone, "the handshake", |_, p| p.link.is_running());
    b.step(&mut phone);
    let events = b.glass.note_speak(speak(6));
    b.take(events);
    b.until(&mut phone, "the speech", |b, _| {
        b.has(note_event::Kind::SpeechFinished)
    });
    let audio: usize = b
        .events
        .iter()
        .filter(|e| e.kind == note_event::Kind::SpeechAudio as i32)
        .inspect(|e| assert_eq!((e.request_id, e.sample_rate), (6, 16_000)))
        .map(|e| e.audio.len())
        .sum();
    assert_eq!(audio, 100_000);
    let done = b.events.last().unwrap();
    assert_eq!((done.request_id, done.detail.as_str()), (6, ""));
}

#[test]
fn links_and_bad_requests_are_refused_without_queueing() {
    let tmp = tempfile::tempdir().unwrap();
    let glass_dir = tmp.path().join("g");
    let target = file(tmp.path(), "secret", b"x");
    let link = tmp.path().join("link.jpg");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let mut b = Bench::new(&glass_dir);

    for (a, what) in [
        (
            attachment(note_attachment::Kind::Photo, "image/jpeg", &link),
            "a symlink",
        ),
        (
            attachment(
                note_attachment::Kind::Photo,
                "image/jpeg",
                Path::new("r.jpg"),
            ),
            "relative",
        ),
        (
            attachment(note_attachment::Kind::Photo, "image/jpeg", tmp.path()),
            "a directory",
        ),
        (
            attachment(note_attachment::Kind::Unspecified, "x/y", &target),
            "no kind",
        ),
    ] {
        let events = b.glass.note_add(NoteAdd {
            request_id: 3,
            attachments: vec![a],
            ..Default::default()
        });
        b.take(events);
        assert!(b.has(note_event::Kind::AddFailed), "{what}");
        b.events.clear();
    }
    // An empty note reaches the sealer, which refuses it there.
    let events = b.glass.note_add(NoteAdd {
        request_id: 4,
        ..Default::default()
    });
    b.take(events);
    let start = Instant::now();
    while !b.has(note_event::Kind::AddFailed) {
        assert!(start.elapsed() < Duration::from_secs(10));
        let events = b.glass.notes_tick();
        b.take(events);
    }
    assert_eq!(note_dirs(&glass_dir), 0);
}
