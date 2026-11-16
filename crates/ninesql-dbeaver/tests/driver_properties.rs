//! JDBC driver properties and URL parameters → NineSQL driver properties (docs/06, "Driver
//! properties"). Synthetic snippets only: hosts under `example.test`, made-up values.

use std::collections::BTreeMap;

use ninesql_dbeaver::mapping::{ImportedConnection, ProjectImport, map_sources};
use ninesql_dbeaver::{IssueKind, PropertyDropReason, parse_data_sources};
use ninesql_driver_api::{Driver, EngineId};

fn map(json: &str) -> ProjectImport {
    let ds = parse_data_sources(json.as_bytes()).unwrap_or_else(|e| panic!("parse: {e}"));
    map_sources("General", &[ds], None)
}

fn one(provider: &str, driver: &str, configuration: &str) -> String {
    format!(
        r#"{{"connections":{{"c-1":{{"provider":"{provider}","driver":"{driver}","name":"c",
        "configuration":{configuration}}}}}}}"#
    )
}

fn only(p: &ProjectImport) -> &ImportedConnection {
    p.connections[0]
        .imported()
        .unwrap_or_else(|| panic!("not imported: {:?}", p.connections[0]))
}

fn props(c: &ImportedConnection) -> Vec<(&str, &str)> {
    c.properties
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect()
}

fn dropped(c: &ImportedConnection) -> Vec<(&str, PropertyDropReason)> {
    c.dropped_properties
        .iter()
        .map(|d| (d.key.as_str(), d.reason))
        .collect()
}

fn has_issue(p: &ProjectImport, kind: IssueKind, key: &str) -> bool {
    p.connections[0]
        .issues
        .iter()
        .any(|i| i.kind == kind && i.key.as_deref() == Some(key))
}

use PropertyDropReason::{InvalidValue, LooksLikeSecret, NoEquivalent, Reserved};

#[test]
fn pgjdbc_properties() {
    // The URL is read when host and database are empty (docs/06, JDBC URL).
    let cfg = r#"{"url":"jdbc:postgresql://db.example.test/nine?currentSchema=from_url&prepareThreshold=0",
        "properties":{
            "currentSchema":"app,public",
            "options":"-c statement_timeout=30000 -c search_path=ignored -c geqo=off -c lock_timeout=5s",
            "ApplicationName":"nine-app","connectTimeout":"7","statement_timeout":"1",
            "socketFactory":"x"}}"#;
    let p = map(&one("postgresql", "postgres-jdbc", cfg));
    let c = only(&p);
    assert_eq!(
        props(c),
        [
            ("lock_timeout", "5s"),
            ("search_path", "from_url"),
            ("statement_timeout", "30000"),
        ]
    );
    let d = dropped(c);
    for expected in [
        ("configuration.properties.ApplicationName", NoEquivalent),
        ("configuration.properties.connectTimeout", NoEquivalent),
        ("configuration.properties.statement_timeout", NoEquivalent),
        ("configuration.properties.socketFactory", Reserved),
        ("configuration.properties.options.geqo", NoEquivalent),
        // `-c search_path` in options loses to currentSchema (startup parameters win).
        ("configuration.properties.options.search_path", NoEquivalent),
        // pgjdbc lets URL parameters win over driver properties of the same name.
        ("configuration.properties.currentSchema", NoEquivalent),
        ("configuration.url.prepareThreshold", NoEquivalent),
    ] {
        assert!(d.contains(&expected), "{expected:?} not in {d:?}");
    }
    assert!(has_issue(
        &p,
        IssueKind::PropertyMapped,
        "configuration.url.currentSchema"
    ));
    assert!(p.connections[0].issues.iter().any(|i| {
        i.key.as_deref() == Some("configuration.properties.currentSchema")
            && i.message.contains("different value")
    }));
    assert!(has_issue(
        &p,
        IssueKind::PropertyDropped,
        "configuration.properties.ApplicationName"
    ));
    // A pgjdbc property named like a NineSQL one had no effect in DBeaver.
    assert!(p.connections[0].issues.iter().any(|i| {
        i.key.as_deref() == Some("configuration.properties.statement_timeout")
            && i.message.contains("no effect in DBeaver")
    }));
}

