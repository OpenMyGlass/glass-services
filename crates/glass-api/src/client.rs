//! A blocking client for the glassd socket.

use crate::codec::{FrameDecoder, FrameError, encode_frame};
use crate::hub::API_VERSION;
use crate::proto::{
    AcquireInputFocus, ClientFrame, Hello, ReleaseInputFocus, ServiceFrame, Subscribe, Topic,
    Welcome, client_frame, service_frame,
};
use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Environment variable that overrides the socket path.
pub const SOCKET_ENV: &str = "GLASS_SERVICES_SOCKET";

/// The default socket path for this build target (without the environment override).
pub fn default_socket_path() -> &'static str {
    if cfg!(target_os = "android") {
        "/dev/socket/glassd"
    } else {
        "/run/glassd/glassd.sock"
    }
}

/// The socket path: `$GLASS_SERVICES_SOCKET` if set and not empty, else the default.
pub fn socket_path() -> PathBuf {
    socket_path_from(std::env::var_os(SOCKET_ENV))
}

fn socket_path_from(env: Option<std::ffi::OsString>) -> PathBuf {
    match env {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(default_socket_path()),
    }
}

/// How long [`Client::connect`] waits for `Welcome`.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum ClientError {
    Io(io::Error),
    Frame(FrameError),
    /// The service closed the connection.
    Closed,
    /// The service answered `Hello` with something other than `Welcome`.
    Handshake(Box<ServiceFrame>),
    /// No `Welcome` within the handshake timeout.
    HandshakeTimeout,
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Io(e) => write!(f, "I/O error: {e}"),
            ClientError::Frame(e) => write!(f, "protocol error: {e}"),
            ClientError::Closed => write!(f, "connection closed by the service"),
            ClientError::Handshake(fr) => write!(f, "unexpected answer to Hello: {fr:?}"),
            ClientError::HandshakeTimeout => write!(f, "no Welcome from the service"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        ClientError::Io(e)
    }
}

impl From<FrameError> for ClientError {
    fn from(e: FrameError) -> Self {
        ClientError::Frame(e)
    }
}

/// A connected, handshaken session.
pub struct Client {
    stream: UnixStream,
    decoder: FrameDecoder,
    welcome: Welcome,
    next_seq: u32,
    /// Closed after a protocol error (docs/api.md: the client closes the connection).
    closed: bool,
}

