//! Conformance checks every driver reuses in its integration tests (feature `mock`).
//!
//! Each function connects with parameters built by the caller (pointing at the driver's
//! `dev/compose.yaml` service) and panics with a descriptive message when the driver breaks the
//! contract. They are test helpers: never call them from production code.
//!
//! ```ignore
//! #[tokio::test]
//! async fn pgwire_conformance() {
//!     let Some(params) = dev_params("postgresql") else { return }; // skipped when unreachable
//!     let driver = PgWireDriver::new();
//!     conformance::assert_reserved_properties_rejected(&driver, &params).await;
//!     let writes = conformance::writes_for(&EngineId::POSTGRESQL);
//!     conformance::assert_read_only_refuses_writes(&driver, dev_params(..), &writes).await;
//!     conformance::assert_multiple_statements_refused(&driver, dev_params(..)).await;
//!     conformance::assert_access_mode_change_detected(&driver, dev_params(..), LIFT_SQL).await;
//!     conformance::assert_stale_cancel_ignored(&driver, dev_params(..), Q1, Q2).await;
//!     conformance::assert_batches_within_budget(&driver, dev_params(..), WIDE_SQL, budget).await;
//!     conformance::assert_identifier_quoting(
//!         &driver, dev_params(..), conformance::ALIAS_TEMPLATE, conformance::IDENTIFIER_SAMPLES,
//!     ).await;
//!     // The TLS endpoint of the driver's dev service, its test CA and a name it does not cover.
//!     conformance::assert_verify_full_rejects_untrusted_certificate(
//!         &driver, &|| tls_params(..), Path::new("dev/tls/ca.pem"), "wrong.example.test",
//!     ).await;
//! }
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use arrow_array::Array;
use arrow_array::cast::AsArray;

use crate::dialect::{Dialect, StatementClass};
use crate::driver::{Driver, Session};
use crate::error::DriverError;
use crate::params::{Auth, ConnectParams, EngineId, PropertyValue, TlsMode};
use crate::result::{DocValue, Document, ResultItem, ResultSetKind};
use crate::secret::Secret;
use crate::statement::{ExecOptions, FetchBudget, Statement};

/// Properties that try to override security settings (H3 of the P0-B security review). Each
/// must make `connect` fail with [`DriverError::InvalidParams`].
pub const RESERVED_PROPERTY_SAMPLES: &[(&str, &str)] = &[
    ("sslmode", "disable"),
    ("useSSL", "false"),
    ("requireSSL", "false"),
    ("verifyServerCertificate", "false"),
    ("trustServerCertificate", "true"),
    ("sslrootcert", "/tmp/evil.pem"),
    ("readonly", "0"),
    ("options", "-c default_transaction_read_only=off"),
    ("default_transaction_read_only", "off"),
    ("sessionVariables", "transaction_read_only=0"),
    ("allowMultiQueries", "true"),
    ("multiStatements", "true"),
    (
        "initSql",
        "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
    ),
    ("init_command", "SET SESSION transaction_read_only = 0"),
    ("sql_mode", "NO_BACKSLASH_ESCAPES"),
    ("password", "not-a-real-password"),
    ("host", "attacker.example.test"),
    ("port", "1"),
    ("user", "root"),
];

/// Engine-neutral write statements (standard SQL DML / DDL); each must fail with
/// [`DriverError::ReadOnly`] in a read-only session. Combine with the engine's list through
/// [`writes_for`].
pub const WRITES: &[&str] = &[
    "INSERT INTO ninesql_conformance VALUES (1)",
    "UPDATE ninesql_conformance SET a = 1 WHERE a = 0",
    "DELETE FROM ninesql_conformance",
    "CREATE TABLE ninesql_conformance_new (a INT)",
    "DROP TABLE ninesql_conformance",
    "CREATE OR REPLACE VIEW ninesql_conformance_view AS SELECT 1 AS a",
];

