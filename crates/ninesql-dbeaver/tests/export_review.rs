//! P5-D security review fixes (M1–M3, L2–L7): URL-unsafe endpoints, MySQL-family
//! `verify-full`, read-only connections, DBeaver markers, type ids, redacted keys, unconfirmed
//! init SQL.

// Test code: a failed unwrap is a failed test (AGENTS.md allows it outside library code).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use ninesql_dbeaver::export::{
    ExportAuth, ExportConnection, ExportConnectionType, ExportEndpoint, ExportIssueKind,
    ExportOptions, ExportTarget, ExportTls, ExportWriteError, Rgb, Severity, TlsMode,
};
use ninesql_dbeaver::{export_connections, mapping, parse_data_sources, write_export};
use ninesql_driver_api::EngineId;
use serde_json::Value;

fn conn(name: &str, engine: &str) -> ExportConnection {
    ExportConnection {
        id: name.to_owned(),
        name: name.to_owned(),
        description: None,
        engine: EngineId::new(engine.to_owned()),
        endpoint: ExportEndpoint::Tcp {
            host: "db.example.test".to_owned(),
            port: Some(5432),
            database: Some("nine".to_owned()),
        },
        auth: ExportAuth::Password {
            username: "nine_user".to_owned(),
            saved: false,
        },
        scram_only: false,
        ssh: None,
        tls: ExportTls::default(),
        connection_type: "dev".to_owned(),
        read_only: None,
        folder: Vec::new(),
        properties: BTreeMap::new(),
        init_sql: Vec::new(),
        init_sql_needs_confirmation: false,
        default_schema: None,
        default_catalog: None,
        auto_commit: None,
        disabled: None,
    }
}

fn opts() -> ExportOptions {
    ExportOptions {
        connection_types: vec![ExportConnectionType {
            id: "staging".to_owned(),
            name: "Staging".to_owned(),
            color: Rgb { r: 1, g: 2, b: 3 },
            auto_commit: true,
            confirm_destructive: false,
            confirm_every_write: false,
            read_only: false,
            log_query_text: true,
            production: false,
        }],
        timestamp_millis: Some(1),
    }
}

fn only_connection(json: &[u8]) -> Value {
    let v: Value = serde_json::from_slice(json).unwrap();
    let conns = v["connections"].as_object().unwrap();
    assert_eq!(conns.len(), 1);
    conns.values().next().unwrap().clone()
}

#[test]
fn m1_url_syntax_in_host_database_or_path_is_skipped() {
    let mut cases = Vec::new();
    for db in [
        "app?sslmode=disable",
        "nine&readOnly=false",
        "a;b",
        "x#y",
        "p%3F",
    ] {
        let mut c = conn(db, "postgresql");
        c.endpoint = ExportEndpoint::Tcp {
            host: "db.example.test".to_owned(),
            port: None,
            database: Some(db.to_owned()),
        };
        cases.push(c);
    }
    for host in [
        "db.example.test?ssl=false",
        "db/x",
        "db\\x",
        "db x",
        "db%2e",
        "::1",
        "fe80::1",
    ] {
        let mut c = conn(host, "mariadb");
        c.endpoint = ExportEndpoint::Tcp {
            host: host.to_owned(),
            port: None,
            database: None,
        };
        cases.push(c);
    }
    let mut file = conn("file", "sqlite");
    file.endpoint = ExportEndpoint::File {
        path: PathBuf::from("/srv/data/x.sqlite?mode=rw"),
    };
    file.auth = ExportAuth::None;
    cases.push(file);
    let export = export_connections(&cases, &opts()).unwrap();
    assert_eq!(export.report.exported_count(), 0);
    for c in &export.report.connections {
        assert!(
            c.issues.iter().any(|i| i.kind == ExportIssueKind::Skipped),
            "{}",
            c.name
        );
    }
    let text = String::from_utf8(export.json).unwrap();
    assert!(!text.contains("sslmode=disable") && !text.contains("readOnly"));
}

