//! A fixed GATT table and the ATT requests a phone sends to discover and use it. Pure.
//!
//! The table never changes, because phones cache it by address. A change here needs a new address.

pub const RX_HANDLE: u16 = 8;
pub const TX_HANDLE: u16 = 10;
pub const CCCD_HANDLE: u16 = 11;
/// What the Glass answers to Exchange MTU. At 27-byte link PDUs a longer value gains little and
/// costs a buffer per fragment on the way.
pub const SERVER_MTU: u16 = 247;
/// An ATT PDU longer than this is not one of ours.
pub const MAX_ATT_PDU: usize = SERVER_MTU as usize;

const OP_ERROR: u8 = 0x01;
const OP_MTU_REQ: u8 = 0x02;
const OP_MTU_RSP: u8 = 0x03;
const OP_FIND_INFO: u8 = 0x04;
const OP_FIND_BY_TYPE: u8 = 0x06;
const OP_READ_BY_TYPE: u8 = 0x08;
const OP_READ: u8 = 0x0A;
const OP_READ_BLOB: u8 = 0x0C;
const OP_READ_BY_GROUP: u8 = 0x10;
const OP_WRITE_REQ: u8 = 0x12;
const OP_WRITE_RSP: u8 = 0x13;
const OP_WRITE_CMD: u8 = 0x52;
const OP_CONFIRM: u8 = 0x1E;
pub const OP_NOTIFY: u8 = 0x1B;

const ERR_INVALID_HANDLE: u8 = 0x01;
const ERR_READ_NOT_PERMITTED: u8 = 0x02;
const ERR_WRITE_NOT_PERMITTED: u8 = 0x03;
const ERR_INVALID_PDU: u8 = 0x04;
const ERR_NOT_SUPPORTED: u8 = 0x06;
const ERR_INVALID_OFFSET: u8 = 0x07;
const ERR_NOT_FOUND: u8 = 0x0A;
const ERR_UNSUPPORTED_GROUP: u8 = 0x10;

/// 6f6d672d-626c-652d-6c69-6e6b2d7631xx ("omg-ble-link-v1" and an index), least significant byte
/// first as on the air. The phone's copy is `omg_core::ble`.
pub fn uuid128(last: u8) -> [u8; 16] {
    let mut u = [
        0x6f, 0x6d, 0x67, 0x2d, 0x62, 0x6c, 0x65, 0x2d, 0x6c, 0x69, 0x6e, 0x6b, 0x2d, 0x76, 0x31,
        last,
    ];
    u.reverse();
    u
}

struct Attr {
    handle: u16,
    typ: Vec<u8>,
    value: Vec<u8>,
    group_end: u16,
}

pub struct Db {
    attrs: Vec<Attr>,
    /// The client's configuration of `tx`. Bit 0 is notifications.
    pub cccd: u16,
}

fn char_decl(props: u8, value_handle: u16, uuid: &[u8]) -> Vec<u8> {
    let mut v = vec![props];
    v.extend_from_slice(&value_handle.to_le_bytes());
    v.extend_from_slice(uuid);
    v
}

impl Db {
    pub fn new(name: &str) -> Db {
        let t16 = |u: u16| u.to_le_bytes().to_vec();
        let a = |handle, typ, value, group_end| Attr {
            handle,
            typ,
            value,
            group_end,
        };
        let attrs = vec![
            a(1, t16(0x2800), t16(0x1800), 5),
            a(
                2,
                t16(0x2803),
                char_decl(0x02, 3, &0x2A00u16.to_le_bytes()),
                0,
            ),
            a(3, t16(0x2A00), name.as_bytes().to_vec(), 0),
            a(
                4,
                t16(0x2803),
                char_decl(0x02, 5, &0x2A01u16.to_le_bytes()),
                0,
            ),
            a(5, t16(0x2A01), vec![0, 0], 0),
            a(6, t16(0x2800), uuid128(0x00).to_vec(), 11),
            // Write and Write Without Response.
            a(
                7,
                t16(0x2803),
                char_decl(0x0C, RX_HANDLE, &uuid128(0x01)),
                0,
            ),
            a(RX_HANDLE, uuid128(0x01).to_vec(), vec![], 0),
            // Notify.
            a(
                9,
                t16(0x2803),
                char_decl(0x10, TX_HANDLE, &uuid128(0x02)),
                0,
            ),
            a(TX_HANDLE, uuid128(0x02).to_vec(), vec![], 0),
            a(CCCD_HANDLE, t16(0x2902), vec![0, 0], 0),
        ];
        Db { attrs, cccd: 0 }
    }

    fn range(&self, start: u16, end: u16) -> impl Iterator<Item = &Attr> {
        self.attrs
            .iter()
            .filter(move |a| a.handle >= start && a.handle <= end)
    }

