//! MySQL client/server protocol framing: packets of at most 16 MiB − 1 bytes with a sequence
//! id, logical payloads split over several packets, length-encoded integers and strings.
//!
//! The reader is streaming: a payload is consumed byte by byte across packet boundaries, so a
//! row holding a value of any size is read without buffering the whole row, and oversized
//! values are skipped on the socket (I6). Only small control packets (OK, ERR, EOF, column
//! definitions, handshake) are read whole, under [`MAX_CONTROL_PACKET`].

use std::net::SocketAddr;
use std::sync::Arc;

use ninesql_driver_api::{DriverError, DriverResult};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufStream};
use tokio::net::TcpStream;

use crate::tls::TlsSetup;

/// Largest physical packet payload; a payload of exactly this size continues in the next packet.
pub(crate) const MAX_PACKET: usize = 0x00FF_FFFF;
/// Largest control packet read whole (OK / ERR / EOF / column definition / handshake).
pub(crate) const MAX_CONTROL_PACKET: usize = 1024 * 1024;

/// The transport: TCP, possibly wrapped in TLS.
pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub(crate) fn io_err(e: std::io::Error) -> DriverError {
    DriverError::Closed(format!("connection lost: {e}"))
}

fn proto(msg: &str) -> DriverError {
    DriverError::Protocol(msg.to_owned())
}

/// Packet reader and writer over one transport.
pub(crate) struct Packets {
    io: BufStream<Box<dyn Io>>,
    /// Next sequence id expected / sent.
    seq: u8,
    /// Bytes left in the current physical packet.
    remaining: usize,
    /// The current physical packet was full: the payload continues in the next one.
    more: bool,
    scratch: Vec<u8>,
}

impl Packets {
    pub(crate) fn new(io: Box<dyn Io>) -> Self {
        Self {
            io: BufStream::new(io),
            seq: 0,
            remaining: 0,
            more: false,
            scratch: vec![0; 64 * 1024],
        }
    }

    /// Takes the transport back (TLS upgrade). Bytes the server sent in clear beyond the
    /// greeting, if any, stay in the dropped buffer: nothing sent in clear after the TLS request
    /// is ever read as if it came over TLS (CVE-2021-23222 class).
    pub(crate) fn into_io(self) -> Box<dyn Io> {
        self.io.into_inner()
    }

    pub(crate) fn seq(&self) -> u8 {
        self.seq
    }

    pub(crate) fn set_seq(&mut self, seq: u8) {
        self.seq = seq;
    }

    /// Starts a new command: sequence ids restart at 0.
    pub(crate) fn reset_seq(&mut self) {
        self.seq = 0;
    }

    /// Writes one logical payload (split into packets) and flushes.
    pub(crate) async fn write_payload(&mut self, payload: &[u8]) -> DriverResult<()> {
        self.buffer_payload(payload).await?;
        self.io.flush().await.map_err(io_err)
    }

    async fn buffer_payload(&mut self, payload: &[u8]) -> DriverResult<()> {
        let mut rest = payload;
        loop {
            let n = rest.len().min(MAX_PACKET);
            let len = u32::try_from(n).map_err(|_| proto("packet too large"))?;
            let header = [
                (len & 0xff) as u8,
                ((len >> 8) & 0xff) as u8,
                ((len >> 16) & 0xff) as u8,
                self.seq,
            ];
            self.seq = self.seq.wrapping_add(1);
            self.io.write_all(&header).await.map_err(io_err)?;
            self.io.write_all(&rest[..n]).await.map_err(io_err)?;
            rest = &rest[n..];
            if n < MAX_PACKET {
                return Ok(());
            }
        }
    }

    /// Writes a secret-bearing payload and zeroes the caller's buffer afterwards.
    pub(crate) async fn write_secret(&mut self, payload: &mut [u8]) -> DriverResult<()> {
        let r = self.write_payload(payload).await;
        zeroize::Zeroize::zeroize(payload);
        r
    }

