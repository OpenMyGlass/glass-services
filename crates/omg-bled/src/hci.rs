//! HCI over H4: the packets the UART carries, and the firmware patch file. Pure.

pub const H4_COMMAND: u8 = 1;
pub const H4_ACL: u8 = 2;
pub const H4_EVENT: u8 = 4;

pub const OP_DISCONNECT: u16 = 0x0406;
pub const OP_SET_EVENT_MASK: u16 = 0x0C01;
pub const OP_RESET: u16 = 0x0C03;
pub const OP_READ_LOCAL_NAME: u16 = 0x0C14;
pub const OP_READ_BUFFER_SIZE: u16 = 0x1005;
pub const OP_LE_SET_EVENT_MASK: u16 = 0x2001;
pub const OP_LE_READ_BUFFER_SIZE: u16 = 0x2002;
pub const OP_LE_SET_RANDOM_ADDRESS: u16 = 0x2005;
pub const OP_LE_SET_ADV_PARAMS: u16 = 0x2006;
pub const OP_LE_SET_ADV_DATA: u16 = 0x2008;
pub const OP_LE_SET_SCAN_RESPONSE: u16 = 0x2009;
pub const OP_LE_SET_ADV_ENABLE: u16 = 0x200A;
pub const OP_LE_LTK_NEGATIVE: u16 = 0x201B;
pub const OP_VS_UPDATE_BAUD: u16 = 0xFC18;
pub const OP_VS_SLEEP_MODE: u16 = 0xFC27;
pub const OP_VS_MINIDRIVER: u16 = 0xFC2E;
pub const OP_VS_LAUNCH_RAM: u16 = 0xFC4E;

pub const EV_DISCONNECT_COMPLETE: u8 = 0x05;
pub const EV_COMMAND_COMPLETE: u8 = 0x0E;
pub const EV_COMMAND_STATUS: u8 = 0x0F;
pub const EV_HARDWARE_ERROR: u8 = 0x10;
pub const EV_COMPLETED_PACKETS: u8 = 0x13;
pub const EV_LE_META: u8 = 0x3E;

/// No ACL packet on this link can be near this: a longer length means the stream lost its place.
const MAX_ACL_LEN: usize = 1100;

/// One packet from the controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    Event { code: u8, params: Vec<u8> },
    Acl { handle_flags: u16, data: Vec<u8> },
}

/// Splits the UART's bytes into packets. An unknown packet type is skipped a byte at a time,
/// which finds the next packet; `desync` counts the bytes lost that way.
#[derive(Debug, Default)]
pub struct H4Parser {
    buf: Vec<u8>,
    pub desync: u64,
}

impl H4Parser {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    pub fn next_packet(&mut self) -> Option<Packet> {
        loop {
            match *self.buf.first()? {
                H4_EVENT => {
                    let need = 3 + *self.buf.get(2)? as usize;
                    if self.buf.len() < need {
                        return None;
                    }
                    let packet = Packet::Event {
                        code: self.buf[1],
                        params: self.buf[3..need].to_vec(),
                    };
                    self.buf.drain(..need);
                    return Some(packet);
                }
                H4_ACL => {
                    if self.buf.len() < 5 {
                        return None;
                    }
                    let len = u16::from_le_bytes([self.buf[3], self.buf[4]]) as usize;
                    if len > MAX_ACL_LEN {
                        self.desync += 1;
                        self.buf.remove(0);
                        continue;
                    }
                    if self.buf.len() < 5 + len {
                        return None;
                    }
                    let packet = Packet::Acl {
                        handle_flags: u16::from_le_bytes([self.buf[1], self.buf[2]]),
                        data: self.buf[5..5 + len].to_vec(),
                    };
                    self.buf.drain(..5 + len);
                    return Some(packet);
                }
                _ => {
                    self.desync += 1;
                    self.buf.remove(0);
                }
            }
        }
    }
}

/// A command as it goes on the UART.
pub fn command(op: u16, params: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + params.len());
    v.push(H4_COMMAND);
    v.extend_from_slice(&op.to_le_bytes());
    v.push(params.len() as u8);
    v.extend_from_slice(params);
    v
}