    fn get(&self, h: u16) -> Option<&Attr> {
        self.attrs.iter().find(|a| a.handle == h)
    }

    /// Answers one ATT PDU. `mtu` is the current MTU. Bytes written to `rx` go to `on_write`.
    pub fn handle(
        &mut self,
        pdu: &[u8],
        mtu: u16,
        on_write: &mut dyn FnMut(&[u8]),
    ) -> Option<Vec<u8>> {
        let op = *pdu.first()?;
        // Commands (bit 6) and confirmations get no answer, whatever they are.
        if op & 0x40 != 0 && op != OP_WRITE_CMD || op == OP_CONFIRM {
            return None;
        }
        match self.answer(op, pdu, mtu as usize, on_write) {
            Some(answer) => answer,
            // Too short for its opcode. A command has nobody to tell.
            None if op == OP_WRITE_CMD => None,
            None => Some(error(op, 0, ERR_INVALID_PDU)),
        }
    }

    /// `None` is a PDU too short to read; `Some(None)` is a valid one that gets no answer.
    fn answer(
        &mut self,
        op: u8,
        pdu: &[u8],
        mtu: usize,
        on_write: &mut dyn FnMut(&[u8]),
    ) -> Option<Option<Vec<u8>>> {
        let u16_at = |i: usize| pdu.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
        let err = |h: u16, code: u8| Some(Some(error(op, h, code)));
        Some(Some(match op {
            OP_MTU_REQ => {
                u16_at(1)?;
                let mut v = vec![OP_MTU_RSP];
                v.extend_from_slice(&SERVER_MTU.to_le_bytes());
                v
            }
            OP_FIND_INFO => {
                let (s, e) = (u16_at(1)?, u16_at(3)?);
                if s == 0 || s > e {
                    return err(s, ERR_INVALID_HANDLE);
                }
                let mut out = vec![OP_FIND_INFO + 1, 0];
                let mut tlen = 0;
                for a in self.range(s, e) {
                    if tlen == 0 {
                        tlen = a.typ.len();
                        out[1] = if tlen == 2 { 1 } else { 2 };
                    }
                    if a.typ.len() != tlen || out.len() + 2 + tlen > mtu {
                        break;
                    }
                    out.extend_from_slice(&a.handle.to_le_bytes());
                    out.extend_from_slice(&a.typ);
                }
                if tlen == 0 {
                    return err(s, ERR_NOT_FOUND);
                }
                out
            }
            OP_FIND_BY_TYPE => {
                let (s, e, t) = (u16_at(1)?, u16_at(3)?, u16_at(5)?);
                let want = pdu.get(7..)?;
                if s == 0 || s > e {
                    return err(s, ERR_INVALID_HANDLE);
                }
                let mut out = vec![OP_FIND_BY_TYPE + 1];
                for a in self.range(s, e) {
                    if a.typ == t.to_le_bytes() && a.value == want && out.len() + 4 <= mtu {
                        out.extend_from_slice(&a.handle.to_le_bytes());
                        let end = if a.group_end != 0 {
                            a.group_end
                        } else {
                            a.handle
                        };
                        out.extend_from_slice(&end.to_le_bytes());
                    }
                }
                if out.len() == 1 {
                    return err(s, ERR_NOT_FOUND);
                }
                out
            }
            OP_READ_BY_TYPE | OP_READ_BY_GROUP => {
                let (s, e) = (u16_at(1)?, u16_at(3)?);
                let want = pdu.get(5..)?;
                if s == 0 || s > e {
                    return err(s, ERR_INVALID_HANDLE);
                }
                if op == OP_READ_BY_GROUP && want != 0x2800u16.to_le_bytes() {
                    return err(s, ERR_UNSUPPORTED_GROUP);
                }
                let extra = if op == OP_READ_BY_GROUP { 4 } else { 2 };
                let mut out = vec![op + 1, 0];
                let mut vlen = 0;
                for a in self.range(s, e).filter(|a| a.typ == want) {
                    if vlen == 0 {
                        vlen = a.value.len().min(mtu.saturating_sub(2 + extra));
                        out[1] = (vlen + extra) as u8;
                    }
                    if a.value.len() != vlen || out.len() + extra + vlen > mtu {
                        break;
                    }
                    out.extend_from_slice(&a.handle.to_le_bytes());
                    if op == OP_READ_BY_GROUP {
                        out.extend_from_slice(&a.group_end.to_le_bytes());
                    }
                    out.extend_from_slice(&a.value[..vlen]);
                }
                if vlen == 0 {
                    return err(s, ERR_NOT_FOUND);
                }
                out
            }
            OP_READ | OP_READ_BLOB => {
                let h = u16_at(1)?;
                let off = if op == OP_READ_BLOB {
                    u16_at(3)? as usize
                } else {
                    0
                };
                let Some(a) = self.get(h) else {
                    return err(h, ERR_INVALID_HANDLE);
                };
                if h == RX_HANDLE || h == TX_HANDLE {
                    return err(h, ERR_READ_NOT_PERMITTED);
                }
                if off > a.value.len() {
                    return err(h, ERR_INVALID_OFFSET);
                }
                let value = if h == CCCD_HANDLE {
                    self.cccd.to_le_bytes().to_vec()
                } else {
                    a.value.clone()
                };
                let mut out = vec![op + 1];
                let n = (value.len() - off).min(mtu - 1);
                out.extend_from_slice(&value[off..off + n]);
                out
            }
            OP_WRITE_REQ | OP_WRITE_CMD => {
                let h = u16_at(1)?;
                let val = pdu.get(3..)?;
                if h == RX_HANDLE {
                    on_write(val);
                } else if h == CCCD_HANDLE && val.len() == 2 {
                    self.cccd = u16::from_le_bytes([val[0], val[1]]);
                } else if op == OP_WRITE_REQ {
                    let code = if self.get(h).is_some() {
                        ERR_WRITE_NOT_PERMITTED
                    } else {
                        ERR_INVALID_HANDLE
                    };
                    return err(h, code);
                }
                if op == OP_WRITE_CMD {
                    return Some(None);
                }
                vec![OP_WRITE_RSP]
            }
            _ => return err(0, ERR_NOT_SUPPORTED),
        }))
    }
}

