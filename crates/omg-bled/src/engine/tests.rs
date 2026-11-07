use super::*;
use crate::fake::{ACL_LEN, CREDITS, FakeController};

/// An engine and a controller joined by a UART that never loses a byte.
struct Bench {
    engine: Engine,
    radio: FakeController,
    now: u64,
    opened: u32,
    closed: u32,
}

impl Bench {
    fn new() -> Bench {
        let engine = Engine::new("t", [1, 2, 3, 4, 5, 0xC6], ACL_LEN, CREDITS, 0);
        let mut b = Bench {
            engine,
            radio: FakeController::new(),
            now: 0,
            opened: 0,
            closed: 0,
        };
        b.settle();
        assert!(b.engine.started());
        assert!(b.radio.advertising);
        b
    }

    /// Runs the UART and the stream's control until nothing moves.
    fn settle(&mut self) {
        loop {
            let out = self.engine.hci_output().to_vec();
            self.engine.hci_written(out.len());
            self.radio.from_host(&out);
            let back = self.radio.take_to_host();
            if !back.is_empty() {
                self.engine.on_hci(&back, self.now);
            }
            for a in self.engine.take_actions() {
                match a {
                    Action::OpenSession => {
                        self.opened += 1;
                        self.engine.session_opened(self.now);
                    }
                    Action::CloseSession => self.closed += 1,
                }
            }
            assert_eq!(self.engine.fatal(), None);
            if out.is_empty() && back.is_empty() && self.engine.hci_output().is_empty() {
                return;
            }
        }
    }

    fn air(&mut self, pdus: usize) {
        self.radio.air(pdus);
        self.settle();
    }

    fn air_until_quiet(&mut self) {
        for _ in 0..1000 {
            self.air(4);
        }
    }

    fn connect_and_subscribe(&mut self) {
        self.radio.phone_connects();
        self.settle();
        self.radio.phone_exchange_mtu(517);
        self.radio.phone_subscribe(true);
        self.air_until_quiet();
        assert_eq!(self.radio.mtu, 247);
    }
}

