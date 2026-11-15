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
//!     conformance::assert_credential_statements_not_logged(&driver, dev_params(..)).await;
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
/// Statements (init SQL, then run) whose text holds a credential: never logged (I1). Valid in
/// PostgreSQL, MySQL, MariaDB and SQLite. They hold [`LOG_HYGIENE_SECRET`].
pub const CREDENTIAL_STATEMENT_SAMPLES: &[&str] = &[
    "SELECT 'hunter2-log-probe' AS user_password",
    "SELECT 'postgresql://u:hunter2-log-probe@h/db' AS dsn",
    "SELECT 'hunter2-log-probe' AS Pwd",
];

/// The value every [`CREDENTIAL_STATEMENT_SAMPLES`] statement holds.
pub const LOG_HYGIENE_SECRET: &str = "hunter2-log-probe";

/// Records the fields of every event, on the thread it is installed on.
#[derive(Clone, Default)]
struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

struct LogFields<'a>(&'a mut String);

impl tracing::field::Visit for LogFields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!("{}={value:?} ", field.name()));
    }
}

impl tracing::Subscriber for LogCapture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = String::new();
        event.record(&mut LogFields(&mut line));
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(line);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// Connects with statement logging allowed and every event captured (`trace` included), runs
/// [`CREDENTIAL_STATEMENT_SAMPLES`] as init SQL and as statements, then a harmless statement
/// as both, and asserts that the log shows the harmless ones (so logging was on) but neither
/// the credential statements nor [`LOG_HYGIENE_SECRET`] (I1, `text_may_hold_credentials`).
///
/// Events are captured on the calling thread: the driver must log statement text from the
/// task that calls `connect` / `execute` (every NineSQL driver does).
///
/// # Panics
/// When a credential statement is logged, or when no statement is (logging was not on).
pub async fn assert_credential_statements_not_logged(driver: &dyn Driver, params: ConnectParams) {
    let capture = LogCapture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());
    // Other tests of the same binary may have hit the drivers' `trace!` callsites with no
    // subscriber in scope, caching "never interested": recompute the interest now that this
    // thread has one, so the capture sees every event.
    tracing::callsite::rebuild_interest_cache();
    let mut p = params;
    p.log_statements = true;
    p.production = false;
    p.read_only = false;
    p.init_statements = CREDENTIAL_STATEMENT_SAMPLES
        .iter()
        .map(|s| (*s).to_owned())
        .chain(["SELECT 7 AS init_log_probe".to_owned()])
        .collect();
    let mut session = match driver.connect(p).await {
        Ok(s) => s,
        Err(e) => panic!("connect failed: {e}"),
    };
    for sql in CREDENTIAL_STATEMENT_SAMPLES {
        if let Err(e) = run(session.as_mut(), sql).await {
            panic!("`{sql}` failed: {e}");
        }
    }
    if let Err(e) = run(session.as_mut(), "SELECT 42 AS log_probe").await {
        panic!("harmless statement failed: {e}");
    }
    let _ = session.close().await;
    let all = capture
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .join("\n");
    assert!(
        all.contains("SELECT 7 AS init_log_probe") && all.contains("SELECT 42 AS log_probe"),
        "statement text was not logged at all: the check proves nothing"
    );
    assert!(
        !all.contains(LOG_HYGIENE_SECRET),
        "a statement holding a credential was logged"
    );
}