#[test]
fn pgjdbc_search_path_keeps_pg_catalog_first() {
    for (value, kept) in [
        ("pg_catalog,app", true),
        (r#"app,\"pg_catalog\""#, false),
        ("app, PG_CATALOG", false),
        ("  ", false),
    ] {
        let cfg =
            format!(r#"{{"host":"db.example.test","properties":{{"currentSchema":"{value}"}}}}"#);
        let p = map(&one("postgresql", "postgres-jdbc", &cfg));
        let c = only(&p);
        assert_eq!(c.properties.contains_key("search_path"), kept, "{value}");
        if !kept {
            assert_eq!(
                dropped(c),
                [("configuration.properties.currentSchema", InvalidValue)]
            );
        }
    }
}

#[test]
fn pgjdbc_options_read_only_and_secrets() {
    let cfg = r#"{"host":"db.example.test","properties":{
        "options":"-c default_transaction_read_only=on -c password=fixture-opt-not-a-secret -B 8"}}"#;
    let p = map(&one("postgresql", "postgres-jdbc", cfg));
    let c = only(&p);
    assert_eq!(c.read_only, Some(true));
    assert!(c.properties.is_empty());
    assert_eq!(
        dropped(c),
        [
            ("configuration.properties.options.password", LooksLikeSecret),
            ("configuration.properties.options", InvalidValue),
        ]
    );
    assert!(!format!("{p:?}").contains("fixture-opt-not-a-secret"));
}

/// QuestDB accepts no driver property (pgwire declares an empty allowlist): every pgjdbc
/// property is dropped with a report entry, `options` is still read for read-only and secrets,
/// TLS properties still feed the TLS settings, and pgjdbc's URL precedence still applies.
#[test]
fn questdb_drops_every_property() {
    let cfg = r#"{"url":"jdbc:postgresql://db.example.test:8812/qdb?currentSchema=from_url",
        "properties":{
            "currentSchema":"app",
            "options":"-c default_transaction_read_only=on -c statement_timeout=1 -c password=fixture-qdb-not-a-secret",
            "ApplicationName":"nine-app","search_path":"app","sslmode":"disable"}}"#;
    let p = map(&one("questdb", "questdb", cfg));
    let c = only(&p);
    assert_eq!(c.engine, EngineId::QUESTDB);
    assert!(c.properties.is_empty(), "{:?}", c.properties);
    assert_eq!(c.read_only, Some(true));
    assert_eq!(c.tls.mode, ninesql_dbeaver::mapping::TlsMode::Disable);
    let d = dropped(c);
    for expected in [
        ("configuration.url.currentSchema", NoEquivalent),
        // pgjdbc lets the URL win: the property is dropped as overridden.
        ("configuration.properties.currentSchema", NoEquivalent),
        ("configuration.properties.ApplicationName", NoEquivalent),
        ("configuration.properties.search_path", NoEquivalent),
        (
            "configuration.properties.options.statement_timeout",
            NoEquivalent,
        ),
        ("configuration.properties.options.password", LooksLikeSecret),
    ] {
        assert!(d.contains(&expected), "{expected:?} not in {d:?}");
    }
    assert!(has_issue(
        &p,
        IssueKind::PropertyMapped,
        "configuration.properties.options.default_transaction_read_only"
    ));
    assert!(p.connections[0].issues.iter().any(|i| {
        i.key.as_deref() == Some("configuration.properties.search_path")
            && i.message
                .contains("QuestDB driver accepts no driver property")
    }));
    assert!(!format!("{p:?}").contains("fixture-qdb-not-a-secret"));
    for i in &p.connections[0].issues {
        assert!(!i.to_string().contains("fixture-qdb-not-a-secret"));
    }
    // What the import produces passes the driver's declared connect checks.
    let caps = ninesql_driver_pgwire::PgWireDriver::default()
        .capabilities(&EngineId::QUESTDB, None)
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        caps.connect.accepted_properties,
        Some(std::collections::BTreeSet::new())
    );
    assert!(caps.tls.contains(&ninesql_driver_api::TlsMode::Disable));
    // A read-only intent is never lost, also through an unreadable entry.
    let ro = |options: &str| {
        read_only_of(
            "questdb",
            "questdb",
            &format!(r#"{{"options":"{options}"}}"#),
        )
    };
    assert_eq!(ro("-c default_transaction_read_only=maybe"), Some(true));
    assert_eq!(ro("-c default_transaction_read_only"), Some(true));
    assert_eq!(ro("-c default_transaction_read_only=off"), None);
}