fn data(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

#[test]
fn a_session_opens_only_when_the_phone_subscribes() {
    let mut b = Bench::new();
    b.radio.phone_connects();
    b.settle();
    b.radio.phone_exchange_mtu(517);
    b.air_until_quiet();
    assert_eq!(b.opened, 0);
    // Written before a subscription: nobody to hand it to.
    b.radio.phone_write(b"early");
    b.air_until_quiet();
    assert!(b.engine.to_session().is_empty());
    b.radio.phone_subscribe(true);
    b.air_until_quiet();
    assert_eq!(b.opened, 1);
    assert!(b.engine.session_room() > 0);
}

#[test]
fn the_stream_reaches_the_phone_whole_and_never_beyond_the_credits() {
    let mut b = Bench::new();
    b.connect_and_subscribe();
    let stream = data(48 * 1024 + 7);
    let mut sent = 0;
    let mut got = Vec::new();
    for _ in 0..200_000 {
        let room = b.engine.session_room().min(stream.len() - sent);
        if room > 0 {
            b.engine.session_data(&stream[sent..sent + room], b.now);
            sent += room;
        }
        b.settle();
        b.air(2);
        got.extend(b.radio.take_stream());
        if got.len() == stream.len() {
            break;
        }
    }
    assert_eq!(got, stream);
    assert_eq!(b.radio.max_outstanding, CREDITS as usize);
}

#[test]
fn a_stalled_radio_stops_taking_bytes_from_glassd() {
    let mut b = Bench::new();
    b.connect_and_subscribe();
    let mut taken = 0;
    for _ in 0..1000 {
        let room = b.engine.session_room();
        if room == 0 {
            break;
        }
        b.engine.session_data(&vec![1; room], b.now);
        taken += room;
        b.settle();
    }
    assert_eq!(b.engine.session_room(), 0);
    // The queue, plus what fits in the controller's credits: about half a second at 3 kB/s.
    assert!(taken < NOTIFY_QUEUE_BYTES + 500, "took {taken}");
}

#[test]
fn a_phone_write_comes_out_in_order_and_a_full_stream_stops_the_uart() {
    let mut b = Bench::new();
    b.connect_and_subscribe();
    let stream = data(TO_SESSION_MAX + 5000);
    for run in stream.chunks(244) {
        b.radio.phone_write(run);
    }
    while b.radio.phone_backlog() > 0 && b.engine.wants_hci_read() {
        b.air(10);
    }
    assert!(!b.engine.wants_hci_read());
    // Nothing is lost: glassd reads, the UART is read again.
    let mut got = b.engine.to_session().to_vec();
    b.engine.to_session_written(got.len());
    assert!(b.engine.wants_hci_read());
    while b.radio.phone_backlog() > 0 {
        b.air(10);
    }
    got.extend_from_slice(b.engine.to_session());
    assert_eq!(got, stream);
}

#[test]
fn an_answer_overtakes_the_stream_at_a_pdu_boundary() {
    let mut b = Bench::new();
    b.connect_and_subscribe();
    b.engine.session_data(&vec![9; 1000], b.now);
    b.settle();
    b.radio.att_replies.clear();
    // A read of the device name while notifications queue.
    b.radio.phone_l2cap(4, &[0x0A, 3, 0]);
    b.radio.air(8);
    b.settle();
    let notified = b.radio.notifications.len();
    b.air_until_quiet();
    assert_eq!(b.radio.att_replies, vec![vec![0x0B, b't']]);
    assert!(b.radio.notifications.len() > notified + 1);
}

#[test]
fn a_lost_link_ends_the_session_and_nothing_crosses_to_the_next() {
    let mut b = Bench::new();
    b.connect_and_subscribe();
    b.engine.session_data(&vec![1; 1000], b.now);
    b.radio.phone_write(b"stale");
    b.air(5);
    assert!(!b.engine.to_session().is_empty());
    b.radio.disconnect(0x08);
    b.settle();
    assert_eq!(b.closed, 1);
    assert!(b.radio.advertising, "advertising again");
    assert!(b.engine.to_session().is_empty());
    assert_eq!(b.engine.session_room(), 0);

    b.connect_and_subscribe();
    assert_eq!(b.opened, 2);
    assert!(b.radio.take_stream().is_empty(), "bytes of the old session");
    b.engine.session_data(b"fresh", b.now);
    b.air_until_quiet();
    assert_eq!(b.radio.take_stream(), b"fresh");
    // All the credits are back, so the new link can use them.
    assert_eq!(b.engine.credits, CREDITS);
}

#[test]
fn glassd_closing_lets_the_last_bytes_out_then_cuts_the_link() {
    let mut b = Bench::new();
    b.connect_and_subscribe();
    b.engine.session_data(&vec![3; 500], b.now);
    b.engine.session_closed(b.now);
    b.settle();
    assert!(b.radio.connected());
    assert_eq!(b.engine.session_room(), 0);
    b.air_until_quiet();
    assert_eq!(b.radio.take_stream().len(), 500);
    assert!(!b.radio.connected());
    assert!(b.radio.advertising);
    // glassd already closed: nothing for the caller to close.
    assert_eq!(b.closed, 0);
}

#[test]
fn glassd_closing_on_a_stalled_radio_cuts_the_link_in_the_end() {
    let mut b = Bench::new();
    b.connect_and_subscribe();
    b.engine.session_data(&vec![3; 500], b.now);
    b.engine.session_closed(b.now);
    b.settle();
    assert!(b.radio.connected());
    b.now = b.engine.next_deadline().unwrap();
    b.engine.tick(b.now);
    b.settle();
    assert!(!b.radio.connected());
}

#[test]
fn glassd_closing_on_a_full_stream_does_not_leave_the_uart_unread() {
    let mut b = Bench::new();
    b.connect_and_subscribe();
    for run in data(TO_SESSION_MAX + 500).chunks(244) {
        b.radio.phone_write(run);
    }
    while b.engine.wants_hci_read() {
        b.air(10);
    }
    b.engine.session_closed(b.now);
    assert!(b.engine.wants_hci_read());
    b.settle();
    assert!(!b.radio.connected());
}

#[test]
fn glassd_not_listening_drops_the_phone_and_keeps_advertising() {
    let mut b = Bench::new();
    b.radio.phone_connects();
    b.settle();
    b.radio.phone_subscribe(true);
    b.radio.air(10);
    let out = b.engine.hci_output().to_vec();
    b.engine.hci_written(out.len());
    b.radio.from_host(&out);
    let back = b.radio.take_to_host();
    b.engine.on_hci(&back, 0);
    assert_eq!(b.engine.take_actions(), vec![Action::OpenSession]);
    b.engine.session_failed();
    b.settle();
    assert!(!b.radio.connected());
    assert!(b.radio.advertising);
}

#[test]
fn a_central_that_never_subscribes_is_dropped() {
    let mut b = Bench::new();
    b.radio.phone_connects();
    b.settle();
    b.now = b.engine.next_deadline().unwrap();
    assert_eq!(b.now, SUBSCRIBE_WITHIN_MS);
    b.engine.tick(b.now);
    b.settle();
    assert!(!b.radio.connected());
    assert!(b.radio.advertising);
}

#[test]
fn a_second_connection_racing_the_advertising_is_refused() {
    let mut b = Bench::new();
    b.radio.phone_connects();
    b.settle();
    // What a controller reports when an enable slipped past the first connection.
    let mut ev = vec![hci::H4_EVENT, hci::EV_LE_META, 19, 1, 0, 0x41, 0];
    ev.extend_from_slice(&[1, 1, 1, 2, 3, 4, 5, 6, 36, 0, 0, 0, 0xF4, 1, 0]);
    b.engine.on_hci(&ev, 0);
    let out = b.engine.hci_output().to_vec();
    assert!(!out.is_empty());
    b.engine.hci_written(out.len());
    // The answer about 0x41 is the only disconnect, and the first link stands.
    b.radio.from_host(&out);
    assert!(
        b.radio
            .commands
            .iter()
            .any(|(op, p)| *op == hci::OP_DISCONNECT && p[0] == 0x41)
    );
}

/// The controller's report of a phone connecting, for a test that decides when it arrives.
fn connection_complete() -> Vec<u8> {
    let mut ev = vec![hci::H4_EVENT, hci::EV_LE_META, 19, 1, 0, 0x40, 0];
    ev.extend_from_slice(&[1, 1, 1, 2, 3, 4, 5, 6, 36, 0, 0, 0, 0xF4, 1, 0]);
    ev
}

fn adv_enables(b: &Bench) -> Vec<u8> {
    b.radio
        .commands
        .iter()
        .filter(|(op, _)| *op == hci::OP_LE_SET_ADV_ENABLE)
        .map(|(_, p)| p[0])
        .collect()
}

/// Up to the point where the UART holds the command `op` of the move to slow advertising, which
/// the controller has not seen. Returns its bytes.
fn slow_down_until_in_flight(b: &mut Bench, op: u16, param: Option<u8>) -> Vec<u8> {
    b.radio.commands.clear();
    b.now = b.engine.next_deadline().unwrap();
    b.engine.tick(b.now);
    loop {
        let out = b.engine.hci_output().to_vec();
        assert!(!out.is_empty(), "the command never went out");
        b.engine.hci_written(out.len());
        let sent = u16::from_le_bytes([out[1], out[2]]);
        if sent == op && param.is_none_or(|v| out.get(4) == Some(&v)) {
            return out;
        }
        b.radio.from_host(&out);
        let back = b.radio.take_to_host();
        b.engine.on_hci(&back, b.now);
    }
}

#[test]
fn a_connection_stops_the_advertising_by_itself_so_nothing_turns_it_off() {
    let mut b = Bench::new();
    b.radio.commands.clear();
    b.engine.take_logs();
    b.radio.phone_connects();
    b.settle();
    assert_eq!(adv_enables(&b), Vec::<u8>::new());
    let logs = b.engine.take_logs();
    assert!(!logs.iter().any(|l| l.contains("failed")), "{logs:?}");
}

#[test]
fn an_enable_in_flight_when_a_phone_connects_is_turned_off_again() {
    let mut b = Bench::new();
    let enable = slow_down_until_in_flight(&mut b, hci::OP_LE_SET_ADV_ENABLE, Some(1));
    // The connection overtakes the enable, which the controller then accepts.
    b.engine.on_hci(&connection_complete(), b.now);
    b.radio.from_host(&enable);
    assert!(b.radio.advertising);
    b.settle();
    assert!(!b.radio.advertising, "still advertising on a connection");
    assert_eq!(adv_enables(&b), vec![0, 1, 0]);
    let logs = b.engine.take_logs();
    assert!(!logs.iter().any(|l| l.contains("failed")), "{logs:?}");
}

#[test]
fn an_enable_refused_because_of_the_connection_leaves_nothing_to_report_but_itself() {
    let mut b = Bench::new();
    slow_down_until_in_flight(&mut b, hci::OP_LE_SET_ADV_ENABLE, Some(1));
    b.engine.on_hci(&connection_complete(), b.now);
    b.engine.take_logs();
    // The enable is refused, so the disable that follows finds it off and is refused too.
    let op = hci::OP_LE_SET_ADV_ENABLE.to_le_bytes();
    let refused = [
        hci::H4_EVENT,
        hci::EV_COMMAND_COMPLETE,
        4,
        1,
        op[0],
        op[1],
        hci::STATUS_COMMAND_DISALLOWED,
    ];
    b.engine.on_hci(&refused, b.now);
    b.settle();
    assert_eq!(adv_enables(&b), vec![0, 0]);
    assert!(!b.radio.advertising);
    assert_eq!(
        b.engine.take_logs(),
        vec!["command 200a (01) failed with status 0c".to_string()]
    );
    assert_eq!(b.engine.fatal(), None);
}

#[test]
fn a_disable_crossing_a_connection_is_not_a_failure() {
    let mut b = Bench::new();
    let disable = slow_down_until_in_flight(&mut b, hci::OP_LE_SET_ADV_ENABLE, Some(0));
    // The phone got in first: the controller stopped, and says so to the disable.
    b.radio.phone_connects();
    let connected = b.radio.take_to_host();
    b.engine.take_logs();
    b.engine.on_hci(&connected, b.now);
    b.radio.from_host(&disable);
    b.settle();
    // Nothing that was queued to bring the advertising back went out.
    assert_eq!(adv_enables(&b), vec![0]);
    assert!(!b.radio.advertising);
    let logs = b.engine.take_logs();
    assert!(!logs.iter().any(|l| l.contains("failed")), "{logs:?}");
}

#[test]
fn advertising_comes_back_after_a_link_without_a_refused_command() {
    let mut b = Bench::new();
    b.connect_and_subscribe();
    b.engine.take_logs();
    b.radio.commands.clear();
    b.radio.disconnect(0x13);
    b.settle();
    assert_eq!(adv_enables(&b), vec![1]);
    assert!(b.radio.advertising);
    let logs = b.engine.take_logs();
    assert!(!logs.iter().any(|l| l.contains("failed")), "{logs:?}");
}

#[test]
fn advertising_slows_down_once_and_stays_quiet() {
    let mut b = Bench::new();
    b.radio.commands.clear();
    b.now = b.engine.next_deadline().unwrap();
    assert_eq!(b.now, FAST_ADV_FOR_MS);
    b.engine.tick(b.now);
    b.settle();
    let params = b
        .radio
        .commands
        .iter()
        .find(|(op, _)| *op == hci::OP_LE_SET_ADV_PARAMS)
        .unwrap();
    assert_eq!(
        u16::from_le_bytes([params.1[0], params.1[1]]),
        SLOW_ADV_UNITS
    );
    assert!(b.radio.advertising);
    assert_eq!(
        b.engine.next_deadline(),
        None,
        "no timer left while nobody comes"
    );
}

#[test]
fn a_quiet_link_asks_for_idle_parameters_once_and_then_has_no_timer() {
    let mut b = Bench::new();
    b.connect_and_subscribe();
    b.now = b.engine.next_deadline().unwrap();
    assert_eq!(b.now, IDLE_AFTER_MS);
    b.engine.tick(b.now);
    b.air_until_quiet();
    let asked = b.radio.signaling.last().unwrap();
    assert_eq!(asked[0], SIG_PARAM_REQUEST);
    assert_eq!(u16::from_le_bytes([asked[4], asked[5]]), PARAMS_IDLE[0]);
    assert_eq!(b.radio.signaling.len(), 1);
    assert_eq!(
        b.engine.next_deadline(),
        None,
        "a quiet link must not wake the host"
    );
}

#[test]
fn a_refused_parameter_request_is_not_repeated() {
    let mut b = Bench::new();
    b.radio.params_result = Some(1);
    b.connect_and_subscribe();
    b.now = b.engine.next_deadline().unwrap();
    b.engine.tick(b.now);
    b.air_until_quiet();
    assert_eq!(b.radio.signaling.len(), 1);
    assert_eq!(b.engine.next_deadline(), None);
    b.engine.session_data(&vec![0; 800], b.now);
    b.air_until_quiet();
    assert_eq!(b.radio.signaling.len(), 1);
}

#[test]
fn a_backlog_asks_for_busy_parameters() {
    let mut b = Bench::new();
    b.connect_and_subscribe();
    b.engine.session_data(&vec![0; 300], b.now);
    b.air_until_quiet();
    let asked = b.radio.signaling.last().unwrap();
    assert_eq!(u16::from_le_bytes([asked[4], asked[5]]), PARAMS_BUSY[0]);
}

#[test]
fn smp_is_refused() {
    let mut b = Bench::new();
    b.connect_and_subscribe();
    b.radio.phone_l2cap(6, &[0x01, 3, 0, 1, 0, 16, 7, 7]);
    b.air_until_quiet();
    assert_eq!(b.radio.smp, vec![vec![0x05, 0x05]]);
}

#[test]
fn a_hardware_error_is_fatal() {
    let mut b = Bench::new();
    b.engine
        .on_hci(&[hci::H4_EVENT, hci::EV_HARDWARE_ERROR, 1, 7], 0);
    assert!(b.engine.fatal().unwrap().contains("07"));
}

/// An ACL packet from the phone, past the fake radio, as a peer that breaks the rules can send it.
fn acl_from_peer(payload: &[u8]) -> Vec<u8> {
    let mut v = vec![hci::H4_ACL];
    v.extend_from_slice(&(0x40u16 | (2 << 12)).to_le_bytes());
    v.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    v.extend_from_slice(payload);
    v
}

fn l2cap(cid: u16, payload: &[u8]) -> Vec<u8> {
    let mut v = (payload.len() as u16).to_le_bytes().to_vec();
    v.extend_from_slice(&cid.to_le_bytes());
    v.extend_from_slice(payload);
    v
}

#[test]
fn an_oversize_pdu_cuts_the_link_and_a_flood_of_them_writes_one_line() {
    let mut b = Bench::new();
    b.radio.phone_connects();
    b.settle();
    b.engine.take_logs();
    let flood = acl_from_peer(&[0xFF, 0xFF, 4, 0]);
    let mut lines = Vec::new();
    for _ in 0..1000 {
        b.engine.on_hci(&flood, b.now);
        lines.extend(b.engine.take_logs());
    }
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("over the limit"));
    b.settle();
    assert!(!b.radio.connected(), "the link was not cut");
    assert!(b.radio.advertising);
}

