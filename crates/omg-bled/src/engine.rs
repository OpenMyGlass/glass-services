//! The peripheral: advertising, one connection, GATT, and a byte stream with flow control. No I/O.
//!
//! Every byte of the stream is carried as is: each notification or write is the next run of bytes,
//! with no header. A session is one LE connection with `tx` subscribed. Its bytes go nowhere else,
//! and nothing carries over to the next: the encrypted link above starts again, and note sync
//! resumes from disk.
//!
//! **Flow control.** Toward the phone: bytes are taken from glassd only while [`Engine::session_room`]
//! says so, and fragments go to the controller only against its credits (Number Of Completed
//! Packets), so a stalled radio stalls glassd's socket and nothing grows here. Toward glassd:
//! when [`Engine::wants_hci_read`] says no, the caller stops reading the UART, RTS and the
//! controller then hold the phone back at the link layer. glassd never waits for this process, so
//! that cannot deadlock.
//!
//! **Time.** Only one-shot deadlines: nothing here wakes the host while the link is quiet.
//!
//! **A lost controller.** A command with no answer, bytes the UART does not take, a link that never
//! confirms its end, or a stream of garbage: each sets [`Engine::fatal`]. The process ends, which
//! frees the UART, and glassd powers the chip off and tries again later.

use std::collections::VecDeque;

use crate::att;
use crate::hci::{self, H4Parser, Packet};

/// Bytes on their way to the phone, held here. About a third of a second at 3 kB/s, so a
/// notification behind them does not wait long. glassd's socket and the controller's credits add
/// a little more.
pub const NOTIFY_QUEUE_BYTES: usize = 1024;
/// Bytes on their way to glassd above which the UART is left unread.
pub const TO_SESSION_MAX: usize = 16 * 1024;
/// Bytes queued for the controller at once: small, because they are out of our hands by then.
const HCI_OUT_MAX: usize = 2048;

/// Advertising right after a link ends, and how long. Phones find it within a second.
const FAST_ADV_MS: u16 = 100;
const FAST_ADV_FOR_MS: u64 = 30_000;
/// The longest interval in Apple's list (QA1931), in 0.625 ms units: 1285 ms.
const SLOW_ADV_UNITS: u16 = 2056;
/// A central that connects and does not subscribe holds the only connection, which stops the
/// advertising: it goes after this long.
const SUBSCRIBE_WITHIN_MS: u64 = 30_000;
/// glassd closed: how long the last bytes get to leave before the link is cut anyway.
const CLOSE_DRAIN_MS: u64 = 2_000;
/// A quiet stream asks for the idle connection parameters after this long.
const IDLE_AFTER_MS: u64 = 5_000;
/// Queued bytes that ask for the busy connection parameters.
const BUSY_BACKLOG: usize = 200;

/// Connection parameters as the L2CAP request carries them: min, max (1.25 ms), latency, timeout
/// (10 ms). Within Apple's rules (QA1931), so that they also hold for an iPhone.
const PARAMS_BUSY: [u16; 4] = [12, 24, 0, 400];
const PARAMS_IDLE: [u16; 4] = [228, 240, 5, 600];

/// How long the controller gets to answer a command. Linux gives it 2 s; nothing here is urgent.
pub const COMMAND_TIMEOUT_MS: u64 = 5_000;
/// How long the UART may take no bytes at all: the chip holding CTS, or gone.
pub const OUTPUT_STALL_MS: u64 = 5_000;
/// How long the end of a link may take. A peer that stops answering is only declared gone at the
/// supervision timeout, and the longest one a central can set is 32 s.
pub const CUT_TIMEOUT_MS: u64 = 40_000;
/// Bytes that were no packet, in one window, above which the stream is lost for good: a wrong baud
/// rate, or a chip that restarted. The occasional lost byte stays below it.
pub const DESYNC_MAX: u64 = 64;
const DESYNC_WINDOW_MS: u64 = 60_000;

const REASON_USER_ENDED: u8 = 0x13;

const CID_ATT: u16 = 4;
const CID_SIGNALING: u16 = 5;
const CID_SMP: u16 = 6;

const SIG_REJECT: u8 = 0x01;
const SIG_PARAM_REQUEST: u8 = 0x12;
const SIG_PARAM_RESPONSE: u8 = 0x13;