#[test]
fn connector_j_properties() {
    let cfg = r#"{"host":"db.example.test","properties":{
        "sessionVariables":"time_zone='+02:00',group_concat_max_len=4096,max_execution_time=1000,sql_mode='ANSI',max_statement_time=5",
        "connectionTimeZone":"Europe/Paris","forceConnectionTimeZoneToSession":"true",
        "useCursorFetch":"true","characterEncoding":"utf8"}}"#;
    let p = map(&one("mysql", "mysql8", cfg));
    let c = only(&p);
    // connectionTimeZone (direct) wins over sessionVariables' time_zone.
    assert_eq!(
        props(c),
        [
            ("group_concat_max_len", "4096"),
            ("max_execution_time", "1000"),
            ("time_zone", "Europe/Paris"),
        ]
    );
    let d = dropped(c);
    for expected in [
        (
            "configuration.properties.sessionVariables.sql_mode",
            NoEquivalent,
        ),
        // MariaDB-only variable on a MySQL connection.
        (
            "configuration.properties.sessionVariables.max_statement_time",
            NoEquivalent,
        ),
        (
            "configuration.properties.sessionVariables.time_zone",
            NoEquivalent,
        ),
        ("configuration.properties.useCursorFetch", NoEquivalent),
    ] {
        assert!(d.contains(&expected), "{expected:?} not in {d:?}");
    }
    assert!(
        d.iter()
            .all(|(k, _)| *k != "configuration.properties.forceConnectionTimeZoneToSession")
    );
}

#[test]
fn connector_j_time_zone_needs_force() {
    for (extra, mapped) in [
        ("", false),
        (r#","forceConnectionTimeZoneToSession":"false""#, false),
        (r#","forceConnectionTimeZoneToSession":"true""#, true),
    ] {
        let cfg = format!(
            r#"{{"host":"db.example.test","properties":{{"serverTimezone":"UTC"{extra}}}}}"#
        );
        let p = map(&one("mysql", "mysql8", &cfg));
        let c = only(&p);
        assert_eq!(c.properties.contains_key("time_zone"), mapped, "{extra}");
        assert_eq!(
            !mapped,
            dropped(c).contains(&("configuration.properties.serverTimezone", NoEquivalent)),
            "{extra}"
        );
    }
    // Connector/J 5.1 (DBeaver's `mysql5`) has no forceConnectionTimeZoneToSession.
    let cfg = r#"{"host":"db.example.test","properties":{"serverTimezone":"UTC",
        "forceConnectionTimeZoneToSession":"true"}}"#;
    let c = only(&map(&one("mysql", "mysql5", cfg))).properties.clone();
    assert!(c.is_empty());
    // LOCAL depends on the machine.
    let cfg = r#"{"host":"db.example.test","properties":{"connectionTimeZone":"LOCAL",
        "forceConnectionTimeZoneToSession":"true"}}"#;
    let c = only(&map(&one("mysql", "mysql8", cfg))).properties.clone();
    assert!(c.is_empty());
}

