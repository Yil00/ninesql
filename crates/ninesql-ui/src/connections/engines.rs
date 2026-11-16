//! What the connection dialog offers per engine, read from the driver registry and the
//! drivers' declared capabilities (I8): an engine without a registered driver is not offered,
//! and a setting the driver does not declare is hidden or disabled.

use std::collections::BTreeSet;

use ninesql_core::DriverRegistry;
use ninesql_core::EngineId;
use ninesql_core::TlsMode;
use ninesql_core::driver_api::{self as api, AuthKind, Capabilities, PasswordExchange};

/// Engines whose endpoint is a database file rather than a server (ADR-0003). The driver API
/// has no endpoint-kind capability, so the dialog keeps this list.
const FILE_ENGINES: &[&str] = &["sqlite", "duckdb"];

/// Display name of an engine id.
#[must_use]
pub fn display_name(engine: &str) -> String {
    match engine {
        "postgresql" => "PostgreSQL".to_owned(),
        "mysql" => "MySQL".to_owned(),
        "mariadb" => "MariaDB".to_owned(),
        "sqlite" => "SQLite".to_owned(),
        "duckdb" => "DuckDB".to_owned(),
        "timescaledb" => "TimescaleDB".to_owned(),
        "yugabytedb" => "YugabyteDB".to_owned(),
        "questdb" => "QuestDB".to_owned(),
        "tidb" => "TiDB".to_owned(),
        other => other.to_owned(),
    }
}

/// Order of the engine picker: the MVP engines first, then the rest alphabetically.
fn picker_rank(engine: &str) -> (usize, String) {
    const ORDER: &[&str] = &["postgresql", "mysql", "mariadb", "sqlite"];
    (
        ORDER
            .iter()
            .position(|e| *e == engine)
            .unwrap_or(ORDER.len()),
        engine.to_owned(),
    )
}

/// The parts of a driver's capabilities the connection dialog reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineOffer {
    /// Engine id (`postgresql`).
    pub engine: String,
    /// Display name (`PostgreSQL`).
    pub name: String,
    /// The endpoint is a file (no host, port, TLS or SSH).
    pub file_based: bool,
    /// TLS modes offered, weakest first; empty when the driver offers no TLS.
    pub tls_modes: Vec<TlsMode>,
    /// Authentication kinds accepted.
    pub auth: BTreeSet<AuthKind>,
    /// Allowed property keys: `None` for any key that is not reserved, `Some(empty)` for none.
    pub accepted_properties: Option<Vec<String>>,
    /// `verify-ca` needs a CA file (ADR-0019).
    pub verify_ca_requires_ca_file: bool,
    /// TLS client certificates are supported.
    pub client_certificates: bool,
    /// Encrypted client keys are supported.
    pub encrypted_client_keys: bool,
    /// The driver can enforce SCRAM-only password exchange.
    pub scram_only: bool,
    /// Default port, shown as the port placeholder.
    pub default_port: Option<u16>,
    /// The full capabilities, for `validate_connection`-style checks.
    pub capabilities: Capabilities,
}

impl EngineOffer {
    /// Builds the offer from declared capabilities.
    #[must_use]
    pub fn from_capabilities(engine: &str, caps: Capabilities) -> Self {
        let mut tls_modes: Vec<TlsMode> = caps.tls.iter().filter_map(|m| model_tls(*m)).collect();
        tls_modes.sort_by_key(|m| tls_rank(*m));
        let file_based = FILE_ENGINES.contains(&engine);
        Self {
            engine: engine.to_owned(),
            name: display_name(engine),
            file_based,
            tls_modes: if file_based { Vec::new() } else { tls_modes },
            auth: caps.connect.auth.clone(),
            accepted_properties: caps
                .connect
                .accepted_properties
                .as_ref()
                .map(|set| set.iter().cloned().collect()),
            verify_ca_requires_ca_file: caps.connect.verify_ca_requires_ca_file,
            client_certificates: caps.connect.client_certificates,
            encrypted_client_keys: caps.connect.encrypted_client_keys,
            scram_only: caps
                .connect
                .password_exchanges
                .contains(&PasswordExchange::ScramOnly),
            default_port: ninesql_core::connect::default_port(engine),
            capabilities: caps,
        }
    }

    /// Whether the driver offers TLS at all.
    #[must_use]
    pub fn offers_tls(&self) -> bool {
        !self.tls_modes.is_empty()
    }