impl Client {
    /// Connects to `path`, sends `Hello` (client name: the executable's file name) and waits
    /// for `Welcome`.
    pub fn connect(path: impl AsRef<Path>) -> Result<Client, ClientError> {
        let name = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "glass-api-client".into());
        Self::connect_as(path, &name)
    }

    /// Connects to [`socket_path()`].
    pub fn connect_default() -> Result<Client, ClientError> {
        Self::connect(socket_path())
    }

    /// Connects to `path` with the given client name.
    pub fn connect_as(path: impl AsRef<Path>, client_name: &str) -> Result<Client, ClientError> {
        Self::connect_with_timeout(path, client_name, HANDSHAKE_TIMEOUT)
    }

    /// Like [`connect_as`](Self::connect_as), waiting up to `handshake` for `Welcome`.
    pub fn connect_with_timeout(
        path: impl AsRef<Path>,
        client_name: &str,
        handshake: Duration,
    ) -> Result<Client, ClientError> {
        let stream = UnixStream::connect(path)?;
        let mut c = Client {
            stream,
            decoder: FrameDecoder::new(),
            welcome: Welcome::default(),
            next_seq: 1,
            closed: false,
        };
        c.send_frame(&ClientFrame {
            seq: 0,
            body: Some(client_frame::Body::Hello(Hello {
                api_version: API_VERSION,
                client_name: client_name.into(),
            })),
        })?;
        match c.recv(Some(handshake))? {
            Some(ServiceFrame {
                body: Some(service_frame::Body::Welcome(w)),
            }) => c.welcome = w,
            Some(other) => return Err(ClientError::Handshake(Box::new(other))),
            None => return Err(ClientError::HandshakeTimeout),
        }
        Ok(c)
    }

    /// The service's `Welcome`.
    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    /// Whether the service announced `cap` (for example `"touchpad"`).
    pub fn has_capability(&self, cap: &str) -> bool {
        self.welcome.capabilities.iter().any(|c| c == cap)
    }

    /// Sends a raw frame as is.
    pub fn send_frame(&mut self, frame: &ClientFrame) -> Result<(), ClientError> {
        if self.closed {
            return Err(ClientError::Closed);
        }
        let mut buf = Vec::new();
        encode_frame(frame, &mut buf)?;
        self.stream.write_all(&buf)?;
        Ok(())
    }

    /// Sends a request with a fresh `seq`, returned so errors can be matched to it.
    pub fn send(&mut self, body: client_frame::Body) -> Result<u32, ClientError> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1).max(1);
        self.send_frame(&ClientFrame {
            seq,
            body: Some(body),
        })?;
        Ok(seq)
    }

    /// Replaces the subscribed topics.
    pub fn subscribe(&mut self, topics: &[Topic]) -> Result<u32, ClientError> {
        self.send(client_frame::Body::Subscribe(Subscribe {
            topics: topics.iter().map(|&t| t as i32).collect(),
        }))
    }

    pub fn acquire_focus(&mut self) -> Result<u32, ClientError> {
        self.send(client_frame::Body::AcquireFocus(AcquireInputFocus {}))
    }

    pub fn release_focus(&mut self) -> Result<u32, ClientError> {
        self.send(client_frame::Body::ReleaseFocus(ReleaseInputFocus {}))
    }

    /// Receives the next frame. `timeout` `None` blocks; on timeout, returns `Ok(None)`.
    /// A malformed or oversize frame is a protocol error: the connection is closed and every
    /// later call returns [`ClientError::Closed`].
    pub fn recv(&mut self, timeout: Option<Duration>) -> Result<Option<ServiceFrame>, ClientError> {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut buf = [0u8; 8192];
        loop {
            if self.closed {
                return Err(ClientError::Closed);
            }
            match self.decoder.next_message::<ServiceFrame>() {
                Ok(Some(f)) => return Ok(Some(f)),
                Ok(None) => {}
                Err(e) => {
                    self.closed = true;
                    let _ = self.stream.shutdown(std::net::Shutdown::Both);
                    return Err(e.into());
                }
            }
            let wait = match deadline {
                None => None,
                Some(d) => {
                    let left = d.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ok(None);
                    }
                    Some(left)
                }
            };
            self.stream.set_read_timeout(wait)?;
            match self.stream.read(&mut buf) {
                Ok(0) => return Err(ClientError::Closed),
                Ok(n) => self.decoder.push(&buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Returns a frame that is already decoded or already in the socket, without waiting:
    /// `Ok(None)` when there is none. Unlike `recv` with a short timeout (which the kernel
    /// rounds up to a scheduler tick), it never blocks, so an event loop that polls the socket
    /// itself drains it with this.
    pub fn try_recv(&mut self) -> Result<Option<ServiceFrame>, ClientError> {
        use std::os::fd::AsRawFd;
        let mut buf = [0u8; 8192];
        loop {
            if self.closed {
                return Err(ClientError::Closed);
            }
            match self.decoder.next_message::<ServiceFrame>() {
                Ok(Some(f)) => return Ok(Some(f)),
                Ok(None) => {}
                Err(e) => {
                    self.closed = true;
                    let _ = self.stream.shutdown(std::net::Shutdown::Both);
                    return Err(e.into());
                }
            }
            // SAFETY: `buf` is valid for `buf.len()` bytes.
            let n = unsafe {
                libc::recv(
                    self.stream.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n == 0 {
                return Err(ClientError::Closed);
            }
            if n > 0 {
                self.decoder.push(&buf[..n as usize]);
                continue;
            }
            let e = io::Error::last_os_error();
            match e.kind() {
                io::ErrorKind::WouldBlock => return Ok(None),
                io::ErrorKind::Interrupted => {}
                _ => return Err(e.into()),
            }
        }
    }

    /// Receives frames until `pred` matches one, within `timeout`; returns it, or `None` on
    /// timeout. Frames that do not match are discarded.
    pub fn recv_until(
        &mut self,
        timeout: Duration,
        mut pred: impl FnMut(&ServiceFrame) -> bool,
    ) -> Result<Option<ServiceFrame>, ClientError> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.recv(Some(left))? {
                None => return Ok(None),
                Some(f) if pred(&f) => return Ok(Some(f)),
                Some(_) => {}
            }
        }
    }

    /// The underlying stream (for example to poll it).
    pub fn stream(&self) -> &UnixStream {
        &self.stream
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_error_closes_the_connection() {
        use crate::codec::frame_bytes;
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("glass-client-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let l = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut d = FrameDecoder::new();
            let mut buf = [0u8; 256];
            while d.next_message::<ClientFrame>().unwrap().is_none() {
                let n = s.read(&mut buf).unwrap();
                d.push(&buf[..n]);
            }
            let welcome = ServiceFrame {
                body: Some(service_frame::Body::Welcome(Welcome::default())),
            };
            s.write_all(&frame_bytes(&welcome).unwrap()).unwrap();
            s.write_all(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x01]).unwrap();
            // The client must close: read returns 0.
            let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
            let mut rest = Vec::new();
            s.read_to_end(&mut rest)
                .map(|_| rest.len())
                .map_err(|e| e.kind())
        });
        let mut c = Client::connect_as(&path, "t").unwrap();
        assert!(matches!(
            c.recv(Some(Duration::from_secs(5))),
            Err(ClientError::Frame(FrameError::BadVarint))
        ));
        assert_eq!(server.join().expect("server thread"), Ok(0));
        assert!(matches!(c.recv(None), Err(ClientError::Closed)));
        assert!(matches!(c.subscribe(&[]), Err(ClientError::Closed)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn socket_path_resolution() {
        assert_eq!(
            socket_path_from(Some("/tmp/x.sock".into())),
            PathBuf::from("/tmp/x.sock")
        );
        assert_eq!(
            socket_path_from(Some("".into())),
            PathBuf::from(default_socket_path())
        );
        assert_eq!(socket_path_from(None), PathBuf::from(default_socket_path()));
        #[cfg(not(target_os = "android"))]
        assert_eq!(default_socket_path(), "/run/glassd/glassd.sock");
    }
}