#[test]
fn mariadb_connector_j_properties() {
    let cfg = r#"{"host":"db.example.test","properties":{
        "sessionVariables":"max_statement_time=2.5,innodb_lock_wait_timeout=20,tx_read_only=1",
        "timezone":"UTC","connectionTimeZone":"UTC"}}"#;
    let p = map(&one("mysql", "mariaDB", cfg));
    let c = only(&p);
    assert_eq!(
        props(c),
        [
            ("innodb_lock_wait_timeout", "20"),
            ("max_statement_time", "2.5")
        ]
    );
    assert_eq!(c.read_only, Some(true));
    // MariaDB Connector/J's own time zone options are not mapped (semantics not verified).
    let d = dropped(c);
    assert!(d.contains(&("configuration.properties.timezone", NoEquivalent)));
    assert!(d.contains(&("configuration.properties.connectionTimeZone", NoEquivalent)));
}

#[test]
fn mysql_values_are_checked_like_mysqlwire() {
    let cfg = r#"{"host":"db.example.test","properties":{
        "sessionVariables":"lock_wait_timeout=-1,group_concat_max_len=1e3,time_zone=''"}}"#;
    let p = map(&one("mysql", "mysql8", cfg));
    let c = only(&p);
    assert!(c.properties.is_empty(), "{:?}", c.properties);
    assert_eq!(
        dropped(c),
        [
            (
                "configuration.properties.sessionVariables.lock_wait_timeout",
                InvalidValue
            ),
            (
                "configuration.properties.sessionVariables.group_concat_max_len",
                InvalidValue
            ),
            (
                "configuration.properties.sessionVariables.time_zone",
                InvalidValue
            ),
        ]
    );
}

#[test]
fn sqlite_jdbc_properties() {
    let cfg = r#"{"database":"/home/nine_user/data/fixture.sqlite","properties":{
        "busy_timeout":"2500","cache_size":"-4096","foreign_keys":"true",
        "enable_load_extension":"true","journal_mode":"WAL","date_class":"TEXT"}}"#;
    let p = map(&one("sqlite", "sqlite_jdbc", cfg));
    let c = only(&p);
    assert_eq!(
        props(c),
        [
            ("busy_timeout", "2500"),
            ("cache_size", "-4096"),
            ("foreign_keys", "true")
        ]
    );
    assert_eq!(
        dropped(c)
            .iter()
            .map(|(k, _)| *k)
            .collect::<std::collections::BTreeSet<_>>(),
        [
            "configuration.properties.date_class",
            "configuration.properties.enable_load_extension",
            "configuration.properties.journal_mode",
        ]
        .into()
    );
    // A page-count cache size cannot be converted without the file's page size.
    let cfg =
        r#"{"database":"/home/nine_user/data/fixture.sqlite","properties":{"cache_size":"2000"}}"#;
    let c = only(&map(&one("sqlite", "sqlite_jdbc", cfg)))
        .dropped_properties
        .clone();
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].reason, InvalidValue);
}

#[test]
fn engines_without_a_nine_driver_keep_their_properties() {
    let cfg =
        r#"{"host":"ch.example.test","properties":{"socket_timeout":"30000","compress":"1"}}"#;
    let p = map(&one("clickhouse", "com_clickhouse", cfg));
    let c = only(&p);
    assert_eq!(
        c.properties.keys().map(String::as_str).collect::<Vec<_>>(),
        ["compress", "socket_timeout"]
    );
    assert!(c.dropped_properties.is_empty());
}

