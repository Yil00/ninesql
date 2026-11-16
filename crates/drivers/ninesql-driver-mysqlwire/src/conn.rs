//! One connection to a MySQL / MariaDB server: the v10 handshake, the TLS upgrade (never a
//! fallback to plain text), authentication, and the text-protocol responses (OK, ERR, EOF,
//! column definitions, rows read value by value under the fetch budget).
//!
//! Client capabilities: protocol 4.1, plugin authentication, transactions, multiple results
//! (`CALL`), connect attributes, found rows (update counts are matched rows, P5-B). Never `CLIENT_MULTI_STATEMENTS` (one statement per protocol
//! call), never `CLIENT_LOCAL_FILES` (a `LOAD DATA LOCAL` request is answered with an empty file
//! and fails), never compression, never `CLIENT_IGNORE_SPACE` (it changes the lexer).

use ninesql_driver_api::{DriverError, DriverResult, RowBudget, Secret};
use tokio::net::TcpStream;

use crate::auth;
use crate::proto::{Buf, Packets, put_lenenc_bytes, tls_handshake};
use crate::types::{BatchBuilder, Column};

pub(crate) const CLIENT_LONG_PASSWORD: u32 = 1;
/// Update counts are rows **matched** by the `WHERE`, not rows whose value changed (as JDBC
/// drivers report them): a row edit that writes an unchanged value still reports one row, so
/// the core's "exactly one row" check of generated DML holds (P5-B).
pub(crate) const CLIENT_FOUND_ROWS: u32 = 2;
pub(crate) const CLIENT_LONG_FLAG: u32 = 4;
pub(crate) const CLIENT_CONNECT_WITH_DB: u32 = 8;
pub(crate) const CLIENT_MULTI_STATEMENTS: u32 = 1 << 16;
pub(crate) const CLIENT_LOCAL_FILES: u32 = 0x80;
pub(crate) const CLIENT_PROTOCOL_41: u32 = 0x200;
pub(crate) const CLIENT_SSL: u32 = 0x800;
pub(crate) const CLIENT_TRANSACTIONS: u32 = 0x2000;
pub(crate) const CLIENT_SECURE_CONNECTION: u32 = 0x8000;
pub(crate) const CLIENT_MULTI_RESULTS: u32 = 1 << 17;
pub(crate) const CLIENT_PLUGIN_AUTH: u32 = 1 << 19;
pub(crate) const CLIENT_CONNECT_ATTRS: u32 = 1 << 20;
pub(crate) const CLIENT_PLUGIN_AUTH_LENENC: u32 = 1 << 21;

pub(crate) const STATUS_IN_TRANS: u16 = 0x1;
pub(crate) const STATUS_AUTOCOMMIT: u16 = 0x2;
pub(crate) const STATUS_MORE_RESULTS: u16 = 0x8;
pub(crate) const STATUS_NO_BACKSLASH_ESCAPES: u16 = 0x200;

/// `utf8mb4_general_ci`: known to every MySQL ≥ 5.5 and MariaDB.
const UTF8MB4_GENERAL_CI: u8 = 45;
const MAX_PACKET_SIZE: u32 = 16 * 1024 * 1024;
/// Largest value kept by internal (metadata) queries; longer values are cut.
const MAX_INTERNAL_VALUE: u64 = 64 * 1024;
/// Most rows an internal query may return (metadata pages are at most 10 000 + 1 rows).
pub(crate) const MAX_INTERNAL_ROWS: usize = 10_001;
/// Most bytes an internal query may return.
const MAX_INTERNAL_BYTES: usize = 32 * 1024 * 1024;
/// Most columns accepted in one result set.
const MAX_COLUMNS: u64 = 4096;
/// Most bytes of column definitions accepted for one result set (a hostile server could
/// otherwise send 4096 definitions of 1 MiB each).
pub(crate) const MAX_COLUMN_DEFS_BYTES: usize = 4 * 1024 * 1024;
/// Widest text form accepted for a number or a date (protects the parsers).
const MAX_FIXED_TEXT: u64 = 64;

pub(crate) const COM_QUIT: u8 = 0x01;
pub(crate) const COM_QUERY: u8 = 0x03;
pub(crate) const COM_PING: u8 = 0x0e;

/// A server error (ERR packet).
#[derive(Clone, Debug, Default)]
pub(crate) struct MyError {
    pub(crate) code: u16,
    pub(crate) state: String,
    pub(crate) message: String,
}

impl MyError {
    fn parse(data: &[u8]) -> Self {
        let mut b = Buf::new(data);
        let _ = b.u8();
        let code = b.u16().unwrap_or(0);
        let mut state = String::new();
        if b.remaining().first() == Some(&b'#') {
            let _ = b.u8();
            if let Ok(s) = b.take(5) {
                state = String::from_utf8_lossy(s).into_owned();
            }
        }
        let message = String::from_utf8_lossy(b.remaining()).into_owned();
        Self {
            code,
            state,
            message,
        }
    }