/// One L2CAP PDU, with its header, as ACL packets of at most `acl_len` bytes of data.
pub fn acl_fragments(handle: u16, cid: u16, payload: &[u8], acl_len: usize) -> Vec<Vec<u8>> {
    let mut pdu = Vec::with_capacity(4 + payload.len());
    pdu.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    pdu.extend_from_slice(&cid.to_le_bytes());
    pdu.extend_from_slice(payload);
    pdu.chunks(acl_len)
        .enumerate()
        .map(|(i, frag)| {
            // Packet boundary: first non-flushable from the host, then continuing.
            let flags = if i == 0 { 0 } else { 1 << 12 };
            let mut p = Vec::with_capacity(5 + frag.len());
            p.push(H4_ACL);
            p.extend_from_slice(&(handle | flags).to_le_bytes());
            p.extend_from_slice(&(frag.len() as u16).to_le_bytes());
            p.extend_from_slice(frag);
            p
        })
        .collect()
}

/// The records of a `.hcd` firmware patch: `op:u16le, len:u8, data`.
pub fn parse_hcd(b: &[u8]) -> Result<Vec<(u16, &[u8])>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let h = b.get(i..i + 3).ok_or("hcd: short header")?;
        let n = h[2] as usize;
        let d = b.get(i + 3..i + 3 + n).ok_or("hcd: short record")?;
        out.push((u16::from_le_bytes([h[0], h[1]]), d));
        i += 3 + n;
    }
    if !out.iter().any(|(op, _)| *op == OP_VS_LAUNCH_RAM) {
        return Err("hcd: no Launch RAM record".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hcd_records_split_and_need_a_launch() {
        let b = [0x4C, 0xFC, 2, 1, 2, 0x4E, 0xFC, 0];
        let r = parse_hcd(&b).unwrap();
        assert_eq!(r, vec![(0xFC4C, &[1u8, 2][..]), (0xFC4E, &[][..])]);
        assert!(parse_hcd(&b[..4]).is_err());
        assert!(parse_hcd(&b[..5]).is_err());
    }

    #[test]
    fn packets_come_out_whole_whatever_the_reads() {
        let mut bytes = vec![H4_EVENT, 0x0E, 2, 1, 2];
        bytes.extend_from_slice(&[H4_ACL, 0x40, 0x20, 3, 0, 7, 8, 9]);
        let mut p = H4Parser::default();
        let mut got = Vec::new();
        for b in &bytes {
            p.push(&[*b]);
            got.extend(p.next_packet());
        }
        assert_eq!(
            got,
            vec![
                Packet::Event {
                    code: 0x0E,
                    params: vec![1, 2]
                },
                Packet::Acl {
                    handle_flags: 0x2040,
                    data: vec![7, 8, 9]
                },
            ]
        );
    }

    #[test]
    fn garbage_is_skipped_and_counted() {
        let mut p = H4Parser::default();
        p.push(&[0xEE, 0xEE, H4_EVENT, 0x05, 1, 9]);
        assert!(matches!(
            p.next_packet(),
            Some(Packet::Event { code: 5, .. })
        ));
        assert_eq!(p.desync, 2);
        // A length no packet has must not make the parser wait for it.
        p.push(&[H4_ACL, 0, 0, 0xFF, 0xFF, H4_EVENT, 0x05, 0]);
        assert!(matches!(
            p.next_packet(),
            Some(Packet::Event { code: 5, .. })
        ));
    }

    #[test]
    fn a_pdu_fragments_with_continuation_flags() {
        let f = acl_fragments(0x40, 4, &[1; 30], 27);
        assert_eq!(f.len(), 2);
        assert_eq!(&f[0][..5], &[H4_ACL, 0x40, 0x00, 27, 0]);
        assert_eq!(&f[0][5..9], &[30, 0, 4, 0]);
        assert_eq!(&f[1][..5], &[H4_ACL, 0x40, 0x10, 7, 0]);
    }
}
