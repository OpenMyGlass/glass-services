//! The companion-link listeners: a Unix socket for `omg-bled`'s BLE sessions (the everyday
//! link), a TCP accept (photos and video, when Bluetooth fails, and a PC), and a Bluetooth RFCOMM
//! server. All hand the daemon a nonblocking byte [`Stream`] whose raw fd goes into the
//! single-threaded `poll` loop, exactly like the launcher socket.
//!
//! BLE reaches glassd as a plain stream because `omg-bled` holds everything radio about it, in its
//! own process: docs/ble-link.md says why. While it runs it owns the Bluetooth UART, so the
//! kernel's Bluetooth stack, and RFCOMM with it, is not there.
//!
//! The RFCOMM server advertises the clean-room OpenMyGlass service UUID
//! [`OMG_COMPANION_RFCOMM_UUID`] (**not** Google's companion UUID) and is gated behind the
//! `linux-bt` feature so images without a guaranteed BlueZ stack stay lean; TCP always works.
//!
//! With `--mock` (or no `--companion`) the daemon creates no listeners, so its tests never open
//! a real socket or touch BlueZ.

use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;

/// OpenMyGlass's own RFCOMM service UUID (clean-room; NOT Google's companion UUID). glassd is
/// the RFCOMM server; the companion connects to it.
pub const OMG_COMPANION_RFCOMM_UUID: &str = "6f6d672d-636f-6d70-616e-696f6e2d7631";

/// Default RFCOMM channel the server binds (advertised over SDP under the UUID above).
pub const DEFAULT_RFCOMM_CHANNEL: u8 = 23;

/// Default TCP port for the Wi-Fi / PC fallback path.
pub const DEFAULT_TCP_PORT: u16 = 7700;

/// The kernel's send buffer on a BLE session, in bytes (the kernel doubles it). At 3 kB/s a default
/// Unix buffer of 160 KiB would be close to a minute of queue in front of every notification.
const BLE_SEND_BUFFER: libc::c_int = 4096;

/// Which radio a connection arrived over (reported to the launcher as the link transport).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Bluetooth Classic RFCOMM.
    BtRfcomm,
    /// Wi-Fi / TCP.
    WifiTcp,
    /// Bluetooth LE, through `omg-bled`.
    Ble,
}

impl Transport {
    /// The string reported to the launcher in `CompanionState.transport`.
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::BtRfcomm => "bt-rfcomm",
            Transport::WifiTcp => "wifi-tcp",
            Transport::Ble => "ble",
        }
    }
}

/// A nonblocking, bidirectional byte stream for one companion connection.
pub trait Stream: Read + Write + AsRawFd + Send {}
impl<T: Read + Write + AsRawFd + Send> Stream for T {}

/// A boxed connection stream, so different transports share one type.
pub type BoxStream = Box<dyn Stream>;

/// The companion listeners glassd polls.
pub struct Listeners {
    tcp: Option<TcpListener>,
    ble: Option<UnixListener>,
    #[cfg(feature = "linux-bt")]
    bt: Option<bt::RfcommListener>,
}

impl Listeners {
    fn bind_tcp(tcp_port: Option<u16>) -> Option<TcpListener> {
        tcp_port.and_then(|port| match TcpListener::bind(("0.0.0.0", port)) {
            Ok(l) => {
                let _ = l.set_nonblocking(true);
                crate::info!("companion: TCP listening on 0.0.0.0:{port}");
                Some(l)
            }
            Err(e) => {
                crate::warn!("companion: cannot bind TCP :{port} ({e})");
                None
            }
        })
    }

    /// The port TCP listens on, if it does.
    #[cfg(test)]
    pub fn tcp_port(&self) -> Option<u16> {
        self.tcp.as_ref()?.local_addr().ok().map(|a| a.port())
    }

    /// Closes the TCP listener, or binds it again. The others are not on the same networks.
    pub fn set_tcp(&mut self, tcp_port: Option<u16>) {
        self.tcp = Self::bind_tcp(tcp_port);
    }