    /// The server's message with its code (`message (1062)`).
    pub(crate) fn display(&self) -> String {
        format!("{} ({})", self.message, self.code)
    }

    /// The server is closing the connection (shutdown, killed connection, packet errors).
    pub(crate) fn is_fatal(&self) -> bool {
        matches!(
            self.code,
            1053 | 1152
                | 1153
                | 1154
                | 1155
                | 1156
                | 1157
                | 1158
                | 1159
                | 1160
                | 1161
                | 1927
                | 4031
        )
    }
}

/// An OK or EOF packet.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct OkInfo {
    pub(crate) affected_rows: u64,
    pub(crate) status: u16,
    pub(crate) warnings: u16,
}

fn parse_ok(data: &[u8]) -> DriverResult<OkInfo> {
    let mut b = Buf::new(data);
    b.u8()?;
    let affected_rows = b.lenenc()?.unwrap_or(0);
    let _last_insert_id = b.lenenc()?;
    let status = b.u16()?;
    let warnings = b.u16()?;
    Ok(OkInfo {
        affected_rows,
        status,
        warnings,
    })
}

fn parse_eof(data: &[u8]) -> DriverResult<OkInfo> {
    let mut b = Buf::new(data);
    b.u8()?;
    let warnings = b.u16()?;
    let status = b.u16()?;
    Ok(OkInfo {
        affected_rows: 0,
        status,
        warnings,
    })
}

/// First packet of a command's response.
pub(crate) enum Head {
    Ok(OkInfo),
    Err(MyError),
    /// `LOAD DATA LOCAL INFILE` request (answered with an empty file).
    LocalInfile,
    /// A result set with this many columns.
    ResultSet(u64),
}

/// One row, or the end of a result set.
pub(crate) enum RowOrEnd {
    Row,
    End(OkInfo),
    Err(MyError),
}

/// Where rows go.
pub(crate) enum RowSink<'a> {
    Arrow(&'a mut BatchBuilder),
    Text(&'a mut TextRows),
    Discard,
}

/// Rows of an internal query, under [`MAX_INTERNAL_ROWS`] and [`MAX_INTERNAL_BYTES`].
#[derive(Debug, Default)]
pub(crate) struct TextRows {
    pub(crate) rows: Vec<Vec<Option<String>>>,
    bytes: usize,
    /// Largest value kept; `None` keeps [`MAX_INTERNAL_VALUE`] and cuts silently.
    pub(crate) value_limit: Option<u64>,
    /// A value longer than `value_limit` was cut.
    pub(crate) cut: bool,
    /// The rows passed [`MAX_INTERNAL_BYTES`]: the rest of the result is read and dropped, and
    /// the statement fails without closing the session (security review L3).
    pub(crate) overflow: bool,
}

impl TextRows {
    fn push(&mut self, row: Vec<Option<String>>) -> DriverResult<()> {
        if self.overflow {
            return Ok(());
        }
        self.bytes += row
            .iter()
            .map(|v| v.as_ref().map_or(1, |s| s.len() + 24))
            .sum::<usize>();
        if self.bytes > MAX_INTERNAL_BYTES {
            self.overflow = true;
            self.rows = Vec::new();
            return Ok(());
        }
        self.rows.push(row);
        if self.rows.len() > MAX_INTERNAL_ROWS {
            return Err(DriverError::Protocol(
                "the server returned more rows than the driver asked for".into(),
            ));
        }
        Ok(())
    }
}

/// Lossy UTF-8 that drops a multi-byte character cut at the end (a value cut to the budget).
pub(crate) fn utf8_prefix(v: Vec<u8>) -> String {
    match String::from_utf8(v) {
        Ok(s) => s,
        Err(e) => {
            let incomplete_tail = e.utf8_error().error_len().is_none();
            let valid = e.utf8_error().valid_up_to();
            let bytes = e.into_bytes();
            if incomplete_tail {
                String::from_utf8_lossy(&bytes[..valid]).into_owned()
            } else {
                String::from_utf8_lossy(&bytes).into_owned()
            }
        }
    }
}

/// Typed connect options; never formatted into a connection string.
pub(crate) struct Startup<'a> {
    pub(crate) user: &'a str,
    pub(crate) database: Option<&'a str>,
    pub(crate) application_name: &'a str,
    /// Verified TLS, or an SSH tunnel with verified host keys to a loopback endpoint: the only
    /// transports over which the password itself may be sent.
    pub(crate) protected_transport: bool,
}

/// One authenticated connection.
pub(crate) struct Conn {
    pub(crate) p: Packets,
    pub(crate) server_version: String,
    pub(crate) status: u16,
}