/// PostgreSQL-family `EXPLAIN` forms that run the explained statement (P1-E review M1): the
/// `ANALYZE` option written quoted or `U&`-escaped. The classifier must see them as what they
/// run (destructive here), never as reads.
pub const POSTGRES_EXPLAIN_WRITES: &[&str] = &[
    "EXPLAIN (\"analyze\") DELETE FROM ninesql_conformance",
    "EXPLAIN (FORMAT JSON, \"analyze\" on) DELETE FROM ninesql_conformance",
    "EXPLAIN (U&\"analyze\") DELETE FROM ninesql_conformance",
    "EXPLAIN ANALYZE DELETE FROM ninesql_conformance",
];

/// MySQL-family statements that run a wrapped write (P1-E review M1): MariaDB
/// `ANALYZE <statement>` executes it, MySQL `EXPLAIN ANALYZE` executes it, and `DESC` /
/// `DESCRIBE` are synonyms of `EXPLAIN`.
pub const MYSQL_EXPLAIN_WRITES: &[&str] = &[
    "ANALYZE DELETE FROM ninesql_conformance",
    "ANALYZE FORMAT=JSON DELETE FROM ninesql_conformance",
    "EXPLAIN ANALYZE DELETE FROM ninesql_conformance",
    "DESC ANALYZE DELETE FROM ninesql_conformance",
];

/// PostgreSQL-family statements that try to lift a read-only session.
pub const POSTGRES_ACCESS_MODE_WRITES: &[&str] = &[
    "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
    "SET default_transaction_read_only = off",
    "SET \"default_transaction_read_only\" = off",
    "SET U&\"default\\005ftransaction\\005fread\\005fonly\" = off",
    "RESET default_transaction_read_only",
    "SELECT set_config('default_transaction_read_only', 'off', false)",
    "BEGIN READ WRITE",
    "SET TRANSACTION ISOLATION LEVEL READ COMMITTED, READ WRITE",
    "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL READ COMMITTED, READ WRITE",
    "SET TRANSACTION READ ONLY, READ WRITE",
];

/// MySQL-family statements that try to lift a read-only session or change lexical modes.
pub const MYSQL_ACCESS_MODE_WRITES: &[&str] = &[
    "SET SESSION TRANSACTION READ WRITE",
    "SET transaction_read_only = 0",
    "SET @@session.transaction_read_only = 0",
    "SET `transaction_read_only` = 0",
    "START TRANSACTION READ WRITE",
    "SET sql_mode = 'NO_BACKSLASH_ESCAPES'",
    "SET TRANSACTION ISOLATION LEVEL READ COMMITTED, READ WRITE",
    "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED, READ WRITE",
    "SET SESSION TRANSACTION READ ONLY, READ WRITE",
];

/// SQLite statements that try to lift a read-only session.
pub const SQLITE_ACCESS_MODE_WRITES: &[&str] = &[
    "PRAGMA query_only = OFF",
    "PRAGMA query_only(0)",
    "PRAGMA writable_schema = ON",
    "ATTACH DATABASE 'ninesql_conformance.db' AS other",
];

/// [`WRITES`] plus the access-mode and running-`EXPLAIN` statements of `engine`'s family
/// (PostgreSQL family, MySQL family, SQLite); other engines get [`WRITES`] only and add their
/// own.
#[must_use]
pub fn writes_for(engine: &EngineId) -> Vec<&'static str> {
    let family: &[&[&str]] = match engine.as_str() {
        "postgresql" | "timescaledb" | "yugabytedb" | "questdb" => {
            &[POSTGRES_ACCESS_MODE_WRITES, POSTGRES_EXPLAIN_WRITES]
        }
        "mysql" | "mariadb" | "tidb" => &[MYSQL_ACCESS_MODE_WRITES, MYSQL_EXPLAIN_WRITES],
        "sqlite" => &[SQLITE_ACCESS_MODE_WRITES],
        _ => &[],
    };
    WRITES
        .iter()
        .chain(family.iter().flat_map(|l| l.iter()))
        .copied()
        .collect()
}

