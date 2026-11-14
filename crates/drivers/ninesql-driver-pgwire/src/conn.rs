//! Low-level connection: message framing over TCP or TLS, the startup / authentication
//! handshake, server parameters (`ParameterStatus`), and a streaming `DataRow` reader that cuts
//! values to the fetch budget **while reading** them, so an oversized value is never held in
//! memory (I6).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::{BufMut, BytesMut};
use fallible_iterator::FallibleIterator as _;
use ninesql_driver_api::{DriverError, DriverResult, RowBudget, Secret};
use postgres_protocol::message::{backend, frontend};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufStream};
use tokio::net::TcpStream;

use crate::auth::{self, Scram};
use crate::tls::TlsSetup;
use crate::types::BatchBuilder;

/// Largest non-`DataRow` message accepted (row descriptions, errors, notices…). A server that
/// sends more is treated as broken rather than buffered.
const MAX_MESSAGE: usize = 16 * 1024 * 1024;
/// Largest value kept by internal (metadata) queries; longer values are cut.
const MAX_INTERNAL_VALUE: usize = 64 * 1024;
/// Largest fixed-width value accepted (protects the binary decoders).
const MAX_FIXED: usize = 16;

/// The transport: TCP, possibly wrapped in TLS.
pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// A PostgreSQL error or notice, decoded from its fields.
#[derive(Clone, Debug, Default)]
pub(crate) struct PgError {
    pub(crate) severity: String,
    pub(crate) code: String,
    pub(crate) message: String,
    pub(crate) hint: Option<String>,
    pub(crate) position: Option<u32>,
}

impl PgError {
    fn from_fields(mut fields: backend::ErrorFields<'_>) -> Self {
        let mut e = Self::default();
        while let Ok(Some(f)) = fields.next() {
            let v = String::from_utf8_lossy(f.value_bytes()).into_owned();
            match f.type_() {
                b'V' => e.severity = v,
                b'S' if e.severity.is_empty() => e.severity = v,
                b'C' => e.code = v,
                b'M' => e.message = v,
                b'H' => e.hint = Some(v),
                b'P' => e.position = v.parse().ok(),
                _ => {}
            }
        }
        e
    }

    /// `FATAL` / `PANIC`: the server closes the connection after it.
    pub(crate) fn is_fatal(&self) -> bool {
        matches!(self.severity.as_str(), "FATAL" | "PANIC")
    }

    /// User-presentable text: the message and the hint. `DETAIL` is left out, since it often
    /// quotes row values (constraint keys).
    pub(crate) fn display(&self) -> String {
        match &self.hint {
            Some(h) => format!("{} (hint: {h})", self.message),
            None => self.message.clone(),
        }
    }
}

/// A decoded server message the caller acts on.
pub(crate) enum Msg {
    /// A `DataRow` was decoded into the row sink.
    Row,
    /// A notice (to forward as a stream item).
    Notice(PgError),
    /// An error response.
    Error(PgError),
    /// Any other message.
    Other(backend::Message),
}

