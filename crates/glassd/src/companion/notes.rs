//! Notes on the Glass: the queue that holds them until the companion has a verified copy.
//!
//! The logic is `omg-core`'s, shared with the companion's tests: [`GlassNotes`] over a crash-safe
//! store. This module fits it to glassd: clients add notes through the hub (`NoteAdd`), sealing
//! runs on its own thread so that copying and hashing a video does not stall touch input, and
//! what happens comes out as `NoteEvent`s for `TOPIC_NOTES`.
//!
//! Without a companion a note just waits. On the next link it is offered, oldest first.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};

use glass_api::proto::{
    NoteAdd, NoteAttachment, NoteEvent, NoteSpeak, note_attachment, note_event,
};
use omg_core::CoreError;
use omg_core::notes::{
    AttachmentSource, GlassNotes, LinkProfile, NoteDraft, NoteSealer, OutgoingNote, SenderConfig,
    SenderEvent, record::BlockReason,
};
use omg_protocol::{AttachmentKind, Frame, Locale, NoteId};

use super::transport::Transport;
use crate::{info, warn};

/// Seals running at once. A client asking for more is told to wait.
const MAX_SEALS_IN_FLIGHT: usize = 4;
/// How often to look for finished seals while one runs, and the shortest poll wait while the
/// note machine has a deadline, so a deadline just passed cannot become a busy loop.
pub const MIN_WAIT_MS: u64 = 100;
/// Bytes waiting for the socket above which a chunk request is ignored. The companion's window
/// never gets near this; only a bonded peer ignoring the window does, and it asks again.
const MAX_BACKLOG: usize = 2 * 1024 * 1024;

/// How to pull speech over this transport.
pub fn profile_for(transport: Transport) -> LinkProfile {
    match transport {
        Transport::BtRfcomm => LinkProfile::moderate(),
        Transport::WifiTcp => LinkProfile::fast(),
    }
}

/// Whether to ignore this frame because the socket is not keeping up.
pub fn over_backlog(frame: &Frame, backlog: usize) -> bool {
    backlog > MAX_BACKLOG && matches!(frame, Frame::NoteChunkRequest(_))
}

/// The answer to a `NoteAdd` where this glassd holds no note queue.
pub fn refused_add(add: &NoteAdd) -> Vec<super::Event> {
    vec![super::Event::Note(add_failed(
        add.request_id,
        "notes are not available here".into(),
    ))]
}

/// The answer to a `NoteSpeak` where this glassd holds no note queue.
pub fn refused_speak(speak: &NoteSpeak) -> Vec<super::Event> {
    vec![super::Event::Note(speech_finished(
        speak.request_id,
        &speak.note_id,
        "no companion".into(),
    ))]
}

fn add_failed(request_id: u32, detail: String) -> NoteEvent {
    NoteEvent {
        kind: note_event::Kind::AddFailed as i32,
        request_id,
        detail,
        ..Default::default()
    }
}

fn speech_finished(request_id: u32, note_id: &str, detail: String) -> NoteEvent {
    NoteEvent {
        kind: note_event::Kind::SpeechFinished as i32,
        request_id,
        note_id: note_id.to_string(),
        detail,
        ..Default::default()
    }
}

fn note_event(kind: note_event::Kind, note_id: &NoteId, detail: String) -> NoteEvent {
    NoteEvent {
        kind: kind as i32,
        note_id: note_id.to_hex(),
        detail,
        ..Default::default()
    }
}

/// Maps the services locale to the companion's, falling back to `current` when unspecified.
fn locale_or(value: i32, current: Locale) -> Locale {
    use glass_api::proto::Locale as P;
    match P::try_from(value) {
        Ok(P::It) => Locale::It,
        Ok(P::En) => Locale::En,
        Ok(P::Fr) => Locale::Fr,
        Ok(P::Es) => Locale::Es,
        _ => current,
    }
}

/// Checks one requested attachment. The file must be a plain file by its own name: a client may
/// not make glassd, which reads with more rights than it has, follow a link to something else.
fn source_of(a: &NoteAttachment) -> Result<AttachmentSource, String> {
    let kind = match note_attachment::Kind::try_from(a.kind) {
        Ok(note_attachment::Kind::Voice) => AttachmentKind::VoiceRecording,
        Ok(note_attachment::Kind::Photo) => AttachmentKind::Photo,
        Ok(note_attachment::Kind::Video) => AttachmentKind::Video,
        _ => return Err(format!("{}: no attachment kind", a.path)),
    };
    let path = PathBuf::from(&a.path);
    if !path.is_absolute() {
        return Err(format!("{}: not an absolute path", a.path));
    }
    let meta = std::fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", a.path))?;
    if !meta.file_type().is_file() {
        return Err(format!("{}: not a regular file", a.path));
    }
    Ok(AttachmentSource {
        kind,
        mime: a.mime.clone(),
        path,
        duration_ms: a.duration_ms,
    })
}