fn clone_params(p: &ConnectParams) -> ConnectParams {
    // `ConnectParams` is not `Clone` (it owns secrets); the helpers only need the
    // non-secret routing fields and rebuild the rest.
    let mut out = ConnectParams::new(p.engine.clone(), p.endpoint.clone());
    out.database.clone_from(&p.database);
    out.tls.mode = p.tls.mode;
    out.tls.ca_file.clone_from(&p.tls.ca_file);
    out.tls.server_name.clone_from(&p.tls.server_name);
    out.tunnel.clone_from(&p.tunnel);
    out.proxy.clone_from(&p.proxy);
    out.auto_commit = p.auto_commit;
    out.connect_timeout = p.connect_timeout;
    out.production = p.production;
    out.log_statements = p.log_statements;
    out.password_exchange = p.password_exchange;
    out.inside_dbeaver_workspace = p.inside_dbeaver_workspace;
    out
}

/// Connects once per [`RESERVED_PROPERTY_SAMPLES`] entry and asserts the driver refuses it.
///
/// `params` is used as a template (credentials are not copied: the check must fail before
/// authentication).
///
/// # Panics
/// When a reserved property is accepted or rejected with the wrong error.
pub async fn assert_reserved_properties_rejected(driver: &dyn Driver, params: &ConnectParams) {
    for (key, value) in RESERVED_PROPERTY_SAMPLES {
        let mut p = clone_params(params);
        let mut props = BTreeMap::new();
        props.insert((*key).to_owned(), PropertyValue::Plain((*value).to_owned()));
        p.properties = props;
        match driver.connect(p).await {
            Err(DriverError::InvalidParams(msg)) => {
                assert!(
                    !msg.contains(value),
                    "error message echoes the value of `{key}`"
                );
            }
            Err(other) => panic!("property `{key}`: expected InvalidParams, got {other:?}"),
            Ok(_) => panic!("property `{key}` was accepted"),
        }
    }
}

/// Opens a read-only session with `params` and asserts every statement of `writes` fails with
/// [`DriverError::ReadOnly`] (refused client-side or rejected engine-side), and that the session
/// is still read-only afterwards.
///
/// # Panics
/// When a write succeeds or fails with another error.
pub async fn assert_read_only_refuses_writes(
    driver: &dyn Driver,
    params: ConnectParams,
    writes: &[&str],
) {
    let mut p = params;
    p.read_only = true;
    let mut session = match driver.connect(p).await {
        Ok(s) => s,
        Err(e) => panic!("read-only connect failed: {e}"),
    };
    for sql in writes {
        let outcome = run(session.as_mut(), sql).await;
        assert!(
            matches!(outcome, Err(DriverError::ReadOnly(_))),
            "write was not refused in a read-only session: {sql} -> {outcome:?}"
        );
    }
    assert!(session.is_read_only(), "session left read-only mode");
}

/// Opens a read-only session and runs `lift_sql`, an engine-specific statement that lifts the
/// read-only mode while bypassing the classifier (the driver's test supplies it; for the mock:
/// `SELECT mock_lift_read_only()`). Asserts that the driver detects the change (it fails with
/// [`DriverError::ReadOnly`]) and closes the session (later calls fail with
/// [`DriverError::Closed`]). Only meaningful for liftable mechanisms
/// ([`ReadOnlyMechanism::is_liftable`](crate::ReadOnlyMechanism::is_liftable)).
///
/// # Panics
/// When the change goes unnoticed or the session stays usable.
pub async fn assert_access_mode_change_detected(
    driver: &dyn Driver,
    params: ConnectParams,
    lift_sql: &str,
) {
    let mut p = params;
    p.read_only = true;
    let mut session = match driver.connect(p).await {
        Ok(s) => s,
        Err(e) => panic!("read-only connect failed: {e}"),
    };
    assert!(
        session.capabilities().read_only.is_liftable(),
        "the engine does not declare a liftable read-only mechanism"
    );
    let outcome = run(session.as_mut(), lift_sql).await;
    assert!(
        matches!(outcome, Err(DriverError::ReadOnly(_))),
        "access-mode change was not detected: {outcome:?}"
    );
    let after = run(session.as_mut(), "SELECT 1").await;
    assert!(
        matches!(after, Err(DriverError::Closed(_))),
        "session still usable after an access-mode change: {after:?}"
    );
}

