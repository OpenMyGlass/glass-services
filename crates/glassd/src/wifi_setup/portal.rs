//! The setup page's socket: a listener bound to the access point's interface, and a few
//! non-blocking connections for the daemon's `poll` loop.

use super::http::{self, Handled, Network, Site};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// Connections at a time. The setup network has one visitor; a browser may open a few.
const MAX_CONNS: usize = 4;
/// A connection gets this long to send its request and read the answer.
const CONN_US: u64 = 10_000_000;

/// Where the page is served.
#[derive(Debug, Clone)]
pub struct Binding {
    /// Packets arriving on any other interface never reach the listener.
    pub device: String,
    pub ip: Ipv4Addr,
    /// 0 picks a free one (tests).
    pub port: u16,
}

/// A TCP listener tied to one interface with `SO_BINDTODEVICE`: the address alone would not do,
/// since Linux accepts a packet for any local address on any interface.
pub fn bind_to_device(b: &Binding) -> io::Result<TcpListener> {
    fn check(r: libc::c_int) -> io::Result<libc::c_int> {
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(r)
        }
    }
    // SAFETY: plain socket calls on a descriptor owned here, with pointers and lengths of live
    // local values.
    unsafe {
        let fd = check(libc::socket(
            libc::AF_INET,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        ))?;
        let owned = OwnedFd::from_raw_fd(fd);
        let one: libc::c_int = 1;
        check(libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            (&raw const one).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        ))?;
        // The name with its NUL. A name that does not exist fails here, so nothing is served.
        let mut name = b.device.clone().into_bytes();
        name.push(0);
        check(libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            name.as_ptr().cast(),
            name.len() as libc::socklen_t,
        ))?;
        let addr = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: b.port.to_be(),
            sin_addr: libc::in_addr {
                s_addr: u32::from(b.ip).to_be(),
            },
            sin_zero: [0; 8],
        };
        check(libc::bind(
            fd,
            (&raw const addr).cast(),
            size_of::<libc::sockaddr_in>() as libc::socklen_t,
        ))?;
        check(libc::listen(fd, MAX_CONNS as libc::c_int))?;
        Ok(TcpListener::from(owned))
    }
}

struct Conn {
    stream: TcpStream,
    /// What the peer sent, which may hold the password. Its whole capacity is reserved at once,
    /// so it is never moved to a bigger buffer and left behind unwiped, and it is wiped when
    /// cleared and when the connection goes, whichever way it goes.
    input: zeroize::Zeroizing<Vec<u8>>,
    /// The answer, once there is one, and how much of it went out.
    output: Vec<u8>,
    sent: usize,
    answered: bool,
    expires_us: u64,
}

impl Conn {
    /// Sends what it can. Returns whether the connection is finished.
    fn flush(&mut self) -> bool {
        while self.sent < self.output.len() {
            match self.stream.write(&self.output[self.sent..]) {
                Ok(0) => return true,
                Ok(n) => self.sent += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return false,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return true,
            }
        }
        self.answered
    }
}

/// The page's server for one session.
pub struct Portal {
    listener: TcpListener,
    conns: Vec<Conn>,
    site: Site,
    /// A form was accepted: everything else now gets [`http::closing`].
    closing: bool,
}

impl Portal {
    /// Serves on `binding`. `token` is the session's form token.
    pub fn bind(binding: &Binding, token: String, lang: http::Lang) -> io::Result<Portal> {
        let listener = bind_to_device(binding)?;
        let SocketAddr::V4(local) = listener.local_addr()? else {
            return Err(io::ErrorKind::InvalidData.into());
        };
        let host = if local.port() == 80 {
            local.ip().to_string()
        } else {
            local.to_string()
        };
        Ok(Portal {
            listener,
            conns: Vec::new(),
            site: Site { host, token, lang },
            closing: false,
        })
    }

    /// The page's address, for the owner to type.
    pub fn url(&self) -> String {
        format!("http://{}/", self.site.host)
    }

    /// The descriptors to poll, with their events.
    pub fn fds(&self) -> Vec<(RawFd, libc::c_short)> {
        let mut v = vec![(self.listener.as_raw_fd(), libc::POLLIN)];
        for c in &self.conns {
            let events = if c.sent < c.output.len() {
                libc::POLLOUT
            } else {
                libc::POLLIN
            };
            v.push((c.stream.as_raw_fd(), events));
        }
        v
    }