struct Sealed {
    request_id: u32,
    result: Result<OutgoingNote, String>,
}

/// See the module docs.
pub struct Notes {
    machine: GlassNotes,
    done_tx: Sender<Sealed>,
    done_rx: Receiver<Sealed>,
    in_flight: usize,
    /// Core TTS request id to the client's.
    speech: BTreeMap<u32, u32>,
    events: Vec<NoteEvent>,
    last_queue: Option<(u32, u64)>,
}

impl Notes {
    /// Opens the queue at `dir`, finishing what a crash interrupted.
    pub fn open(dir: &Path) -> Result<Notes, CoreError> {
        // The default delete policy applies: see docs/api.md "Notes".
        let machine = GlassNotes::open(dir, SenderConfig::default())?;
        for name in machine.set_aside() {
            warn!("notes: an unreadable record was set aside: {name}");
        }
        let (done_tx, done_rx) = channel();
        let mut notes = Notes {
            machine,
            done_tx,
            done_rx,
            in_flight: 0,
            speech: BTreeMap::new(),
            events: Vec::new(),
            last_queue: None,
        };
        notes.report_queue();
        Ok(notes)
    }

    /// Starts sealing a note on its own thread. Returns what to publish right away, if refused.
    pub fn add(&mut self, add: NoteAdd, current: Locale) -> Option<NoteEvent> {
        let request_id = add.request_id;
        if self.in_flight >= MAX_SEALS_IN_FLIGHT {
            return Some(add_failed(request_id, "too many notes being saved".into()));
        }
        let attachments = match add.attachments.iter().map(source_of).collect() {
            Ok(a) => a,
            Err(e) => return Some(add_failed(request_id, e)),
        };
        let draft = NoteDraft {
            created_at_ms: super::now_ms(),
            locale: locale_or(add.locale, current),
            text: add.text.filter(|t| !t.is_empty()),
            attachments,
        };
        let sealer = self.machine.sealer();
        self.spawn_seal(request_id, draft, sealer);
        None
    }

    fn spawn_seal(&mut self, request_id: u32, draft: NoteDraft, sealer: NoteSealer) {
        let done = self.done_tx.clone();
        let spawned = std::thread::Builder::new()
            .name("note-seal".into())
            .spawn(move || {
                let result = sealer.seal(&draft).map_err(|e| e.to_string());
                // The receiver lives as long as the daemon.
                let _ = done.send(Sealed { request_id, result });
            });
        match spawned {
            Ok(_) => self.in_flight += 1,
            Err(e) => self
                .events
                .push(add_failed(request_id, format!("no thread to save it: {e}"))),
        }
    }

    /// Asks the companion to read a note aloud, or reports that there is none to ask.
    pub fn speak(
        &mut self,
        speak: NoteSpeak,
        current: Locale,
        linked: bool,
        now: u64,
    ) -> Result<Vec<Frame>, CoreError> {
        let Some(note_id) = NoteId::from_hex(&speak.note_id) else {
            self.events.push(speech_finished(
                speak.request_id,
                &speak.note_id,
                "not a note id".into(),
            ));
            return Ok(Vec::new());
        };
        let asked = if linked {
            let locale = locale_or(speak.locale, current);
            self.machine
                .request_tts(note_id, speak.text.clone(), locale, now)?
        } else {
            None
        };
        match asked {
            Some((core_id, frames)) => {
                // A new request replaces the one in flight, in the machine too.
                self.speech.clear();
                self.speech.insert(core_id, speak.request_id);
                self.drain();
                Ok(frames)
            }
            None => {
                self.events.push(speech_finished(
                    speak.request_id,
                    &speak.note_id,
                    "no companion".into(),
                ));
                Ok(Vec::new())
            }
        }
    }

    /// A link came up and the companion speaks note sync.
    pub fn link_up(&mut self, profile: LinkProfile, now: u64) -> Result<Vec<Frame>, CoreError> {
        let frames = self.machine.link_up(profile, now);
        self.drain();
        frames
    }