/// Asserts that a two-statement text is refused with [`DriverError::MultipleStatements`].
///
/// # Panics
/// When the driver executes it or fails with another error.
pub async fn assert_multiple_statements_refused(driver: &dyn Driver, params: ConnectParams) {
    let mut session = match driver.connect(params).await {
        Ok(s) => s,
        Err(e) => panic!("connect failed: {e}"),
    };
    let outcome = run(session.as_mut(), "SELECT 1; SELECT 2").await;
    assert!(
        matches!(outcome, Err(DriverError::MultipleStatements(2))),
        "multi-statement text was not refused: {outcome:?}"
    );
}

/// TLS verification conformance (I5). `make_params` builds fresh parameters (credentials
/// included) for an endpoint whose certificate is **not** trusted by the platform store but is
/// signed by `trusted_ca` for the endpoint's real host name (for example the TLS-enabled
/// `dev/compose.yaml` service of the engine with its test CA). `mismatched_server_name` is a
/// name the certificate does not cover. The helper asserts:
/// 1. positive control: `verify-full` with `trusted_ca` connects, which proves the endpoint
///    really speaks TLS (so a "server does not support TLS" error cannot pass as a rejection);
/// 2. without `trusted_ca`, `verify-full` and `verify-ca` both fail with [`DriverError::Tls`];
/// 3. with `trusted_ca` but `server_name = mismatched_server_name`, `verify-ca` connects and
///    `verify-full` fails with [`DriverError::Tls`] (host name checked only by `verify-full`);
///
/// and that no TLS error echoes the password or token.
///
/// # Panics
/// When a check fails.
pub async fn assert_verify_full_rejects_untrusted_certificate(
    driver: &dyn Driver,
    make_params: &(dyn Fn() -> ConnectParams + Sync),
    trusted_ca: &Path,
    mismatched_server_name: &str,
) {
    let secret = {
        let p = make_params();
        match &p.auth {
            Auth::Password {
                password: Some(pw), ..
            } => Some(Secret::new(pw.expose().to_owned())),
            Auth::Token { token, .. } => Some(Secret::new(token.expose().to_owned())),
            _ => None,
        }
    };
    let with = |mode: TlsMode, ca: Option<&Path>, server_name: Option<&str>| {
        let mut p = make_params();
        p.tls.mode = mode;
        p.tls.ca_file = ca.map(Path::to_path_buf);
        if let Some(name) = server_name {
            p.tls.server_name = Some(name.to_owned());
        }
        p
    };
    let expect_tls_error =
        |case: &str, outcome: Result<Box<dyn Session>, DriverError>| match outcome {
            Err(DriverError::Tls(msg)) => {
                if let Some(s) = &secret
                    && !s.is_empty()
                {
                    assert!(
                        !msg.contains(s.expose()),
                        "{case}: TLS error echoes the secret"
                    );
                }
            }
            Err(other) => panic!("{case}: expected a TLS error, got {other:?}"),
            Ok(_) => panic!("{case}: an untrusted or mismatched certificate was accepted"),
        };
    // 1. Positive control.
    match driver
        .connect(with(TlsMode::VerifyFull, Some(trusted_ca), None))
        .await
    {
        Ok(session) => {
            let _ = session.close().await;
        }
        Err(e) => panic!("positive control: verify-full with the trusted CA failed: {e}"),
    }
    // 2. Untrusted issuer.
    for mode in [TlsMode::VerifyFull, TlsMode::VerifyCa] {
        let outcome = driver.connect(with(mode, None, None)).await;
        expect_tls_error(&format!("{mode:?} without the CA"), outcome);
    }
    // 3. Host name mismatch.
    match driver
        .connect(with(
            TlsMode::VerifyCa,
            Some(trusted_ca),
            Some(mismatched_server_name),
        ))
        .await
    {
        Ok(session) => {
            let _ = session.close().await;
        }
        Err(e) => panic!("verify-ca must not check the host name: {e}"),
    }
    let outcome = driver
        .connect(with(
            TlsMode::VerifyFull,
            Some(trusted_ca),
            Some(mismatched_server_name),
        ))
        .await;
    expect_tls_error("verify-full with a mismatched host name", outcome);
}

