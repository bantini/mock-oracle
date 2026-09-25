//! TTC encoding: the message layer inside TNS DATA packets.
//!
//! Integers use Oracle's variable-length "UB" format: a length byte followed by
//! that many big-endian bytes. Byte strings are length-prefixed, or chunked
//! when longer than 252 bytes.

use std::io;

use crate::tns::invalid;

pub const LONG_LENGTH_INDICATOR: u8 = 254;
pub const NULL_LENGTH_INDICATOR: u8 = 255;
const MAX_SHORT_LENGTH: usize = 252;
const CHUNK_SIZE: usize = 65536;

pub struct ReadBuf<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ReadBuf<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn bytes(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if n > self.remaining() {
            return Err(invalid(format!(
                "TTC message truncated: need {n} bytes, have {}",
                self.remaining()
            )));
        }
        let b = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(b)
    }

    pub fn u8(&mut self) -> io::Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    fn var_int(&mut self, max: usize) -> io::Result<(u64, bool)> {
        let mut size = self.u8()? as usize;
        let negative = size & 0x80 != 0;
        size &= 0x7f;
        if size > max {
            return Err(invalid(format!("integer of {size} bytes exceeds {max}")));
        }
        let v = self
            .bytes(size)?
            .iter()
            .fold(0u64, |acc, b| (acc << 8) | *b as u64);
        Ok((v, negative))
    }

    pub fn ub2(&mut self) -> io::Result<u16> {
        Ok(self.var_int(2)?.0 as u16)
    }

    pub fn ub4(&mut self) -> io::Result<u32> {
        Ok(self.var_int(4)?.0 as u32)
    }

    /// A length-prefixed byte string; `None` for NULL (length 0 or 255).
    pub fn bytes_with_length(&mut self) -> io::Result<Option<Vec<u8>>> {
        let len = self.u8()?;
        match len {
            0 | NULL_LENGTH_INDICATOR => Ok(None),
            LONG_LENGTH_INDICATOR => {
                let mut out = Vec::new();
                loop {
                    let chunk = self.ub4()? as usize;
                    if chunk == 0 {
                        break;
                    }
                    out.extend_from_slice(self.bytes(chunk)?);
                }
                Ok(Some(out))
            }
            n => Ok(Some(self.bytes(n as usize)?.to_vec())),
        }
    }

    pub fn string_with_length(&mut self) -> io::Result<String> {
        let b = self.bytes_with_length()?.unwrap_or_default();
        String::from_utf8(b).map_err(|_| invalid("string is not UTF-8".into()))
    }

    /// A key/value pair as sent in login messages.
    pub fn key_value(&mut self) -> io::Result<(String, String, u32)> {
        let key_len = self.ub4()?;
        let key = if key_len > 0 {
            self.string_with_length()?
        } else {
            String::new()
        };
        let val_len = self.ub4()?;
        let val = if val_len > 0 {
            self.string_with_length()?
        } else {
            String::new()
        };
        let flags = self.ub4()?;
        Ok((key, val, flags))
    }
}

#[derive(Default)]
pub struct WriteBuf {
    pub buf: Vec<u8>,
}

impl WriteBuf {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn u16_be(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn u16_le(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn raw(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    fn var_int(&mut self, v: u64, negative: bool) {
        let sign = if negative { 0x80 } else { 0 };
        if v == 0 {
            self.u8(0);
        } else if v <= 0xff {
            self.u8(1 | sign);
            self.u8(v as u8);
        } else if v <= 0xffff {
            self.u8(2 | sign);
            self.u16_be(v as u16);
        } else if v <= 0xffff_ffff {
            self.u8(4 | sign);
            self.raw(&(v as u32).to_be_bytes());
        } else {
            self.u8(8 | sign);
            self.raw(&v.to_be_bytes());
        }
    }

    pub fn ub2(&mut self, v: u16) {
        self.var_int(v as u64, false);
    }

    pub fn ub4(&mut self, v: u32) {
        self.var_int(v as u64, false);
    }

    pub fn ub8(&mut self, v: u64) {
        self.var_int(v, false);
    }

    pub fn sb2(&mut self, v: i16) {
        self.var_int(v.unsigned_abs() as u64, v < 0);
    }

    pub fn bytes_with_length(&mut self, b: &[u8]) {
        if b.len() <= MAX_SHORT_LENGTH {
            self.u8(b.len() as u8);
            self.raw(b);
        } else {
            self.u8(LONG_LENGTH_INDICATOR);
            for chunk in b.chunks(CHUNK_SIZE) {
                self.ub4(chunk.len() as u32);
                self.raw(chunk);
            }
            self.ub4(0);
        }
    }

    /// A string preceded by its UB4 byte length and then its own length prefix,
    /// the layout used for names in column metadata.
    pub fn str_with_ub4_length(&mut self, s: &str) {
        self.ub4(s.len() as u32);
        if !s.is_empty() {
            self.bytes_with_length(s.as_bytes());
        }
    }

    /// A key/value pair as returned in login responses.
    pub fn key_value(&mut self, key: &str, value: &str, flags: u32) {
        self.str_with_ub4_length(key);
        self.str_with_ub4_length(value);
        self.ub4(flags);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn var_ints_round_trip() {
        let mut w = WriteBuf::new();
        for v in [0u32, 1, 255, 256, 65535, 65536, u32::MAX] {
            w.ub4(v);
        }
        w.sb2(-300);
        let mut r = ReadBuf::new(&w.buf);
        for v in [0u32, 1, 255, 256, 65535, 65536, u32::MAX] {
            assert_eq!(r.ub4().unwrap(), v);
        }
        assert_eq!(r.u8().unwrap(), 0x82);
        assert_eq!(r.bytes(2).unwrap(), &300u16.to_be_bytes());
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn long_byte_strings_are_chunked() {
        let data = vec![7u8; 1000];
        let mut w = WriteBuf::new();
        w.bytes_with_length(&data);
        assert_eq!(w.buf[0], LONG_LENGTH_INDICATOR);
        assert_eq!(
            ReadBuf::new(&w.buf).bytes_with_length().unwrap(),
            Some(data)
        );
    }

    #[test]
    fn key_values_round_trip() {
        let mut w = WriteBuf::new();
        w.key_value("AUTH_SESSKEY", "ABCD", 1);
        assert_eq!(
            ReadBuf::new(&w.buf).key_value().unwrap(),
            ("AUTH_SESSKEY".into(), "ABCD".into(), 1)
        );
    }
}