    /// The link is gone. The queue stays as it is. Speech is not queued, so it ends here.
    pub fn link_down(&mut self) {
        self.machine.link_down();
        for (_, client_id) in std::mem::take(&mut self.speech) {
            self.events
                .push(speech_finished(client_id, "", "link lost".into()));
        }
        self.drain();
    }

    /// A frame of note sync arrived.
    pub fn on_frame(&mut self, frame: Frame, now: u64) -> Result<Vec<Frame>, CoreError> {
        let frames = self.machine.on_frame(frame, now);
        self.drain();
        frames
    }

    /// Takes in finished seals and runs the retry timers.
    pub fn tick(&mut self, now: u64) -> Result<Vec<Frame>, CoreError> {
        let mut frames = Vec::new();
        while let Ok(sealed) = self.done_rx.try_recv() {
            self.in_flight -= 1;
            match sealed.result {
                Ok(record) => {
                    let note_id = record.manifest.id;
                    info!("notes: {} queued", note_id.to_hex());
                    let mut added = note_event(note_event::Kind::Added, &note_id, String::new());
                    added.request_id = sealed.request_id;
                    self.events.push(added);
                    frames.extend(self.machine.enqueue_sealed(record, now)?);
                }
                Err(e) => {
                    warn!("notes: cannot save a note: {e}");
                    self.events.push(add_failed(sealed.request_id, e));
                }
            }
        }
        frames.extend(self.machine.tick(now)?);
        self.drain();
        Ok(frames)
    }

    /// When [`tick`](Notes::tick) next has something to do.
    pub fn next_deadline(&self, now: u64) -> Option<u64> {
        let sealing = (self.in_flight > 0).then_some(now + MIN_WAIT_MS);
        [sealing, self.machine.machine().next_deadline()]
            .into_iter()
            .flatten()
            .min()
    }

    /// Events to publish.
    pub fn take_events(&mut self) -> Vec<NoteEvent> {
        std::mem::take(&mut self.events)
    }

    /// Notes not yet verified on the companion.
    #[cfg(test)]
    pub fn pending(&self) -> u32 {
        self.machine.machine().stats().pending_notes
    }

    fn report_queue(&mut self) {
        let stats = self.machine.machine().stats();
        let queue = (stats.pending_notes, stats.pending_bytes);
        if self.last_queue != Some(queue) {
            self.last_queue = Some(queue);
            self.events.push(NoteEvent {
                kind: note_event::Kind::Queue as i32,
                pending_notes: stats.pending_notes,
                pending_bytes: stats.pending_bytes,
                ..Default::default()
            });
        }
    }

    fn drain(&mut self) {
        for event in self.machine.take_events() {
            if let Some(e) = self.map_event(event) {
                self.events.push(e);
            }
        }
        self.report_queue();
    }

    fn map_event(&mut self, event: SenderEvent) -> Option<NoteEvent> {
        use note_event::Kind;
        Some(match event {
            SenderEvent::NoteSynced { note_id } => {
                note_event(Kind::Synced, &note_id, String::new())
            }
            SenderEvent::NoteDeferred { note_id, reason } => {
                note_event(Kind::Deferred, &note_id, format!("{reason:?}"))
            }
            SenderEvent::NoteBlocked { note_id, reason } => {
                let detail = match reason {
                    BlockReason::Rejected(r) => format!("refused by the companion: {r:?}"),
                    BlockReason::SourceCorrupt => "a file on the Glass is damaged".to_string(),
                };
                note_event(Kind::Blocked, &note_id, detail)
            }
            SenderEvent::AckIgnored { note_id } => {
                warn!("notes: ignored an acknowledgement for {}", note_id.to_hex());
                return None;
            }
            SenderEvent::Transcript {
                note_id,
                transcript,
                ..
            } => match transcript.error {
                None => note_event(Kind::Transcript, &note_id, transcript.text),
                Some(e) => {
                    info!("notes: no transcript for {}: {e:?}", note_id.to_hex());
                    return None;
                }
            },
            SenderEvent::TtsAudio {
                request_id,
                note_id,
                sample_rate,
                data,
            } => NoteEvent {
                kind: Kind::SpeechAudio as i32,
                request_id: self.speech.get(&request_id).copied().unwrap_or(0),
                note_id: note_id.to_hex(),
                sample_rate,
                audio: data,
                ..Default::default()
            },
            SenderEvent::TtsFinished {
                request_id,
                note_id,
                error,
            } => {
                let client_id = self.speech.remove(&request_id)?;
                speech_finished(client_id, &note_id.to_hex(), error.unwrap_or_default())
            }
        })
    }
}