#[test]
fn a_peer_coming_back_again_and_again_cannot_flood_the_log() {
    let mut b = Bench::new();
    b.engine.take_logs();
    let flood = acl_from_peer(&[0xFF, 0xFF, 4, 0]);
    let mut lines = 0;
    for _ in 0..1000 {
        b.radio.phone_connects();
        b.settle();
        b.engine.on_hci(&flood, b.now);
        b.settle();
        lines += b.engine.take_logs().len();
    }
    assert!(lines <= LOG_LINES_PER_WINDOW as usize, "{lines} lines");

    // Time passing says how many went, and the budget is back.
    b.now += LOG_WINDOW_MS;
    b.engine.tick(b.now);
    b.settle();
    let logs = b.engine.take_logs();
    assert!(
        logs.iter().any(|l| l.contains("log lines dropped")),
        "{logs:?}"
    );
    b.radio.phone_connects();
    b.settle();
    assert!(b.engine.take_logs().iter().any(|l| l.contains("connected")));
}

#[test]
fn answers_with_no_credits_are_bounded_and_end_the_link() {
    let mut b = Bench::new();
    b.radio.phone_connects();
    b.settle();
    // A read by type, which always has an answer. The radio is stalled: nothing returns credits.
    let request = acl_from_peer(&l2cap(CID_ATT, &[0x08, 6, 0, 11, 0, 0x03, 0x28]));
    let mut most = 0;
    for _ in 0..10_000 {
        b.engine.on_hci(&request, b.now);
        let out = b.engine.hci_output().to_vec();
        b.engine.hci_written(out.len());
        b.radio.from_host(&out);
        most = most.max(b.engine.control.len());
    }
    assert!(most <= CONTROL_QUEUE_MAX, "{most} answers queued");
    b.settle();
    assert!(!b.radio.connected(), "the link was not cut");
    assert_eq!(b.engine.control.len(), 0);
}

