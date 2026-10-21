//! Per-client outgoing queue with the backpressure rule of docs/api.md ("Transport").
//!
//! - The queue holds encoded frames up to [`QUEUE_LIMIT`] bytes.
//! - When it is full, a new `Scroll` that has not ended is merged into a `Scroll` at the back
//!   of the queue (newest displacement, velocity and time; deltas summed). An ended `Scroll`
//!   and every other frame are always queued, even past the limit.
//! - A queue that stays full for [`FULL_TIMEOUT_US`] gets its client disconnected.

use glass_api::proto::{Scroll, ServiceFrame, service_frame};
use glass_api::{encode_frame, frame_bytes};
use std::collections::VecDeque;
use std::io::{self, Write};

pub const QUEUE_LIMIT: usize = 256 * 1024;
pub const FULL_TIMEOUT_US: u64 = 2_000_000;

#[derive(Debug)]
struct Queued {
    bytes: Vec<u8>,
    /// Set for a `Scroll` that has not ended (it may absorb a newer one).
    scroll: Option<Scroll>,
}

#[derive(Debug, Default)]
pub struct OutQueue {
    frames: VecDeque<Queued>,
    /// Bytes of the front frame already written.
    head_written: usize,
    bytes: usize,
    full_since: Option<u64>,
}

