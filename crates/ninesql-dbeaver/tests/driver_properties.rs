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
            ("search_path", "app,public"),
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
        // URL parameters lose to driver properties.
        ("configuration.url.currentSchema", NoEquivalent),
        ("configuration.url.prepareThreshold", NoEquivalent),
    ] {
        assert!(d.contains(&expected), "{expected:?} not in {d:?}");
    }
    assert!(has_issue(
        &p,
        IssueKind::PropertyMapped,
        "configuration.properties.currentSchema"
    ));
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
    let cases: [Case; 4] = [
        (
            "postgresql",
            "postgres-jdbc",
            EngineId::POSTGRESQL,
            Box::new(ninesql_driver_pgwire::PgWireDriver::default()),
            r#"{"host":"db.example.test","properties":{"currentSchema":"app",
            "options":"-c TimeZone=UTC -c statement_timeout=1 -c lock_timeout=1 -c work_mem=4MB -c client_min_messages=warning"}}"#,
        ),
        (
            "mysql",
            "mysql8",
            EngineId::MYSQL,
            Box::new(ninesql_driver_mysqlwire::MySqlWireDriver::default()),
            r#"{"host":"db.example.test","properties":{"connectionTimeZone":"UTC",
            "forceConnectionTimeZoneToSession":"true",
            "sessionVariables":"lock_wait_timeout=1,innodb_lock_wait_timeout=1,group_concat_max_len=1,max_execution_time=1"}}"#,
        ),
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