#[test]
fn a_well_behaved_peer_never_gets_near_the_answer_limit() {
    let mut b = Bench::new();
    b.connect_and_subscribe();
    for _ in 0..200 {
        b.radio.phone_write(b"x");
        b.air(4);
        assert!(b.engine.control.len() < CONTROL_QUEUE_MAX);
    }
    assert!(b.radio.connected());
}

/// Takes what the engine wrote, as a controller that has gone silent would.
fn swallow(b: &mut Bench) {
    let out = b.engine.hci_output().to_vec();
    b.engine.hci_written(out.len());
}

#[test]
fn a_command_with_no_answer_is_fatal_at_its_deadline() {
    let mut e = Engine::new("t", [1, 2, 3, 4, 5, 0xC6], ACL_LEN, CREDITS, 1000);
    let out = e.hci_output().len();
    e.hci_written(out);
    assert_eq!(e.next_deadline(), Some(1000 + COMMAND_TIMEOUT_MS));
    e.tick(1000 + COMMAND_TIMEOUT_MS - 1);
    assert_eq!(e.fatal(), None);
    e.tick(1000 + COMMAND_TIMEOUT_MS);
    assert!(e.fatal().unwrap().contains("did not answer"));
}

#[test]
fn an_answered_command_leaves_no_deadline() {
    let mut b = Bench::new();
    // Only the slowing of the advertising is left.
    assert_eq!(b.engine.next_deadline(), Some(FAST_ADV_FOR_MS));
    b.now = 1_000_000;
    b.engine.tick(b.now);
    b.settle();
    assert_eq!(b.engine.next_deadline(), None);
    b.engine.tick(b.now + 10 * COMMAND_TIMEOUT_MS);
    assert_eq!(b.engine.fatal(), None);
}

