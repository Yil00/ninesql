//! Behavior on inputs the P0-E fixtures do not cover: synthetic `data-sources.json` snippets
//! (fake hosts under `example.test`, made-up secrets) and credential files built with the
//! test-only encryption helper.

mod common;

use ninesql_dbeaver::credentials::CredentialsError;
use ninesql_dbeaver::mapping::{
    Auth, CredentialsStatus, Endpoint, ImportedConnection, ProjectImport, SecretState, SshAuth,
    Status, TlsMode, map_sources,
};
use ninesql_dbeaver::{
    IssueKind, Project, decrypt_credentials, import_project, open_folder, parse_data_sources,
};
use ninesql_driver_api::EngineId;

fn map(json: &str, creds: Option<&[u8]>) -> ProjectImport {
    let ds = parse_data_sources(json.as_bytes()).unwrap_or_else(|e| panic!("parse: {e}"));
    map_sources("General", &[ds], creds.map(decrypt_credentials))
}

/// One connection with the given provider, driver and `configuration` body.
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

fn issue(p: &ProjectImport, kind: IssueKind, key: Option<&str>) -> bool {
    p.issues
        .iter()
        .chain(p.connections.iter().flat_map(|c| &c.issues))
        .any(|i| {
            i.kind == kind && key.is_none_or(|k| i.key.as_deref().is_some_and(|x| x.contains(k)))
        })
}

const CREDS: &[u8] =
    br##"{"c-1":{"#connection":{"user":"nine_user","password":"fixture-edge-not-a-secret"}}}"##;

