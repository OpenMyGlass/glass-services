//! Glass services interface, version 1 (`docs/api.md`).
//!
//! - [`proto`]: the messages, generated at build time from
//!   `proto/glass/services/v1/services.proto`;
//! - [`codec`]: varint32-length framing with the 64 KiB limit;
//! - [`Client`]: a blocking client;
//! - [`Hub`]: the session and routing rules as a pure state machine (used by glassd and the mock);
//! - [`companion`]: the companion link provider and the dictation relay, used by the hub;
//! - [`MockService`]: an in-process mock server for tests of clients.

pub mod client;
pub mod codec;
pub mod companion;
pub mod hub;
pub mod mock;

/// The generated message types (package `glass.services.v1`).
#[allow(clippy::all, missing_docs)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/glass.services.v1.rs"));
}

pub use client::{Client, ClientError, SOCKET_ENV, default_socket_path, socket_path};
pub use codec::{FrameDecoder, FrameError, MAX_FRAME_LEN, encode_frame, frame_bytes};
pub use hub::{API_VERSION, ClientId, Effect, Hub};
pub use mock::MockService;
pub use prost::Message;

/// `CLOCK_MONOTONIC` in microseconds, the time base of every `timestamp_us`.
pub fn monotonic_us() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64) * 1_000_000 + (ts.tv_nsec as u64) / 1_000
}