struct Greeting {
    version: String,
    nonce: Vec<u8>,
    caps: u32,
    plugin: String,
}

fn parse_greeting(data: &[u8]) -> DriverResult<Greeting> {
    let mut b = Buf::new(data);
    let protocol = b.u8()?;
    if protocol == 0xff {
        let e = MyError::parse(data);
        return Err(DriverError::Connect(format!(
            "the server refused the connection: {}",
            e.message
        )));
    }
    if protocol != 10 {
        return Err(DriverError::Connect(format!(
            "unsupported MySQL protocol version {protocol}"
        )));
    }
    let version = String::from_utf8_lossy(b.nul_str()).into_owned();
    let _thread_id = b.u32()?;
    let mut nonce = b.take(8)?.to_vec();
    b.u8()?;
    let caps_lo = u32::from(b.u16()?);
    let _charset = b.u8()?;
    let _status = b.u16()?;
    let caps_hi = u32::from(b.u16()?);
    let caps = caps_lo | caps_hi << 16;
    let auth_len = usize::from(b.u8()?);
    b.take(10)?;
    if caps & CLIENT_SECURE_CONNECTION != 0 {
        let n = auth_len.saturating_sub(8).max(13);
        let part2 = b.take(n.min(b.remaining().len()))?;
        nonce.extend_from_slice(auth::nonce(part2));
    }
    let plugin = if caps & CLIENT_PLUGIN_AUTH != 0 {
        String::from_utf8_lossy(b.nul_str()).into_owned()
    } else {
        auth::NATIVE.to_owned()
    };
    Ok(Greeting {
        version,
        nonce,
        caps,
        plugin,
    })
}

/// Maps an ERR packet received before the session is ready.
pub(crate) fn connect_error(e: &MyError) -> DriverError {
    let msg = ninesql_driver_api::redact_connection_string(&e.display());
    match e.code {
        1044 | 1045 | 1698 | 1251 | 1862 | 3118 | 3159 | 3162 => DriverError::Auth(msg),
        _ => DriverError::Connect(msg),
    }
}

impl Conn {
    /// Handshake, optional TLS upgrade and authentication, up to the first OK. The password is
    /// used for the exchange and dropped by the caller right after.
    pub(crate) async fn connect(
        tcp: TcpStream,
        tls: Option<&crate::tls::TlsSetup>,
        startup: &Startup<'_>,
        password: Option<&Secret>,
    ) -> DriverResult<Self> {
        let mut p = Packets::new(Box::new(tcp));
        let greeting = parse_greeting(&p.read_packet().await?)?;
        let need = CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH;
        if greeting.caps & need != need {
            return Err(DriverError::Connect(
                "the server is too old: protocol 4.1 with plugin authentication is required".into(),
            ));
        }
        let mut caps = CLIENT_LONG_PASSWORD
            | CLIENT_FOUND_ROWS
            | CLIENT_LONG_FLAG
            | CLIENT_PROTOCOL_41
            | CLIENT_TRANSACTIONS
            | CLIENT_SECURE_CONNECTION
            | CLIENT_MULTI_RESULTS
            | CLIENT_PLUGIN_AUTH
            | CLIENT_PLUGIN_AUTH_LENENC;
        caps &= greeting.caps | CLIENT_LONG_PASSWORD;
        if greeting.caps & CLIENT_CONNECT_ATTRS != 0 {
            caps |= CLIENT_CONNECT_ATTRS;
        }
        if startup.database.is_some() {
            caps |= CLIENT_CONNECT_WITH_DB;
        }
        debug_assert_eq!(caps & (CLIENT_MULTI_STATEMENTS | CLIENT_LOCAL_FILES), 0);
        if let Some(tls) = tls {
            if greeting.caps & CLIENT_SSL == 0 {
                return Err(DriverError::Tls(
                    "the server does not accept TLS connections; NineSQL never falls back to an \
                     unencrypted connection when TLS is required"
                        .into(),
                ));
            }
            caps |= CLIENT_SSL;
            let mut req = Vec::with_capacity(32);
            req.extend_from_slice(&caps.to_le_bytes());
            req.extend_from_slice(&MAX_PACKET_SIZE.to_le_bytes());
            req.push(UTF8MB4_GENERAL_CI);
            req.extend_from_slice(&[0u8; 23]);
            p.write_payload(&req).await?;
            let seq = p.seq();
            let io = tls_handshake(p.into_io(), tls).await?;
            p = Packets::new(io);
            p.set_seq(seq);
        }
        let mut conn = Self {
            p,
            server_version: greeting.version.clone(),
            status: 0,
        };
        conn.authenticate(caps, &greeting, startup, password)
            .await?;
        Ok(conn)
    }