    /// A descriptor from [`Portal::fds`] is ready. Returns the network, when a form was accepted.
    pub fn on_ready(&mut self, fd: RawFd, now_us: u64) -> Option<Network> {
        if fd == self.listener.as_raw_fd() {
            self.accept(now_us);
            return None;
        }
        let i = self.conns.iter().position(|c| c.stream.as_raw_fd() == fd)?;
        let (done, network) = self.service(i);
        if done {
            self.conns.swap_remove(i);
        }
        network
    }

    /// Closes the connections past their time.
    pub fn expire(&mut self, now_us: u64) {
        self.conns.retain(|c| now_us < c.expires_us);
    }

    fn accept(&mut self, now_us: u64) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if self.conns.len() >= MAX_CONNS || stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    self.conns.push(Conn {
                        stream,
                        input: zeroize::Zeroizing::new(Vec::with_capacity(http::MAX_REQUEST)),
                        output: Vec::new(),
                        sent: 0,
                        answered: false,
                        expires_us: now_us + CONN_US,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    }

    /// Reads, answers and writes on one connection. Returns whether it is finished, and the
    /// network if its form was accepted.
    fn service(&mut self, i: usize) -> (bool, Option<Network>) {
        let closing = self.closing;
        let mut network = None;
        let c = &mut self.conns[i];
        if !c.answered {
            let mut eof = false;
            let mut buf = zeroize::Zeroizing::new([0u8; 1024]);
            while c.input.len() < http::MAX_REQUEST {
                match c.stream.read(&mut buf[..]) {
                    Ok(0) => {
                        eof = true;
                        break;
                    }
                    Ok(n) => {
                        let room = http::MAX_REQUEST - c.input.len();
                        c.input.extend_from_slice(&buf[..n.min(room)]);
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => return (true, None),
                }
            }
            match http::handle(&self.site, &c.input) {
                // Full and still not whole: it would stay readable with nothing reading it, and
                // the loop would spin. `http::handle` should not let this happen.
                Handled::NeedMore if eof || c.input.len() >= http::MAX_REQUEST => {
                    return (true, None);
                }
                Handled::NeedMore => return (false, None),
                Handled::Respond(out) => c.output = out,
                Handled::Accepted(_, _) if closing => c.output = http::closing(),
                Handled::Accepted(out, net) => {
                    c.output = out;
                    network = Some(net);
                }
            }
            c.answered = true;
            // What it sent holds the password: not kept while the answer goes out.
            zeroize::Zeroize::zeroize(&mut *c.input);
        }
        let done = c.flush();
        if network.is_some() {
            self.closing = true;
        }
        (done, network)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::time::Duration;

    fn lo() -> Binding {
        Binding {
            device: "lo".into(),
            ip: Ipv4Addr::LOCALHOST,
            port: 0,
        }
    }

    /// Runs the portal until `done`, feeding it what `poll` reports.
    fn pump(p: &mut Portal, ms: u64, mut done: impl FnMut() -> bool) -> Option<Network> {
        let start = std::time::Instant::now();
        let mut net = None;
        while start.elapsed() < Duration::from_millis(ms) && !done() {
            let list = p.fds();
            let mut fds: Vec<_> = list
                .iter()
                .map(|&(fd, events)| libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                })
                .collect();
            // SAFETY: a valid array of pollfd of this length.
            unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 20) };
            for f in fds.iter().filter(|f| f.revents != 0) {
                net = net.or(p.on_ready(f.fd, 0));
            }
        }
        net
    }

    #[test]
    fn the_listener_is_tied_to_its_device() {
        let l = bind_to_device(&lo()).unwrap();
        let mut name = [0u8; 16];
        let mut len = name.len() as libc::socklen_t;
        // SAFETY: a live buffer and its length.
        let r = unsafe {
            libc::getsockopt(
                l.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_BINDTODEVICE,
                name.as_mut_ptr().cast(),
                &mut len,
            )
        };
        assert_eq!(r, 0);
        assert_eq!(&name[..2], b"lo");
        assert_eq!(name[2], 0);
    }