/// Longest L2CAP PDU a phone may send: the MTU plus the header.
const MAX_REASSEMBLY: usize = att::MAX_ATT_PDU + 4;
/// Answers waiting for controller credits. ATT allows one request at a time, so a peer that gets
/// this far is not following the protocol while it holds the credits back: the link is cut.
pub const CONTROL_QUEUE_MAX: usize = 32;

/// At most this many lines per window. A line reaches the console with interrupts off, and what
/// causes one can be in the hands of whoever is in radio range.
pub const LOG_LINES_PER_WINDOW: u32 = 20;
pub const LOG_WINDOW_MS: u64 = 10_000;

/// What the caller must do with the session stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// A phone subscribed: connect a new stream to glassd, then call
    /// [`Engine::session_opened`] or [`Engine::session_failed`].
    OpenSession,
    /// The session ended here: close the stream, without flushing anything.
    CloseSession,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Session {
    /// Not subscribed.
    None,
    /// Waiting for the caller to connect.
    Opening,
    Open,
    /// glassd closed: the rest goes out, then the link is cut.
    Draining {
        until_ms: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Params {
    Default,
    Busy,
    Idle,
}

struct Conn {
    handle: u16,
    mtu: u16,
    reassembly: Vec<u8>,
    session: Session,
    subscribe_by_ms: u64,
    /// `Disconnect` was sent: nothing more goes out.
    cutting: bool,
    /// The controller must have ended the link by then.
    cut_by_ms: Option<u64>,
    params: Params,
    params_asked: Option<Params>,
    params_refused: bool,
    last_activity_ms: u64,
    sig_id: u8,
}

/// See the module docs.
pub struct Engine {
    parser: H4Parser,
    db: att::Db,
    name: String,
    address: [u8; 6],
    acl_len: usize,
    max_credits: u16,
    credits: u16,
    conn: Option<Conn>,
    /// Answers and signalling: these overtake the stream, at a PDU's boundary.
    control: VecDeque<Vec<Vec<u8>>>,
    /// Notifications, as ACL packets per PDU.
    notify: VecDeque<Vec<Vec<u8>>>,
    /// Payload bytes in `notify`.
    notify_bytes: usize,
    /// The PDU going out now: its remaining ACL packets, first at the back.
    current: Vec<Vec<u8>>,
    commands: VecDeque<Vec<u8>>,
    /// The command in flight must be answered by then.
    command_by_ms: Option<u64>,
    /// The command the controller has not answered: its opcode and first parameter byte.
    in_flight: Option<(u16, u8)>,
    hci_out: Vec<u8>,
    /// `hci_out` must have moved by then.
    output_by_ms: Option<u64>,
    /// The latest time seen, for what happens without one being passed in.
    now_ms: u64,
    desync_seen: u64,
    desync_window_ends_ms: u64,
    to_session: Vec<u8>,
    actions: Vec<Action>,
    logs: Vec<String>,
    log_window_ends_ms: u64,
    log_lines_left: u32,
    log_lines_dropped: u32,
    fatal: Option<String>,
    started: bool,
    /// Advertising fast, until this time.
    fast_adv_until_ms: Option<u64>,
}

impl Engine {
    /// `acl_len` and `credits` are what the controller reported. Advertising starts at once.
    pub fn new(name: &str, address: [u8; 6], acl_len: usize, credits: u16, now_ms: u64) -> Engine {
        let mut e = Engine {
            parser: H4Parser::default(),
            db: att::Db::new(name),
            name: name.to_string(),
            address,
            acl_len,
            max_credits: credits,
            credits,
            conn: None,
            control: VecDeque::new(),
            notify: VecDeque::new(),
            notify_bytes: 0,
            current: Vec::new(),
            commands: VecDeque::new(),
            command_by_ms: None,
            in_flight: None,
            hci_out: Vec::new(),
            output_by_ms: None,
            now_ms,
            desync_seen: 0,
            desync_window_ends_ms: now_ms + DESYNC_WINDOW_MS,
            to_session: Vec::new(),
            actions: Vec::new(),
            logs: Vec::new(),
            log_window_ends_ms: now_ms + LOG_WINDOW_MS,
            log_lines_left: LOG_LINES_PER_WINDOW,
            log_lines_dropped: 0,
            fatal: None,
            started: false,
            fast_adv_until_ms: None,
        };
        e.queue_setup();
        e.advertise_fast(now_ms);
        e.pump();
        e
    }

    fn queue_setup(&mut self) {
        // Everything but the rarely useful bits, as stock does.
        self.cmd(
            hci::OP_SET_EVENT_MASK,
            &[0xFF, 0xFF, 0xFB, 0xFF, 0x07, 0xF8, 0xBF, 0x3D],
        );
        // Connection complete, advertising report, update complete, remote features, LTK request.
        self.cmd(hci::OP_LE_SET_EVENT_MASK, &[0x1F, 0, 0, 0, 0, 0, 0, 0]);
        let address = self.address;
        self.cmd(hci::OP_LE_SET_RANDOM_ADDRESS, &address);

        let mut ad = vec![2, 1, 6, 17, 7];
        ad.extend_from_slice(&att::uuid128(0));
        self.cmd(hci::OP_LE_SET_ADV_DATA, &adv_field(&ad));
        let name = self.name.clone();
        let mut sr = vec![name.len() as u8 + 1, 9];
        sr.extend_from_slice(name.as_bytes());
        self.cmd(hci::OP_LE_SET_SCAN_RESPONSE, &adv_field(&sr));
    }

    fn cmd(&mut self, op: u16, params: &[u8]) {
        self.commands.push_back(hci::command(op, params));
    }

    /// What was queued for the advertising is no longer wanted. What was queued for a link still
    /// is: a second connection racing the advertising stays up unless its `Disconnect` goes out.
    fn forget_advertising_commands(&mut self) {
        self.commands.retain(|c| link_command_handle(c).is_some());
    }

    fn set_adv_interval(&mut self, units: u16) {
        let mut p = Vec::with_capacity(15);
        p.extend_from_slice(&units.to_le_bytes());
        p.extend_from_slice(&units.to_le_bytes());
        // Connectable undirected, own address random, no direct peer, all channels, no filter.
        p.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0, 0, 7, 0]);
        self.cmd(hci::OP_LE_SET_ADV_PARAMS, &p);
    }

    fn advertise_fast(&mut self, now_ms: u64) {
        self.set_adv_interval((FAST_ADV_MS as u32 * 8 / 5) as u16);
        self.cmd(hci::OP_LE_SET_ADV_ENABLE, &[1]);
        self.fast_adv_until_ms = Some(now_ms + FAST_ADV_FOR_MS);
    }

    // ------------------------------ the controller's side ------------------------------

    /// Bytes from the UART.
    pub fn on_hci(&mut self, bytes: &[u8], now_ms: u64) {
        self.advance_clock(now_ms);
        if now_ms >= self.desync_window_ends_ms {
            self.desync_window_ends_ms = now_ms + DESYNC_WINDOW_MS;
            self.desync_seen = self.parser.desync;
        }
        self.parser.push(bytes);
        while let Some(packet) = self.parser.next_packet() {
            match packet {
                Packet::Event { code, params } => self.on_event(code, &params, now_ms),
                Packet::Acl { handle_flags, data } => self.on_acl(handle_flags, &data, now_ms),
            }
        }
        self.check_desync();
        self.pump();
    }

    /// What should be written to the UART next.
    pub fn hci_output(&self) -> &[u8] {
        &self.hci_out
    }

    /// `n` bytes of [`Engine::hci_output`] were written.
    pub fn hci_written(&mut self, n: usize) {
        self.hci_out.drain(..n);
        if n > 0 {
            // It moved: the wait starts again.
            self.output_by_ms = None;
        }
        self.pump();
    }

    /// Whether to read the UART. No while glassd is not keeping up.
    pub fn wants_hci_read(&self) -> bool {
        self.to_session.len() < TO_SESSION_MAX
    }

    // ------------------------------ glassd's side ------------------------------

    /// Bytes to take from glassd now, at most.
    pub fn session_room(&self) -> usize {
        match self.conn.as_ref().map(|c| c.session) {
            Some(Session::Open) => NOTIFY_QUEUE_BYTES.saturating_sub(self.notify_bytes),
            _ => 0,
        }
    }

    /// The caller connected the stream.
    pub fn session_opened(&mut self, now_ms: u64) {
        if let Some(c) = &mut self.conn
            && c.session == Session::Opening
        {
            c.session = Session::Open;
            c.last_activity_ms = now_ms;
        } else {
            // The link ended while connecting.
            self.actions.push(Action::CloseSession);
        }
    }

    /// The stream could not be connected: glassd is not listening yet. The phone tries again.
    pub fn session_failed(&mut self) {
        self.log("glassd is not listening: dropping the phone".to_string());
        self.cut_link();
        self.pump();
    }

    /// Bytes from glassd for the phone, at most [`Engine::session_room`].
    pub fn session_data(&mut self, bytes: &[u8], now_ms: u64) {
        let Some(c) = &mut self.conn else { return };
        if c.session != Session::Open {
            return;
        }
        c.last_activity_ms = now_ms;
        let run = c.mtu as usize - 3;
        let handle = c.handle;
        for chunk in bytes.chunks(run) {
            let mut pdu = Vec::with_capacity(3 + chunk.len());
            pdu.push(att::OP_NOTIFY);
            pdu.extend_from_slice(&att::TX_HANDLE.to_le_bytes());
            pdu.extend_from_slice(chunk);
            self.notify
                .push_back(hci::acl_fragments(handle, CID_ATT, &pdu, self.acl_len));
            self.notify_bytes += chunk.len();
        }
        if self.notify_bytes >= BUSY_BACKLOG {
            self.ask_params(Params::Busy);
        }
        self.pump();
    }

    /// glassd closed its end. What it sent last goes out first. What was on its way to it has
    /// nowhere to go, and must not keep the UART unread, or the link's end would never be seen.
    pub fn session_closed(&mut self, now_ms: u64) {
        self.to_session.clear();
        if let Some(c) = &mut self.conn
            && matches!(c.session, Session::Open | Session::Opening)
        {
            c.session = Session::Draining {
                until_ms: now_ms + CLOSE_DRAIN_MS,
            };
        }
        self.pump();
    }

    /// Bytes from the phone for glassd.
    pub fn to_session(&self) -> &[u8] {
        &self.to_session
    }

    /// `n` bytes of [`Engine::to_session`] were written.
    pub fn to_session_written(&mut self, n: usize) {
        self.to_session.drain(..n);
    }

    // ------------------------------ time and the rest ------------------------------

    /// When [`Engine::tick`] has something to do. Never recurs by itself.
    pub fn next_deadline(&self) -> Option<u64> {
        let conn = self.conn.as_ref().and_then(|c| {
            if c.cutting {
                return c.cut_by_ms;
            }
            match c.session {
                Session::None => Some(c.subscribe_by_ms),
                Session::Draining { until_ms } => Some(until_ms),
                Session::Open
                    if c.params != Params::Idle
                        && c.params_asked.is_none()
                        && !c.params_refused =>
                {
                    Some(c.last_activity_ms + IDLE_AFTER_MS)
                }
                _ => None,
            }
        });
        [
            self.fast_adv_until_ms,
            conn,
            self.command_by_ms,
            self.output_by_ms,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Time passed.
    pub fn tick(&mut self, now_ms: u64) {
        self.advance_clock(now_ms);
        self.check_lost_controller(now_ms);
        if self.fast_adv_until_ms.is_some_and(|t| now_ms >= t) {
            self.fast_adv_until_ms = None;
            self.cmd(hci::OP_LE_SET_ADV_ENABLE, &[0]);
            self.set_adv_interval(SLOW_ADV_UNITS);
            self.cmd(hci::OP_LE_SET_ADV_ENABLE, &[1]);
        }
        if let Some(c) = &self.conn {
            match c.session {
                Session::None if !c.cutting && now_ms >= c.subscribe_by_ms => {
                    self.log("the phone did not subscribe: dropping it".to_string());
                    self.cut_link();
                }
                Session::Draining { until_ms } if !c.cutting && now_ms >= until_ms => {
                    self.cut_link();
                }
                Session::Open if now_ms >= c.last_activity_ms + IDLE_AFTER_MS => {
                    self.ask_params(Params::Idle);
                }
                _ => {}
            }
        }
        self.pump();
    }

    /// What the caller must do about the stream.
    pub fn take_actions(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.actions)
    }

    /// Lines for the log. Few, and limited per window: a line reaches the console with interrupts
    /// off.
    pub fn take_logs(&mut self) -> Vec<String> {
        std::mem::take(&mut self.logs)
    }

    /// The controller is lost. The process should end, and the chip be reset by the next run.
    pub fn fatal(&self) -> Option<&str> {
        self.fatal.as_deref()
    }

    /// Advertising is on for the first time since the start.
    pub fn started(&self) -> bool {
        self.started
    }

    fn advance_clock(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
        self.advance_log_window(now_ms);
    }

    fn check_lost_controller(&mut self, now_ms: u64) {
        if self.fatal.is_some() {
            return;
        }
        let late = |by: Option<u64>| by.is_some_and(|t| now_ms >= t);
        if late(self.command_by_ms) {
            self.fatal = Some("the controller did not answer a command".to_string());
        } else if late(self.output_by_ms) {
            self.fatal = Some("the UART took no bytes: the controller holds CTS".to_string());
        } else if late(self.conn.as_ref().and_then(|c| c.cut_by_ms)) {
            self.fatal = Some("the controller did not end the link".to_string());
        }
    }

    fn check_desync(&mut self) {
        let lost = self.parser.desync - self.desync_seen;
        if lost > DESYNC_MAX && self.fatal.is_none() {
            self.fatal = Some(format!("{lost} bytes from the UART were no packet"));
        }
    }

    /// One command to the UART's queue, unless one is in flight.
    fn send_next_command(&mut self) {
        if self.in_flight.is_some() {
            return;
        }
        if let Some(c) = self.commands.pop_front() {
            self.in_flight = Some((
                u16::from_le_bytes([c[1], c[2]]),
                c.get(4).copied().unwrap_or(0),
            ));
            self.hci_out.extend_from_slice(&c);
            self.command_by_ms = Some(self.now_ms + COMMAND_TIMEOUT_MS);
        }
    }

    /// Waiting bytes have a deadline, and no bytes have none.
    fn watch_output(&mut self) {
        if self.hci_out.is_empty() {
            self.output_by_ms = None;
        } else if self.output_by_ms.is_none() {
            self.output_by_ms = Some(self.now_ms + OUTPUT_STALL_MS);
        }
    }

    fn log(&mut self, line: String) {
        if self.log_lines_left == 0 {
            self.log_lines_dropped += 1;
            return;
        }
        self.log_lines_left -= 1;
        self.logs.push(line);
    }

    /// Needs no timer: what was dropped is said with the next thing that happens.
    fn advance_log_window(&mut self, now_ms: u64) {
        if now_ms < self.log_window_ends_ms {
            return;
        }
        self.log_window_ends_ms = now_ms + LOG_WINDOW_MS;
        self.log_lines_left = LOG_LINES_PER_WINDOW;
        let dropped = std::mem::take(&mut self.log_lines_dropped);
        if dropped > 0 {
            self.log(format!("{dropped} log lines dropped"));
        }
    }

    /// Moves what may go now to the UART's queue: one command at a time, and ACL against credits.
    fn pump(&mut self) {
        self.send_next_command();
        let cutting = self.conn.as_ref().is_none_or(|c| c.cutting);
        while !cutting && self.credits > 0 && self.hci_out.len() < HCI_OUT_MAX {
            if self.current.is_empty() {
                if let Some(mut pdu) = self.control.pop_front() {
                    pdu.reverse();
                    self.current = pdu;
                } else if let Some(mut pdu) = self.notify.pop_front() {
                    // Credit for the room goes when the PDU starts: it is on its way.
                    self.notify_bytes -= notify_payload(&pdu);
                    pdu.reverse();
                    self.current = pdu;
                } else {
                    break;
                }
            }
            let Some(p) = self.current.pop() else { break };
            self.hci_out.extend_from_slice(&p);
            self.credits -= 1;
        }
        self.finish_draining();
        self.watch_output();
    }

    fn finish_draining(&mut self) {
        let Some(c) = &self.conn else { return };
        if matches!(c.session, Session::Draining { .. })
            && !c.cutting
            && self.current.is_empty()
            && self.control.is_empty()
            && self.notify.is_empty()
            && self.credits == self.max_credits
        {
            self.cut_link();
        }
    }

    /// Ends the LE link. The session ends with the event that confirms it.
    fn cut_link(&mut self) {
        let Some(c) = &mut self.conn else { return };
        if c.cutting {
            return;
        }
        c.cutting = true;
        c.cut_by_ms = Some(self.now_ms + CUT_TIMEOUT_MS);
        let handle = c.handle;
        let mut p = handle.to_le_bytes().to_vec();
        p.push(REASON_USER_ENDED);
        self.cmd(hci::OP_DISCONNECT, &p);
        self.control.clear();
        self.notify.clear();
        self.notify_bytes = 0;
        self.current.clear();
        self.send_next_command();
        self.watch_output();
    }

    fn send_control(&mut self, cid: u16, payload: &[u8]) {
        if self.control.len() >= CONTROL_QUEUE_MAX {
            self.log("too many answers waiting for the radio: cutting the link".to_string());
            self.cut_link();
            return;
        }
        let Some(c) = &self.conn else { return };
        if c.cutting {
            return;
        }
        self.control
            .push_back(hci::acl_fragments(c.handle, cid, payload, self.acl_len));
    }

    fn ask_params(&mut self, want: Params) {
        let Some(c) = &mut self.conn else { return };
        if c.params == want || c.params_asked.is_some() || c.params_refused || c.cutting {
            return;
        }
        c.sig_id = c.sig_id.wrapping_add(1).max(1);
        c.params_asked = Some(want);
        let values = match want {
            Params::Busy => PARAMS_BUSY,
            _ => PARAMS_IDLE,
        };
        let mut p = vec![SIG_PARAM_REQUEST, c.sig_id, 8, 0];
        for v in values {
            p.extend_from_slice(&v.to_le_bytes());
        }
        self.send_control(CID_SIGNALING, &p);
    }

    fn on_event(&mut self, code: u8, e: &[u8], now_ms: u64) {
        match code {
            hci::EV_COMMAND_COMPLETE if e.len() >= 3 => {
                let op = u16::from_le_bytes([e[1], e[2]]);
                let status = e.get(3).copied().unwrap_or(0);
                self.command_done(op, status);
            }
            hci::EV_COMMAND_STATUS if e.len() >= 4 => {
                let op = u16::from_le_bytes([e[2], e[3]]);
                self.command_done(op, e[0]);
            }
            hci::EV_COMPLETED_PACKETS => {
                let n = e.first().copied().unwrap_or(0) as usize;
                for i in 0..n {
                    if let Some(b) = e.get(1 + i * 4..5 + i * 4) {
                        let handle = u16::from_le_bytes([b[0], b[1]]) & 0x0FFF;
                        // A late count for a link already gone was given back when it ended.
                        if self.conn.as_ref().is_some_and(|c| c.handle == handle) {
                            let done = u16::from_le_bytes([b[2], b[3]]);
                            self.credits = (self.credits + done).min(self.max_credits);
                        }
                    }
                }
            }
            hci::EV_DISCONNECT_COMPLETE if e.len() >= 4 => {
                let handle = u16::from_le_bytes([e[1], e[2]]) & 0x0FFF;
                // Nothing queued may reach that handle now: the next link can get it.
                if e[0] == 0 {
                    self.commands
                        .retain(|c| link_command_handle(c) != Some(handle));
                }
                let ours = self.conn.as_ref().filter(|c| c.handle == handle);
                match (e[0], ours.map(|c| c.cutting)) {
                    (0, Some(_)) => self.on_disconnected(e[3], now_ms),
                    // It was asked to end the link and says it cannot: no way forward from here.
                    (status, Some(true)) => {
                        self.fatal = Some(format!(
                            "the controller could not end the link: {status:02x}"
                        ));
                    }
                    (status, Some(false)) => {
                        self.log(format!("a failed disconnect, status {status:02x}"));
                    }
                    _ => {}
                }
            }
            hci::EV_HARDWARE_ERROR => {
                self.fatal = Some(format!(
                    "controller hardware error {:02x}",
                    e.first().copied().unwrap_or(0)
                ));
            }
            hci::EV_LE_META => self.on_le_event(e, now_ms),
            _ => {}
        }
    }

    fn command_done(&mut self, op: u16, status: u8) {
        let Some((_, first)) = self.in_flight.filter(|&(sent, _)| sent == op) else {
            // An answer to nothing in flight: a stray, or opcode 0, which only hands out credits.
            // The command in flight still waits for its own, and keeps its deadline.
            if op != 0 {
                self.log(format!(
                    "an answer to command {op:04x}, which is not in flight"
                ));
            }
            return;
        };
        self.in_flight = None;
        self.command_by_ms = None;
        let first_param = Some(first);
        // Refused because a connection got there first and stopped it: off is what was asked for.
        if op == hci::OP_LE_SET_ADV_ENABLE
            && first_param == Some(0)
            && status == hci::STATUS_COMMAND_DISALLOWED
        {
            return;
        }
        if status != 0 {
            let line = match first_param {
                Some(p) => format!("command {op:04x} ({p:02x}) failed with status {status:02x}"),
                None => format!("command {op:04x} failed with status {status:02x}"),
            };
            // The setup has no second chance. Later, a failure races with a connection arriving.
            if !self.started {
                self.fatal = Some(line);
            } else {
                self.log(line);
            }
        } else if op == hci::OP_LE_SET_ADV_ENABLE {
            self.started = true;
        }
    }

    fn on_le_event(&mut self, e: &[u8], now_ms: u64) {
        match e.first() {
            // Connection complete.
            Some(1) if e.len() >= 18 => {
                let handle = u16::from_le_bytes([e[2], e[3]]) & 0x0FFF;
                let interval = u16::from_le_bytes([e[12], e[13]]);
                if e[1] != 0 {
                    self.log(format!("a connection failed with status {:02x}", e[1]));
                    return;
                }
                if self.conn.is_some() {
                    // A race with re-enabling the advertising. One phone at a time.
                    let mut p = handle.to_le_bytes().to_vec();
                    p.push(REASON_USER_ENDED);
                    self.cmd(hci::OP_DISCONNECT, &p);
                    return;
                }
                self.log(format!(
                    "phone connected, interval {} ms",
                    interval as u32 * 5 / 4
                ));
                self.fast_adv_until_ms = None;
                // Whatever was on its way to the advertising is no longer wanted. The connection
                // stopped it, and BCM4330 answered 0c to turning it off here (bled36, one link).
                // Only an enable the controller has yet to answer can bring it back.
                self.forget_advertising_commands();
                if self.in_flight == Some((hci::OP_LE_SET_ADV_ENABLE, 1)) {
                    self.cmd(hci::OP_LE_SET_ADV_ENABLE, &[0]);
                }
                self.db.cccd = 0;
                self.conn = Some(Conn {
                    handle,
                    mtu: 23,
                    reassembly: Vec::new(),
                    session: Session::None,
                    subscribe_by_ms: now_ms + SUBSCRIBE_WITHIN_MS,
                    cutting: false,
                    cut_by_ms: None,
                    params: Params::Default,
                    params_asked: None,
                    params_refused: false,
                    last_activity_ms: now_ms,
                    sig_id: 0,
                });
                self.credits = self.max_credits;
            }
            // Connection update complete.
            Some(3) if e.len() >= 10 => {
                self.log(format!(
                    "link parameters: status {:02x}, interval {} ms, latency {}, timeout {} ms",
                    e[1],
                    u16::from_le_bytes([e[4], e[5]]) as u32 * 5 / 4,
                    u16::from_le_bytes([e[6], e[7]]),
                    u16::from_le_bytes([e[8], e[9]]) as u32 * 10,
                ));
            }
            // LTK request: we hold no keys. Nothing relies on Bluetooth encryption.
            Some(5) if e.len() >= 3 => {
                let handle = [e[1], e[2]];
                self.cmd(hci::OP_LE_LTK_NEGATIVE, &handle);
            }
            _ => {}
        }
    }

    fn on_disconnected(&mut self, reason: u8, now_ms: u64) {
        self.log(format!("phone gone, reason {reason:02x}"));
        if let Some(c) = self.conn.take()
            && c.session != Session::None
            && !matches!(c.session, Session::Draining { .. })
        {
            self.actions.push(Action::CloseSession);
        }
        self.control.clear();
        self.notify.clear();
        self.notify_bytes = 0;
        self.current.clear();
        self.to_session.clear();
        self.credits = self.max_credits;
        self.db.cccd = 0;
        self.forget_advertising_commands();
        self.advertise_fast(now_ms);
    }

    fn on_acl(&mut self, handle_flags: u16, data: &[u8], now_ms: u64) {
        let Some(c) = &mut self.conn else { return };
        // Going anyway: nothing it says now gets an answer, or a line in the log.
        if handle_flags & 0x0FFF != c.handle || c.cutting {
            return;
        }
        // Packet boundary 01 continues; anything else starts a PDU.
        if (handle_flags >> 12) & 3 != 1 {
            c.reassembly.clear();
        }
        c.reassembly.extend_from_slice(data);
        if c.reassembly.len() < 4 {
            return;
        }
        let need = 4 + u16::from_le_bytes([c.reassembly[0], c.reassembly[1]]) as usize;
        if need > MAX_REASSEMBLY {
            c.reassembly.clear();
            self.log("an L2CAP PDU over the limit: cutting the link".to_string());
            self.cut_link();
            return;
        }
        if c.reassembly.len() < need {
            return;
        }
        let cid = u16::from_le_bytes([c.reassembly[2], c.reassembly[3]]);
        let body = c.reassembly[4..need].to_vec();
        c.reassembly.clear();
        match cid {
            CID_ATT => self.on_att(&body, now_ms),
            CID_SIGNALING => self.on_signaling(&body),
            // Pairing Failed, Pairing Not Supported: nothing relies on Bluetooth encryption.
            CID_SMP => self.send_control(CID_SMP, &[0x05, 0x05]),
            _ => {}
        }
    }

    fn on_att(&mut self, pdu: &[u8], now_ms: u64) {
        let Some(c) = &mut self.conn else { return };
        if pdu.first() == Some(&0x02) && pdu.len() >= 3 {
            let asked = u16::from_le_bytes([pdu[1], pdu[2]]);
            c.mtu = asked.clamp(23, att::SERVER_MTU);
        }
        let mtu = c.mtu;
        let session_open = matches!(c.session, Session::Open);
        let mut written = 0;
        let to_session = &mut self.to_session;
        let reply = self.db.handle(pdu, mtu, &mut |v| {
            written += v.len();
            if session_open {
                to_session.extend_from_slice(v);
            }
        });
        if written > 0
            && let Some(c) = &mut self.conn
        {
            c.last_activity_ms = now_ms;
        }
        if let Some(r) = reply {
            self.send_control(CID_ATT, &r);
        }
        self.after_cccd();
    }

    fn after_cccd(&mut self) {
        let subscribed = self.db.cccd & 1 != 0;
        let Some(c) = &mut self.conn else { return };
        match (subscribed, c.session) {
            (true, Session::None) if !c.cutting => {
                c.session = Session::Opening;
                self.actions.push(Action::OpenSession);
            }
            (false, Session::Open | Session::Opening) => {
                // Unsubscribing ends the session and the link: a stream has one owner at a time.
                c.session = Session::None;
                self.actions.push(Action::CloseSession);
                self.notify.clear();
                self.notify_bytes = 0;
                self.to_session.clear();
                self.cut_link();
            }
            _ => {}
        }
    }

    fn on_signaling(&mut self, p: &[u8]) {
        match p.first() {
            Some(&SIG_PARAM_RESPONSE) => {
                let accepted = p.get(4..6) == Some(&[0, 0]);
                let Some(c) = &mut self.conn else { return };
                match c.params_asked.take() {
                    Some(want) if accepted => c.params = want,
                    // A phone that refuses once will not change its mind on this link.
                    Some(_) => c.params_refused = true,
                    None => {}
                }
            }
            Some(&SIG_REJECT) => {
                if let Some(c) = &mut self.conn
                    && c.params_asked.take().is_some()
                {
                    c.params_refused = true;
                }
            }
            // Any other command: "not understood". As peripheral we serve none.
            Some(_) => {
                if let Some(&id) = p.get(1) {
                    self.send_control(CID_SIGNALING, &[SIG_REJECT, id, 2, 0, 0, 0]);
                }
            }
            None => {}
        }
    }
}

/// The handle a queued command is about, if it is about one link.
fn link_command_handle(command: &[u8]) -> Option<u16> {
    let op = u16::from_le_bytes([command[1], command[2]]);
    match op {
        hci::OP_DISCONNECT | hci::OP_LE_LTK_NEGATIVE => {
            Some(u16::from_le_bytes([command[4], command[5]]) & 0x0FFF)
        }
        _ => None,
    }
}

/// `len || data`, padded to the 31 bytes the advertising commands always carry.
fn adv_field(data: &[u8]) -> Vec<u8> {
    let mut f = vec![data.len() as u8];
    f.extend_from_slice(data);
    f.resize(32, 0);
    f
}

/// The stream bytes in one notification's ACL packets.
fn notify_payload(acl: &[Vec<u8>]) -> usize {
    // Each packet has an H4 and ACL header; the first also has L2CAP and ATT headers.
    acl.iter().map(|p| p.len() - 5).sum::<usize>() - 4 - 3
}

#[cfg(test)]
mod tests;
