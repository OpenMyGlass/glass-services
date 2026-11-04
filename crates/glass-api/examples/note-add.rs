//! Adds a note to a running glassd, for testing without a Glass app.
//!
//! ```text
//! GLASS_SERVICES_SOCKET=/tmp/g.sock cargo run -p glass-api --example note-add -- \
//!     --text hello --voice /abs/a.wav --photo /abs/b.jpg --video /abs/c.mp4
//! ```

use std::time::Duration;

use glass_api::proto::{NoteAdd, NoteAttachment, Topic, client_frame, note_attachment, note_event};
use glass_api::{Client, proto::service_frame};

fn main() {
    let mut note = NoteAdd {
        request_id: 1,
        ..Default::default()
    };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args.next().expect("every flag takes a value");
        let (kind, mime) = match flag.as_str() {
            "--text" => {
                note.text = Some(value);
                continue;
            }
            "--voice" => (note_attachment::Kind::Voice, "audio/wav"),
            "--photo" => (note_attachment::Kind::Photo, "image/jpeg"),
            "--video" => (note_attachment::Kind::Video, "video/mp4"),
            other => panic!("unknown flag {other}"),
        };
        note.attachments.push(NoteAttachment {
            kind: kind as i32,
            mime: mime.into(),
            path: value,
            duration_ms: None,
        });
    }

    let mut client = Client::connect_as(glass_api::socket_path(), "note-add").unwrap();
    client.subscribe(&[Topic::Notes]).unwrap();
    client.send(client_frame::Body::NoteAdd(note)).unwrap();
    loop {
        let frame = client
            .recv(Some(Duration::from_secs(60)))
            .unwrap()
            .expect("no answer from glassd");
        let Some(service_frame::Body::NoteEvent(event)) = frame.body else {
            continue;
        };
        if event.request_id != 1 {
            continue;
        }
        if event.kind == note_event::Kind::Added as i32 {
            println!("queued {}", event.note_id);
            return;
        }
        panic!("refused: {}", event.detail);
    }
}
