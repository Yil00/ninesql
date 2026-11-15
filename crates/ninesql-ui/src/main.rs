//! The `ninesql` binary: registers the drivers NineSQL ships in the core's `DriverRegistry`
//! (L5) and starts the application. Only this file names driver crates; the library reaches
//! drivers through `ninesql-core` (docs/02-architecture.md).
#![forbid(unsafe_code)]

use std::sync::Arc;

use ninesql_core::DriverRegistry;

fn main() {
    let mut registry = DriverRegistry::new();
    registry.register(Arc::new(ninesql_driver_pgwire::PgWireDriver::new()));
    registry.register(Arc::new(ninesql_driver_mysqlwire::MySqlWireDriver::new()));
    registry.register(Arc::new(ninesql_driver_sqlite::SqliteDriver::new()));
    ninesql_ui::app::run(registry);
}