/// Bytes a batch may exceed `2 × max_batch_bytes + max_row_bytes` by (Arrow buffer alignment,
/// offsets, validity bitmaps). The factor 2 covers builders that double their capacity; the
/// row term covers the row that fills the batch.
pub const BATCH_BYTES_SLACK: u64 = 64 * 1024;

/// What [`assert_batches_within_budget`] observed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BudgetReport {
    /// Rows and documents received.
    pub rows: u64,
    /// `Rows` and `Documents` items received.
    pub batches: u64,
    /// Largest batch seen, in Arrow memory (or estimated document) bytes.
    pub max_batch_bytes_seen: u64,
    /// At least one column was flagged with
    /// [`TRUNCATED_VALUES_METADATA_KEY`](crate::TRUNCATED_VALUES_METADATA_KEY) or one document
    /// held a [`DocValue::Truncated`].
    pub truncated_values: bool,
}

/// Runs `sql` with `budget` and asserts that the driver bounds what it builds (I6, F5):
/// - every `Rows` batch has at most [`FetchBudget::effective_batch_rows`] rows and at most
///   `2 × effective_max_batch_bytes + effective_max_row_bytes +` [`BATCH_BYTES_SLACK`] bytes;
/// - every variable-length value (UTF-8 / binary, offset or view arrays) is at most
///   [`FetchBudget::effective_max_value_bytes`], and their sum per row at most
///   [`FetchBudget::effective_max_row_bytes`];
/// - every document value (strings, bytes, recursively) is at most `effective_max_value_bytes`
///   and every document's estimate at most `effective_max_row_bytes +` [`BATCH_BYTES_SLACK`].
///
/// Pick a statement that returns wide values (larger than `budget.max_value_bytes`) and enough
/// rows for several batches. Returns what was observed so the test can assert more (for example
/// that truncation was flagged).
///
/// # Panics
/// When a bound is exceeded or the statement fails.
pub async fn assert_batches_within_budget(
    driver: &dyn Driver,
    params: ConnectParams,
    sql: &str,
    budget: FetchBudget,
) -> BudgetReport {
    let mut session = match driver.connect(params).await {
        Ok(s) => s,
        Err(e) => panic!("connect failed: {e}"),
    };
    let mut stream = match session
        .execute(
            Statement::sql(sql),
            ExecOptions::default().with_budget(budget),
        )
        .await
    {
        Ok(s) => s,
        Err(e) => panic!("execute failed: {e}"),
    };
    let max_rows = u64::from(budget.effective_batch_rows());
    let max_value = budget.effective_max_value_bytes();
    let max_row = budget.effective_max_row_bytes();
    let max_batch = 2 * budget.effective_max_batch_bytes() + max_row + BATCH_BYTES_SLACK;
    let mut report = BudgetReport::default();
    while let Some(item) = stream.next().await {
        match item {
            Ok(ResultItem::Rows(batch)) => {
                let n = batch.num_rows() as u64;
                let size = batch.get_array_memory_size() as u64;
                assert!(n <= max_rows, "batch of {n} rows, budget {max_rows}");
                assert!(
                    size <= max_batch,
                    "batch of {size} bytes, bound {max_batch}"
                );
                let mut row_bytes = vec![0u64; batch.num_rows()];
                for (field, column) in batch.schema().fields().iter().zip(batch.columns()) {
                    report.truncated_values |= field
                        .metadata()
                        .get(crate::TRUNCATED_VALUES_METADATA_KEY)
                        .is_some_and(|v| v == "true");
                    for (row, len) in var_len_values(column.as_ref()) {
                        assert!(
                            len <= max_value,
                            "value of {len} bytes in `{}`, budget {max_value}",
                            field.name()
                        );
                        row_bytes[row] += len;
                    }
                }
                if let Some(widest) = row_bytes.iter().max() {
                    assert!(
                        *widest <= max_row,
                        "row of {widest} bytes, budget {max_row}"
                    );
                }
                report.rows += n;
                report.batches += 1;
                report.max_batch_bytes_seen = report.max_batch_bytes_seen.max(size);
            }
            Ok(ResultItem::Documents(docs)) => {
                let mut size = 0u64;
                for d in &docs {
                    let s = d.estimated_size();
                    assert!(
                        s <= max_row + BATCH_BYTES_SLACK,
                        "document of {s} bytes, budget {max_row}"
                    );
                    assert_doc_values_within(d, max_value);
                    report.truncated_values |= d.has_truncated_values();
                    size += s;
                }
                assert!(
                    size <= max_batch,
                    "document batch of {size} bytes, bound {max_batch}"
                );
                report.rows += docs.len() as u64;
                report.batches += 1;
                report.max_batch_bytes_seen = report.max_batch_bytes_seen.max(size);
            }
            Ok(_) => {}
            Err(e) => panic!("stream failed: {e}"),
        }
    }
    report
}