    async fn authenticate(
        &mut self,
        caps: u32,
        greeting: &Greeting,
        startup: &Startup<'_>,
        password: Option<&Secret>,
    ) -> DriverResult<()> {
        let pw: &[u8] = password.map_or(&[][..], |p| p.expose().as_bytes());
        let mut plugin = greeting.plugin.clone();
        let mut nonce = greeting.nonce.clone();
        let first = match plugin.as_str() {
            auth::NATIVE => auth::native(pw, &nonce),
            auth::CACHING_SHA2 => auth::caching_sha2(pw, &nonce),
            // Unknown or cleartext default plugin: answer with nothing and let the server switch
            // to the account's plugin.
            _ => zeroize::Zeroizing::new(Vec::new()),
        };
        let mut msg = zeroize::Zeroizing::new(Vec::with_capacity(128));
        msg.extend_from_slice(&caps.to_le_bytes());
        msg.extend_from_slice(&MAX_PACKET_SIZE.to_le_bytes());
        msg.push(UTF8MB4_GENERAL_CI);
        msg.extend_from_slice(&[0u8; 23]);
        msg.extend_from_slice(startup.user.as_bytes());
        msg.push(0);
        put_lenenc_bytes(&mut msg, &first);
        if let Some(db) = startup.database {
            msg.extend_from_slice(db.as_bytes());
            msg.push(0);
        }
        msg.extend_from_slice(plugin.as_bytes());
        msg.push(0);
        if caps & CLIENT_CONNECT_ATTRS != 0 {
            let mut attrs = Vec::new();
            put_lenenc_bytes(&mut attrs, b"_client_name");
            put_lenenc_bytes(&mut attrs, b"ninesql-driver-mysqlwire");
            put_lenenc_bytes(&mut attrs, b"program_name");
            put_lenenc_bytes(&mut attrs, startup.application_name.as_bytes());
            put_lenenc_bytes(&mut msg, &attrs);
        }
        self.p.write_secret(&mut msg).await?;
        // One plugin switch at most: a server switching twice to obtain a weaker proof is refused.
        let mut switched = false;
        loop {
            let packet = self.p.read_packet().await?;
            match packet.first().copied() {
                Some(0x00) => {
                    self.status = parse_ok(&packet)?.status;
                    return Ok(());
                }
                Some(0xff) => return Err(connect_error(&MyError::parse(&packet))),
                Some(0xfe) => {
                    if switched {
                        return Err(DriverError::Auth(
                            "the server asked for a second authentication method switch".into(),
                        ));
                    }
                    switched = true;
                    let mut b = Buf::new(&packet[1..]);
                    plugin = String::from_utf8_lossy(b.nul_str()).into_owned();
                    nonce = auth::nonce(b.remaining()).to_vec();
                    let mut reply = match plugin.as_str() {
                        auth::NATIVE => auth::native(pw, &nonce),
                        auth::CACHING_SHA2 => auth::caching_sha2(pw, &nonce),
                        auth::CLEAR if startup.protected_transport => auth::cleartext(pw),
                        auth::CLEAR => return Err(cleartext_refused()),
                        other => {
                            return Err(DriverError::Auth(format!(
                                "the server asks for the `{other}` authentication plugin, which \
                                 the mysqlwire driver does not support (supported: \
                                 caching_sha2_password, mysql_native_password)"
                            )));
                        }
                    };
                    self.p.write_secret(&mut reply).await?;
                }
                Some(0x01) if plugin == auth::CACHING_SHA2 => match packet.get(1).copied() {
                    // Fast authentication succeeded: the OK packet follows.
                    Some(0x03) => {}
                    Some(0x04) => {
                        if !startup.protected_transport {
                            return Err(DriverError::Auth(
                                "the server has no cached credential for this account and asks \
                                 for the password itself (caching_sha2_password full \
                                 authentication), which NineSQL sends only over verified TLS \
                                 (verify-ca / verify-full) or a verified SSH tunnel to a loopback \
                                 endpoint; connect once that way, or use verified TLS"
                                    .into(),
                            ));
                        }
                        let mut reply = auth::cleartext(pw);
                        self.p.write_secret(&mut reply).await?;
                    }
                    _ => {
                        return Err(DriverError::Auth(
                            "unexpected caching_sha2_password exchange".into(),
                        ));
                    }
                },
                _ => {
                    return Err(DriverError::Protocol(
                        "unexpected message during authentication".into(),
                    ));
                }
            }
        }
    }

    /// Sends a command (sequence ids restart).
    pub(crate) async fn command(&mut self, cmd: u8, body: &[u8]) -> DriverResult<()> {
        self.p.reset_seq();
        let mut payload = Vec::with_capacity(body.len() + 1);
        payload.push(cmd);
        payload.extend_from_slice(body);
        self.p.write_payload(&payload).await
    }