/// Where `DataRow` messages go.
pub(crate) enum RowSink<'a> {
    /// No row expected: a `DataRow` is a protocol error.
    None,
    /// Decoded into Arrow builders under the fetch budget.
    Arrow(&'a mut BatchBuilder),
    /// Text values of an internal query (each cut at 64 KiB).
    Text(&'a mut Vec<Vec<Option<String>>>),
    /// Read and dropped (draining after a cancel or a dropped stream).
    Discard,
}

/// Server-reported parameters and the lexical flag shared with the dialect.
#[derive(Debug)]
pub(crate) struct ServerParams {
    pub(crate) values: HashMap<String, String>,
    pub(crate) standard_conforming_strings: Arc<AtomicBool>,
}

impl ServerParams {
    fn set(&mut self, name: &str, value: &str) {
        if name == "standard_conforming_strings" {
            self.standard_conforming_strings
                .store(value == "on", Ordering::SeqCst);
        }
        self.values.insert(name.to_owned(), value.to_owned());
    }

    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// Major version from `server_version` (`"18.0 (Debian…)"` → 18, `"9.6.24"` → 9).
    pub(crate) fn major_version(&self) -> Option<u32> {
        let v = self.get("server_version")?;
        let digits: String = v.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    }
}

/// One open connection to a backend.
pub(crate) struct Conn {
    io: BufStream<Box<dyn Io>>,
    out: BytesMut,
    pub(crate) server: ServerParams,
    pub(crate) process_id: i32,
    pub(crate) secret_key: i32,
    /// Transaction status of the last `ReadyForQuery`: `I`, `T` or `E`.
    pub(crate) txn_status: u8,
    scratch: Vec<u8>,
}

/// Opens a TCP connection, negotiates TLS when `tls` is set (never falling back to plain text),
/// and returns the transport and the peer address (reused by cancel requests).
pub(crate) async fn open_transport(
    host: &str,
    port: u16,
    tls: Option<&TlsSetup>,
) -> DriverResult<(Box<dyn Io>, SocketAddr)> {
    let tcp = TcpStream::connect((host, port))
        .await
        .map_err(|e| DriverError::Connect(format!("cannot reach {host}:{port}: {e}")))?;
    let peer = tcp
        .peer_addr()
        .map_err(|e| DriverError::Connect(format!("cannot read the peer address: {e}")))?;
    tcp.set_nodelay(true).ok();
    let io = negotiate_tls(tcp, tls).await?;
    Ok((io, peer))
}

/// Opens a transport to an already resolved address (cancel requests).
pub(crate) async fn open_transport_addr(
    addr: SocketAddr,
    tls: Option<&TlsSetup>,
) -> DriverResult<Box<dyn Io>> {
    let tcp = TcpStream::connect(addr)
        .await
        .map_err(|e| DriverError::Connect(format!("cannot reach the server: {e}")))?;
    tcp.set_nodelay(true).ok();
    negotiate_tls(tcp, tls).await
}

async fn negotiate_tls(mut tcp: TcpStream, tls: Option<&TlsSetup>) -> DriverResult<Box<dyn Io>> {
    let Some(tls) = tls else {
        return Ok(Box::new(tcp));
    };
    let mut buf = BytesMut::new();
    frontend::ssl_request(&mut buf);
    tcp.write_all(&buf)
        .await
        .map_err(|e| DriverError::Connect(format!("cannot send the TLS request: {e}")))?;
    // Exactly one byte, unbuffered: nothing sent in clear after it may be read (CVE-2021-23222).
    let answer = tcp
        .read_u8()
        .await
        .map_err(|e| DriverError::Connect(format!("no answer to the TLS request: {e}")))?;
    match answer {
        b'S' => {}
        b'N' => {
            return Err(DriverError::Tls(
                "the server does not accept TLS connections; NineSQL never falls back to an \
                 unencrypted connection when TLS is required"
                    .into(),
            ));
        }
        other => {
            return Err(DriverError::Protocol(format!(
                "unexpected answer {other:#04x} to the TLS request"
            )));
        }
    }
    let connector = tokio_rustls::TlsConnector::from(Arc::clone(&tls.config));
    let stream = connector
        .connect(tls.server_name.clone(), tcp)
        .await
        .map_err(|e| DriverError::Tls(format!("handshake failed: {e}")))?;
    Ok(Box::new(stream))
}

/// Typed startup options; never formatted into a connection string.
pub(crate) struct Startup<'a> {
    pub(crate) user: &'a str,
    pub(crate) database: Option<&'a str>,
    pub(crate) application_name: &'a str,
    /// Accepted driver properties, sent as startup parameters.
    pub(crate) extra: &'a [(String, String)],
    /// Whether the transport is protected (TLS or an SSH tunnel): cleartext passwords are
    /// refused otherwise.
    pub(crate) protected_transport: bool,
}

