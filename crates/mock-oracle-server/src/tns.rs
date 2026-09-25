//! TNS packet framing: the outer layer of Oracle Net.
//!
//! Every packet starts with an 8-byte header: length, checksum, type, flags,
//! header checksum. Before the connection is accepted the length is 2 bytes;
//! after accepting protocol version 315 or later ("large SDU") it is 4 bytes.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const CONNECT: u8 = 1;
pub const ACCEPT: u8 = 2;
pub const REFUSE: u8 = 4;
pub const DATA: u8 = 6;
pub const MARKER: u8 = 12;
pub const CONTROL: u8 = 14;

pub const HEADER_LEN: usize = 8;

/// Data packet flags (the 2 bytes after the header of a DATA packet).
pub const DATA_FLAG_EOF: u16 = 0x0040;
pub const DATA_FLAG_END_OF_REQUEST: u16 = 0x0800;

/// Protocol version we accept: 3.18 (large SDU, data flags, no end-of-response).
pub const VERSION: u16 = 318;
pub const MIN_VERSION: u16 = 315;

#[derive(Debug)]
pub struct Packet {
    pub kind: u8,
    /// Everything after the 8-byte header.
    pub body: Vec<u8>,
}

impl Packet {
    /// For DATA packets: the data flags.
    pub fn data_flags(&self) -> u16 {
        u16::from_be_bytes([self.body[0], self.body[1]])
    }

    /// For DATA packets: the payload after the data flags.
    pub fn data(&self) -> &[u8] {
        &self.body[2..]
    }
}

pub async fn read_packet<R: AsyncRead + Unpin>(
    r: &mut R,
    large_sdu: bool,
) -> io::Result<Option<Packet>> {
    let mut header = [0u8; HEADER_LEN];
    match r.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = if large_sdu {
        u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize
    } else {
        u16::from_be_bytes([header[0], header[1]]) as usize
    };
    if !(HEADER_LEN..=4 * 1024 * 1024).contains(&len) {
        return Err(invalid(format!("bad TNS packet length {len}")));
    }
    let mut body = vec![0u8; len - HEADER_LEN];
    r.read_exact(&mut body).await?;
    let packet = Packet {
        kind: header[4],
        body,
    };
    if packet.kind == DATA && packet.body.len() < 2 {
        return Err(invalid("DATA packet without data flags".into()));
    }
    Ok(Some(packet))
}

pub async fn write_packet<W: AsyncWrite + Unpin>(
    w: &mut W,
    large_sdu: bool,
    kind: u8,
    body: &[u8],
) -> io::Result<()> {
    let len = HEADER_LEN + body.len();
    let mut out = Vec::with_capacity(len);
    if large_sdu {
        out.extend_from_slice(&(len as u32).to_be_bytes());
    } else {
        out.extend_from_slice(&(len as u16).to_be_bytes());
        out.extend_from_slice(&[0, 0]);
    }
    out.extend_from_slice(&[kind, 0, 0, 0]);
    out.extend_from_slice(body);
    w.write_all(&out).await
}

/// Writes a TTC payload as one or more DATA packets that each fit in `sdu`.
pub async fn write_data<W: AsyncWrite + Unpin>(
    w: &mut W,
    sdu: usize,
    payload: &[u8],
) -> io::Result<()> {
    let max_chunk = sdu - HEADER_LEN - 2;
    for chunk in payload.chunks(max_chunk) {
        let mut body = Vec::with_capacity(chunk.len() + 2);
        body.extend_from_slice(&0u16.to_be_bytes());
        body.extend_from_slice(chunk);
        write_packet(w, true, DATA, &body).await?;
    }
    w.flush().await
}

/// The fields of a CONNECT packet that matter to us.
#[derive(Debug)]
pub struct ConnectRequest {
    pub version: u16,
    pub sdu: u32,
    pub tdu: u32,
    /// Bytes of connect data announced; they follow in a separate DATA packet
    /// when they do not fit in the CONNECT packet itself.
    pub data_len: usize,
    pub data: Vec<u8>,
}