async fn run(session: &mut dyn Session, sql: &str) -> Result<(), DriverError> {
    let mut stream = session
        .execute(Statement::sql(sql), ExecOptions::default())
        .await?;
    while let Some(item) = stream.next().await {
        item?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Classification corpus and script-splitting invariants (P3-D, ADR-0007)

/// The shared classification corpus (`tests/corpus/classification.txt`): statements with their
/// expected class per dialect. Every driver runs it with [`assert_classification_corpus`].
///
/// Format: a case starts with a `## <name>` line, followed by header lines and the statement:
/// - `expect: <key>=<class> …` (required): keys `all`, `my` (MySQL and MariaDB), `pg`,
///   `mysql`, `mariadb`, `sqlite`, the most specific key winning; a class is a
///   [`StatementClass`] variant name (`Destructive`), `>=<variant>` (at least that class) or
///   `skip` (not applicable);
/// - `modes: <mode> …` (optional): lexical session modes the dialect must be built with
///   (`no_backslash_escapes`, `ansi_quotes`, `scs_off`);
/// - `escapes: yes` (optional): `\r`, `\n`, `\t`, `\\` and `\u{…}` in the statement are decoded.
///
/// Every following line up to the next `## ` line is the statement (trailing blank lines
/// removed). Lines starting with `%%` are corpus comments, anywhere (a `#` line is statement
/// text: a MySQL comment).
pub const CLASSIFICATION_CORPUS: &str = include_str!("../tests/corpus/classification.txt");

/// Dialect keys of the corpus.
pub const CORPUS_DIALECTS: &[&str] = &["pg", "mysql", "mariadb", "sqlite"];

/// The expected class of a corpus case for one dialect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expectation {
    /// Exactly this class.
    Exactly(StatementClass),
    /// This class or a riskier one.
    AtLeast(StatementClass),
    /// Not applicable to the dialect.
    Skip,
}

impl Expectation {
    /// `true` when `class` meets the expectation.
    #[must_use]
    pub fn accepts(self, class: StatementClass) -> bool {
        match self {
            Self::Exactly(c) => class == c,
            Self::AtLeast(c) => class >= c,
            Self::Skip => true,
        }
    }

    /// The lowest class the expectation accepts (`None` for [`Expectation::Skip`]).
    #[must_use]
    pub fn floor(self) -> Option<StatementClass> {
        match self {
            Self::Exactly(c) | Self::AtLeast(c) => Some(c),
            Self::Skip => None,
        }
    }
}

/// One case of the corpus.
#[derive(Clone, Debug)]
pub struct CorpusCase {
    /// Name (the `## ` line).
    pub name: String,
    /// 1-based line of the `## ` line in the corpus file.
    pub line: usize,
    /// The statement.
    pub sql: String,
    /// Lexical modes the dialect must be built with.
    pub modes: Vec<String>,
    expect: Vec<(String, Expectation)>,
}

impl CorpusCase {
    /// The expectation for `dialect` (one of [`CORPUS_DIALECTS`]).
    #[must_use]
    pub fn expectation(&self, dialect: &str) -> Expectation {
        let group = match dialect {
            "mysql" | "mariadb" => Some("my"),
            _ => None,
        };
        let find = |k: &str| {
            self.expect
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, e)| *e)
        };
        find(dialect)
            .or_else(|| group.and_then(find))
            .or_else(|| find("all"))
            .unwrap_or(Expectation::Skip)
    }
}

fn parse_class(name: &str) -> Option<StatementClass> {
    use StatementClass as C;
    Some(match name {
        "Read" => C::Read,
        "Session" => C::Session,
        "Transaction" => C::Transaction,
        "Write" => C::Write,
        "Ddl" => C::Ddl,
        "ChangesAccessMode" => C::ChangesAccessMode,
        "Destructive" => C::Destructive,
        "Unknown" => C::Unknown,
        _ => return None,
    })
}

fn decode_escapes(s: &str) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('u') => {
                if chars.next() != Some('{') {
                    return Err("\\u must be followed by {".into());
                }
                let hex: String = chars.by_ref().take_while(|c| *c != '}').collect();
                let cp = u32::from_str_radix(&hex, 16).map_err(|e| e.to_string())?;
                out.push(char::from_u32(cp).ok_or("invalid code point")?);
            }
            other => return Err(format!("unknown escape \\{other:?}")),
        }
    }
    Ok(out)
}

