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
//!     conformance::assert_read_only_refuses_writes(&driver, &params, conformance::WRITES).await;
//!     conformance::assert_multiple_statements_refused(&driver, &params).await;
//! }
//! ```

use std::collections::BTreeMap;

use crate::driver::{Driver, Session};
use crate::error::DriverError;
use crate::params::{ConnectParams, PropertyValue};
use crate::statement::{ExecOptions, Statement};

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

/// Generic write statements; each must fail with [`DriverError::ReadOnly`] in a read-only
/// session. Drivers add engine-specific ones.
pub const WRITES: &[&str] = &[
    "INSERT INTO ninesql_conformance VALUES (1)",
    "UPDATE ninesql_conformance SET a = 1 WHERE a = 0",
    "DELETE FROM ninesql_conformance",
    "CREATE TABLE ninesql_conformance_new (a INT)",
    "DROP TABLE ninesql_conformance",
    "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
    "SET default_transaction_read_only = off",
    "SET \"default_transaction_read_only\" = off",
    "SET transaction_read_only = 0",
    "SELECT set_config('default_transaction_read_only', 'off', false)",
];

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