impl Conn {
    fn new(io: Box<dyn Io>, scs: Arc<AtomicBool>) -> Self {
        Self {
            io: BufStream::new(io),
            out: BytesMut::with_capacity(8 * 1024),
            server: ServerParams {
                values: HashMap::new(),
                standard_conforming_strings: scs,
            },
            process_id: 0,
            secret_key: 0,
            txn_status: b'I',
            scratch: vec![0; 64 * 1024],
        }
    }

    /// Startup and authentication, up to the first `ReadyForQuery`. The password is used for
    /// the exchange and dropped by the caller right after.
    pub(crate) async fn startup(
        io: Box<dyn Io>,
        scs: Arc<AtomicBool>,
        startup: &Startup<'_>,
        password: Option<&Secret>,
    ) -> DriverResult<Self> {
        let mut conn = Self::new(io, scs);
        let mut params: Vec<(&str, &str)> = vec![
            ("user", startup.user),
            ("application_name", startup.application_name),
            ("client_encoding", "UTF8"),
            ("DateStyle", "ISO"),
        ];
        if let Some(db) = startup.database {
            params.push(("database", db));
        }
        for (k, v) in startup.extra {
            params.push((k.as_str(), v.as_str()));
        }
        frontend::startup_message(params.iter().copied(), &mut conn.out)
            .map_err(|e| DriverError::InvalidParams(format!("invalid startup parameter: {e}")))?;
        conn.flush().await?;
        conn.authenticate(startup, password).await?;
        // BackendKeyData, ParameterStatus… up to ReadyForQuery.
        loop {
            match conn.next(RowSink::None).await? {
                Msg::Other(backend::Message::BackendKeyData(k)) => {
                    conn.process_id = k.process_id();
                    conn.secret_key = k.secret_key();
                }
                Msg::Other(backend::Message::ReadyForQuery(r)) => {
                    conn.txn_status = r.status();
                    break;
                }
                Msg::Error(e) => return Err(connect_error(&e)),
                Msg::Notice(_) | Msg::Other(_) | Msg::Row => {}
            }
        }
        Ok(conn)
    }