    /// Binds the enabled listeners. TCP binds `0.0.0.0:port`; RFCOMM binds
    /// [`DEFAULT_RFCOMM_CHANNEL`] when built with `linux-bt`. A listener that cannot bind is
    /// logged and skipped (best effort), so one transport being unavailable never stops the
    /// other.
    pub fn bind(tcp_port: Option<u16>, bt: bool) -> Listeners {
        let tcp = Self::bind_tcp(tcp_port);
        #[cfg(feature = "linux-bt")]
        let bt = bt.then(bt::RfcommListener::bind).and_then(|r| match r {
            Ok(l) => {
                crate::info!(
                    "companion: RFCOMM listening on channel {} ({OMG_COMPANION_RFCOMM_UUID})",
                    DEFAULT_RFCOMM_CHANNEL
                );
                Some(l)
            }
            Err(e) => {
                crate::warn!("companion: cannot bind RFCOMM ({e})");
                None
            }
        });
        #[cfg(not(feature = "linux-bt"))]
        let _ = bt;
        Listeners {
            tcp,
            ble: None,
            #[cfg(feature = "linux-bt")]
            bt,
        }
    }

    /// Opens the socket `omg-bled` connects to, for this user alone. Whoever connects speaks to
    /// the handshake like anyone on TCP, but the path should not widen that.
    pub fn bind_ble(&mut self, path: &Path) {
        let bound = (|| {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            // Left by a previous run.
            match std::fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
            let l = UnixListener::bind(path)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            l.set_nonblocking(true)?;
            Ok(l)
        })();
        match bound {
            Ok(l) => {
                crate::info!("companion: BLE sessions on {}", path.display());
                self.ble = Some(l);
            }
            Err(e) => crate::warn!("companion: cannot bind {} ({e})", path.display()),
        }
    }

    /// The raw fds to add to the poll set, each tagged with its transport.
    pub fn fds(&self) -> Vec<(RawFd, Transport)> {
        let mut v = Vec::new();
        if let Some(t) = &self.tcp {
            v.push((t.as_raw_fd(), Transport::WifiTcp));
        }
        if let Some(b) = &self.ble {
            v.push((b.as_raw_fd(), Transport::Ble));
        }
        #[cfg(feature = "linux-bt")]
        if let Some(b) = &self.bt {
            v.push((b.as_raw_fd(), Transport::BtRfcomm));
        }
        v
    }