    /// Shuts the transport down (best effort).
    pub(crate) async fn shutdown(&mut self) {
        let _ = self.io.flush().await;
        let _ = self.io.shutdown().await;
    }

    async fn read_header(&mut self) -> DriverResult<()> {
        let mut h = [0u8; 4];
        self.io.read_exact(&mut h).await.map_err(io_err)?;
        let len = usize::from(h[0]) | usize::from(h[1]) << 8 | usize::from(h[2]) << 16;
        if h[3] != self.seq {
            return Err(proto("packets out of order"));
        }
        self.seq = self.seq.wrapping_add(1);
        self.remaining = len;
        self.more = len == MAX_PACKET;
        Ok(())
    }

    /// Starts reading the next logical payload; returns the length of its first physical packet
    /// (used to tell an EOF packet from a row starting with `0xFE`).
    pub(crate) async fn begin(&mut self) -> DriverResult<usize> {
        if self.remaining != 0 || self.more {
            return Err(proto("previous packet not fully read"));
        }
        self.read_header().await?;
        Ok(self.remaining)
    }

    /// Moves to the next physical packet while the current one is exhausted but continued.
    async fn fill(&mut self) -> DriverResult<()> {
        while self.remaining == 0 && self.more {
            self.read_header().await?;
        }
        Ok(())
    }

    /// `true` when the current logical payload is fully read.
    pub(crate) async fn at_end(&mut self) -> DriverResult<bool> {
        self.fill().await?;
        Ok(self.remaining == 0)
    }

    pub(crate) async fn u8(&mut self) -> DriverResult<u8> {
        self.fill().await?;
        if self.remaining == 0 {
            return Err(proto("packet shorter than expected"));
        }
        self.remaining -= 1;
        self.io.read_u8().await.map_err(io_err)
    }

    /// Reads exactly `buf.len()` payload bytes.
    pub(crate) async fn read_exact(&mut self, mut buf: &mut [u8]) -> DriverResult<()> {
        while !buf.is_empty() {
            self.fill().await?;
            if self.remaining == 0 {
                return Err(proto("packet shorter than expected"));
            }
            let n = buf.len().min(self.remaining);
            self.io.read_exact(&mut buf[..n]).await.map_err(io_err)?;
            self.remaining -= n;
            buf = &mut buf[n..];
        }
        Ok(())
    }

    /// Skips `n` payload bytes without keeping them.
    pub(crate) async fn skip(&mut self, mut n: u64) -> DriverResult<()> {
        while n > 0 {
            self.fill().await?;
            if self.remaining == 0 {
                return Err(proto("packet shorter than expected"));
            }
            let chunk = usize::try_from(n)
                .unwrap_or(usize::MAX)
                .min(self.remaining)
                .min(self.scratch.len());
            self.io
                .read_exact(&mut self.scratch[..chunk])
                .await
                .map_err(io_err)?;
            self.remaining -= chunk;
            n -= chunk as u64;
        }
        Ok(())
    }

    /// Skips the rest of the current logical payload.
    pub(crate) async fn skip_rest(&mut self) -> DriverResult<()> {
        loop {
            self.fill().await?;
            if self.remaining == 0 {
                return Ok(());
            }
            let r = self.remaining as u64;
            self.skip(r).await?;
        }
    }