#[test]
fn a_uart_that_takes_nothing_is_fatal() {
    let mut b = Bench::new();
    b.connect_and_subscribe();
    // No command in flight: only the stream's bytes are waiting for the UART.
    b.engine.session_data(&data(20), b.now);
    assert!(!b.engine.hci_output().is_empty());
    b.engine.tick(OUTPUT_STALL_MS - 1);
    // Part of it moves: that buys time.
    b.engine.hci_written(1);
    b.engine.tick(2 * OUTPUT_STALL_MS - 2);
    assert_eq!(b.engine.fatal(), None, "it moved a moment ago");
    b.engine.tick(2 * OUTPUT_STALL_MS - 1);
    assert!(b.engine.fatal().unwrap().contains("took no bytes"));
}

#[test]
fn a_link_that_is_never_confirmed_ended_is_fatal() {
    let mut b = Bench::new();
    b.connect_and_subscribe();
    // Nothing left to send, so glassd closing cuts the link at once.
    b.engine.session_closed(b.now);
    // The controller takes the Disconnect, says it started, and then nothing more.
    swallow(&mut b);
    let mut status = vec![hci::H4_EVENT, hci::EV_COMMAND_STATUS, 4, 0, 1];
    status.extend_from_slice(&hci::OP_DISCONNECT.to_le_bytes());
    b.engine.on_hci(&status, b.now);
    assert_eq!(b.engine.next_deadline(), Some(CUT_TIMEOUT_MS));
    b.engine.tick(CUT_TIMEOUT_MS - 1);
    assert_eq!(b.engine.fatal(), None);
    b.engine.tick(CUT_TIMEOUT_MS);
    assert!(b.engine.fatal().unwrap().contains("did not end the link"));
}

