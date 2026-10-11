//! One link frame. Layout before COBS (all little endian):
//!
//! | off | size | field                                                   |
//! |-----|------|---------------------------------------------------------|
//! | 0   | 1    | kind: 1 DATA, 2 ACK, 3 PING                             |
//! | 1   | 4    | src_epoch: random per boot of the sender                |
//! | 5   | 4    | dst_epoch: DATA/PING: sender's view of the peer's epoch |
//! |     |      | (0 = unknown); ACK: epoch of the DATA being acked       |
//! | 9   | 2    | seq: DATA sequence / ACK: acked sequence                |
//! | 11  | n    | payload (DATA only)                                     |
//! | end | 4    | CRC-32 over everything before                           |
//!
//! On the wire: COBS(frame) followed by one 0x00 delimiter.

use crate::{cobs, crc::crc32};

pub const HEADER_LEN: usize = 11;
pub const CRC_LEN: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Data = 1,
    Ack = 2,
    Ping = 3,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: Kind,
    pub src_epoch: u32,
    pub dst_epoch: u32,
    pub seq: u16,
    pub payload: Vec<u8>,
}

impl Frame {
    /// Appends the encoded frame and its delimiter to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let mut raw = Vec::with_capacity(HEADER_LEN + self.payload.len() + CRC_LEN);
        raw.push(self.kind as u8);
        raw.extend_from_slice(&self.src_epoch.to_le_bytes());
        raw.extend_from_slice(&self.dst_epoch.to_le_bytes());
        raw.extend_from_slice(&self.seq.to_le_bytes());
        raw.extend_from_slice(&self.payload);
        let c = crc32(&raw);
        raw.extend_from_slice(&c.to_le_bytes());
        cobs::encode(&raw, out);
        out.push(0);
    }

    /// Decodes one frame from the bytes between two delimiters.
    pub fn decode(stuffed: &[u8]) -> Option<Frame> {
        let raw = cobs::decode(stuffed)?;
        if raw.len() < HEADER_LEN + CRC_LEN {
            return None;
        }
        let (body, c) = raw.split_at(raw.len() - CRC_LEN);
        if crc32(body) != u32::from_le_bytes([c[0], c[1], c[2], c[3]]) {
            return None;
        }
        let kind = match body[0] {
            1 => Kind::Data,
            2 => Kind::Ack,
            3 => Kind::Ping,
            _ => return None,
        };
        let u32_at = |o: usize| u32::from_le_bytes([body[o], body[o + 1], body[o + 2], body[o + 3]]);
        Some(Frame {
            kind,
            src_epoch: u32_at(1),
            dst_epoch: u32_at(5),
            seq: u16::from_le_bytes([body[9], body[10]]),
            payload: body[HEADER_LEN..].to_vec(),
        })
    }
}

/// Splits a byte stream into delimiter-separated chunks.
pub struct Deframer {
    buf: Vec<u8>,
    max: usize,
    overflow: bool,
}

impl Deframer {
    pub fn new(max_frame: usize) -> Self {
        Deframer {
            buf: Vec::new(),
            max: max_frame,
            overflow: false,
        }
    }

    /// Feeds bytes; calls `f` for every frame that decodes and checks out.
    /// Returns the number of chunks that were discarded as corrupt.
    pub fn push(&mut self, bytes: &[u8], mut f: impl FnMut(Frame)) -> usize {
        let mut bad = 0;
        for &b in bytes {
            if b == 0 {
                if self.overflow {
                    bad += 1;
                } else if !self.buf.is_empty() {
                    match Frame::decode(&self.buf) {
                        Some(fr) => f(fr),
                        None => bad += 1,
                    }
                }
                self.buf.clear();
                self.overflow = false;
            } else if self.buf.len() < self.max {
                self.buf.push(b);
            } else {
                self.overflow = true;
            }
        }
        bad
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_crc() {
        let f = Frame {
            kind: Kind::Data,
            src_epoch: 0xDEAD_BEEF,
            dst_epoch: 7,
            seq: 513,
            payload: vec![0, 1, 0, 2, 255],
        };
        let mut w = Vec::new();
        f.encode(&mut w);
        assert_eq!(*w.last().unwrap(), 0);
        let mut got = Vec::new();
        assert_eq!(Deframer::new(4096).push(&w, |x| got.push(x)), 0);
        assert_eq!(got, vec![f.clone()]);
        // Flip one bit anywhere: the frame is rejected, never misdelivered.
        for i in 0..w.len() - 1 {
            let mut c = w.clone();
            c[i] ^= 0x10;
            let mut got = Vec::new();
            Deframer::new(4096).push(&c, |x| got.push(x));
            assert!(got.is_empty() || got == vec![f.clone()], "bit flip at {i} accepted");
            assert!(got.is_empty(), "bit flip at {i} gave a frame");
        }
    }
}