    /// Whether driver properties can be set (an allowlist with entries, or no allowlist).
    #[must_use]
    pub fn offers_properties(&self) -> bool {
        self.accepted_properties
            .as_ref()
            .is_none_or(|keys| !keys.is_empty())
    }

    /// Whether `key` may be offered as a property key (allowlist and reserved rule).
    #[must_use]
    pub fn accepts_property(&self, key: &str) -> bool {
        self.capabilities.connect.accepts_property(key)
            && !self.capabilities.is_reserved_property(key)
    }
}

/// The model's TLS mode for a declared driver API mode; `None` for a mode the model does not
/// know (not offered rather than mislabelled, I8).
#[must_use]
pub fn model_tls(mode: api::TlsMode) -> Option<TlsMode> {
    Some(match mode {
        api::TlsMode::Disable => TlsMode::Disable,
        api::TlsMode::Require => TlsMode::Require,
        api::TlsMode::VerifyCa => TlsMode::VerifyCa,
        api::TlsMode::VerifyFull => TlsMode::VerifyFull,
        _ => return None,
    })
}

/// Weakest first, so the picker reads from "off" to "fully verified".
#[must_use]
pub fn tls_rank(mode: TlsMode) -> u8 {
    match mode {
        TlsMode::Disable => 0,
        TlsMode::Require => 1,
        TlsMode::VerifyCa => 2,
        TlsMode::VerifyFull => 3,
    }
}

/// Label of a TLS mode in the dialog.
#[must_use]
pub fn tls_label(mode: TlsMode) -> &'static str {
    match mode {
        TlsMode::Disable => "Disabled",
        TlsMode::Require => "Encrypt only",
        TlsMode::VerifyCa => "Verify CA",
        TlsMode::VerifyFull => "Verify CA and host name",
    }
}

/// Every engine of the registry, with its offer, in picker order. Engines whose capabilities
/// cannot be read are left out (logged).
#[must_use]
pub fn offers(registry: &DriverRegistry) -> Vec<EngineOffer> {
    let mut out: Vec<EngineOffer> = registry
        .engines()
        .filter_map(|engine| {
            let id = EngineId::new(engine).ok()?;
            match registry.capabilities(&id) {
                Ok(caps) => Some(EngineOffer::from_capabilities(engine, caps)),
                Err(e) => {
                    tracing::warn!(engine, error = %e, "engine left out of the connection dialog");
                    None
                }
            }
        })
        .collect();
    out.sort_by_key(|o| picker_rank(&o.engine));
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn shipped() -> DriverRegistry {
        let mut r = DriverRegistry::new();
        r.register(Arc::new(ninesql_driver_pgwire::PgWireDriver::new()));
        r.register(Arc::new(ninesql_driver_mysqlwire::MySqlWireDriver::new()));
        r.register(Arc::new(ninesql_driver_sqlite::SqliteDriver::new()));
        r
    }

    #[test]
    fn the_shipped_drivers_offer_the_mvp_engines_first_then_the_wire_compatible_ones() {
        let offers = offers(&shipped());
        let names: Vec<_> = offers.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "PostgreSQL",
                "MySQL",
                "MariaDB",
                "SQLite",
                "QuestDB",
                "TiDB",
                "TimescaleDB",
                "YugabyteDB"
            ]
        );
    }

    #[test]
    fn offers_follow_the_declared_capabilities() {
        let offers = offers(&shipped());
        let pg = &offers[0];
        assert!(!pg.file_based);
        assert_eq!(
            pg.tls_modes,
            [
                TlsMode::Disable,
                TlsMode::Require,
                TlsMode::VerifyCa,
                TlsMode::VerifyFull
            ]
        );
        assert!(pg.scram_only, "pgwire enforces ScramOnly");
        assert!(pg.verify_ca_requires_ca_file);
        assert_eq!(pg.default_port, Some(5432));
        assert!(pg.offers_properties());
        let mysql = &offers[1];
        assert!(!mysql.scram_only, "mysqlwire refuses ScramOnly");
        let sqlite = &offers[3];
        assert!(sqlite.file_based);
        assert!(!sqlite.offers_tls());
        assert_eq!(sqlite.auth, BTreeSet::from([AuthKind::None]));
        assert!(sqlite.accepts_property("busy_timeout"));
        assert!(!sqlite.accepts_property("journal_mode"));
    }

    #[test]
    fn an_empty_registry_offers_nothing() {
        assert!(offers(&DriverRegistry::new()).is_empty());
    }
}