#[test]
fn a_cut_that_the_controller_refuses_is_fatal_at_once() {
    let mut b = Bench::new();
    b.connect_and_subscribe();
    b.engine.session_closed(b.now);
    swallow(&mut b);
    let event = [
        hci::H4_EVENT,
        hci::EV_DISCONNECT_COMPLETE,
        4,
        0x0C,
        0x40,
        0,
        0x13,
    ];
    b.engine.on_hci(&event, b.now);
    assert!(b.engine.fatal().unwrap().contains("could not end the link"));
}

#[test]
fn a_failed_disconnect_nobody_asked_for_is_only_logged() {
    let mut b = Bench::new();
    b.connect_and_subscribe();
    b.engine.take_logs();
    let event = [
        hci::H4_EVENT,
        hci::EV_DISCONNECT_COMPLETE,
        4,
        0x0C,
        0x40,
        0,
        0x13,
    ];
    b.engine.on_hci(&event, b.now);
    assert_eq!(b.engine.fatal(), None);
    assert_eq!(b.engine.take_logs().len(), 1);
    assert!(b.radio.connected());
}

#[test]
fn a_lost_byte_is_survived_and_a_stream_of_garbage_is_not() {
    let mut b = Bench::new();
    b.engine.on_hci(&[0x7F; DESYNC_MAX as usize], b.now);
    assert_eq!(b.engine.fatal(), None);
    // A new window forgives what came before.
    b.now = DESYNC_WINDOW_MS;
    b.engine.on_hci(&[0x7F; DESYNC_MAX as usize], b.now);
    assert_eq!(b.engine.fatal(), None);
    b.engine.on_hci(&[0x7F; 1], b.now);
    assert!(b.engine.fatal().unwrap().contains("no packet"));
}