#[test]
fn pro_master_password_file_is_detected_and_skipped() {
    let json = one(
        "postgresql",
        "postgres-jdbc",
        r#"{"host":"db.example.test","port":"5432"}"#,
    )
    .replace(r#""name":"c","#, r#""name":"c","save-password":true,"#);
    let protected = common::encrypt_with_key(CREDS, [0x42; 16], [3; 16]);
    let p = map(&json, Some(&protected));
    assert_eq!(
        p.credentials,
        CredentialsStatus::Unreadable(CredentialsError::NotCommunityFormat)
    );
    assert_eq!(p.secret_count(), 0);
    assert!(issue(
        &p,
        IssueKind::Credentials,
        Some("credentials-config.json")
    ));
    let c = only(&p);
    assert!(matches!(
        c.auth,
        Auth::Password {
            secret: SecretState::Prompt,
            ..
        }
    ));
    assert!(
        p.connections[0]
            .issues
            .iter()
            .any(|i| i.kind == IssueKind::Credentials)
    );

    // Truncated / random files are rejected the same way.
    assert_eq!(
        decrypt_credentials(&protected[..protected.len() - 1]).err(),
        Some(CredentialsError::NotCommunityFormat)
    );
    // Right key, but the content is not JSON.
    let not_json = common::encrypt_like_dbeaver(b"\x00\x01binary", [5; 16]);
    assert_eq!(
        decrypt_credentials(&not_json).err(),
        Some(CredentialsError::NotJson)
    );
}

#[test]
fn saved_credentials_are_matched_and_leftovers_reported() {
    let json = one(
        "postgresql",
        "postgres-jdbc",
        r#"{"host":"db.example.test"}"#,
    );
    let creds = br##"{"c-1":{"#connection":{"user":"nine_user","password":"fixture-x-not-a-secret"}},"ghost":{"#connection":{"user":"u"}}}"##;
    let p = map(&json, Some(&common::encrypt_like_dbeaver(creds, [9; 16])));
    assert_eq!(p.secret_count(), 1);
    assert!(
        matches!(&only(&p).auth, Auth::Password { username, secret: SecretState::Imported } if username == "nine_user")
    );
    assert!(
        p.issues
            .iter()
            .any(|i| i.kind == IssueKind::Credentials && i.message.contains("ghost"))
    );
}

#[test]
fn unsupported_network_handlers_disable_the_connection() {
    for handler in [
        r#""socks_proxy":{"type":"PROXY","enabled":true,"properties":{"host":"proxy.example.test"}}"#,
        r#""custom_tunnel":{"type":"TUNNEL","enabled":true,"properties":{}}"#,
    ] {
        let cfg = format!(r#"{{"host":"db.example.test","handlers":{{{handler}}}}}"#);
        let p = map(&one("postgresql", "postgres-jdbc", &cfg), None);
        assert_eq!(p.connections[0].status(), Status::ImportedDisabled);
        assert!(issue(&p, IssueKind::Disabled, None));
    }
    // A disabled handler is not used by DBeaver and not imported.
    let cfg =
        r#"{"host":"db.example.test","handlers":{"socks_proxy":{"type":"PROXY","enabled":false}}}"#;
    let p = map(&one("postgresql", "postgres-jdbc", cfg), None);
    assert_eq!(p.connections[0].status(), Status::Imported);
}

#[test]
fn url_mode_mysql_with_tls_parameters() {
    let cfg = r#"{"configurationType":"URL",
        "url":"jdbc:mysql://db1.example.test:3307,db2.example.test/app?sslMode=VERIFY_IDENTITY&connectTimeout=10&allowLoadLocalInfile=true",
        "host":"ignored.example.test"}"#;
    let p = map(&one("mysql", "mysql8", cfg), None);
    let c = only(&p);
    assert_eq!(
        c.endpoint,
        Endpoint::Tcp {
            host: "db1.example.test".to_owned(),
            port: Some(3307),
            database: Some("app".to_owned()),
        }
    );
    assert_eq!(c.tls.mode, TlsMode::VerifyFull);
    assert_eq!(
        c.properties.keys().map(String::as_str).collect::<Vec<_>>(),
        ["connectTimeout"]
    );
    assert!(issue(
        &p,
        IssueKind::ReservedProperty,
        Some("allowLoadLocalInfile")
    ));
    assert!(issue(&p, IssueKind::Unsupported, Some("configuration.url")));
}

#[test]
fn tls_modes_and_caps() {
    let cases = [
        (r#"{"sslmode":"disable"}"#, TlsMode::Disable),
        (r#"{"sslmode":"prefer"}"#, TlsMode::Require),
        (r#"{"sslmode":"verify-ca"}"#, TlsMode::VerifyCa),
        (r#"{"ssl":"true"}"#, TlsMode::VerifyFull),
        (r#"{"useSSL":"false"}"#, TlsMode::Disable),
        (
            r#"{"sslMode":"VERIFY_CA","verifyServerCertificate":"false"}"#,
            TlsMode::Require,
        ),
        (r#"{"trustServerCertificate":true}"#, TlsMode::Require),
    ];
    for (props, expected) in cases {
        let cfg = format!(r#"{{"host":"db.example.test","properties":{props}}}"#);
        let p = map(&one("postgresql", "postgres-jdbc", &cfg), None);
        let c = only(&p);
        assert_eq!(c.tls.mode, expected, "{props}");
        assert!(
            c.properties.is_empty(),
            "{props}: TLS keys must not stay properties"
        );
        assert_eq!(
            expected != TlsMode::VerifyFull,
            issue(&p, IssueKind::TlsNotVerified, None)
        );
    }
    // A TLS handler wins over properties.
    let cfg = r#"{"host":"db.example.test","properties":{"sslmode":"disable"},
        "handlers":{"postgre_ssl":{"type":"CONFIG","enabled":true,"properties":{"sslMode":"verify-full"}}}}"#;
    assert_eq!(
        only(&map(&one("postgresql", "postgres-jdbc", cfg), None))
            .tls
            .mode,
        TlsMode::VerifyFull
    );
}

#[test]
fn reserved_secret_and_readonly_properties() {
    let cfg = r#"{"host":"db.example.test","properties":{
        "readOnly":"true","sessionVariables":"sql_mode=''","password":"fixture-p-not-a-secret",
        "user":"nine_user","ApplicationName":"app","weird":{"a":1}}}"#;
    let p = map(&one("mysql", "mariaDB", cfg), None);
    let c = only(&p);
    assert_eq!(c.read_only, Some(true));
    assert_eq!(
        c.properties.keys().map(String::as_str).collect::<Vec<_>>(),
        ["ApplicationName"]
    );
    assert!(issue(
        &p,
        IssueKind::ReservedProperty,
        Some("sessionVariables")
    ));
    assert!(issue(&p, IssueKind::SecretRemoved, Some("password")));
    assert!(issue(&p, IssueKind::InvalidValue, Some("weird")));
    assert!(
        matches!(&c.auth, Auth::Password { username, secret: SecretState::Prompt } if username == "nine_user")
    );
}

#[test]
fn ssh_variants() {
    // Numeric ports (int and float), agent auth.
    for port in ["22", "2200.0"] {
        let cfg = format!(
            r#"{{"host":"db.example.test","handlers":{{"ssh_tunnel":{{"type":"TUNNEL","enabled":true,
            "properties":{{"host":"bastion.example.test","port":{port},"authType":"AGENT","user":"nine_ssh"}}}}}}}}"#
        );
        let p = map(&one("postgresql", "postgres-jdbc", &cfg), None);
        let ssh = only(&p).ssh.clone().unwrap_or_else(|| panic!("no ssh"));
        assert_eq!(ssh.server.auth, SshAuth::Agent);
        assert_eq!(ssh.server.port, if port == "22" { 22 } else { 2200 });
    }
    // No user name: cannot be imported (connecting without the tunnel would be wrong).
    let cfg = r#"{"host":"db.example.test","handlers":{"ssh_tunnel":{"type":"TUNNEL","enabled":true,
        "properties":{"host":"bastion.example.test"}}}}"#;
    let p = map(&one("postgresql", "postgres-jdbc", cfg), None);
    assert_eq!(p.connections[0].status(), Status::Failed);
    // Host key bypass settings are reported, never honored.
    let cfg = r#"{"host":"db.example.test","handlers":{"ssh_tunnel":{"type":"TUNNEL","enabled":true,
        "user":"nine_ssh","properties":{"host":"bastion.example.test","bypassHostVerification":"true"}}}}"#;
    let p = map(&one("postgresql", "postgres-jdbc", cfg), None);
    assert!(issue(
        &p,
        IssueKind::Unsupported,
        Some("bypassHostVerification")
    ));
    assert!(issue(&p, IssueKind::SshHostKeyToConfirm, None));
}