    /// Reads the rest of the current logical payload, at most `max` bytes (else a protocol
    /// error: control packets are small).
    pub(crate) async fn rest(&mut self, max: usize) -> DriverResult<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            self.fill().await?;
            if self.remaining == 0 {
                return Ok(out);
            }
            if out.len() + self.remaining > max {
                return Err(proto("server packet too large"));
            }
            let start = out.len();
            out.resize(start + self.remaining, 0);
            let n = self.remaining;
            self.io
                .read_exact(&mut out[start..start + n])
                .await
                .map_err(io_err)?;
            self.remaining = 0;
        }
    }

    /// Reads a whole logical payload (control packets).
    pub(crate) async fn read_packet(&mut self) -> DriverResult<Vec<u8>> {
        self.begin().await?;
        self.rest(MAX_CONTROL_PACKET).await
    }

    /// Reads a length-encoded integer from the stream. `Ok(None)` for the NULL marker `0xFB`.
    pub(crate) async fn lenenc(&mut self) -> DriverResult<Option<u64>> {
        let first = self.u8().await?;
        self.lenenc_after(first).await
    }

    /// Like [`Packets::lenenc`], with its first byte already read.
    pub(crate) async fn lenenc_after(&mut self, first: u8) -> DriverResult<Option<u64>> {
        let width = match first {
            0..=0xfa => return Ok(Some(u64::from(first))),
            0xfb => return Ok(None),
            0xfc => 2,
            0xfd => 3,
            0xfe => 8,
            0xff => return Err(proto("invalid length-encoded integer")),
        };
        let mut b = [0u8; 8];
        self.read_exact(&mut b[..width]).await?;
        Ok(Some(u64::from_le_bytes(b)))
    }
}

/// Cursor over a control packet held in memory.
pub(crate) struct Buf<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Buf<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub(crate) fn remaining(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    pub(crate) fn u8(&mut self) -> DriverResult<u8> {
        let b = *self
            .data
            .get(self.pos)
            .ok_or_else(|| proto("packet shorter than expected"))?;
        self.pos += 1;
        Ok(b)
    }

    pub(crate) fn take(&mut self, n: usize) -> DriverResult<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.data.len())
            .ok_or_else(|| proto("packet shorter than expected"))?;
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub(crate) fn u16(&mut self) -> DriverResult<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub(crate) fn u32(&mut self) -> DriverResult<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// NUL-terminated bytes (the NUL is consumed); the rest of the packet when no NUL follows.
    pub(crate) fn nul_str(&mut self) -> &'a [u8] {
        let rest = self.remaining();
        match rest.iter().position(|b| *b == 0) {
            Some(i) => {
                self.pos += i + 1;
                &rest[..i]
            }
            None => {
                self.pos = self.data.len();
                rest
            }
        }
    }

    pub(crate) fn lenenc(&mut self) -> DriverResult<Option<u64>> {
        let first = self.u8()?;
        let width = match first {
            0..=0xfa => return Ok(Some(u64::from(first))),
            0xfb => return Ok(None),
            0xfc => 2,
            0xfd => 3,
            0xfe => 8,
            0xff => return Err(proto("invalid length-encoded integer")),
        };
        let b = self.take(width)?;
        let mut v = [0u8; 8];
        v[..width].copy_from_slice(b);
        Ok(Some(u64::from_le_bytes(v)))
    }

    pub(crate) fn lenenc_bytes(&mut self) -> DriverResult<&'a [u8]> {
        let n = self.lenenc()?.unwrap_or(0);
        let n = usize::try_from(n).map_err(|_| proto("length out of range"))?;
        self.take(n)
    }
}

/// Appends a length-encoded integer.
pub(crate) fn put_lenenc(out: &mut Vec<u8>, v: u64) {
    if v < 251 {
        out.push(v as u8);
    } else if v < 1 << 16 {
        out.push(0xfc);
        out.extend_from_slice(&(v as u16).to_le_bytes());
    } else if v < 1 << 24 {
        out.push(0xfd);
        out.extend_from_slice(&(v as u32).to_le_bytes()[..3]);
    } else {
        out.push(0xfe);
        out.extend_from_slice(&v.to_le_bytes());
    }
}

/// Appends length-encoded bytes.
pub(crate) fn put_lenenc_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_lenenc(out, b.len() as u64);
    out.extend_from_slice(b);
}