/// Report entries and `Debug` output name keys, never values (I1).
#[test]
fn dropped_values_are_never_echoed() {
    let marker = "fixture-drop-marker-value";
    for (provider, driver, cfg) in [
        (
            "postgresql",
            "postgres-jdbc",
            format!(
                r#"{{"host":"db.example.test","properties":{{"ApplicationName":"{marker}",
                "options":"-c geqo={marker} -c {marker}","currentSchema":"app,pg_catalog,{marker}"}}}}"#
            ),
        ),
        (
            "mysql",
            "mysql8",
            format!(
                r#"{{"host":"db.example.test","properties":{{"serverTimezone":"{marker}",
                "sessionVariables":"sql_mode='{marker}',{marker},lock_wait_timeout={marker}"}}}}"#
            ),
        ),
        (
            "sqlite",
            "sqlite_jdbc",
            format!(
                r#"{{"database":"/home/nine_user/data/fixture.sqlite","properties":{{"busy_timeout":"{marker}","date_class":"{marker}"}}}}"#
            ),
        ),
    ] {
        let p = map(&one(provider, driver, &cfg));
        let c = only(&p);
        assert!(!c.dropped_properties.is_empty(), "{provider}");
        assert!(
            c.properties.values().all(|v| !v.contains(marker)),
            "{provider}"
        );
        for i in &p.connections[0].issues {
            assert!(!i.to_string().contains(marker), "{provider}: {i}");
        }
        assert!(!format!("{p:?}").contains(marker), "{provider}");
    }
}

/// DBeaver provider, DBeaver driver, engine, NineSQL driver, `configuration` body.
type Case = (
    &'static str,
    &'static str,
    EngineId,
    Box<dyn Driver>,
    &'static str,
);

/// Every property name the import can produce is on the driver's declared allowlist
/// (`ConnectCapabilities::accepted_properties`) and not reserved, so the core's
/// `validate_for_driver` and the driver's connect accept it.
#[test]
fn produced_names_are_on_the_driver_allowlists() {
    const PG_ALL: &str = r#"{"host":"db.example.test","properties":{"currentSchema":"app",
            "options":"-c TimeZone=UTC -c statement_timeout=1 -c lock_timeout=1 -c work_mem=4MB -c client_min_messages=warning"}}"#;
    const MYSQL_ALL: &str = r#"{"host":"db.example.test","properties":{"connectionTimeZone":"UTC",
            "forceConnectionTimeZoneToSession":"true",
            "sessionVariables":"lock_wait_timeout=1,innodb_lock_wait_timeout=1,group_concat_max_len=1,max_execution_time=1"}}"#;
    let pg = || Box::new(ninesql_driver_pgwire::PgWireDriver::default());
    let my = || Box::new(ninesql_driver_mysqlwire::MySqlWireDriver::default());
    let cases: [Case; 8] = [
        (
            "postgresql",
            "postgres-jdbc",
            EngineId::POSTGRESQL,
            pg(),
            PG_ALL,
        ),
        (
            "postgresql",
            "postgres-timescale-jdbc",
            EngineId::TIMESCALEDB,
            pg(),
            PG_ALL,
        ),
        (
            "postgresql",
            "postgres-yugabytedb-jdbc",
            EngineId::YUGABYTEDB,
            pg(),
            PG_ALL,
        ),
        // QuestDB accepts no property: nothing to produce.
        (
            "questdb",
            "questdb",
            EngineId::QUESTDB,
            pg(),
            r#"{"host":"db.example.test","properties":{}}"#,
        ),
        ("mysql", "mysql8", EngineId::MYSQL, my(), MYSQL_ALL),
        ("tidb", "tidb", EngineId::TIDB, my(), MYSQL_ALL),
        (
            "mysql",
            "mariaDB",
            EngineId::MARIADB,
            Box::new(ninesql_driver_mysqlwire::MySqlWireDriver::default()),
            r#"{"host":"db.example.test","properties":{
            "sessionVariables":"time_zone='UTC',lock_wait_timeout=1,innodb_lock_wait_timeout=1,group_concat_max_len=1,max_statement_time=1.5"}}"#,
        ),
        (
            "sqlite",
            "sqlite_jdbc",
            EngineId::SQLITE,
            Box::new(ninesql_driver_sqlite::SqliteDriver::default()),
            r#"{"database":"/home/nine_user/data/fixture.sqlite","properties":{
            "busy_timeout":"1","cache_size":"-1","foreign_keys":"false"}}"#,
        ),
    ];
    for (provider, driver_id, engine, driver, cfg) in cases {
        let caps = driver
            .capabilities(&engine, None)
            .unwrap_or_else(|e| panic!("{engine}: {e}"));
        let accepted = caps
            .connect
            .accepted_properties
            .clone()
            .unwrap_or_else(|| panic!("{engine}: no allowlist"));
        let p = map(&one(provider, driver_id, cfg));
        let c = only(&p);
        assert!(
            c.dropped_properties.is_empty(),
            "{engine}: {:?}",
            c.dropped_properties
        );
        // Every name of the allowlist is produced by the case above.
        assert_eq!(
            c.properties
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            accepted,
            "{engine}"
        );
        for key in c.properties.keys() {
            assert!(!caps.is_reserved_property(key), "{engine}: {key}");
        }
        let values: BTreeMap<_, _> = c
            .properties
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    ninesql_driver_api::PropertyValue::Plain(v.clone()),
                )
            })
            .collect();
        caps.check_properties(&values)
            .unwrap_or_else(|e| panic!("{engine}: {e}"));
    }
}

