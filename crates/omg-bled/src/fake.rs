//! A fake controller with one fake phone on the air, for host tests. Not used on the Glass.
//!
//! It holds the [`Engine`](crate::engine::Engine) to what a real controller does: one command at a
//! time, and never more ACL packets than credits. It panics on a breach, so a test cannot pass
//! with a broken flow control. The radio is `air`: each call moves a few fragments each way, and
//! nothing moves between calls, so a test decides when the radio stalls.

use std::collections::VecDeque;

use crate::att;
use crate::hci;

/// Bytes a `FakeController` wants to see in ACL packets. What BCM4330 reports.
pub const ACL_LEN: usize = 27;
pub const CREDITS: u16 = 15;
const HANDLE: u16 = 0x40;

#[derive(Default)]
pub struct FakeController {
    from_host: Vec<u8>,
    to_host: Vec<u8>,
    pub commands: Vec<(u16, Vec<u8>)>,
    /// ACL packets the host handed over and the air has not carried, for the credit check.
    glass_to_air: VecDeque<(u16, Vec<u8>)>,
    /// ACL packets the phone sent, waiting for the air.
    phone_to_air: VecDeque<Vec<u8>>,
    /// Reassembly of what reached the phone.
    at_phone: Vec<u8>,
    connected: bool,
    pub advertising: bool,
    pub connections: u32,
    /// The stream as the phone received it, run by run.
    pub notifications: Vec<Vec<u8>>,
    pub att_replies: Vec<Vec<u8>>,
    pub signaling: Vec<Vec<u8>>,
    pub smp: Vec<Vec<u8>>,
    /// The most credits ever in use.
    pub max_outstanding: usize,
    /// Forwards host packets to [`FakeController::bringup_reply`] when a test wants them.
    pub mtu: u16,
    /// Answer connection parameter requests with this result; `None` leaves them unanswered.
    pub params_result: Option<u16>,
}

impl FakeController {
    pub fn new() -> FakeController {
        FakeController {
            mtu: 23,
            params_result: Some(0),
            ..Default::default()
        }
    }

    /// Takes bytes from the host's UART.
    pub fn from_host(&mut self, bytes: &[u8]) {
        self.from_host.extend_from_slice(bytes);
        loop {
            match self.from_host.first() {
                Some(&hci::H4_COMMAND) => {
                    let Some(&n) = self.from_host.get(3) else {
                        return;
                    };
                    let need = 4 + n as usize;
                    if self.from_host.len() < need {
                        return;
                    }
                    let op = u16::from_le_bytes([self.from_host[1], self.from_host[2]]);
                    let params = self.from_host[4..need].to_vec();
                    self.from_host.drain(..need);
                    self.on_command(op, params);
                }
                Some(&hci::H4_ACL) => {
                    if self.from_host.len() < 5 {
                        return;
                    }
                    let n = u16::from_le_bytes([self.from_host[3], self.from_host[4]]) as usize;
                    assert!(n <= ACL_LEN, "ACL packet of {n} over the controller's size");
                    if self.from_host.len() < 5 + n {
                        return;
                    }
                    let hf = u16::from_le_bytes([self.from_host[1], self.from_host[2]]);
                    let data = self.from_host[5..5 + n].to_vec();
                    self.from_host.drain(..5 + n);
                    self.glass_to_air.push_back((hf, data));
                    self.max_outstanding = self.max_outstanding.max(self.glass_to_air.len());
                    assert!(
                        self.glass_to_air.len() <= CREDITS as usize,
                        "the host sent more ACL packets than it had credits"
                    );
                }
                Some(other) => panic!("the host sent H4 type {other}"),
                None => return,
            }
        }
    }