/// `(row, byte length)` of every non-null variable-length value of `column`.
fn var_len_values(column: &dyn Array) -> Vec<(usize, u64)> {
    let lens: Vec<Option<usize>> = if let Some(a) = column.as_string_opt::<i32>() {
        a.iter().map(|v| v.map(str::len)).collect()
    } else if let Some(a) = column.as_string_opt::<i64>() {
        a.iter().map(|v| v.map(str::len)).collect()
    } else if let Some(a) = column.as_binary_opt::<i32>() {
        a.iter().map(|v| v.map(<[u8]>::len)).collect()
    } else if let Some(a) = column.as_binary_opt::<i64>() {
        a.iter().map(|v| v.map(<[u8]>::len)).collect()
    } else if let Some(a) = column.as_string_view_opt() {
        a.iter().map(|v| v.map(str::len)).collect()
    } else if let Some(a) = column.as_binary_view_opt() {
        a.iter().map(|v| v.map(<[u8]>::len)).collect()
    } else {
        return Vec::new();
    };
    lens.into_iter()
        .enumerate()
        .filter_map(|(i, l)| l.map(|l| (i, l as u64)))
        .collect()
}

fn assert_doc_values_within(doc: &Document, max_value: u64) {
    fn check(v: &DocValue, max_value: u64) {
        let len = match v {
            DocValue::String(s) | DocValue::Decimal(s) => s.len() as u64,
            DocValue::Bytes(b) => b.len() as u64,
            DocValue::Other { text, .. } => text.len() as u64,
            DocValue::Array(a) => {
                a.iter().for_each(|x| check(x, max_value));
                0
            }
            DocValue::Object(d) => {
                d.fields.iter().for_each(|(_, x)| check(x, max_value));
                0
            }
            DocValue::Truncated { kept, .. } => {
                check(kept, max_value);
                0
            }
            _ => 0,
        };
        assert!(
            len <= max_value,
            "document value of {len} bytes, budget {max_value}"
        );
    }
    doc.fields.iter().for_each(|(_, v)| check(v, max_value));
}

/// Names that stress identifier quoting: case, spaces, every quote character, statement and
/// comment delimiters, a keyword, non-ASCII.
pub const IDENTIFIER_SAMPLES: &[&str] = &[
    "plain",
    "MixedCase",
    "with space",
    "a\"b",
    "a`b",
    "a'b",
    "a;b",
    "a.b",
    "select",
    "a--b",
    "a/*b*/c",
    "ünïcødé",
    "\"\"",
    "a\\b",
    "a$b",
    "$1",
    "$$",
    "a\nb",
    "]",
    "[x]",
    "1abc",
];

/// Template for [`assert_identifier_quoting`]: `{ident}` is replaced by the quoted name.
pub const ALIAS_TEMPLATE: &str = "SELECT 1 AS {ident}";

/// Asserts, without a server, that `dialect` quotes every name of `names` into a single token:
/// the statement built from `template` stays one statement and classifies as
/// [`StatementClass::Read`].
///
/// # Panics
/// When a quoted name splits the statement or changes its class.
pub fn assert_dialect_quoting_roundtrip(dialect: &dyn Dialect, template: &str, names: &[&str]) {
    for name in names {
        let sql = template.replace("{ident}", &dialect.quote_identifier(name));
        assert_eq!(
            dialect.split_statements(&sql).len(),
            1,
            "quoting {name:?} split the statement: {sql}"
        );
        assert_eq!(
            dialect.classify(&sql),
            StatementClass::Read,
            "quoting {name:?} changed the class: {sql}"
        );
    }
}

