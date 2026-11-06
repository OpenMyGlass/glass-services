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