    #[test]
    fn a_missing_interface_serves_nothing() {
        let b = Binding {
            device: "nosuchif0".into(),
            ..lo()
        };
        assert!(bind_to_device(&b).is_err());
    }

    #[test]
    fn serves_a_page_and_takes_a_form_in_pieces() {
        let mut p = Portal::bind(&lo(), "abcd".into(), http::Lang::It).unwrap();
        let addr = p.listener.local_addr().unwrap();
        let host = addr.to_string();
        assert_eq!(p.url(), format!("http://{host}/"));

        let page = std::thread::scope(|s| {
            let client = s.spawn(|| {
                let mut c = TcpStream::connect(addr).unwrap();
                write!(c, "GET / HTTP/1.1\r\nHost: {host}\r\n\r\n").unwrap();
                let mut out = String::new();
                c.read_to_string(&mut out).unwrap();
                out
            });
            pump(&mut p, 3000, || client.is_finished());
            client.join().unwrap()
        });
        assert!(page.contains("200 OK") && page.contains("Nome della rete"));
        assert!(p.conns.is_empty());

        let form = "token=abcd&ssid=Net&password=password1";
        let (net, answer) = std::thread::scope(|s| {
            let client = s.spawn(|| {
                let mut c = TcpStream::connect(addr).unwrap();
                write!(
                    c,
                    "POST /join HTTP/1.1\r\nHost: {host}\r\n\
                     Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n",
                    form.len()
                )
                .unwrap();
                std::thread::sleep(Duration::from_millis(50));
                c.write_all(form.as_bytes()).unwrap();
                let mut out = String::new();
                c.read_to_string(&mut out).unwrap();
                out
            });
            let net = pump(&mut p, 3000, || client.is_finished());
            (net, client.join().unwrap())
        });
        assert_eq!(net.unwrap().ssid, "Net");
        assert!(answer.contains("200 OK"));

        // The session is closing now: one network per session.
        let second = std::thread::scope(|s| {
            let client = s.spawn(|| {
                let mut c = TcpStream::connect(addr).unwrap();
                write!(
                    c,
                    "POST /join HTTP/1.1\r\nHost: {host}\r\n\
                     Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{form}",
                    form.len()
                )
                .unwrap();
                let mut out = String::new();
                c.read_to_string(&mut out).unwrap();
                out
            });
            let net = pump(&mut p, 3000, || client.is_finished());
            assert!(net.is_none());
            client.join().unwrap()
        });
        assert!(second.contains("409"));
    }

    #[test]
    fn the_request_buffer_never_grows_past_what_was_reserved() {
        let mut p = Portal::bind(&lo(), "abcd".into(), http::Lang::En).unwrap();
        let addr = p.listener.local_addr().unwrap();
        let mut c = TcpStream::connect(addr).unwrap();
        pump(&mut p, 100, || false);
        assert_eq!(p.conns.len(), 1);
        let reserved = p.conns[0].input.capacity();
        assert!(reserved >= http::MAX_REQUEST);
        // More than the limit, with no end of head: refused, after reading up to the limit.
        let _ = c.write_all(&vec![b'a'; 2 * http::MAX_REQUEST]);
        let mut seen = 0;
        for _ in 0..50 {
            pump(&mut p, 10, || false);
            if let Some(conn) = p.conns.first() {
                seen = seen.max(conn.input.len());
                assert_eq!(conn.input.capacity(), reserved, "no reallocation");
            } else {
                break;
            }
        }
        assert!(seen <= http::MAX_REQUEST);
    }

    #[test]
    fn a_silent_connection_ends_and_the_count_is_bounded() {
        let mut p = Portal::bind(&lo(), "abcd".into(), http::Lang::En).unwrap();
        let addr = p.listener.local_addr().unwrap();
        // One by one: more than the backlog at once would have `connect` wait.
        let mut silent = Vec::new();
        for _ in 0..MAX_CONNS + 3 {
            silent.push(TcpStream::connect(addr).unwrap());
            pump(&mut p, 50, || false);
        }
        assert_eq!(p.conns.len(), MAX_CONNS);
        p.expire(CONN_US - 1);
        assert_eq!(p.conns.len(), MAX_CONNS);
        p.expire(CONN_US);
        assert!(p.conns.is_empty());
        drop(silent);
    }
}