#[test]
fn m2_mysql_family_verify_full_keeps_a_verifying_handler() {
    for (engine, mode_value, version) in [
        ("mysql", "VERIFY_IDENTITY", "8.0.13"),
        ("mariadb", "verify-full", "3.0"),
    ] {
        let mut c = conn(engine, engine);
        c.tls = ExportTls {
            mode: TlsMode::VerifyFull,
            ca_file: Some(PathBuf::from("/home/nine_user/certs/ca.pem")),
            client_certificate: None,
        };
        let export = export_connections(&[c], &opts()).unwrap();
        let v = only_connection(&export.json);
        let cfg = &v["configuration"];
        assert_eq!(cfg["properties"]["sslMode"], mode_value, "{engine}");
        let h = &cfg["handlers"]["mysql_ssl"];
        assert_eq!(h["enabled"], true);
        assert_eq!(h["properties"]["ssl.require"], true);
        assert_eq!(h["properties"]["ssl.verify.server"], true);
        assert_eq!(
            h["properties"]["ssl.ca.cert"],
            "/home/nine_user/certs/ca.pem"
        );
        let issues = &export.report.connections[0].issues;
        assert!(
            issues
                .iter()
                .any(|i| i.severity == Severity::Warning && i.message.contains(version)),
            "{engine}: {issues:?}"
        );
        // NineSQL's importer keeps the stricter mode of the handler and the property.
        let imp = mapping::map_sources("G", &[parse_data_sources(&export.json).unwrap()], None);
        let back = imp.connections[0].imported().unwrap();
        assert_eq!(back.tls.mode, mapping::TlsMode::VerifyFull, "{engine}");
        assert!(back.tls.ca_file.is_some());
    }
}

#[test]
fn m3_read_only_connections() {
    let mut pg = conn("pg-ro", "postgresql");
    pg.read_only = Some(true);
    pg.init_sql = vec![
        "SELECT 1".to_owned(),
        "SET search_path = app".to_owned(),
        "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE".to_owned(),
        "SELECT set_config('default_transaction_read_only', 'off', false)".to_owned(),
        "DELETE FROM t WHERE id = 1".to_owned(),
    ];
    let mut my = conn("my-ro", "mysql");
    my.read_only = Some(true);
    my.init_sql = vec!["SET SESSION TRANSACTION READ WRITE".to_owned()];
    let mut rw = conn("pg-rw", "postgresql");
    rw.init_sql = vec!["SET search_path = app".to_owned()];
    let export = export_connections(&[pg, my, rw], &opts()).unwrap();
    let v: Value = serde_json::from_slice(&export.json).unwrap();
    let by_name = |n: &str| {
        v["connections"]
            .as_object()
            .unwrap()
            .values()
            .find(|c| c["name"] == n)
            .unwrap()
            .clone()
    };
    let pg = by_name("pg-ro");
    assert_eq!(pg["read-only"], true);
    assert_eq!(pg["configuration"]["properties"]["readOnlyMode"], "always");
    assert_eq!(
        pg["configuration"]["bootstrap"]["query"],
        serde_json::json!(["SELECT 1"])
    );
    let my = by_name("my-ro");
    assert!(my["configuration"].get("bootstrap").is_none());
    assert!(
        my["configuration"]
            .get("properties")
            .is_none_or(|p| p.get("readOnlyMode").is_none())
    );
    // A writable connection keeps its session statements and has no readOnlyMode.
    let rw = by_name("pg-rw");
    assert_eq!(
        rw["configuration"]["bootstrap"]["query"],
        serde_json::json!(["SET search_path = app"])
    );
    assert!(rw.get("read-only").is_none());
    for (i, ro) in [(0, true), (1, true), (2, false)] {
        let warned = export.report.connections[i].issues.iter().any(|x| {
            x.severity == Severity::Warning
                && x.field.as_deref() == Some("read_only")
                && x.message.contains("JDBC driver and its UI")
        });
        assert_eq!(warned, ro, "connection {i}");
    }
    assert!(
        export.report.connections[0]
            .issues
            .iter()
            .any(|x| x.field.as_deref() == Some("init_sql") && x.message.starts_with('4'))
    );
}