/// For every name of `names`, runs `template` with the session dialect's quoted form of the
/// name and asserts that the engine returns a column with exactly that name (per-driver
/// identifier quoting). Also runs [`assert_dialect_quoting_roundtrip`].
///
/// # Panics
/// When the engine rejects a quoted name or returns another column name.
pub async fn assert_identifier_quoting(
    driver: &dyn Driver,
    params: ConnectParams,
    template: &str,
    names: &[&str],
) {
    let mut session = match driver.connect(params).await {
        Ok(s) => s,
        Err(e) => panic!("connect failed: {e}"),
    };
    let dialect = session.dialect();
    assert_dialect_quoting_roundtrip(dialect.as_ref(), template, names);
    for name in names {
        let sql = template.replace("{ident}", &dialect.quote_identifier(name));
        let mut stream = match session
            .execute(Statement::sql(&sql), ExecOptions::default())
            .await
        {
            Ok(s) => s,
            Err(e) => panic!("{name:?}: execute failed: {e}"),
        };
        let mut column = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(ResultItem::ResultSet(h)) if column.is_none() => {
                    if let ResultSetKind::Tabular(schema) = &h.kind {
                        column = schema.fields().first().map(|f| f.name().clone());
                    }
                }
                Ok(_) => {}
                Err(e) => panic!("{name:?}: stream failed: {e}"),
            }
        }
        assert_eq!(
            column.as_deref(),
            Some(*name),
            "quoted name did not round-trip"
        );
    }
}

/// Asserts that cancel handles are scoped to one execution (M6): handles obtained while
/// `first_sql` runs ([`Session::cancel_handle`] and
/// [`ResultStream::cancel_handle`](crate::ResultStream::cancel_handle)) are used after it
/// ended, while `second_sql` runs, and `second_sql` must still complete without error. Pick a
/// `second_sql` that takes a while (several batches) so a stray cancel would hit it. Returns
/// the rows of `second_sql`.
///
/// # Panics
/// When the second statement is cancelled or fails.
pub async fn assert_stale_cancel_ignored(
    driver: &dyn Driver,
    params: ConnectParams,
    first_sql: &str,
    second_sql: &str,
) -> u64 {
    let mut session = match driver.connect(params).await {
        Ok(s) => s,
        Err(e) => panic!("connect failed: {e}"),
    };
    let mut first = match session
        .execute(Statement::sql(first_sql), ExecOptions::default())
        .await
    {
        Ok(s) => s,
        Err(e) => panic!("first execute failed: {e}"),
    };
    let stale_session = session.cancel_handle();
    let stale_stream = first.cancel_handle();
    while let Some(item) = first.next().await {
        if let Err(e) = item {
            panic!("first statement failed: {e}");
        }
    }
    drop(first);
    let mut second = match session
        .execute(Statement::sql(second_sql), ExecOptions::default())
        .await
    {
        Ok(s) => s,
        Err(e) => panic!("second execute failed: {e}"),
    };
    let mut rows = 0u64;
    let mut cancelled = false;
    while let Some(item) = second.next().await {
        if !cancelled {
            // After the first item the statement is surely running on the server.
            if let Err(e) = stale_session.cancel().await {
                panic!("stale session cancel failed: {e}");
            }
            if let Err(e) = stale_stream.cancel().await {
                panic!("stale stream cancel failed: {e}");
            }
            cancelled = true;
        }
        match item {
            Ok(ResultItem::Rows(b)) => rows += b.num_rows() as u64,
            Ok(ResultItem::Documents(d)) => rows += d.len() as u64,
            Ok(_) => {}
            Err(e) => panic!("a stale cancel reached the next statement: {e}"),
        }
    }
    rows
}

/// Executes `sql` and drains the stream; the first error wins.
async fn run(session: &mut dyn Session, sql: &str) -> Result<(), DriverError> {
    let mut stream = session
        .execute(Statement::sql(sql), ExecOptions::default())
        .await?;
    while let Some(item) = stream.next().await {
        item?;
    }
    Ok(())
}