/// Which source wins when a URL parameter and a driver property have the same key, per JDBC
/// driver (pgjdbc and MariaDB Connector/J: the URL; MySQL Connector/J and sqlite-jdbc: the
/// property).
#[test]
fn url_versus_property_precedence_follows_the_jdbc_driver() {
    let cases = [
        (
            "mysql",
            "mysql8",
            r#"{"url":"jdbc:mysql://db.example.test/nine?sessionVariables=lock_wait_timeout%3D1",
            "properties":{"sessionVariables":"lock_wait_timeout=2"}}"#,
            ("lock_wait_timeout", "2"),
            "configuration.url.sessionVariables",
        ),
        (
            "mysql",
            "mariaDB",
            r#"{"url":"jdbc:mariadb://db.example.test/nine?sessionVariables=lock_wait_timeout%3D1",
            "properties":{"sessionVariables":"lock_wait_timeout=2"}}"#,
            ("lock_wait_timeout", "1"),
            "configuration.properties.sessionVariables",
        ),
        (
            "sqlite",
            "sqlite_jdbc",
            r#"{"url":"jdbc:sqlite:/home/nine_user/data/fixture.sqlite?busy_timeout=1",
            "properties":{"busy_timeout":"2"}}"#,
            ("busy_timeout", "2"),
            "configuration.url.busy_timeout",
        ),
    ];
    for (provider, driver, cfg, kept, loser) in cases {
        let p = map(&one(provider, driver, cfg));
        let c = only(&p);
        assert_eq!(props(c), [kept], "{driver}");
        assert!(
            dropped(c).contains(&(loser, NoEquivalent)),
            "{driver}: {:?}",
            dropped(c)
        );
        assert!(
            p.connections[0].issues.iter().any(|i| {
                i.key.as_deref() == Some(loser) && i.message.contains("different value")
            }),
            "{driver}"
        );
    }
    // Different keys, same NineSQL property, different sources: pgjdbc's URL `currentSchema`
    // beats `options` in the properties anyway (dedicated property), with a conflict note.
    let cfg = r#"{"url":"jdbc:postgresql://db.example.test/nine?options=-c%20search_path%3Durl_schema",
        "properties":{"currentSchema":"prop_schema"}}"#;
    let p = map(&one("postgresql", "postgres-jdbc", cfg));
    assert_eq!(props(only(&p)), [("search_path", "prop_schema")]);
    assert!(p.connections[0].issues.iter().any(|i| {
        i.key.as_deref() == Some("configuration.url.options.search_path")
            && i.message.contains("different values")
    }));
}