    /// Bytes for the host's UART.
    pub fn take_to_host(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.to_host)
    }

    fn event(&mut self, code: u8, params: &[u8]) {
        self.to_host
            .extend_from_slice(&[hci::H4_EVENT, code, params.len() as u8]);
        self.to_host.extend_from_slice(params);
    }

    fn on_command(&mut self, op: u16, params: Vec<u8>) {
        let mut reply = vec![1, op as u8, (op >> 8) as u8, 0];
        match op {
            hci::OP_LE_READ_BUFFER_SIZE => {
                reply.extend_from_slice(&(ACL_LEN as u16).to_le_bytes());
                reply.push(CREDITS as u8);
            }
            // Turning off what a connection already stopped: as BCM4330 answered in bled36, one link.
            hci::OP_LE_SET_ADV_ENABLE if params.first() == Some(&0) && !self.advertising => {
                reply[3] = hci::STATUS_COMMAND_DISALLOWED;
            }
            hci::OP_LE_SET_ADV_ENABLE => self.advertising = params.first() == Some(&1),
            // Parameters may not change while advertising.
            hci::OP_LE_SET_ADV_PARAMS if self.advertising => {
                reply[3] = hci::STATUS_COMMAND_DISALLOWED;
            }
            hci::OP_DISCONNECT => {
                let mut status = vec![0, 1, op as u8, (op >> 8) as u8];
                self.commands.push((op, params));
                self.event(hci::EV_COMMAND_STATUS, &status);
                status.clear();
                self.disconnect(0x16);
                return;
            }
            _ => {}
        }
        self.commands.push((op, params));
        self.event(hci::EV_COMMAND_COMPLETE, &reply);
    }

    /// A phone connects, as it does after seeing the advertising.
    pub fn phone_connects(&mut self) {
        assert!(self.advertising, "connecting to a Glass not advertising");
        assert!(!self.connected);
        self.advertising = false;
        self.connected = true;
        self.connections += 1;
        self.mtu = 23;
        self.at_phone.clear();
        self.notifications.clear();
        let mut e = vec![1, 0];
        e.extend_from_slice(&HANDLE.to_le_bytes());
        // Peripheral, random address, then the address, interval, latency, timeout, accuracy.
        e.extend_from_slice(&[1, 1, 1, 2, 3, 4, 5, 6, 36, 0, 0, 0, 0xF4, 1, 0]);
        self.event(hci::EV_LE_META, &e);
    }

    /// The link ends, by supervision timeout or by the phone, with what was on the air lost.
    pub fn disconnect(&mut self, reason: u8) {
        if !self.connected {
            return;
        }
        self.connected = false;
        self.glass_to_air.clear();
        self.phone_to_air.clear();
        let mut e = vec![0];
        e.extend_from_slice(&HANDLE.to_le_bytes());
        e.push(reason);
        self.event(hci::EV_DISCONNECT_COMPLETE, &e);
    }

    pub fn connected(&self) -> bool {
        self.connected
    }

    /// The phone sends one L2CAP PDU.
    pub fn phone_l2cap(&mut self, cid: u16, payload: &[u8]) {
        for packet in hci::acl_fragments(HANDLE, cid, payload, ACL_LEN) {
            // The same packet from the controller's side: 10 starts a PDU, 01 continues it.
            let mut packet = packet;
            let hf = u16::from_le_bytes([packet[1], packet[2]]);
            let hf = if hf >> 12 == 0 { hf | (2 << 12) } else { hf };
            packet[1..3].copy_from_slice(&hf.to_le_bytes());
            self.phone_to_air.push_back(packet);
        }
    }

    /// The phone asks for an MTU. The answer sets it when it reaches the phone.
    pub fn phone_exchange_mtu(&mut self, mtu: u16) {
        let mut p = vec![0x02];
        p.extend_from_slice(&mtu.to_le_bytes());
        self.phone_l2cap(4, &p);
    }

    /// The phone turns notifications on or off.
    pub fn phone_subscribe(&mut self, on: bool) {
        let mut p = vec![0x12];
        p.extend_from_slice(&att::CCCD_HANDLE.to_le_bytes());
        p.extend_from_slice(&[on as u8, 0]);
        self.phone_l2cap(4, &p);
    }

    /// One write without response. Panics if it does not fit the MTU, as Android would refuse it.
    pub fn phone_write(&mut self, value: &[u8]) {
        assert!(value.len() <= self.mtu as usize - 3);
        let mut p = vec![0x52];
        p.extend_from_slice(&att::RX_HANDLE.to_le_bytes());
        p.extend_from_slice(value);
        self.phone_l2cap(4, &p);
    }

    /// Fragments still on their way to the phone's radio.
    pub fn phone_backlog(&self) -> usize {
        self.phone_to_air.len()
    }

    /// One connection event: at most `pdus` fragments each way.
    pub fn air(&mut self, pdus: usize) {
        if !self.connected {
            return;
        }
        for _ in 0..pdus {
            let Some(p) = self.phone_to_air.pop_front() else {
                break;
            };
            self.to_host.extend_from_slice(&p);
        }
        let mut done = 0u16;
        for _ in 0..pdus {
            let Some((hf, data)) = self.glass_to_air.pop_front() else {
                break;
            };
            done += 1;
            self.at_phone(hf, &data);
        }
        if done > 0 {
            let mut e = vec![1];
            e.extend_from_slice(&HANDLE.to_le_bytes());
            e.extend_from_slice(&done.to_le_bytes());
            self.event(hci::EV_COMPLETED_PACKETS, &e);
        }
    }

    fn at_phone(&mut self, hf: u16, data: &[u8]) {
        if (hf >> 12) & 3 != 1 {
            assert!(self.at_phone.is_empty(), "a PDU cut by another");
        }
        self.at_phone.extend_from_slice(data);
        if self.at_phone.len() < 4 {
            return;
        }
        let need = 4 + u16::from_le_bytes([self.at_phone[0], self.at_phone[1]]) as usize;
        if self.at_phone.len() < need {
            return;
        }
        let cid = u16::from_le_bytes([self.at_phone[2], self.at_phone[3]]);
        let body = self.at_phone[4..need].to_vec();
        self.at_phone.clear();
        match cid {
            4 => match body.first() {
                Some(&att::OP_NOTIFY) => {
                    assert!(
                        body.len() <= self.mtu as usize,
                        "a notification over the MTU"
                    );
                    self.notifications.push(body[3..].to_vec());
                }
                Some(0x03) => {
                    let server = u16::from_le_bytes([body[1], body[2]]);
                    self.mtu = server.min(self.mtu.max(247));
                    self.att_replies.push(body);
                }
                _ => self.att_replies.push(body),
            },
            5 => {
                if body.first() == Some(&0x12)
                    && let Some(result) = self.params_result
                {
                    let mut r = vec![0x13, body[1], 2, 0];
                    r.extend_from_slice(&result.to_le_bytes());
                    self.phone_l2cap(5, &r);
                }
                self.signaling.push(body);
            }
            6 => self.smp.push(body),
            _ => panic!("the Glass sent on CID {cid}"),
        }
    }

    /// The stream the phone received so far, and forgets it.
    pub fn take_stream(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.notifications).concat()
    }
}