    /// Sends `COM_QUIT` and shuts the transport down (best effort).
    pub(crate) async fn quit(&mut self) {
        let _ = self.command(COM_QUIT, &[]).await;
        self.p.shutdown().await;
    }

    /// Reads the first packet of a command's response.
    pub(crate) async fn read_head(&mut self) -> DriverResult<Head> {
        let len = self.p.begin().await?;
        let first = self.p.u8().await?;
        match first {
            0x00 if len >= 7 => {
                let mut data = vec![0];
                data.extend(self.p.rest(crate::proto::MAX_CONTROL_PACKET).await?);
                let ok = parse_ok(&data)?;
                self.status = ok.status;
                Ok(Head::Ok(ok))
            }
            0xff => {
                let mut data = vec![0xff];
                data.extend(self.p.rest(crate::proto::MAX_CONTROL_PACKET).await?);
                Ok(Head::Err(MyError::parse(&data)))
            }
            0xfb => {
                self.p.skip_rest().await?;
                Ok(Head::LocalInfile)
            }
            _ => {
                let n = self
                    .p
                    .lenenc_after(first)
                    .await?
                    .ok_or_else(|| DriverError::Protocol("invalid result set header".into()))?;
                if !self.p.at_end().await? {
                    return Err(DriverError::Protocol("invalid result set header".into()));
                }
                if n == 0 || n > MAX_COLUMNS {
                    return Err(DriverError::Protocol(format!(
                        "result set with {n} columns"
                    )));
                }
                Ok(Head::ResultSet(n))
            }
        }
    }

    /// Answers a `LOAD DATA LOCAL` request with an empty file (nothing is ever read locally).
    pub(crate) async fn refuse_local_infile(&mut self) -> DriverResult<()> {
        self.p.write_payload(&[]).await
    }

    /// Reads `n` column definitions and the EOF packet that follows them.
    pub(crate) async fn read_columns(&mut self, n: u64) -> DriverResult<Vec<Column>> {
        let mut cols = Vec::with_capacity(usize::try_from(n).unwrap_or(0));
        let mut total = 0usize;
        for _ in 0..n {
            let data = self.p.read_packet().await?;
            total = total.saturating_add(data.len());
            if total > MAX_COLUMN_DEFS_BYTES {
                return Err(DriverError::Protocol(
                    "the server sent oversized column definitions".into(),
                ));
            }
            cols.push(Column::parse(&data)?);
        }
        let eof = self.p.read_packet().await?;
        if eof.first() != Some(&0xfe) || eof.len() >= 9 {
            return Err(DriverError::Protocol(
                "missing end of column definitions".into(),
            ));
        }
        self.status = parse_eof(&eof)?.status;
        Ok(cols)
    }

    /// Reads one row into `sink`, or the end of the result set.
    pub(crate) async fn read_row(
        &mut self,
        ncols: usize,
        sink: RowSink<'_>,
    ) -> DriverResult<RowOrEnd> {
        let len = self.p.begin().await?;
        let first = self.p.u8().await?;
        if first == 0xfe && len < 9 {
            let mut data = vec![0xfe];
            data.extend(self.p.rest(16).await?);
            let eof = parse_eof(&data)?;
            self.status = eof.status;
            return Ok(RowOrEnd::End(eof));
        }
        if first == 0xff {
            let mut data = vec![0xff];
            data.extend(self.p.rest(crate::proto::MAX_CONTROL_PACKET).await?);
            return Ok(RowOrEnd::Err(MyError::parse(&data)));
        }
        match sink {
            RowSink::Arrow(b) => self.read_arrow_row(first, b).await?,
            RowSink::Text(rows) => {
                let keep = rows.value_limit.unwrap_or(MAX_INTERNAL_VALUE);
                let (row, cut, overflow) = self.read_text_row(first, ncols, keep).await?;
                rows.cut |= cut;
                rows.overflow |= overflow;
                rows.push(row)?;
            }
            RowSink::Discard => self.p.skip_rest().await?,
        }
        if !self.p.at_end().await? {
            return Err(DriverError::Protocol("row longer than its columns".into()));
        }
        Ok(RowOrEnd::Row)
    }

    async fn value_len(&mut self, first: Option<u8>) -> DriverResult<Option<u64>> {
        match first {
            Some(f) => self.p.lenenc_after(f).await,
            None => self.p.lenenc().await,
        }
    }