impl OutQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Queued bytes not yet written.
    pub fn len_bytes(&self) -> usize {
        self.bytes - self.head_written
    }

    pub fn frames(&self) -> usize {
        self.frames.len()
    }

    fn is_full(&self) -> bool {
        self.len_bytes() >= QUEUE_LIMIT
    }

    fn update_full(&mut self, now_us: u64) {
        if self.is_full() {
            self.full_since.get_or_insert(now_us);
        } else {
            self.full_since = None;
        }
    }

    /// When the client must be disconnected if the queue is still full.
    pub fn deadline(&self) -> Option<u64> {
        self.full_since.map(|t| t + FULL_TIMEOUT_US)
    }

    /// Whether the queue has been full for too long.
    pub fn expired(&self, now_us: u64) -> bool {
        self.deadline().is_some_and(|d| now_us >= d)
    }

    /// Queues a frame (see the module rules).
    pub fn push(&mut self, frame: &ServiceFrame, now_us: u64) {
        let open_scroll = match &frame.body {
            Some(service_frame::Body::Scroll(s)) if !s.ended => Some(*s),
            _ => None,
        };
        if let Some(new) = open_scroll
            && self.is_full()
        {
            let back_untouched = self.frames.len() > 1 || self.head_written == 0;
            if let Some(back) = self.frames.back_mut()
                && back_untouched
                && let Some(old) = back.scroll
            {
                let merged = Scroll {
                    delta: old.delta + new.delta,
                    ..new
                };
                let bytes = frame_bytes(&ServiceFrame {
                    body: Some(service_frame::Body::Scroll(merged)),
                })
                .expect("a Scroll is far below the frame limit");
                self.bytes = self.bytes - back.bytes.len() + bytes.len();
                back.bytes = bytes;
                back.scroll = Some(merged);
                self.update_full(now_us);
                return;
            }
        }
        let mut bytes = Vec::new();
        if encode_frame(frame, &mut bytes).is_err() {
            // Service frames are built by glassd and never exceed the limit.
            return;
        }
        self.bytes += bytes.len();
        self.frames.push_back(Queued {
            bytes,
            scroll: open_scroll,
        });
        self.update_full(now_us);
    }

    /// Writes as much as the writer takes without blocking.
    pub fn flush_to(&mut self, w: &mut impl Write, now_us: u64) -> io::Result<()> {
        while let Some(front) = self.frames.front() {
            match w.write(&front.bytes[self.head_written..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.head_written += n;
                    if self.head_written == front.bytes.len() {
                        self.bytes -= front.bytes.len();
                        self.head_written = 0;
                        self.frames.pop_front();
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        self.update_full(now_us);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_api::FrameDecoder;
    use glass_api::proto::{Gesture, GestureKind};

    fn scroll(d: f32, delta: f32, ended: bool) -> ServiceFrame {
        ServiceFrame {
            body: Some(service_frame::Body::Scroll(Scroll {
                displacement: d,
                delta,
                velocity: d * 10.0,
                fingers: 1,
                ended,
                cancelled: false,
                timestamp_us: (d * 1000.0) as u64,
            })),
        }
    }

    fn big_gesture() -> ServiceFrame {
        ServiceFrame {
            body: Some(service_frame::Body::Gesture(Gesture {
                kind: GestureKind::Tap as i32,
                fingers: 1,
                velocity: 0.0,
                timestamp_us: u64::MAX,
            })),
        }
    }

    /// A writer that accepts a fixed number of bytes, then blocks.
    struct Sink {
        out: Vec<u8>,
        room: usize,
    }

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.room == 0 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let n = buf.len().min(self.room);
            self.out.extend_from_slice(&buf[..n]);
            self.room -= n;
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn decode(bytes: &[u8]) -> Vec<ServiceFrame> {
        let mut d = FrameDecoder::new();
        d.push(bytes);
        let mut v = Vec::new();
        while let Some(f) = d.next_message().unwrap() {
            v.push(f);
        }
        v
    }

    fn fill(q: &mut OutQueue) {
        while q.len_bytes() < QUEUE_LIMIT {
            q.push(&big_gesture(), 0);
        }
    }

    #[test]
    fn frames_flow_in_order_with_partial_writes() {
        let mut q = OutQueue::new();
        for i in 0..10 {
            q.push(&scroll(i as f32, 1.0, false), 0);
        }
        let mut sink = Sink {
            out: Vec::new(),
            room: 7,
        };
        q.flush_to(&mut sink, 0).unwrap();
        assert!(!q.is_empty());
        sink.room = usize::MAX;
        q.flush_to(&mut sink, 0).unwrap();
        assert!(q.is_empty());
        assert_eq!(q.len_bytes(), 0);
        assert_eq!(decode(&sink.out).len(), 10);
    }

    #[test]
    fn scrolls_merge_only_when_full() {
        let mut q = OutQueue::new();
        q.push(&scroll(0.1, 0.1, false), 0);
        q.push(&scroll(0.2, 0.1, false), 0);
        assert_eq!(q.frames(), 2);
        fill(&mut q);
        q.push(&scroll(0.3, 0.1, false), 0);
        let n = q.frames();
        q.push(&scroll(0.4, 0.1, false), 0);
        q.push(&scroll(0.5, 0.1, false), 0);
        assert_eq!(q.frames(), n, "merged into the back scroll");
        let mut sink = Sink {
            out: Vec::new(),
            room: usize::MAX,
        };
        q.flush_to(&mut sink, 0).unwrap();
        let frames = decode(&sink.out);
        let Some(service_frame::Body::Scroll(last)) = &frames.last().unwrap().body else {
            panic!()
        };
        assert_eq!(last.displacement, 0.5);
        assert!((last.delta - 0.3).abs() < 1e-6);
    }

    #[test]
    fn ended_scroll_and_other_frames_are_never_dropped() {
        let mut q = OutQueue::new();
        fill(&mut q);
        q.push(&scroll(0.3, 0.1, false), 0);
        let n = q.frames();
        q.push(&scroll(0.4, 0.1, true), 0);
        q.push(&big_gesture(), 0);
        q.push(&scroll(0.5, 0.1, false), 0); // back is a gesture: not merged
        assert_eq!(q.frames(), n + 3);
    }

    #[test]
    fn full_for_two_seconds_expires() {
        let mut q = OutQueue::new();
        q.push(&big_gesture(), 0);
        assert_eq!(q.deadline(), None);
        fill(&mut q);
        q.push(&big_gesture(), 1_000);
        let d = q.deadline().unwrap();
        assert!(!q.expired(d - 1));
        assert!(q.expired(d));
        // Draining clears it.
        let mut sink = Sink {
            out: Vec::new(),
            room: usize::MAX,
        };
        q.flush_to(&mut sink, d - 1).unwrap();
        assert_eq!(q.deadline(), None);
    }
}
