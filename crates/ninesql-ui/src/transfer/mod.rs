//! Data transfer UI (P5-A over the P5-C core service): "Export results…", "Export table…"
//! (F-XFER-01) and "Import CSV…" (F-XFER-03).
//!
//! - The dialogs only talk to `TransferService` (`CoreServices::transfer()`): an export or an
//!   import runs on the driver runtime; its events are awaited on GPUI's background executor
//!   and applied on the foreground thread (I6). Cancel drops the job, which cancels it in the
//!   core (a cancelled export removes its partial file).
//! - Export targets are a **new** file: `TransferService::check_export_path` refuses an
//!   existing file unless the user confirmed the overwrite in its own step, a folder inside a
//!   DBeaver workspace (I4) and NineSQL's own storage (I1).
//! - An import on a production or confirm-every-write connection stops at
//!   `ImportEvent::NeedsConfirmation`: the dialog shows the confirmation's details exactly
//!   (connection, table, columns, statement, file and size) and runs again with
//!   `ImportRequest::confirmed(token)` only on "Import" (I3, ADR-0007).
//! - Starting is refused while the session is leased by another transfer, runs a statement or
//!   holds an open transaction (import); the buttons say why (I8).

pub mod export;
pub mod import;

#[cfg(test)]
mod tests;

use ninesql_core::driver_api::ObjectKind;
use ninesql_core::driver_api::metadata::ObjectPath;
use ninesql_core::{SessionHandle, TransferJobError};

/// The table name parts a transfer request takes (`[schema, table]`, `[catalog, table]` or
/// `[table]`): the nearest schema (else catalog) and the object's name, as "Generate SQL" does.
#[must_use]
pub fn table_parts(path: &ObjectPath) -> Vec<String> {
    let Some((leaf, parents)) = path.0.split_last() else {
        return Vec::new();
    };
    let container = parents
        .iter()
        .rev()
        .find(|s| s.kind == ObjectKind::Schema)
        .or_else(|| parents.iter().rev().find(|s| s.kind == ObjectKind::Catalog));
    match container {
        Some(c) => vec![c.name.clone(), leaf.name.clone()],
        None => vec![leaf.name.clone()],
    }
}

/// Why a transfer cannot start on `session` now, if so.
#[must_use]
pub fn session_problem(session: &SessionHandle, import: bool) -> Option<&'static str> {
    if session.is_closed() {
        Some("The session is closed: connect again.")
    } else if session.is_leased() {
        Some("Another export or import is using this session: wait for it to end.")
    } else if import && session.info().read_only {
        Some("The session is read-only: an import writes data.")
    } else if import && session.transaction_state().in_transaction {
        Some("A transaction is open on this session: commit or roll back first.")
    } else {
        None
    }
}

/// The user-facing text of a transfer error (no value from the data or the file).
#[must_use]
pub fn error_text(e: &TransferJobError) -> String {
    crate::browse::display_message(&e.to_string())
}

/// `12.3 MiB`.
#[must_use]
pub fn bytes_text(bytes: u64) -> String {
    ninesql_grid::execution::format_bytes(bytes)
}