    async fn authenticate(
        &mut self,
        startup: &Startup<'_>,
        password: Option<&Secret>,
    ) -> DriverResult<()> {
        let pw = || -> DriverResult<&[u8]> {
            password.map(|p| p.expose().as_bytes()).ok_or_else(|| {
                DriverError::Auth("the server asks for a password and none was given".into())
            })
        };
        let mut scram: Option<Scram> = None;
        loop {
            match self.next(RowSink::None).await? {
                Msg::Other(backend::Message::AuthenticationOk) => return Ok(()),
                Msg::Other(backend::Message::AuthenticationCleartextPassword) => {
                    if !startup.protected_transport {
                        return Err(DriverError::Auth(
                            "the server asks for a cleartext password over an unencrypted \
                             connection; enable TLS or use SCRAM-SHA-256"
                                .into(),
                        ));
                    }
                    let secret = pw()?;
                    self.send_secret(|buf| frontend::password_message(secret, buf))
                        .await?;
                }
                Msg::Other(backend::Message::AuthenticationMd5Password(body)) => {
                    let hash = auth::md5_password(startup.user, pw()?, body.salt());
                    self.send_secret(|buf| frontend::password_message(hash.as_bytes(), buf))
                        .await?;
                }
                Msg::Other(backend::Message::AuthenticationSasl(body)) => {
                    let mut mechanisms = body.mechanisms();
                    let mut has_scram = false;
                    while let Ok(Some(m)) = mechanisms.next() {
                        has_scram |= m == auth::SCRAM_SHA_256;
                    }
                    if !has_scram {
                        return Err(DriverError::Auth(
                            "the server offers no supported SASL mechanism (SCRAM-SHA-256)".into(),
                        ));
                    }
                    let s = Scram::new(pw()?).map_err(|e| DriverError::Auth(e.0))?;
                    frontend::sasl_initial_response(
                        auth::SCRAM_SHA_256,
                        &s.client_first(),
                        &mut self.out,
                    )
                    .map_err(|e| DriverError::Auth(e.to_string()))?;
                    self.flush().await?;
                    scram = Some(s);
                }
                Msg::Other(backend::Message::AuthenticationSaslContinue(body)) => {
                    let s = scram.as_mut().ok_or_else(|| {
                        DriverError::Protocol("SASL continue without a SASL exchange".into())
                    })?;
                    let reply = zeroize::Zeroizing::new(
                        s.client_final(body.data())
                            .map_err(|e| DriverError::Auth(e.0))?,
                    );
                    self.send_secret(|buf| frontend::sasl_response(&reply, buf))
                        .await?;
                }
                Msg::Other(backend::Message::AuthenticationSaslFinal(body)) => {
                    let s = scram.as_ref().ok_or_else(|| {
                        DriverError::Protocol("SASL final without a SASL exchange".into())
                    })?;
                    s.verify_final(body.data())
                        .map_err(|e| DriverError::Auth(e.0))?;
                }
                Msg::Other(
                    backend::Message::AuthenticationGss
                    | backend::Message::AuthenticationSspi
                    | backend::Message::AuthenticationKerberosV5
                    | backend::Message::AuthenticationScmCredential
                    | backend::Message::AuthenticationGssContinue(_),
                ) => {
                    return Err(DriverError::Auth(
                        "the server asks for GSSAPI / SSPI / Kerberos authentication, which the \
                         pgwire driver does not support; use a password (SCRAM-SHA-256)"
                            .into(),
                    ));
                }
                Msg::Error(e) => return Err(connect_error(&e)),
                Msg::Notice(_) => {}
                Msg::Other(_) | Msg::Row => {
                    return Err(DriverError::Protocol(
                        "unexpected message during authentication".into(),
                    ));
                }
            }
        }
    }

    /// Builds a message derived from the password in a private buffer, sends it and zeroes the
    /// buffer.
    async fn send_secret(
        &mut self,
        build: impl FnOnce(&mut BytesMut) -> std::io::Result<()>,
    ) -> DriverResult<()> {
        let mut buf = BytesMut::new();
        let built = build(&mut buf);
        let sent = match built {
            Ok(()) => match self.io.write_all(&buf).await {
                Ok(()) => self.io.flush().await.map_err(io_err),
                Err(e) => Err(io_err(e)),
            },
            Err(_) => Err(DriverError::Auth(
                "cannot encode the password message".into(),
            )),
        };
        zero_bytes(&mut buf);
        sent
    }

    /// Output buffer for frontend messages; sent by [`Conn::flush`].
    pub(crate) fn out(&mut self) -> &mut BytesMut {
        &mut self.out
    }

    /// Sends the buffered messages.
    pub(crate) async fn flush(&mut self) -> DriverResult<()> {
        self.io.write_all(&self.out).await.map_err(io_err)?;
        self.out.clear();
        self.io.flush().await.map_err(io_err)
    }

    /// Sends `Terminate` and shuts the transport down (best effort).
    pub(crate) async fn terminate(&mut self) {
        self.out.clear();
        frontend::terminate(&mut self.out);
        let _ = self.flush().await;
        let _ = self.io.shutdown().await;
    }

