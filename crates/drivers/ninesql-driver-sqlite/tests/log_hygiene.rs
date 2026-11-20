//! I1: statement text (init SQL included) that may hold a credential never reaches the log,
//! even at `trace` level with statement logging allowed (`text_may_hold_credentials`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use ninesql_driver_api::{ConnectParams, Endpoint, EngineId, conformance};
use ninesql_driver_sqlite::SqliteDriver;

#[tokio::test]
async fn statements_with_credentials_are_never_logged() {
    let dir = tempfile::tempdir().unwrap();
    let endpoint = Endpoint::File {
        path: dir.path().join("l.db"),
        create_if_missing: true,
    };
    let p = ConnectParams::new(EngineId::SQLITE, endpoint);
    conformance::assert_credential_statements_not_logged(&SqliteDriver::new(), p).await;
}

/// ADR-0029 §3.2: an engine's error message (here SQLite's `no such table: …`, which quotes the
/// name) never reaches an event at `debug` and above.
#[tokio::test]
async fn engine_messages_are_never_logged() {
    let dir = tempfile::tempdir().unwrap();
    let endpoint = Endpoint::File {
        path: dir.path().join("m.db"),
        create_if_missing: true,
    };
    let p = ConnectParams::new(EngineId::SQLITE, endpoint);
    let probe = conformance::ENGINE_MESSAGE_PROBE;
    let insert = format!("INSERT INTO ninesql_log_probe VALUES ('{probe}')");
    conformance::assert_engine_messages_not_logged(
        &SqliteDriver::new(),
        p,
        &[
            "CREATE TABLE ninesql_log_probe (k TEXT PRIMARY KEY)",
            &insert,
        ],
        &[&insert, &format!("SELECT * FROM \"{probe}\"")],
    )
    .await;
}