#[test]
fn legacy_plaintext_secrets_are_moved_and_flagged() {
    let cfg =
        r#"{"host":"db.example.test","user":"nine_user","password":"fixture-legacy-not-a-secret"}"#;
    let p = map(&one("postgresql", "postgres-jdbc", cfg), None);
    assert_eq!(p.secret_count(), 1);
    assert!(issue(
        &p,
        IssueKind::PlaintextSecret,
        Some("configuration.password")
    ));
}

#[test]
fn failures_are_per_connection() {
    let json = r#"{"connections":{
        "a":{"provider":"sqlite","driver":"sqlite_jdbc","name":"mem","configuration":{"database":":memory:"}},
        "b":{"provider":"postgresql","driver":"postgres-jdbc","name":"nohost","configuration":{}},
        "c":{"provider":"postgresql","driver":"postgres-jdbc","name":"badhost","configuration":{"host":"u@h"}},
        "d":{"provider":"sqlite","driver":"sqlite_jdbc","name":"url","configuration":{"url":"jdbc:sqlite:/home/nine_user/x.db"}},
        "e":{"provider":"acme","driver":"acme-jdbc","configuration":{"host":"db.example.test","type":"Custom Type"}}
    }}"#;
    let p = map(json, None);
    let status: Vec<_> = p
        .connections
        .iter()
        .map(|c| (c.dbeaver_id.as_str(), c.status()))
        .collect();
    assert_eq!(
        status,
        [
            ("a", Status::Failed),
            ("b", Status::Failed),
            ("c", Status::Failed),
            ("d", Status::Imported),
            ("e", Status::ImportedDisabled),
        ]
    );
    let e = p.connections[4].imported().unwrap_or_else(|| panic!("e"));
    assert_eq!(e.name, "e", "missing name falls back to the id");
    assert_eq!(e.engine.as_str(), "acme");
    assert_eq!(e.connection_type, "custom-type");
    assert!(
        p.connections[4]
            .issues
            .iter()
            .any(|i| i.message.contains("production rules"))
    );
    assert_eq!(
        p.connections[3].imported().map(|c| c.engine.clone()),
        Some(EngineId::SQLITE)
    );
}