    async fn read_arrow_row(&mut self, first: u8, b: &mut BatchBuilder) -> DriverResult<()> {
        let ncols = b.columns();
        let mut row = RowBudget::new(b.budget());
        let mut first = Some(first);
        for col in 0..ncols {
            let len = self.value_len(first.take()).await?;
            let Some(len) = len else {
                b.null(col);
                row.add_fixed(1);
                continue;
            };
            if b.is_parsed(col) {
                if len > MAX_FIXED_TEXT {
                    return Err(DriverError::Protocol(format!(
                        "value of {len} bytes for a numeric or date column"
                    )));
                }
                let mut v = [0u8; MAX_FIXED_TEXT as usize];
                let n = usize::try_from(len).unwrap_or(0);
                self.p.read_exact(&mut v[..n]).await?;
                row.add_fixed(8);
                b.parsed(col, &v[..n])?;
            } else {
                let keep = row.allow(len);
                let mut v = vec![0u8; usize::try_from(keep).unwrap_or(0)];
                self.p.read_exact(&mut v).await?;
                self.p.skip(len - keep).await?;
                b.bytes(col, v, keep < len);
            }
        }
        b.end_row(row.used() + 4 * ncols as u64);
        Ok(())
    }

    async fn read_text_row(
        &mut self,
        first: u8,
        ncols: usize,
        max_value: u64,
    ) -> DriverResult<(Vec<Option<String>>, bool, bool)> {
        let mut cut = false;
        let mut overflow = false;
        let mut out = Vec::with_capacity(ncols);
        let mut first = Some(first);
        let mut bytes = 0u64;
        for _ in 0..ncols {
            match self.value_len(first.take()).await? {
                None => out.push(None),
                Some(len) => {
                    // Past the byte cap the rest of the row is skipped, and the statement
                    // fails without closing the session (security review L3).
                    let room = (MAX_INTERNAL_BYTES as u64).saturating_sub(bytes);
                    let keep = len.min(max_value).min(room);
                    cut |= keep < len;
                    bytes = bytes.saturating_add(keep + 24);
                    if bytes > MAX_INTERNAL_BYTES as u64 {
                        overflow = true;
                    }
                    let mut v = vec![0u8; usize::try_from(keep).unwrap_or(0)];
                    self.p.read_exact(&mut v).await?;
                    self.p.skip(len - keep).await?;
                    out.push(Some(utf8_prefix(v)));
                }
            }
        }
        Ok((out, cut, overflow))
    }

    /// Runs a driver statement (text protocol) and returns its rows as text, bounded. Further
    /// result sets (procedures) are read and dropped.
    pub(crate) async fn query(&mut self, sql: &str) -> DriverResult<Result<TextRows, MyError>> {
        self.query_limited(sql, None).await
    }

    /// [`query`](Self::query) keeping values up to `value_limit` bytes (see `TextRows`).
    pub(crate) async fn query_limited(
        &mut self,
        sql: &str,
        value_limit: Option<u64>,
    ) -> DriverResult<Result<TextRows, MyError>> {
        self.command(COM_QUERY, sql.as_bytes()).await?;
        let mut out = TextRows {
            value_limit,
            ..TextRows::default()
        };
        let mut error = None;
        loop {
            let status = match self.read_head().await? {
                Head::Ok(ok) => ok.status,
                Head::Err(e) => {
                    error.get_or_insert(e);
                    break;
                }
                Head::LocalInfile => {
                    self.refuse_local_infile().await?;
                    error.get_or_insert(local_infile_error());
                    continue;
                }
                Head::ResultSet(n) => {
                    let cols = self.read_columns(n).await?;
                    let first_set = error.is_none() && out.rows.is_empty();
                    loop {
                        let sink = if first_set {
                            RowSink::Text(&mut out)
                        } else {
                            RowSink::Discard
                        };
                        match self.read_row(cols.len(), sink).await? {
                            RowOrEnd::Row => {}
                            RowOrEnd::End(eof) => break eof.status,
                            RowOrEnd::Err(e) => {
                                error.get_or_insert(e);
                                break 0;
                            }
                        }
                    }
                }
            };
            if status & STATUS_MORE_RESULTS == 0 {
                break;
            }
        }
        Ok(match error {
            Some(e) => Err(e),
            None => Ok(out),
        })
    }

    /// `COM_PING`.
    pub(crate) async fn ping(&mut self) -> DriverResult<Result<(), MyError>> {
        self.command(COM_PING, &[]).await?;
        match self.read_head().await? {
            Head::Ok(_) => Ok(Ok(())),
            Head::Err(e) => Ok(Err(e)),
            _ => Err(DriverError::Protocol("unexpected answer to a ping".into())),
        }
    }
}

pub(crate) fn local_infile_error() -> MyError {
    MyError {
        code: 0,
        state: String::new(),
        message: "LOAD DATA LOCAL INFILE is not supported by NineSQL: no local file is ever sent"
            .into(),
    }
}