#[test]
fn l2_more_dbeaver_markers_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let export = export_connections(&[conn("pg", "postgresql")], &opts()).unwrap();
    let make = |name: &str, setup: &dyn Fn(&std::path::Path)| {
        let d = dir.path().join(name);
        fs::create_dir_all(&d).unwrap();
        setup(&d);
        d
    };
    let nature = make("nature", &|d| {
        fs::write(
            d.join(".project"),
            "<projectDescription><natures><nature>org.jkiss.dbeaver.DBeaverNature</nature>\
             </natures></projectDescription>",
        )
        .unwrap();
    });
    let dot = make("dot", &|d| {
        fs::create_dir(d.join(".dbeaver")).unwrap();
        fs::write(d.join(".dbeaver/anything"), "x").unwrap();
    });
    let legacy = make("legacy", &|d| {
        fs::write(d.join(".dbeaver-data-sources.xml"), "<x/>").unwrap();
    });
    let lower = make("x/dbeaverdata/y", &|_| {});
    for d in [nature, dot, legacy, lower] {
        let err =
            write_export(&export, &ExportTarget::File(d.join("out.json")), &|_| false).unwrap_err();
        assert!(
            matches!(err, ExportWriteError::InsideDbeaver { .. }),
            "{err}"
        );
        assert!(!d.join("out.json").exists());
    }
    // An Eclipse project of another kind and an empty `.dbeaver` folder do not block.
    let other = make("other", &|d| {
        fs::write(d.join(".project"), "<projectDescription/>").unwrap();
        fs::create_dir(d.join(".dbeaver")).unwrap();
    });
    write_export(
        &export,
        &ExportTarget::File(other.join("out.json")),
        &|_| false,
    )
    .unwrap();
}

#[test]
fn l3_custom_type_ids_are_prefixed() {
    let mut c = conn("pg", "postgresql");
    c.connection_type = "staging".to_owned();
    let export = export_connections(&[c], &opts()).unwrap();
    let v: Value = serde_json::from_slice(&export.json).unwrap();
    assert!(v["connection-types"].get("ninesql-staging").is_some());
    assert!(v["connection-types"].get("staging").is_none());
    assert_eq!(
        only_connection(&export.json)["configuration"]["type"],
        "ninesql-staging"
    );
    assert!(export.report.issues.iter().any(|i| {
        i.severity == Severity::Warning && i.message.contains("keeps its own settings")
    }));
}

#[test]
fn l4_every_ssh_tunnel_warns_about_host_keys() {
    use ninesql_dbeaver::export::{
        ExportHostKeyPolicy, ExportSshAuth, ExportSshHop, ExportSshTunnel,
    };
    let hop = |h: &str| ExportSshHop {
        host: h.to_owned(),
        port: 22,
        username: "u".to_owned(),
        auth: ExportSshAuth::Agent,
        host_key_policy: ExportHostKeyPolicy::Strict,
    };
    let mut c = conn("pg", "postgresql");
    c.ssh = Some(ExportSshTunnel {
        server: hop("bastion.example.test"),
        jump_hosts: vec![hop("j1.example.test"), hop("j2.example.test")],
    });
    let export = export_connections(&[c], &opts()).unwrap();
    let issues = &export.report.connections[0].issues;
    assert!(
        issues.iter().any(|i| i.severity == Severity::Warning
            && i.field.as_deref() == Some("ssh.host_key_policy"))
    );
    assert!(
        issues
            .iter()
            .any(|i| i.severity == Severity::Warning && i.message.starts_with("route change"))
    );
}