/// Parses a corpus in the [`CLASSIFICATION_CORPUS`] format.
///
/// # Errors
/// A message with the line number of the first malformed case.
pub fn parse_classification_corpus(text: &str) -> Result<Vec<CorpusCase>, String> {
    let mut cases: Vec<CorpusCase> = Vec::new();
    let mut escapes = false;
    let mut in_header = false;
    let mut body: Vec<&str> = Vec::new();
    let finish = |cases: &mut Vec<CorpusCase>, body: &mut Vec<&str>, escapes: bool| {
        if let Some(case) = cases.last_mut() {
            while body.last().is_some_and(|l| l.trim().is_empty()) {
                body.pop();
            }
            let sql = body.join("\n");
            body.clear();
            if sql.trim().is_empty() {
                return Err(format!("line {}: case without a statement", case.line));
            }
            case.sql = if escapes {
                decode_escapes(&sql).map_err(|e| format!("line {}: {e}", case.line))?
            } else {
                sql
            };
        }
        Ok(())
    };
    for (n, line) in text.lines().enumerate() {
        let lineno = n + 1;
        if line.starts_with("%%") {
            continue;
        }
        if let Some(name) = line.strip_prefix("## ") {
            finish(&mut cases, &mut body, escapes)?;
            escapes = false;
            in_header = true;
            cases.push(CorpusCase {
                name: name.trim().to_owned(),
                line: lineno,
                sql: String::new(),
                modes: Vec::new(),
                expect: Vec::new(),
            });
            continue;
        }
        let Some(case) = cases.last_mut() else {
            if line.trim().is_empty() {
                continue;
            }
            return Err(format!("line {lineno}: text before the first case"));
        };
        if in_header {
            if let Some(rest) = line.strip_prefix("expect:") {
                for pair in rest.split_whitespace() {
                    let (key, value) = pair
                        .split_once('=')
                        .ok_or_else(|| format!("line {lineno}: `{pair}` is not key=class"))?;
                    if !matches!(key, "all" | "my") && !CORPUS_DIALECTS.contains(&key) {
                        return Err(format!("line {lineno}: unknown dialect key `{key}`"));
                    }
                    let exp = if value == "skip" {
                        Expectation::Skip
                    } else if let Some(v) = value.strip_prefix(">=") {
                        Expectation::AtLeast(
                            parse_class(v)
                                .ok_or_else(|| format!("line {lineno}: unknown class `{v}`"))?,
                        )
                    } else {
                        Expectation::Exactly(
                            parse_class(value)
                                .ok_or_else(|| format!("line {lineno}: unknown class `{value}`"))?,
                        )
                    };
                    case.expect.push((key.to_owned(), exp));
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("modes:") {
                case.modes = rest.split_whitespace().map(str::to_owned).collect();
                continue;
            }
            if let Some(rest) = line.strip_prefix("escapes:") {
                escapes = rest.trim() == "yes";
                continue;
            }
            in_header = false;
            if case.expect.is_empty() {
                return Err(format!("line {}: case without `expect:`", case.line));
            }
        }
        body.push(line);
    }
    finish(&mut cases, &mut body, escapes)?;
    if let Some(case) = cases.iter().find(|c| c.expect.is_empty()) {
        return Err(format!("line {}: case without `expect:`", case.line));
    }
    Ok(cases)
}

/// Builds a dialect for a corpus case's lexical modes (see [`assert_classification_corpus`]).
pub type DialectFactory<'a> = dyn Fn(&[String]) -> Option<Box<dyn Dialect>> + 'a;

/// Runs the [`CLASSIFICATION_CORPUS`] for one dialect key (`pg`, `mysql`, `mariadb`,
/// `sqlite`). `make` builds the dialect for a case's lexical modes, or returns `None` when the
/// dialect has no such mode (a case that expects a class for it then fails). Returns the number
/// of cases checked.
///
/// For each case it also checks that a statement before it (`SELECT 1;`) never lowers its
/// class, and that the whole case text classifies at least as high as every statement
/// [`Dialect::split_script`] finds in it.
///
/// # Panics
/// With the list of every failing case.
pub fn assert_classification_corpus(dialect_key: &str, make: &DialectFactory<'_>) -> usize {
    let cases = match parse_classification_corpus(CLASSIFICATION_CORPUS) {
        Ok(c) => c,
        Err(e) => panic!("corpus: {e}"),
    };
    let mut failures = Vec::new();
    let mut checked = 0;
    for case in &cases {
        let exp = case.expectation(dialect_key);
        if exp == Expectation::Skip {
            continue;
        }
        let Some(dialect) = make(&case.modes) else {
            failures.push(format!(
                "line {} `{}`: modes {:?} not available",
                case.line, case.name, case.modes
            ));
            continue;
        };
        checked += 1;
        let class = dialect.classify(&case.sql);
        if !exp.accepts(class) {
            failures.push(format!(
                "line {} `{}`: got {class:?}, expected {exp:?}\n    {}",
                case.line,
                case.name,
                case.sql.replace('\n', "\n    ")
            ));
            continue;
        }
        let prefixed = format!("SELECT 1;\n{}", case.sql);
        let p = dialect.classify(&prefixed);
        if p < class {
            failures.push(format!(
                "line {} `{}`: a leading `SELECT 1;` lowered the class to {p:?}",
                case.line, case.name
            ));
        }
        for item in dialect.split_script(&case.sql) {
            if item.kind == crate::dialect::ScriptItemKind::Statement {
                let c = dialect.classify(item.text(&case.sql));
                if c > class {
                    failures.push(format!(
                        "line {} `{}`: the script classifies {class:?} but its statement `{}` \
                         classifies {c:?}",
                        case.line,
                        case.name,
                        item.text(&case.sql)
                    ));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} corpus failure(s) for {dialect_key}:\n{}",
        failures.len(),
        failures.join("\n")
    );
    checked
}

/// Asserts the invariants of [`Dialect::split_script`] on `script`:
/// - items are ordered, do not overlap, lie on `char` boundaries and carry the positions of
///   their offsets;
/// - every statement item is at most one statement for [`Dialect::split_statements`] (so
///   [`ensure_single_statement`](crate::ensure_single_statement) accepts it) and is not blank;
/// - the whole script classifies at least as high as every statement item (classifying a
///   script before running it never hides what one of its statements does), unless the
///   script holds a refused piece or a client directive (callers stop at the former, and
///   classify the latter's script as `Unknown` anyway).
///
/// # Panics
/// With the script and the broken invariant.
pub fn assert_script_split_invariants(dialect: &dyn Dialect, script: &str) {
    use crate::dialect::{LineIndex, ScriptItemKind};
    let items = dialect.split_script(script);
    let index = LineIndex::new(script);
    let mut last_end = 0;
    let whole = dialect.classify(script);
    let special = items.iter().any(|i| i.kind != ScriptItemKind::Statement);
    for item in &items {
        let s = item.span;
        assert!(
            s.start >= last_end && s.start <= s.end && s.end <= script.len(),
            "span {s:?} out of order in {script:?}"
        );
        assert!(
            script.is_char_boundary(s.start) && script.is_char_boundary(s.end),
            "span {s:?} not on char boundaries in {script:?}"
        );
        assert_eq!(
            item.start,
            index.position(s.start),
            "start position in {script:?}"
        );
        assert_eq!(
            item.end,
            index.position(s.end),
            "end position in {script:?}"
        );
        last_end = s.end;
        if item.kind == ScriptItemKind::Statement {
            let text = item.text(script);
            // Blank as the lexers read it: ASCII whitespace only (U+2000 is an identifier byte).
            assert!(
                !text.bytes().all(|b| b.is_ascii_whitespace()),
                "empty statement in {script:?}"
            );
            // At most one: a statement whose last token was followed, in the script, by a
            // comment can end with a `--` that is a comment only at the end of a text (MySQL).
            // A piece that starts inside an executable comment (after a `;` in it) is lexed
            // without that context on its own and may split further: the driver then refuses
            // it as several statements, which fails closed.
            let inside_executable = dialect.lexical_rules().executable_comments
                && (text.contains("*/") || text.contains("/*"));
            assert!(
                dialect.split_statements(text).len() <= 1 || inside_executable,
                "statement item {text:?} of {script:?} is not one statement"
            );
            if !special {
                let c = dialect.classify(text);
                assert!(
                    whole >= c,
                    "script {script:?} classifies {whole:?} below its statement {text:?} ({c:?})"
                );
            }
        }
    }
}

/// Statements of every class, valid in the four corpus dialects, for property tests that join,
/// split and classify them.
pub const STATEMENT_POOL: &[&str] = &[
    "SELECT 1",
    "SELECT * FROM t WHERE a = 'x;y'",
    "SELECT 'it''s' AS \"a;b\"",
    "INSERT INTO t VALUES (1)",
    "UPDATE t SET a = 1 WHERE id = 2",
    "UPDATE t SET a = 1",
    "DELETE FROM t",
    "DELETE FROM t WHERE id = 1",
    "DROP TABLE t",
    "CREATE TABLE t2 (a INT)",
    "ALTER TABLE t ADD COLUMN b INT",
    "BEGIN",
    "COMMIT",
    "WITH x AS (SELECT 1) SELECT * FROM x",
    "SELECT a /* ; */ FROM t -- ;\n",
];

/// Asserts that joining `stmts` with `sep` (`";\n"`, `" ; "`…) and splitting the script gives
/// back exactly `stmts` (trimmed), each with its own class, and that the joined script
/// classifies as the riskiest of them (concatenation never hides a write).
///
/// # Panics
/// With the script and the mismatch.
pub fn assert_join_split_roundtrip(dialect: &dyn Dialect, stmts: &[&str], sep: &str) {
    let script = stmts.join(sep);
    let items = dialect.split_script(&script);
    let texts: Vec<&str> = items.iter().map(|i| i.text(&script)).collect();
    let expected: Vec<String> = stmts
        .iter()
        .map(|s| {
            // A trailing line comment is part of the statement's span only up to its last token.
            let spans = dialect.split_statements(s);
            spans
                .first()
                .map_or_else(String::new, |sp| sp.text(s).to_owned())
        })
        .collect();
    assert_eq!(
        texts.len(),
        expected.len(),
        "{script:?} split into {texts:?}"
    );
    for (got, want) in texts.iter().zip(&expected) {
        assert_eq!(got, want, "{script:?}");
        assert_eq!(dialect.classify(got), dialect.classify(want), "{script:?}");
    }
    let worst = stmts
        .iter()
        .map(|s| dialect.classify(s))
        .max()
        .unwrap_or(StatementClass::Read);
    assert_eq!(
        dialect.classify(&script),
        worst,
        "joining hid a statement in {script:?}"
    );
}

/// One step of [`script_plan`]: a statement's text and the class it had just before it ran,
/// or a refused piece (`class: None`), where the plan stops.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedStatement {
    /// The text sent (or refused).
    pub text: String,
    /// The class computed with the session's dialect right before running; `None` for a
    /// refused piece.
    pub class: Option<StatementClass>,
}

/// Reference implementation of the "execute script" loop that the contract of
/// [`Dialect::split_script`] asks of callers, without a server:
/// - items come lazily from [`Dialect::split_script_ahead`], a few at a time, with the
///   [`ScriptState`](crate::ScriptState) kept by [`Dialect::script_state_after`];
/// - each statement is classified with the current dialect right before it "runs";
/// - `run(text)` simulates the server and returns the session's new dialect when the
///   statement changed it;
/// - after **every** item the dialect's `lexical_rules()` are compared with the ones the pending
///   items were split with; when they differ the pending items are dropped and splitting goes on
///   from the end of that item (re-review L2);
/// - client directives are skipped; the plan stops at a refused piece.
///
/// Each step costs a small window of text, so the loop stays linear even when every statement
/// changes the lexical modes (re-review M1).
pub fn script_plan(
    script: &str,
    initial: Box<dyn Dialect>,
    run: &mut dyn FnMut(&str) -> Option<Box<dyn Dialect>>,
) -> Vec<PlannedStatement> {
    use crate::dialect::{ScriptItemKind, ScriptState, TextPosition};
    let mut current = initial;
    let mut rules = current.lexical_rules();
    let mut state = ScriptState::default();
    let mut pending = std::collections::VecDeque::new();
    let mut resume = TextPosition::START;
    let mut out = Vec::new();
    loop {
        if pending.is_empty() {
            if resume.offset >= script.len() {
                break;
            }
            let more = current.split_script_ahead(script, resume, &state);
            if more.is_empty() {
                break;
            }
            pending.extend(more);
        }
        let Some(item) = pending.pop_front() else {
            break;
        };
        resume = item.end;
        state = current.script_state_after(script, &item, &state);
        let text = item.text(script);
        match item.kind {
            ScriptItemKind::Statement => {}
            ScriptItemKind::ClientDirective => continue,
            _ => {
                out.push(PlannedStatement {
                    text: text.to_owned(),
                    class: None,
                });
                break;
            }
        }
        out.push(PlannedStatement {
            text: text.to_owned(),
            class: Some(current.classify(text)),
        });
        if let Some(next) = run(text) {
            current = next;
        }
        if current.lexical_rules() != rules {
            rules = current.lexical_rules();
            pending.clear();
        }
    }
    out
}