fn cleartext_refused() -> DriverError {
    DriverError::Auth(
        "the server asks for a cleartext password over a connection that is not protected (TLS \
         verify-ca / verify-full, or a verified SSH tunnel to a loopback endpoint)"
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

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

    fn greeting(plugin: &str, ssl: bool) -> Vec<u8> {
        let mut g = vec![10];
        g.extend_from_slice(b"8.4.13\0");
        g.extend_from_slice(&7u32.to_le_bytes());
        g.extend_from_slice(b"abcdefgh");
        g.push(0);
        let caps = CLIENT_PROTOCOL_41
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH
            | CLIENT_PLUGIN_AUTH_LENENC
            | CLIENT_TRANSACTIONS
            | CLIENT_MULTI_RESULTS
            | CLIENT_MULTI_STATEMENTS
            | CLIENT_LOCAL_FILES
            | if ssl { CLIENT_SSL } else { 0 };
        g.extend_from_slice(&((caps & 0xffff) as u16).to_le_bytes());
        g.push(45);
        g.extend_from_slice(&2u16.to_le_bytes());
        g.extend_from_slice(&((caps >> 16) as u16).to_le_bytes());
        g.push(21);
        g.extend_from_slice(&[0u8; 10]);
        g.extend_from_slice(b"ijklmnopqrst\0");
        g.extend_from_slice(plugin.as_bytes());
        g.push(0);
        g
    }

    async fn read_frame(s: &mut TcpStream) -> Option<Vec<u8>> {
        let mut h = [0u8; 4];
        s.read_exact(&mut h).await.ok()?;
        let n = usize::from(h[0]) | usize::from(h[1]) << 8 | usize::from(h[2]) << 16;
        let mut v = vec![0; n];
        s.read_exact(&mut v).await.ok()?;
        Some(v)
    }

    async fn serve<F, Fut>(script: F) -> u16
    where
        F: FnOnce(TcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            script(s).await;
        });
        port
    }

    async fn client(port: u16, protected: bool) -> DriverResult<Conn> {
        let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let startup = Startup {
            user: "nine",
            database: Some("nine"),
            application_name: "t",
            protected_transport: protected,
        };
        let pw = Secret::new("Pa55-secret".into());
        Conn::connect(tcp, None, &startup, Some(&pw)).await
    }

    #[tokio::test]
    async fn full_caching_sha2_auth_is_refused_unprotected() {
        let (tx, rx) = oneshot::channel();
        let port = serve(|mut s| async move {
            s.write_all(&frame(0, &greeting(auth::CACHING_SHA2, false)))
                .await
                .unwrap();
            let response = read_frame(&mut s).await.unwrap();
            s.write_all(&frame(2, &[0x01, 0x04])).await.unwrap();
            let mut rest = Vec::new();
            let _ = s.read_to_end(&mut rest).await;
            let _ = tx.send((response, rest));
        })
        .await;
        assert!(matches!(
            client(port, false).await,
            Err(DriverError::Auth(_))
        ));
        let (response, rest) = rx.await.unwrap();
        let caps = u32::from_le_bytes(response[..4].try_into().unwrap());
        assert_eq!(
            caps & CLIENT_MULTI_STATEMENTS,
            0,
            "multi-statements requested"
        );
        assert_eq!(caps & CLIENT_LOCAL_FILES, 0, "local files requested");
        assert!(!response.windows(6).any(|w| w == b"Pa55-s"));
        assert!(!rest.windows(6).any(|w| w == b"Pa55-s"), "password sent");
    }

    #[tokio::test]
    async fn cleartext_switch_is_refused_unprotected() {
        let (tx, rx) = oneshot::channel();
        let port = serve(|mut s| async move {
            s.write_all(&frame(0, &greeting(auth::NATIVE, false)))
                .await
                .unwrap();
            let _ = read_frame(&mut s).await;
            let mut sw = vec![0xfe];
            sw.extend_from_slice(b"mysql_clear_password\0");
            s.write_all(&frame(2, &sw)).await.unwrap();
            let mut rest = Vec::new();
            let _ = s.read_to_end(&mut rest).await;
            let _ = tx.send(rest);
        })
        .await;
        assert!(matches!(
            client(port, false).await,
            Err(DriverError::Auth(_))
        ));
        let rest = rx.await.unwrap();
        assert!(!rest.windows(6).any(|w| w == b"Pa55-s"), "password sent");
    }

    #[tokio::test]
    async fn a_second_switch_is_refused_and_tls_refusal_fails() {
        let port = serve(|mut s| async move {
            s.write_all(&frame(0, &greeting(auth::NATIVE, false)))
                .await
                .unwrap();
            let _ = read_frame(&mut s).await;
            let mut sw = vec![0xfe];
            sw.extend_from_slice(b"caching_sha2_password\0abcdefghijklmnopqrst\0");
            s.write_all(&frame(2, &sw)).await.unwrap();
            let _ = read_frame(&mut s).await;
            let mut sw = vec![0xfe];
            sw.extend_from_slice(b"mysql_native_password\0abcdefghijklmnopqrst\0");
            s.write_all(&frame(4, &sw)).await.unwrap();
            let _ = read_frame(&mut s).await;
        })
        .await;
        assert!(matches!(
            client(port, true).await,
            Err(DriverError::Auth(_))
        ));
        // TLS required, server without CLIENT_SSL: no fallback.
        let port = serve(|mut s| async move {
            s.write_all(&frame(0, &greeting(auth::NATIVE, false)))
                .await
                .unwrap();
            let _ = read_frame(&mut s).await;
        })
        .await;
        let mut cfg = ninesql_driver_api::TlsConfig::default();
        cfg.mode = ninesql_driver_api::TlsMode::Require;
        let tls = crate::tls::setup(&cfg, "127.0.0.1").unwrap();
        let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let startup = Startup {
            user: "nine",
            database: None,
            application_name: "t",
            protected_transport: false,
        };
        let r = Conn::connect(tcp, tls.as_ref(), &startup, None).await;
        assert!(matches!(r, Err(DriverError::Tls(_))));
    }

    /// Handshake with a fake server that accepts at once, then `script` on the same socket.
    async fn after_ok<F, Fut>(script: F) -> Conn
    where
        F: FnOnce(TcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let port = serve(|mut s| async move {
            s.write_all(&frame(0, &greeting(auth::NATIVE, false)))
                .await
                .unwrap();
            let _ = read_frame(&mut s).await;
            s.write_all(&frame(2, &[0, 0, 0, 2, 0, 0, 0]))
                .await
                .unwrap();
            script(s).await;
        })
        .await;
        client(port, true).await.unwrap()
    }

    #[tokio::test]
    async fn local_infile_request_gets_an_empty_file_and_fails() {
        let (tx, rx) = oneshot::channel();
        let mut conn = after_ok(|mut s| async move {
            let _query = read_frame(&mut s).await.unwrap();
            let mut req = vec![0xfb];
            req.extend_from_slice(b"/etc/passwd");
            s.write_all(&frame(1, &req)).await.unwrap();
            let mut h = [0u8; 4];
            s.read_exact(&mut h).await.unwrap();
            let mut e = vec![0xff];
            e.extend_from_slice(&1148u16.to_le_bytes());
            e.extend_from_slice(b"#42000refused");
            s.write_all(&frame(3, &e)).await.unwrap();
            let _ = tx.send(h);
        })
        .await;
        let r = conn
            .query("LOAD DATA LOCAL INFILE 'x' INTO TABLE t")
            .await
            .unwrap();
        assert!(r.is_err(), "the statement must fail");
        let h = rx.await.unwrap();
        assert_eq!(
            &h[..3],
            &[0, 0, 0],
            "a zero-length packet (empty file) is sent"
        );
        assert_eq!(h[3], 2, "with the next sequence id");
    }

    #[tokio::test]
    async fn stuck_control_connection_setup_times_out_and_is_dropped() {
        let conn = after_ok(|mut s| async move {
            let _query = read_frame(&mut s).await;
            // Never answers.
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        })
        .await;
        let control = crate::cancel::Control::new(conn);
        let started = std::time::Instant::now();
        assert!(matches!(
            control.setup("SET SESSION TRANSACTION READ ONLY").await,
            Err(DriverError::Timeout(_))
        ));
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        // Dropped: nothing left to configure.
        assert!(
            control
                .setup("SET SESSION TRANSACTION READ ONLY")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn oversized_column_definitions_are_refused() {
        let mut conn = after_ok(|mut s| async move {
            let _query = read_frame(&mut s).await.unwrap();
            s.write_all(&frame(1, &[5])).await.unwrap();
            let name = vec![b'n'; 1_000_000];
            let mut def = vec![0, 0, 0, 0];
            def.push(0xfd);
            def.extend_from_slice(&u32::try_from(name.len()).unwrap().to_le_bytes()[..3]);
            def.extend_from_slice(&name);
            for seq in 2..7u8 {
                if s.write_all(&frame(seq, &def)).await.is_err() {
                    return;
                }
            }
            let mut rest = Vec::new();
            let _ = s.read_to_end(&mut rest).await;
        })
        .await;
        assert!(matches!(
            conn.query("SELECT 1").await,
            Err(DriverError::Protocol(_))
        ));
    }

    #[test]
    fn errors_parse_and_map() {
        let mut p = vec![0xff];
        p.extend_from_slice(&1045u16.to_le_bytes());
        p.extend_from_slice(b"#28000Access denied for user 'nine'");
        let e = MyError::parse(&p);
        assert_eq!((e.code, e.state.as_str()), (1045, "28000"));
        assert!(matches!(connect_error(&e), DriverError::Auth(_)));
        assert_eq!(utf8_prefix("é".as_bytes()[..1].to_vec()), "");
        assert_eq!(utf8_prefix(b"ab\xffcd".to_vec()), "ab\u{fffd}cd");
    }
}
