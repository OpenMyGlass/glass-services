//! The companion-link listeners: a Bluetooth RFCOMM server (the normal, lower-power link) and
//! a TCP accept (the fallback for a PC without Bluetooth, and the on-demand high-bandwidth
//! path). Both hand the daemon a nonblocking byte [`Stream`] whose raw fd goes into the
//! single-threaded `poll` loop, exactly like the launcher socket.
//!
//! Bluetooth is default/preferred (it is up anyway for A2DP/HFP, and cheap for the bursty
//! companion traffic); Wi-Fi/TCP is the fallback and the high-bandwidth path (media/OTA). The
//! RFCOMM server advertises the clean-room OpenMyGlass service UUID
//! [`OMG_COMPANION_RFCOMM_UUID`] (**not** Google's companion UUID) and is gated behind the
//! `linux-bt` feature so images without a guaranteed BlueZ stack stay lean; TCP always works.
//!
//! With `--mock` (or no `--companion`) the daemon creates no listeners, so its tests never open
//! a real socket or touch BlueZ.

use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::os::fd::{AsRawFd, RawFd};

/// OpenMyGlass's own RFCOMM service UUID (clean-room; NOT Google's companion UUID). glassd is
/// the RFCOMM server; the companion connects to it.
pub const OMG_COMPANION_RFCOMM_UUID: &str = "6f6d672d-636f-6d70-616e-696f6e2d7631";

/// Default RFCOMM channel the server binds (advertised over SDP under the UUID above).
pub const DEFAULT_RFCOMM_CHANNEL: u8 = 23;

/// Default TCP port for the Wi-Fi / PC fallback path.
pub const DEFAULT_TCP_PORT: u16 = 7700;

/// Which radio a connection arrived over (reported to the launcher as the link transport).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Bluetooth Classic RFCOMM.
    BtRfcomm,
    /// Wi-Fi / TCP.
    WifiTcp,
}

impl Transport {
    /// The string reported to the launcher in `CompanionState.transport`.
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::BtRfcomm => "bt-rfcomm",
            Transport::WifiTcp => "wifi-tcp",
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
    #[cfg(feature = "linux-bt")]
    bt: Option<bt::RfcommListener>,
}

impl Listeners {
    /// Binds the enabled listeners. TCP binds `0.0.0.0:port`; RFCOMM binds
    /// [`DEFAULT_RFCOMM_CHANNEL`] when built with `linux-bt`. A listener that cannot bind is
    /// logged and skipped (best effort), so one transport being unavailable never stops the
    /// other.
    pub fn bind(tcp_port: Option<u16>, bt: bool) -> Listeners {
        let tcp = tcp_port.and_then(|port| match TcpListener::bind(("0.0.0.0", port)) {
            Ok(l) => {
                let _ = l.set_nonblocking(true);
                crate::info!("companion: TCP listening on 0.0.0.0:{port}");
                Some(l)
            }
            Err(e) => {
                crate::warn!("companion: cannot bind TCP :{port} ({e})");
                None
            }
        });
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
            #[cfg(feature = "linux-bt")]
            bt,
        }
    }

    /// The raw fds to add to the poll set, each tagged with its transport.
    pub fn fds(&self) -> Vec<(RawFd, Transport)> {
        let mut v = Vec::new();
        if let Some(t) = &self.tcp {
            v.push((t.as_raw_fd(), Transport::WifiTcp));
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
    fn no_listeners_when_disabled() {
        let l = Listeners::bind(None, false);
        assert!(l.fds().is_empty());
    }
}