#[test]
fn l6_sensitive_or_unprintable_property_keys_are_redacted() {
    let mut c = conn("ch", "clickhouse");
    c.properties = BTreeMap::from([
        ("api_token".to_owned(), "abc".to_owned()),
        ("bad\u{7}key".to_owned(), "1".to_owned()),
        ("socket_timeout".to_owned(), "5".to_owned()),
    ]);
    let export = export_connections(&[c], &opts()).unwrap();
    let issues = &export.report.connections[0].issues;
    let fields: Vec<&str> = issues.iter().filter_map(|i| i.field.as_deref()).collect();
    assert!(fields.contains(&"properties.<redacted #1>"));
    assert!(fields.contains(&"properties.<redacted #2>"));
    let shown = format!("{issues:?}");
    assert!(!shown.contains("api_token") && !shown.contains("bad\u{7}key"));
    let props = &only_connection(&export.json)["configuration"]["properties"];
    assert_eq!(props.as_object().unwrap().len(), 1);
}

#[test]
fn l7_unconfirmed_init_sql_is_not_exported() {
    let mut c = conn("pg", "postgresql");
    c.init_sql = vec!["SELECT 1".to_owned()];
    c.init_sql_needs_confirmation = true;
    let export = export_connections(&[c], &opts()).unwrap();
    assert!(
        only_connection(&export.json)["configuration"]
            .get("bootstrap")
            .is_none()
    );
    assert!(export.report.connections[0].issues.iter().any(|i| {
        i.field.as_deref() == Some("init_sql") && i.message.contains("not confirmed")
    }));
}

#[test]
fn la_read_only_init_sql_with_function_calls_is_dropped() {
    let mut c = conn("ts-ro", "timescaledb");
    c.read_only = Some(true);
    c.init_sql = vec![
        "SELECT 1".to_owned(),
        "SELECT drop_chunks('metrics', INTERVAL '1 day')".to_owned(),
        "SELECT \"my_fn\" (1)".to_owned(),
        "SELECT 1 /*T! , 2 */".to_owned(),
        // Parentheses inside a string or a comment are not calls.
        "SELECT 'f(x)' /* g(y) */".to_owned(),
    ];
    let mut my = conn("my-ro", "mysql");
    my.read_only = Some(true);
    my.init_sql = vec!["SELECT GET_LOCK('x', 10)".to_owned()];
    let export = export_connections(&[c, my], &opts()).unwrap();
    let v: Value = serde_json::from_slice(&export.json).unwrap();
    let queries: Vec<Value> = v["connections"]
        .as_object()
        .unwrap()
        .values()
        .map(|c| c["configuration"]["bootstrap"]["query"].clone())
        .collect();
    assert!(queries.contains(&serde_json::json!(["SELECT 1", "SELECT 'f(x)' /* g(y) */"])));
    assert!(
        queries.contains(&Value::Null),
        "the MySQL connection keeps no init SQL"
    );
    assert!(
        export.report.connections[0]
            .issues
            .iter()
            .any(|i| { i.field.as_deref() == Some("init_sql") && i.message.starts_with('3') })
    );
}

#[test]
fn lb_host_lists_and_brackets_are_skipped() {
    let cases: Vec<ExportConnection> = ["db1.example.test,db2.example.test", "[db]", "db]"]
        .iter()
        .map(|h| {
            let mut c = conn(h, "postgresql");
            c.endpoint = ExportEndpoint::Tcp {
                host: (*h).to_owned(),
                port: None,
                database: None,
            };
            c
        })
        .collect();
    let export = export_connections(&cases, &opts()).unwrap();
    assert_eq!(export.report.exported_count(), 0);
}

#[test]
fn lc_sensitive_folder_or_type_skips_only_that_connection() {
    let mut folder = conn("folder", "postgresql");
    folder.folder = vec!["Team".to_owned(), "api_token".to_owned()];
    let mut ty = conn("type", "postgresql");
    ty.connection_type = "secret".to_owned();
    let ok = conn("ok", "postgresql");
    let mut o = opts();
    o.connection_types.push(ExportConnectionType {
        id: "secret".to_owned(),
        ..o.connection_types[0].clone()
    });
    let export = export_connections(&[folder, ty, ok], &o).unwrap();
    let exported: Vec<&str> = export
        .report
        .connections
        .iter()
        .filter(|c| c.exported())
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(exported, ["ok"]);
    let text = String::from_utf8(export.json).unwrap();
    assert!(!text.contains("api_token") && !text.contains("ninesql-secret"));
}
