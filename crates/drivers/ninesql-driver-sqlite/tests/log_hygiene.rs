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