/// Offsets below are relative to the packet start, as in Oracle's docs; `body`
/// starts at offset 8.
pub fn parse_connect(p: &Packet) -> io::Result<ConnectRequest> {
    let b = &p.body;
    let at = |off: usize| off - HEADER_LEN;
    if b.len() < at(74) {
        return Err(invalid("short CONNECT packet".into()));
    }
    let u16_at = |off: usize| u16::from_be_bytes([b[at(off)], b[at(off) + 1]]);
    let u32_at = |off: usize| u32::from_be_bytes(b[at(off)..at(off) + 4].try_into().unwrap());
    let data_len = u16_at(24) as usize;
    let data_off = u16_at(26) as usize;
    let data = if data_off >= HEADER_LEN && b.len() >= at(data_off) + data_len {
        b[at(data_off)..at(data_off) + data_len].to_vec()
    } else {
        Vec::new()
    };
    Ok(ConnectRequest {
        version: u16_at(8),
        sdu: u32_at(58),
        tdu: u32_at(62),
        data_len,
        data,
    })
}

/// Builds the body of an ACCEPT packet (version 3.18, no network services, no
/// compression, no end-of-response or fast-auth support).
pub fn accept_body(sdu: u32, tdu: u32) -> Vec<u8> {
    const NA_NO_SERVICES: u8 = 0x08;
    let mut b = vec![0u8; 45 - HEADER_LEN];
    let put16 = |b: &mut Vec<u8>, off: usize, v: u16| {
        b[off - HEADER_LEN..off - HEADER_LEN + 2].copy_from_slice(&v.to_be_bytes())
    };
    put16(&mut b, 8, VERSION);
    put16(&mut b, 10, 0x0001); // options: don't care
    put16(&mut b, 12, sdu.min(u16::MAX as u32) as u16);
    put16(&mut b, 14, tdu.min(u16::MAX as u32) as u16);
    put16(&mut b, 16, 0x0100); // hardware byte order
    put16(&mut b, 18, 0); // accept data length
    put16(&mut b, 20, 45); // accept data offset
    b[23 - HEADER_LEN] = NA_NO_SERVICES; // flag1
    b[32 - HEADER_LEN..36 - HEADER_LEN].copy_from_slice(&sdu.to_be_bytes());
    b[36 - HEADER_LEN..40 - HEADER_LEN].copy_from_slice(&tdu.to_be_bytes());
    // offset 40: compression off; 41..45: accept flags2 = 0
    b
}

/// Builds the body of a REFUSE packet carrying an Oracle error number.
pub fn refuse_body(error: u32) -> Vec<u8> {
    let text = format!("(DESCRIPTION=(ERR={error}))");
    let mut b = Vec::with_capacity(4 + text.len());
    b.push(4); // user reason
    b.push(0); // system reason
    b.extend_from_slice(&(text.len() as u16).to_be_bytes());
    b.extend_from_slice(text.as_bytes());
    b
}

/// Extracts `KEY=value` from a connect descriptor, case-insensitively.
pub fn descriptor_value(descriptor: &str, key: &str) -> Option<String> {
    let upper = descriptor.to_uppercase();
    let needle = format!("({}=", key.to_uppercase());
    let start = upper.find(&needle)? + needle.len();
    let end = descriptor[start..].find(')')? + start;
    Some(descriptor[start..end].to_string())
}

pub fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_service_name_from_descriptor() {
        let d = "(DESCRIPTION=(CONNECT_DATA=(SERVICE_NAME=freepdb1)(CID=(PROGRAM=node)))(ADDRESS=(PROTOCOL=tcp)(HOST=localhost)(PORT=1521)))";
        assert_eq!(
            descriptor_value(d, "service_name").as_deref(),
            Some("freepdb1")
        );
        assert_eq!(descriptor_value(d, "SID"), None);
    }

    #[test]
    fn accept_packet_layout() {
        let b = accept_body(8192, 65535);
        assert_eq!(b.len() + HEADER_LEN, 45);
        assert_eq!(u16::from_be_bytes([b[0], b[1]]), VERSION);
        assert_eq!(u32::from_be_bytes(b[24..28].try_into().unwrap()), 8192);
    }
}
