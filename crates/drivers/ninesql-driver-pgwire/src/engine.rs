//! The engines of the pgwire family (ADR-0003, P4-C): PostgreSQL, TimescaleDB, YugabyteDB (YSQL)
//! and QuestDB. Each one is its own engine id with its own capabilities (I8); the server is
//! checked against the declared engine at connect, and a mismatch fails the connect.
//!
//! Detection (docs/08):
//! - **QuestDB**: the `server_version` startup parameter or `version()` names QuestDB;
//! - **YugabyteDB**: the server version holds `-YB-` (`15.12-YB-2026.1.3.0-b0`);
//! - **TimescaleDB**: a PostgreSQL server whose current database has the `timescaledb`
//!   extension (an extension, so it is per database);
//! - **PostgreSQL**: anything else. A PostgreSQL connection to a database with the
//!   `timescaledb` extension is accepted (it is PostgreSQL, browsed without hypertable
//!   awareness); a TimescaleDB connection to a database without the extension is refused.

use std::collections::BTreeSet;

use ninesql_driver_api::{
    AuthKind, CancelSupport, Capabilities, ConnectCapabilities, DriverError, DriverResult,
    EditingCapabilities, EngineId, ExplainFormat, IsolationLevel, MetadataCapabilities, ObjectKind,
    PasswordExchange, ReadOnlyMechanism, TlsMode, TransactionCapabilities,
};

use crate::{ACCEPTED_PROPERTIES, EXTRA_RESERVED_PROPERTIES};

/// One engine of the family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flavor {
    Postgres,
    Timescale,
    Yugabyte,
    QuestDb,
}

impl Flavor {
    /// The flavor of a declared engine id, `None` for engines outside the family.
    pub(crate) fn of(engine: &EngineId) -> Option<Self> {
        if *engine == EngineId::POSTGRESQL {
            Some(Self::Postgres)
        } else if *engine == EngineId::TIMESCALEDB {
            Some(Self::Timescale)
        } else if *engine == EngineId::YUGABYTEDB {
            Some(Self::Yugabyte)
        } else if *engine == EngineId::QUESTDB {
            Some(Self::QuestDb)
        } else {
            None
        }
    }

    pub(crate) fn engine(self) -> EngineId {
        match self {
            Self::Postgres => EngineId::POSTGRESQL,
            Self::Timescale => EngineId::TIMESCALEDB,
            Self::Yugabyte => EngineId::YUGABYTEDB,
            Self::QuestDb => EngineId::QUESTDB,
        }
    }

    /// Human name, for messages.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Postgres => "PostgreSQL",
            Self::Timescale => "TimescaleDB",
            Self::Yugabyte => "YugabyteDB",
            Self::QuestDb => "QuestDB",
        }
    }
}

/// The server family seen from its version strings: QuestDB, YugabyteDB or PostgreSQL proper
/// (`Flavor::Postgres`, which TimescaleDB also reports).
pub(crate) fn server_family(server_version: &str, version_fn: Option<&str>) -> Flavor {
    let is = |needle: &str| {
        server_version.to_ascii_lowercase().contains(needle)
            || version_fn.is_some_and(|v| v.to_ascii_lowercase().contains(needle))
    };
    if is("questdb") {
        Flavor::QuestDb
    } else if server_version.contains("-YB-") || version_fn.is_some_and(|v| v.contains("-YB-")) {
        Flavor::Yugabyte
    } else {
        Flavor::Postgres
    }
}

/// Checks the server family against the declared engine (the `timescaledb` extension is
/// checked separately, with a query).
pub(crate) fn check_family(declared: Flavor, server: Flavor, version: &str) -> DriverResult<()> {
    let ok = match declared {
        Flavor::Postgres | Flavor::Timescale => server == Flavor::Postgres,
        other => server == other,
    };
    if ok {
        Ok(())
    } else {
        let shown: String = version
            .chars()
            .filter(|c| !c.is_control())
            .take(120)
            .collect();
        Err(DriverError::Connect(format!(
            "this server reports version {shown}, which is {}, not {}; choose the matching \
             engine",
            server.name(),
            declared.name()
        )))
    }
}