    /// Accepts every pending connection on the listener for `transport`. Each stream is set
    /// nonblocking.
    pub fn accept(&self, transport: Transport) -> Vec<BoxStream> {
        let mut out = Vec::new();
        match transport {
            Transport::WifiTcp => {
                if let Some(l) = &self.tcp {
                    loop {
                        match l.accept() {
                            Ok((s, peer)) => {
                                let _ = s.set_nonblocking(true);
                                let _ = s.set_nodelay(true);
                                crate::info!("companion: TCP connection from {peer}");
                                out.push(Box::new(s) as BoxStream);
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                            Err(e) => {
                                crate::warn!("companion: TCP accept ({e})");
                                break;
                            }
                        }
                    }
                }
            }
            Transport::Ble => {
                if let Some(l) = &self.ble {
                    loop {
                        match l.accept() {
                            Ok((s, _)) => {
                                let _ = s.set_nonblocking(true);
                                set_send_buffer(&s, BLE_SEND_BUFFER);
                                crate::info!("companion: a phone over BLE");
                                out.push(Box::new(s) as BoxStream);
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                            Err(e) => {
                                crate::warn!("companion: BLE accept ({e})");
                                break;
                            }
                        }
                    }
                }
            }
            Transport::BtRfcomm =>
            {
                #[cfg(feature = "linux-bt")]
                if let Some(b) = &self.bt {
                    out.extend(b.accept_all());
                }
            }
        }
        out
    }
}

fn set_send_buffer(socket: &impl AsRawFd, bytes: libc::c_int) {
    // SAFETY: a valid option value of the given size, on an open socket.
    let r = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&bytes as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if r != 0 {
        crate::warn!(
            "companion: cannot set the send buffer ({})",
            io::Error::last_os_error()
        );
    }
}

/// The Bluetooth RFCOMM server (Linux BlueZ socket API), behind the `linux-bt` feature.
#[cfg(feature = "linux-bt")]
mod bt {
    use super::{BoxStream, DEFAULT_RFCOMM_CHANNEL};
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

    const AF_BLUETOOTH: libc::c_int = 31;
    const BTPROTO_RFCOMM: libc::c_int = 3;

    // `struct sockaddr_rc { sa_family_t rc_family; bdaddr_t rc_bdaddr; u8 rc_channel; }`.
    #[repr(C)]
    struct SockaddrRc {
        rc_family: libc::sa_family_t,
        rc_bdaddr: [u8; 6],
        rc_channel: u8,
    }

    /// A nonblocking RFCOMM listening socket.
    pub struct RfcommListener {
        fd: OwnedFd,
    }

    impl RfcommListener {
        pub fn bind() -> io::Result<RfcommListener> {
            // SAFETY: a plain socket() call; an error is returned as -1.
            let raw = unsafe { libc::socket(AF_BLUETOOTH, libc::SOCK_STREAM, BTPROTO_RFCOMM) };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: we own the freshly created descriptor.
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            let addr = SockaddrRc {
                rc_family: AF_BLUETOOTH as libc::sa_family_t,
                rc_bdaddr: [0; 6], // BDADDR_ANY: the first/only adapter.
                rc_channel: DEFAULT_RFCOMM_CHANNEL,
            };
            // SAFETY: a valid sockaddr_rc of the given size on our socket.
            let r = unsafe {
                libc::bind(
                    fd.as_raw_fd(),
                    (&addr as *const SockaddrRc).cast(),
                    std::mem::size_of::<SockaddrRc>() as libc::socklen_t,
                )
            };
            if r != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: listen on our bound socket.
            if unsafe { libc::listen(fd.as_raw_fd(), 2) } != 0 {
                return Err(io::Error::last_os_error());
            }
            set_nonblocking(fd.as_raw_fd())?;
            Ok(RfcommListener { fd })
        }

        pub fn accept_all(&self) -> Vec<BoxStream> {
            let mut out = Vec::new();
            loop {
                // SAFETY: accept on our listening socket; we discard the peer address.
                let raw = unsafe {
                    libc::accept(
                        self.fd.as_raw_fd(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                if raw < 0 {
                    let e = io::Error::last_os_error();
                    if e.kind() != io::ErrorKind::WouldBlock
                        && e.kind() != io::ErrorKind::Interrupted
                    {
                        crate::warn!("companion: RFCOMM accept ({e})");
                    }
                    break;
                }
                if set_nonblocking(raw).is_err() {
                    // SAFETY: close a descriptor we just accepted and will not use.
                    unsafe { libc::close(raw) };
                    continue;
                }
                crate::info!("companion: RFCOMM connection accepted");
                // SAFETY: we own the accepted descriptor.
                out.push(Box::new(BtStream {
                    fd: unsafe { OwnedFd::from_raw_fd(raw) },
                }) as BoxStream);
            }
            out
        }
    }

    impl AsRawFd for RfcommListener {
        fn as_raw_fd(&self) -> RawFd {
            self.fd.as_raw_fd()
        }
    }

    /// An accepted RFCOMM connection.
    pub struct BtStream {
        fd: OwnedFd,
    }

    impl Read for BtStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            // SAFETY: read into a valid buffer of the given length.
            let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }
    }

    impl Write for BtStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            // SAFETY: write from a valid buffer of the given length.
            let n = unsafe { libc::write(self.fd.as_raw_fd(), buf.as_ptr().cast(), buf.len()) };
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl AsRawFd for BtStream {
        fn as_raw_fd(&self) -> RawFd {
            self.fd.as_raw_fd()
        }
    }

    fn set_nonblocking(fd: RawFd) -> io::Result<()> {
        // SAFETY: fcntl on a descriptor we own.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: as above.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_strings() {
        assert_eq!(Transport::BtRfcomm.as_str(), "bt-rfcomm");
        assert_eq!(Transport::WifiTcp.as_str(), "wifi-tcp");
        assert_eq!(Transport::Ble.as_str(), "ble");
    }

    #[test]
    fn tcp_listener_binds_and_reports_its_fd() {
        let l = Listeners::bind(Some(0), false);
        // Port 0 fails the fixed-port expectation? bind(("0.0.0.0", 0)) is valid and picks a
        // free port, so the fd is present.
        assert_eq!(l.fds().len(), 1);
        assert_eq!(l.fds()[0].1, Transport::WifiTcp);
    }

    #[test]
    fn the_ble_socket_replaces_a_stale_one_and_is_private() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("sub/ble.sock");
        let mut l = Listeners::bind(None, false);
        l.bind_ble(&path);
        assert_eq!(l.fds().len(), 1);
        drop(l);
        let mut l = Listeners::bind(None, false);
        l.bind_ble(&path);
        assert_eq!(l.fds()[0].1, Transport::Ble);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _client = std::os::unix::net::UnixStream::connect(&path).unwrap();
        assert_eq!(l.accept(Transport::Ble).len(), 1);
    }

    #[test]
    fn no_listeners_when_disabled() {
        let l = Listeners::bind(None, false);
        assert!(l.fds().is_empty());
    }
}