    /// Reads the next message. `ParameterStatus` is applied to [`Conn::server`] and skipped,
    /// `NotificationResponse` is skipped, `DataRow` goes to `sink`.
    pub(crate) async fn next(&mut self, mut sink: RowSink<'_>) -> DriverResult<Msg> {
        loop {
            let tag = self.io.read_u8().await.map_err(io_err)?;
            let len = self.io.read_i32().await.map_err(io_err)?;
            let body_len = usize::try_from(len)
                .ok()
                .and_then(|l| l.checked_sub(4))
                .ok_or_else(|| DriverError::Protocol(format!("invalid message length {len}")))?;
            if tag == b'D' {
                match &mut sink {
                    RowSink::Arrow(b) => self.read_arrow_row(body_len, b).await?,
                    RowSink::Text(rows) => {
                        let row = self.read_text_row(body_len).await?;
                        rows.push(row);
                    }
                    RowSink::Discard => self.skip(body_len).await?,
                    RowSink::None => {
                        return Err(DriverError::Protocol("unexpected data row".into()));
                    }
                }
                return Ok(Msg::Row);
            }
            if body_len > MAX_MESSAGE {
                return Err(DriverError::Protocol(format!(
                    "server message of {body_len} bytes is too large"
                )));
            }
            let mut buf = BytesMut::with_capacity(body_len + 5);
            buf.put_u8(tag);
            buf.put_i32(len);
            buf.resize(body_len + 5, 0);
            self.io.read_exact(&mut buf[5..]).await.map_err(io_err)?;
            let msg = backend::Message::parse(&mut buf)
                .map_err(|e| DriverError::Protocol(format!("malformed server message: {e}")))?
                .ok_or_else(|| DriverError::Protocol("truncated server message".into()))?;
            match msg {
                backend::Message::ParameterStatus(p) => {
                    let (Ok(name), Ok(value)) = (p.name(), p.value()) else {
                        return Err(DriverError::Protocol("malformed parameter status".into()));
                    };
                    self.server.set(name, value);
                }
                backend::Message::NotificationResponse(_) => {
                    // LISTEN / NOTIFY is not offered; notifications are dropped.
                }
                backend::Message::NoticeResponse(n) => {
                    return Ok(Msg::Notice(PgError::from_fields(n.fields())));
                }
                backend::Message::ErrorResponse(e) => {
                    return Ok(Msg::Error(PgError::from_fields(e.fields())));
                }
                backend::Message::ReadyForQuery(ref r) => {
                    self.txn_status = r.status();
                    return Ok(Msg::Other(msg));
                }
                other => return Ok(Msg::Other(other)),
            }
        }
    }

    async fn skip(&mut self, mut n: usize) -> DriverResult<()> {
        while n > 0 {
            let chunk = n.min(self.scratch.len());
            self.io
                .read_exact(&mut self.scratch[..chunk])
                .await
                .map_err(io_err)?;
            n -= chunk;
        }
        Ok(())
    }

    /// Reads `n` bytes, keeping the first `keep`.
    async fn read_value(&mut self, n: usize, keep: usize) -> DriverResult<Vec<u8>> {
        let keep = keep.min(n);
        let mut v = vec![0; keep];
        self.io.read_exact(&mut v).await.map_err(io_err)?;
        self.skip(n - keep).await?;
        Ok(v)
    }

    async fn read_arrow_row(&mut self, body_len: usize, b: &mut BatchBuilder) -> DriverResult<()> {
        let ncols = self.io.read_i16().await.map_err(io_err)?;
        if usize::try_from(ncols).ok() != Some(b.plans().len()) {
            return Err(DriverError::Protocol(format!(
                "data row with {ncols} columns, expected {}",
                b.plans().len()
            )));
        }
        let mut consumed = 2usize;
        let mut row = RowBudget::new(b.budget());
        let mut fixed = [0u8; MAX_FIXED];
        for col in 0..b.plans().len() {
            let len = self.io.read_i32().await.map_err(io_err)?;
            consumed += 4;
            if len < 0 {
                b.null(col);
                row.add_fixed(1);
                continue;
            }
            let n = usize::try_from(len).unwrap_or(usize::MAX);
            consumed = consumed.saturating_add(n);
            if consumed > body_len {
                return Err(DriverError::Protocol(
                    "data row longer than its message".into(),
                ));
            }
            match b.plans()[col].kind.fixed_len() {
                Some(width) => {
                    if n != width || n > MAX_FIXED {
                        return Err(DriverError::Protocol(format!(
                            "binary value of {n} bytes for a {width}-byte type"
                        )));
                    }
                    self.io.read_exact(&mut fixed[..n]).await.map_err(io_err)?;
                    row.add_fixed(n as u64);
                    b.value(col, &fixed[..n], false)?;
                }
                None => {
                    let keep = usize::try_from(row.allow(n as u64)).unwrap_or(usize::MAX);
                    let v = self.read_value(n, keep).await?;
                    b.value(col, &v, keep < n)?;
                }
            }
        }
        if consumed != body_len {
            return Err(DriverError::Protocol(
                "data row shorter than its message".into(),
            ));
        }
        // Per-value Arrow overhead (offsets, validity).
        b.end_row(row.used() + 4 * b.plans().len() as u64);
        Ok(())
    }