/// What a session learned about its server at connect, for the exact capabilities.
#[derive(Clone, Debug, Default)]
pub(crate) struct ServerInfo {
    /// YugabyteDB runs `READ COMMITTED` for real (otherwise it maps it to snapshot isolation).
    pub(crate) yb_read_committed: bool,
    /// TimescaleDB extension version.
    pub(crate) timescale_version: Option<String>,
    /// QuestDB answers cancel requests (it sent a backend key).
    pub(crate) cancel_key: bool,
}

/// Capabilities of `flavor` through this driver (mirrored in docs/08). `info` is `None`
/// before connect (connection dialog), where the conservative value of each server-dependent
/// field is given.
pub(crate) fn capabilities(
    flavor: Flavor,
    server_version: Option<&str>,
    info: Option<&ServerInfo>,
) -> Capabilities {
    let mut c = Capabilities::minimal(flavor.engine());
    c.server_version = match (
        server_version,
        info.and_then(|i| i.timescale_version.as_deref()),
    ) {
        (Some(v), Some(ts)) => Some(format!("{v}, TimescaleDB {ts}")),
        (v, _) => v.map(str::to_owned),
    };
    if flavor == Flavor::QuestDb {
        return questdb_capabilities(c, info);
    }
    let mut metadata = MetadataCapabilities::default();
    metadata.kinds = BTreeSet::from([
        ObjectKind::Schema,
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::MaterializedView,
        ObjectKind::Column,
        ObjectKind::Constraint,
        ObjectKind::ForeignKey,
        ObjectKind::Index,
        ObjectKind::Trigger,
        ObjectKind::Partition,
        ObjectKind::Routine,
        ObjectKind::Sequence,
        ObjectKind::Type,
    ]);
    // Every listed kind but `Column` has DDL (P4-A, F-NAV-07).
    metadata.ddl = metadata
        .kinds
        .iter()
        .copied()
        .filter(|k| *k != ObjectKind::Column)
        .collect();
    c.metadata = metadata;
    let mut editing = EditingCapabilities::default();
    editing.insert = true;
    editing.update = true;
    editing.delete = true;
    editing.requires_unique_key = true;
    c.editing = editing;
    let mut tx = TransactionCapabilities::default();
    tx.manual_commit = true;
    tx.savepoints = true;
    tx.isolation_levels = if flavor == Flavor::Yugabyte {
        // Without its read-committed mode, YugabyteDB runs `READ COMMITTED` as snapshot
        // isolation: offered only when the server says it really runs it.
        let mut levels =
            BTreeSet::from([IsolationLevel::RepeatableRead, IsolationLevel::Serializable]);
        if info.is_some_and(|i| i.yb_read_committed) {
            levels.insert(IsolationLevel::ReadCommitted);
        }
        levels
    } else {
        BTreeSet::from([
            IsolationLevel::ReadCommitted,
            IsolationLevel::RepeatableRead,
            IsolationLevel::Serializable,
        ])
    };
    c.transactions = tx;
    c.read_only = ReadOnlyMechanism::SessionSetting { liftable: true };
    c.cancel = CancelSupport::ServerSide;
    c.explain = BTreeSet::from([
        ExplainFormat::Text,
        ExplainFormat::Json,
        ExplainFormat::Analyze,
    ]);
    c.multiple_result_sets = false;
    c.server_side_timeout = false;
    c.tls = BTreeSet::from([
        TlsMode::Disable,
        TlsMode::Require,
        TlsMode::VerifyCa,
        TlsMode::VerifyFull,
    ]);
    c.parameters = true;
    c.extra_reserved_properties = EXTRA_RESERVED_PROPERTIES
        .iter()
        .map(|k| (*k).to_owned())
        .collect();
    c.retains_secret_after_connect = false;
    let mut connect = ConnectCapabilities::default();
    connect.accepted_properties = Some(
        ACCEPTED_PROPERTIES
            .iter()
            .map(|(_, name)| (*name).to_owned())
            .collect(),
    );
    connect.auth = BTreeSet::from([AuthKind::Password]);
    connect.password_exchanges =
        BTreeSet::from([PasswordExchange::Any, PasswordExchange::ScramOnly]);
    connect.verify_ca_requires_ca_file = true;
    connect.client_certificates = true;
    connect.encrypted_client_keys = false;
    c.connect = connect;
    c
}