fn read_only_of(provider: &str, driver: &str, props_json: &str) -> Option<bool> {
    let cfg = format!(r#"{{"host":"db.example.test","properties":{props_json}}}"#);
    only(&map(&one(provider, driver, &cfg))).read_only
}

/// A read-only intent is never lost: PostgreSQL boolean prefixes, `-` in long names, combined
/// switches, values other than an explicit false, unreadable or late entries.
#[test]
fn read_only_intent_is_never_lost() {
    let pg = |options: &str| {
        read_only_of(
            "postgresql",
            "postgres-jdbc",
            &format!(r#"{{"options":"{options}"}}"#),
        )
    };
    for on in [
        "-c default_transaction_read_only=t",
        "-c default_transaction_read_only=TR",
        "-c default_transaction_read_only=ye",
        "-c default_transaction_read_only=1",
        "-c default_transaction_read_only=o",
        "-c default_transaction_read_only=maybe",
        "--default-transaction-read-only=on",
        "-c default-transaction-read-only=on",
        "-ec default_transaction_read_only=on",
        "-ecdefault_transaction_read_only=on",
        "-c transaction_read_only=on",
        // No value: unreadable, but it names read-only.
        "-c default_transaction_read_only",
        "-c default_transaction_read_only=",
    ] {
        assert_eq!(pg(on), Some(true), "{on}");
    }
    for off in [
        "-c default_transaction_read_only=of",
        "-c default_transaction_read_only=f",
        "-c default_transaction_read_only=No",
        "-c default_transaction_read_only=0",
    ] {
        assert_eq!(pg(off), None, "{off}");
    }
    // Past the entry limit.
    let many = format!(
        "{} -c default_transaction_read_only=on",
        "-c geqo=off ".repeat(70)
    );
    assert_eq!(pg(&many), Some(true));
    let my = |vars: &str| {
        read_only_of(
            "mysql",
            "mysql8",
            &format!(r#"{{"sessionVariables":"{vars}"}}"#),
        )
    };
    assert_eq!(my("tx_read_only=1"), Some(true));
    assert_eq!(my("transaction_read_only=yes_please"), Some(true));
    assert_eq!(my("innodb_read_only=1"), Some(true));
    assert_eq!(my("transaction_read_only"), Some(true));
    assert_eq!(my("transaction_read_only=OFF"), None);
    assert_eq!(my("tx_read_only='0'"), None);
}

/// Token-shaped strings are never echoed as setting names (L2).
#[test]
fn token_like_names_are_not_echoed() {
    let tokens = ["c2VjcmV0VG9rZW4xMjM0NTY3OA", "dGhpc2lzYXNlY3JldDEyMzQ"];
    for (provider, driver, props_json) in [
        (
            "postgresql",
            "postgres-jdbc",
            format!(r#"{{"options":"-c {}== -c {}=x"}}"#, tokens[0], tokens[1]),
        ),
        (
            "mysql",
            "mysql8",
            format!(
                r#"{{"sessionVariables":"{}==,{}=x"}}"#,
                tokens[0], tokens[1]
            ),
        ),
    ] {
        let cfg = format!(r#"{{"host":"db.example.test","properties":{props_json}}}"#);
        let p = map(&one(provider, driver, &cfg));
        let c = only(&p);
        assert!(!c.dropped_properties.is_empty(), "{driver}");
        let shown = format!("{p:?}")
            + &p.connections[0]
                .issues
                .iter()
                .map(ToString::to_string)
                .collect::<String>();
        for t in tokens {
            assert!(!shown.contains(t), "{driver}: {t} echoed");
        }
    }
}

/// The list of dropped properties is bounded per connection (L3).
#[test]
fn dropped_properties_are_capped() {
    let props_json = (0..1100)
        .map(|i| format!(r#""fixtureProp{i}":"v""#))
        .collect::<Vec<_>>()
        .join(",");
    let cfg = format!(r#"{{"host":"db.example.test","properties":{{{props_json}}}}}"#);
    let p = map(&one("postgresql", "postgres-jdbc", &cfg));
    let c = only(&p);
    assert_eq!(c.dropped_properties.len(), 1000);
    assert!(p.connections[0].issues.iter().any(|i| {
        i.kind == IssueKind::Truncated && i.message.starts_with("100 more dropped properties")
    }));
}