    async fn read_text_row(&mut self, body_len: usize) -> DriverResult<Vec<Option<String>>> {
        let ncols = self.io.read_i16().await.map_err(io_err)?;
        let ncols = usize::try_from(ncols)
            .map_err(|_| DriverError::Protocol("negative column count".into()))?;
        let mut consumed = 2usize;
        let mut out = Vec::with_capacity(ncols);
        for _ in 0..ncols {
            let len = self.io.read_i32().await.map_err(io_err)?;
            consumed += 4;
            if len < 0 {
                out.push(None);
                continue;
            }
            let n = usize::try_from(len).unwrap_or(usize::MAX);
            consumed = consumed.saturating_add(n);
            if consumed > body_len {
                return Err(DriverError::Protocol(
                    "data row longer than its message".into(),
                ));
            }
            let v = self.read_value(n, MAX_INTERNAL_VALUE).await?;
            let s = match String::from_utf8(v) {
                Ok(s) => s,
                Err(e) => {
                    let valid = e.utf8_error().valid_up_to();
                    let mut v = e.into_bytes();
                    v.truncate(valid);
                    String::from_utf8(v).unwrap_or_default()
                }
            };
            out.push(Some(s));
        }
        if consumed != body_len {
            return Err(DriverError::Protocol(
                "data row shorter than its message".into(),
            ));
        }
        Ok(out)
    }
}

fn zero_bytes(buf: &mut BytesMut) {
    for b in buf.iter_mut() {
        *b = 0;
    }
}

pub(crate) fn io_err(e: std::io::Error) -> DriverError {
    DriverError::Closed(format!("connection lost: {e}"))
}

/// Maps an error received before the session is ready.
pub(crate) fn connect_error(e: &PgError) -> DriverError {
    let msg = ninesql_driver_api::redact_connection_string(&e.display());
    match e.code.as_str() {
        "28P01" | "28000" => DriverError::Auth(msg),
        _ => DriverError::Connect(msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn major_version_parses() {
        let mut p = ServerParams {
            values: HashMap::new(),
            standard_conforming_strings: Arc::new(AtomicBool::new(true)),
        };
        p.set("server_version", "18.0 (Debian 18.0-1.pgdg13+3)");
        assert_eq!(p.major_version(), Some(18));
        p.set("server_version", "9.6.24");
        assert_eq!(p.major_version(), Some(9));
        p.set("standard_conforming_strings", "off");
        assert!(!p.standard_conforming_strings.load(Ordering::SeqCst));
    }

    #[test]
    fn error_display_leaves_detail_out() {
        let e = PgError {
            severity: "ERROR".into(),
            code: "23505".into(),
            message: "duplicate key value violates unique constraint".into(),
            hint: Some("check the key".into()),
            position: None,
        };
        assert_eq!(
            e.display(),
            "duplicate key value violates unique constraint (hint: check the key)"
        );
        assert!(matches!(
            connect_error(&PgError {
                code: "28P01".into(),
                ..PgError::default()
            }),
            DriverError::Auth(_)
        ));
    }
}