/// Opens a TCP connection and returns the transport and the peer address (reused by the control
/// connection, so `KILL QUERY` reaches the same server).
pub(crate) async fn open_tcp(host: &str, port: u16) -> DriverResult<(TcpStream, SocketAddr)> {
    let tcp = TcpStream::connect((host, port))
        .await
        .map_err(|e| DriverError::Connect(format!("cannot reach {host}:{port}: {e}")))?;
    let peer = tcp
        .peer_addr()
        .map_err(|e| DriverError::Connect(format!("cannot read the peer address: {e}")))?;
    tcp.set_nodelay(true).ok();
    Ok((tcp, peer))
}

/// Opens a TCP connection to an already resolved address.
pub(crate) async fn open_tcp_addr(addr: SocketAddr) -> DriverResult<TcpStream> {
    let tcp = TcpStream::connect(addr)
        .await
        .map_err(|e| DriverError::Connect(format!("cannot reach the server: {e}")))?;
    tcp.set_nodelay(true).ok();
    Ok(tcp)
}

/// Runs the TLS handshake over `io` (after the client sent its TLS request).
pub(crate) async fn tls_handshake(io: Box<dyn Io>, tls: &TlsSetup) -> DriverResult<Box<dyn Io>> {
    let connector = tokio_rustls::TlsConnector::from(Arc::clone(&tls.config));
    let stream = connector
        .connect(tls.server_name.clone(), io)
        .await
        .map_err(|e| DriverError::Tls(format!("handshake failed: {e}")))?;
    Ok(Box::new(stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn packets(bytes: Vec<u8>) -> Packets {
        let (client, mut server) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            let _ = server.write_all(&bytes).await;
            let _ = server.shutdown().await;
        });
        Packets::new(Box::new(client))
    }

    fn frame(seq: u8, payload: &[u8]) -> Vec<u8> {
        let n = payload.len();
        let mut v = vec![
            (n & 0xff) as u8,
            (n >> 8 & 0xff) as u8,
            (n >> 16) as u8,
            seq,
        ];
        v.extend_from_slice(payload);
        v
    }

    #[tokio::test]
    async fn payload_spanning_packets_reads_through() {
        let big = vec![7u8; MAX_PACKET];
        let mut bytes = frame(0, &big);
        bytes.extend(frame(1, b"tail"));
        let mut p = packets(bytes);
        assert_eq!(p.begin().await.unwrap(), MAX_PACKET);
        p.skip(MAX_PACKET as u64 - 2).await.unwrap();
        let mut b = [0u8; 6];
        p.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"\x07\x07tail");
        assert!(p.at_end().await.unwrap());
    }

    #[tokio::test]
    async fn out_of_order_and_oversized_control_packets_fail() {
        let mut p = packets(frame(3, b"x"));
        assert!(matches!(p.begin().await, Err(DriverError::Protocol(_))));
        let mut p = packets(frame(0, &vec![0u8; MAX_CONTROL_PACKET + 1]));
        assert!(matches!(
            p.read_packet().await,
            Err(DriverError::Protocol(_))
        ));
    }

    #[test]
    fn lenenc_roundtrip() {
        for v in [
            0u64,
            250,
            251,
            65_535,
            65_536,
            (1 << 24) - 1,
            1 << 24,
            u64::MAX,
        ] {
            let mut out = Vec::new();
            put_lenenc(&mut out, v);
            assert_eq!(Buf::new(&out).lenenc().unwrap(), Some(v));
        }
        assert_eq!(Buf::new(&[0xfb]).lenenc().unwrap(), None);
        assert!(Buf::new(&[0xff]).lenenc().is_err());
        assert!(Buf::new(&[0xfe, 1]).lenenc().is_err());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Arbitrary control-packet bytes never panic the cursor.
        #[test]
        fn cursor_never_panics(data in prop::collection::vec(any::<u8>(), 0..64)) {
            let mut b = Buf::new(&data);
            while !b.is_empty() {
                if b.lenenc_bytes().is_err() { break; }
            }
            let mut b = Buf::new(&data);
            let _ = b.nul_str();
            let _ = b.u32();
        }
    }
}