#[test]
fn folders_cycles_and_unknown_keys() {
    let json = r#"{"folders":{"A":{"parent":"B"},"B":{"parent":"A"},"C":{"parent":"C"},"X/Y":{"color":"red"}},
        "connections":{"k":{"provider":"postgresql","driver":"postgres-jdbc","name":"k","folder":"/X//Y/",
        "future-flag":true,"configuration":{"host":"db.example.test","new-setting":1,
        "bootstrap":{"txnIsolation":2,"query":["SELECT 1", 5, " "]}}}},
        "network-profiles":{},"something-new":[]}"#;
    let p = map(json, None);
    let c = only(&p);
    assert_eq!(c.folder, ["X", "Y"]);
    assert_eq!(c.init_sql, ["SELECT 1"]);
    assert!(p.folders.iter().any(|f| f.path == ["X", "Y"]));
    assert!(issue(&p, IssueKind::UnknownKey, Some("folders.X/Y.color")));
    assert!(issue(&p, IssueKind::UnknownKey, Some("future-flag")));
    assert!(issue(
        &p,
        IssueKind::UnknownKey,
        Some("configuration.new-setting")
    ));
    assert!(issue(&p, IssueKind::UnknownKey, Some("something-new")));
    assert!(issue(&p, IssueKind::Unsupported, Some("network-profiles")));
    assert!(issue(&p, IssueKind::Unsupported, Some("txnIsolation")));
    assert!(issue(&p, IssueKind::InvalidValue, Some("bootstrap.query")));
}

#[test]
fn several_data_source_files_and_an_invalid_one() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let first = one(
        "postgresql",
        "postgres-jdbc",
        r#"{"host":"one.example.test"}"#,
    );
    let dir = common::write_project(tmp.path(), "General", &first, Some(b"short"))
        .unwrap_or_else(|e| panic!("{e}"));
    let dot = dir.join(".dbeaver");
    let second = one(
        "postgresql",
        "postgres-jdbc",
        r#"{"host":"two.example.test"}"#,
    )
    .replace("c-1", "c-2")
    .replace(r#""c-2":"#, r#""c-1":{"provider":"x"},"c-2":"#);
    std::fs::write(dot.join("data-sources-2.json"), second).unwrap_or_default();
    std::fs::write(dot.join("data-sources-3.json"), "{ broken").unwrap_or_default();

    let ws = open_folder(&dir).unwrap_or_else(|e| panic!("{e}"));
    let project: &Project = &ws.projects[0];
    assert_eq!(project.data_source_files.len(), 3);
    assert!(project.data_source_files[0].ends_with("data-sources.json"));
    let p = import_project(project);
    assert_eq!(p.connections.len(), 2);
    assert_eq!(p.source_files.len(), 2);
    assert!(issue(&p, IssueKind::File, None));
    assert!(issue(&p, IssueKind::InvalidValue, Some("connections.c-1")));
    assert_eq!(
        p.credentials,
        CredentialsStatus::Unreadable(CredentialsError::NotCommunityFormat)
    );
    let host = |i: usize| match &p.connections[i].imported().map(|c| c.endpoint.clone()) {
        Some(Endpoint::Tcp { host, .. }) => host.clone(),
        _ => String::new(),
    };
    assert_eq!(
        (host(0), host(1)),
        ("one.example.test".to_owned(), "two.example.test".to_owned())
    );
}