/// QuestDB: no schemas, no transactions, no session read-only mode, cleartext passwords and no
/// TLS in the open-source server.
fn questdb_capabilities(mut c: Capabilities, info: Option<&ServerInfo>) -> Capabilities {
    let mut metadata = MetadataCapabilities::default();
    metadata.kinds = BTreeSet::from([
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::MaterializedView,
        ObjectKind::Column,
        ObjectKind::Partition,
    ]);
    metadata.ddl = BTreeSet::from([
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::MaterializedView,
    ]);
    c.metadata = metadata;
    // No unique keys (an edit could not target one row) and no `DELETE`.
    c.editing = EditingCapabilities::default();
    // `BEGIN` / `COMMIT` are accepted but are no transactions in the PostgreSQL sense.
    c.transactions = TransactionCapabilities::default();
    // QuestDB's read-only user (`pg.readonly.user`) or a read-only server
    // (`pg.security.readonly`, `readonly`): enforced by the server for the whole session, and
    // no statement lifts it. Verified at connect; any other read-only request is refused.
    c.read_only = ReadOnlyMechanism::ServerSetting { liftable: false };
    // Before connect the conservative value; QuestDB answers cancel requests when it sent a
    // backend key.
    c.cancel = if info.is_some_and(|i| i.cancel_key) {
        CancelSupport::ServerSide
    } else {
        CancelSupport::LocalOnly
    };
    c.explain = BTreeSet::from([ExplainFormat::Text, ExplainFormat::Json]);
    c.multiple_result_sets = false;
    c.server_side_timeout = false;
    // The open-source server has no TLS on its PostgreSQL port.
    c.tls = BTreeSet::from([TlsMode::Disable]);
    c.parameters = true;
    c.extra_reserved_properties = EXTRA_RESERVED_PROPERTIES
        .iter()
        .map(|k| (*k).to_owned())
        .collect();
    c.retains_secret_after_connect = false;
    let mut connect = ConnectCapabilities::default();
    // QuestDB ignores PostgreSQL session settings: no property is accepted.
    connect.accepted_properties = Some(BTreeSet::new());
    connect.auth = BTreeSet::from([AuthKind::Password]);
    // QuestDB asks for a cleartext password: `ScramOnly` cannot be honoured.
    connect.password_exchanges = BTreeSet::from([PasswordExchange::Any]);
    connect.verify_ca_requires_ca_file = true;
    connect.client_certificates = false;
    connect.encrypted_client_keys = false;
    c.connect = connect;
    c
}

/// Table-valued settings QuestDB reports for its read-only modes (`SHOW PARAMETERS`).
pub(crate) const QUESTDB_READ_ONLY_SETTINGS_SQL: &str = "SELECT property_path, value FROM (SHOW PARAMETERS) \
     WHERE property_path IN ('readonly', 'pg.security.readonly', 'pg.readonly.user.enabled', \
     'pg.readonly.user')";

/// A write that changes nothing whatever the account: `?` is not allowed in QuestDB table names
/// (`CREATE TABLE` refuses it), so the table never exists, and the `SELECT` yields no row. The
/// read-only user fails it with a permission error before the table is looked up; a writer
/// fails it with "table does not exist".
pub(crate) const QUESTDB_WRITE_PROBE_SQL: &str =
    "INSERT INTO \"ninesql?probe\" SELECT x FROM long_sequence(0)";