fn error(op: u8, handle: u16, code: u8) -> Vec<u8> {
    let mut v = vec![OP_ERROR, op];
    v.extend_from_slice(&handle.to_le_bytes());
    v.push(code);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(db: &mut Db, pdu: &[u8]) -> Vec<u8> {
        db.handle(pdu, 247, &mut |_| {}).unwrap()
    }

    #[test]
    fn primary_services_come_in_runs_of_equal_uuid_length() {
        let mut db = Db::new("x");
        let r = ask(&mut db, &[0x10, 1, 0, 0xff, 0xff, 0x00, 0x28]);
        assert_eq!(r, vec![0x11, 6, 1, 0, 5, 0, 0x00, 0x18]);
        let r = ask(&mut db, &[0x10, 6, 0, 0xff, 0xff, 0x00, 0x28]);
        assert_eq!(&r[..6], &[0x11, 20, 6, 0, 11, 0]);
        assert_eq!(&r[6..], &uuid128(0)[..]);
        let r = ask(&mut db, &[0x10, 12, 0, 0xff, 0xff, 0x00, 0x28]);
        assert_eq!(r[0], 0x01);
        assert_eq!(r[4], 0x0A);
    }

    #[test]
    fn the_uuid_bytes_are_reversed_on_the_air() {
        assert_eq!(uuid128(0x02)[0], 0x02);
        assert_eq!(uuid128(0x02)[15], 0x6f);
    }

    #[test]
    fn characteristics_and_descriptors_are_found() {
        let mut db = Db::new("x");
        let r = ask(&mut db, &[0x08, 6, 0, 11, 0, 0x03, 0x28]);
        assert_eq!(&r[..5], &[0x09, 21, 7, 0, 0x0C]);
        assert_eq!(r.len(), 2 + 21 * 2);
        let r = ask(&mut db, &[0x04, 11, 0, 11, 0]);
        assert_eq!(r, vec![0x05, 1, 11, 0, 0x02, 0x29]);
    }

    #[test]
    fn writes_reach_the_callback_and_cccd_sticks() {
        let mut db = Db::new("x");
        let mut got = vec![];
        assert!(
            db.handle(&[0x52, 8, 0, 7, 9], 247, &mut |v| got.extend_from_slice(v))
                .is_none()
        );
        assert_eq!(got, vec![7, 9]);
        assert_eq!(ask(&mut db, &[0x12, 11, 0, 1, 0]), vec![0x13]);
        assert_eq!(db.cccd, 1);
    }

    #[test]
    fn unknown_request_is_refused_and_unknown_command_ignored() {
        let mut db = Db::new("x");
        assert_eq!(ask(&mut db, &[0x20]), vec![0x01, 0x20, 0, 0, 0x06]);
        assert!(db.handle(&[0x70], 247, &mut |_| {}).is_none());
    }

    #[test]
    fn a_truncated_request_is_an_error_and_a_truncated_command_nothing() {
        let mut db = Db::new("x");
        assert_eq!(ask(&mut db, &[0x10, 1]), vec![0x01, 0x10, 0, 0, 0x04]);
        assert!(db.handle(&[0x52, 8], 247, &mut |_| {}).is_none());
    }
}