/// Whether QuestDB's settings make this session read-only: the whole server is read-only, or
/// the session's user is the enabled read-only user.
pub(crate) fn questdb_settings_read_only(rows: &[(String, String)], current_user: &str) -> bool {
    let get = |k: &str| {
        rows.iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    let on = |k: &str| get(k).is_some_and(|v| v.eq_ignore_ascii_case("true"));
    on("readonly")
        || on("pg.security.readonly")
        || (on("pg.readonly.user.enabled") && get("pg.readonly.user") == Some(current_user))
}

/// The probe's error says the write was denied for lack of permission.
pub(crate) fn questdb_probe_denied(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("permission denied") || m.contains("read-only") || m.contains("read only")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_are_detected() {
        assert_eq!(server_family("18.6", None), Flavor::Postgres);
        assert_eq!(
            server_family("15.12-YB-2026.1.3.0-b0", None),
            Flavor::Yugabyte
        );
        assert_eq!(server_family("12.3 (questdb)", None), Flavor::QuestDb);
        assert_eq!(
            server_family(
                "12.3",
                Some("PostgreSQL 12.3, compiled by Visual C++, QuestDB")
            ),
            Flavor::QuestDb
        );
        assert!(check_family(Flavor::Timescale, Flavor::Postgres, "18").is_ok());
        assert!(check_family(Flavor::Postgres, Flavor::Yugabyte, "15-YB").is_err());
        assert!(check_family(Flavor::QuestDb, Flavor::Postgres, "18").is_err());
        assert!(check_family(Flavor::Yugabyte, Flavor::Postgres, "18").is_err());
        let msg = check_family(Flavor::Postgres, Flavor::QuestDb, "12.3\u{1b}[2J (questdb)")
            .unwrap_err()
            .to_string();
        assert!(!msg.chars().any(char::is_control), "{msg:?}");
    }

    #[test]
    fn questdb_read_only_needs_settings_for_this_user() {
        let rows = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        let user_mode = rows(&[
            ("readonly", "false"),
            ("pg.security.readonly", "false"),
            ("pg.readonly.user.enabled", "true"),
            ("pg.readonly.user", "nine_ro"),
        ]);
        assert!(questdb_settings_read_only(&user_mode, "nine_ro"));
        assert!(!questdb_settings_read_only(&user_mode, "nine"));
        let server_mode = rows(&[("pg.security.readonly", "true")]);
        assert!(questdb_settings_read_only(&server_mode, "admin"));
        assert!(!questdb_settings_read_only(&[], "nine_ro"));
        assert!(questdb_probe_denied("Write permission denied"));
        assert!(!questdb_probe_denied(
            "table does not exist [table=ninesql?probe]"
        ));
    }

    #[test]
    fn capabilities_differ_per_engine() {
        let pg = capabilities(Flavor::Postgres, None, None);
        let q = capabilities(Flavor::QuestDb, None, None);
        assert_eq!(
            q.read_only,
            ReadOnlyMechanism::ServerSetting { liftable: false }
        );
        assert!(q.read_only.is_engine_side() && !q.read_only.is_liftable());
        assert!(!q.transactions.manual_commit && q.transactions.isolation_levels.is_empty());
        assert_eq!(q.tls, BTreeSet::from([TlsMode::Disable]));
        assert!(
            !q.connect
                .accepts_password_exchange(PasswordExchange::ScramOnly)
        );
        assert!(!q.connect.accepts_property("search_path"));
        assert!(!q.metadata.kinds.contains(&ObjectKind::Schema));
        assert_eq!(q.cancel, CancelSupport::LocalOnly);
        let yb = capabilities(Flavor::Yugabyte, None, None);
        assert!(
            !yb.transactions
                .isolation_levels
                .contains(&IsolationLevel::ReadCommitted)
        );
        let info = ServerInfo {
            yb_read_committed: true,
            ..ServerInfo::default()
        };
        let yb = capabilities(Flavor::Yugabyte, Some("15.12-YB"), Some(&info));
        assert!(
            yb.transactions
                .isolation_levels
                .contains(&IsolationLevel::ReadCommitted)
        );
        let ts = capabilities(
            Flavor::Timescale,
            Some("18.6"),
            Some(&ServerInfo {
                timescale_version: Some("2.32.0".into()),
                ..ServerInfo::default()
            }),
        );
        assert_eq!(ts.engine, EngineId::TIMESCALEDB);
        assert_eq!(
            ts.server_version.as_deref(),
            Some("18.6, TimescaleDB 2.32.0")
        );
        assert_eq!(ts.metadata, pg.metadata);
    }
}
