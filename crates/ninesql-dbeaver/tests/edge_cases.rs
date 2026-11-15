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
    IssueKind, Project, PropertyDropReason, decrypt_credentials, import_project, open_folder,
    parse_data_sources,
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
    // Connector/J's connectTimeout has no NineSQL property: dropped, reported.
    assert!(c.properties.is_empty(), "{:?}", c.properties.keys());
    assert!(issue(
        &p,
        IssueKind::PropertyDropped,
        Some("configuration.url.connectTimeout")
    ));
    assert_eq!(
        c.dropped_properties
            .iter()
            .map(|d| (d.key.as_str(), d.reason))
            .collect::<Vec<_>>(),
        [
            (
                "configuration.url.connectTimeout",
                PropertyDropReason::NoEquivalent
            ),
            (
                "configuration.url.allowLoadLocalInfile",
                PropertyDropReason::Reserved
            ),
        ]
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
    // MariaDB Connector/J has no `ApplicationName`; `sql_mode` is not on mysqlwire's allowlist.
    assert!(c.properties.is_empty(), "{:?}", c.properties.keys());
    assert!(issue(
        &p,
        IssueKind::PropertyDropped,
        Some("configuration.properties.ApplicationName")
    ));
    assert!(issue(
        &p,
        IssueKind::PropertyDropped,
        Some("configuration.properties.sessionVariables.sql_mode")
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
fn benign_password_named_keys_are_checked_by_value_shape() {
    let leaked = "fixture-pce-not-a-secret";
    let secret_removed = |p: &ProjectImport, key: &str| {
        p.connections[0]
            .issues
            .iter()
            .any(|i| i.kind == IssueKind::SecretRemoved && i.key.as_deref() == Some(key))
    };
    let no_leak = |p: &ProjectImport| {
        let c = only(p);
        assert!(c.properties.values().all(|v| !v.contains(leaked)));
        assert!(!format!("{p:?}").contains(leaked));
    };
    // Driver property: a misplaced password is removed, a charset name is not flagged.
    let cfg = format!(
        r#"{{"host":"db.example.test","properties":{{"passwordCharacterEncoding":"{leaked}"}}}}"#
    );
    let p = map(&one("mysql", "mysql8", &cfg), None);
    assert!(secret_removed(
        &p,
        "configuration.properties.passwordCharacterEncoding"
    ));
    no_leak(&p);
    let cfg = r#"{"host":"db.example.test","properties":{"passwordCharacterEncoding":"UTF-8"}}"#;
    let p = map(&one("mysql", "mysql8", cfg), None);
    assert!(!issue(&p, IssueKind::SecretRemoved, None));
    // SSH tunnel and TLS handler properties.
    let cfg = format!(
        r#"{{"host":"db.example.test","handlers":{{"ssh_tunnel":{{"type":"TUNNEL","enabled":true,
        "user":"nine_ssh","properties":{{"host":"bastion.example.test","authType":"AGENT",
        "passwordCharacterEncoding":"{leaked}"}}}},
        "postgre_ssl":{{"type":"CONFIG","enabled":true,"properties":{{"sslMode":"verify-full",
        "passwordEncryption":"{leaked}"}}}}}}}}"#
    );
    let p = map(&one("postgresql", "postgres-jdbc", &cfg), None);
    assert!(secret_removed(
        &p,
        "configuration.handlers.ssh_tunnel.properties.passwordCharacterEncoding"
    ));
    assert!(secret_removed(
        &p,
        "configuration.handlers.postgre_ssl.properties.passwordEncryption"
    ));
    no_leak(&p);
    // Unknown key in `configuration`: the value is scrubbed from `extra`, the key still reported.
    let cfg = format!(r#"{{"host":"db.example.test","passwordEncryption":"{leaked}"}}"#);
    let ds = parse_data_sources(one("postgresql", "postgres-jdbc", &cfg).as_bytes())
        .unwrap_or_else(|e| panic!("parse: {e}"));
    assert_eq!(
        ds.connections[0]
            .configuration
            .extra
            .get("passwordEncryption"),
        Some(&serde_json::Value::Null)
    );
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

// ---- Security review fixes (H1, M1, M2, M4, M5, M6, L4, L5) ----

fn status_of(json: &str) -> Status {
    map(json, None).connections[0].status()
}

#[test]
fn network_profile_references_disable_the_connection() {
    for key in ["network-profile", "networkProfile", "network_profile_id"] {
        let cfg = format!(r#"{{"host":"db.example.test","{key}":"bastion-profile"}}"#);
        let p = map(&one("postgresql", "postgres-jdbc", &cfg), None);
        assert_eq!(p.connections[0].status(), Status::ImportedDisabled, "{key}");
        assert!(issue(&p, IssueKind::Disabled, Some(key)), "{key}");
    }
    // With profiles defined in the file, any unrecognized network* / profile* key disables.
    let with_profiles = |cfg: &str, profiles: &str| {
        one("postgresql", "postgres-jdbc", cfg).replacen(
            r#"{"connections""#,
            &format!(r#"{{"network-profiles":{profiles},"connections""#),
            1,
        )
    };
    let cfg = r#"{"host":"db.example.test","profileId":"p1"}"#;
    assert_eq!(
        status_of(&with_profiles(cfg, r#"{"p1":{}}"#)),
        Status::ImportedDisabled
    );
    // Any non-empty *profile* value disables, even without `network-profiles` in the file.
    assert_eq!(
        status_of(&with_profiles(cfg, "{}")),
        Status::ImportedDisabled
    );
    let empty = r#"{"host":"db.example.test","profileId":""}"#;
    assert_eq!(status_of(&with_profiles(empty, "{}")), Status::Imported);
    let cfg = r#"{"host":"db.example.test","networkHandlerRef":"p1"}"#;
    assert_eq!(
        status_of(&with_profiles(cfg, r#"{"p1":{}}"#)),
        Status::ImportedDisabled
    );
}

#[test]
fn handlers_fail_closed() {
    for handler in [
        r#""gcp_iap_tunnel":{"enabled":true}"#,
        r#""aws_ssm":{"enabled":true}"#,
        r#""kubectl_forward":{"enabled":true}"#,
        r#""custom_ssl":{"enabled":true}"#,
        r#""ssl_proxy":{"type":"CONFIG","enabled":true}"#,
        r#""tls_over_ssh":{"type":"CONFIG","enabled":true}"#,
        r#""whatever":{"type":"SOMETHING","enabled":true}"#,
        r#""x":{"type":"CONFIG","enabled":1}"#,
    ] {
        let cfg = format!(r#"{{"host":"db.example.test","handlers":{{{handler}}}}}"#);
        let p = map(&one("postgresql", "postgres-jdbc", &cfg), None);
        assert_eq!(
            p.connections[0].status(),
            Status::ImportedDisabled,
            "{handler}"
        );
        assert!(
            issue(&p, IssueKind::Disabled, Some("configuration.handlers")),
            "{handler}"
        );
    }
    // A TLS-only CONFIG handler stays enabled, with a warning.
    let cfg = r#"{"host":"db.example.test","handlers":{"clickhouse_ssl":{"type":"CONFIG","enabled":true}}}"#;
    let p = map(&one("clickhouse", "com_clickhouse", cfg), None);
    assert_eq!(p.connections[0].status(), Status::Imported);
    assert!(issue(&p, IssueKind::Unsupported, Some("clickhouse_ssl")));
}

#[test]
fn ssh_forwarding_target() {
    let ssh = |remote: &str| {
        format!(
            r#"{{"host":"db.example.test","port":"5432","handlers":{{"ssh_tunnel":{{"type":"TUNNEL",
            "enabled":true,"user":"nine_ssh","properties":{{"host":"bastion.example.test"{remote}}}}}}}}}"#
        )
    };
    let st = |remote: &str| status_of(&one("postgresql", "postgres-jdbc", &ssh(remote)));
    assert_eq!(st(""), Status::Imported);
    assert_eq!(
        st(r#","remoteHost":"DB.example.test","remotePort":"5432""#),
        Status::Imported
    );
    assert_eq!(
        st(r#","remoteHost":"other.example.test""#),
        Status::ImportedDisabled
    );
    assert_eq!(st(r#","remotePort":5433"#), Status::ImportedDisabled);
    assert_eq!(st(r#","remoteHost":"""#), Status::Imported);
}

#[test]
fn file_endpoints_inside_the_workspace_are_read_only() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let ws = tmp.path().join("DBeaverData").join("workspace6");
    std::fs::create_dir_all(ws.join(".metadata")).unwrap_or_default();
    let inside = ws.join("General").join("data.sqlite");
    let outside = tmp.path().join("elsewhere.sqlite");
    let link = tmp.path().join("link.sqlite");
    let json = format!(
        r#"{{"connections":{{
        "in":{{"provider":"sqlite","driver":"sqlite_jdbc","name":"in","configuration":{{"database":"{}"}}}},
        "out":{{"provider":"sqlite","driver":"sqlite_jdbc","name":"out","configuration":{{"database":"{}"}}}},
        "link":{{"provider":"sqlite","driver":"sqlite_jdbc","name":"link","configuration":{{"database":"{}"}}}},
        "rel":{{"provider":"sqlite","driver":"sqlite_jdbc","name":"rel","configuration":{{"database":"data/x.db"}}}},
        "var":{{"provider":"sqlite","driver":"sqlite_jdbc","name":"var","configuration":{{"database":"${{workspace}}/x.db"}}}}
        }}}}"#,
        inside.display(),
        outside.display(),
        link.display()
    );
    let project =
        common::write_project(&ws, "General", &json, None).unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(&inside, b"").unwrap_or_default();
    std::fs::write(&outside, b"").unwrap_or_default();
    std::os::unix::fs::symlink(&inside, &link).unwrap_or_default();

    let picked = open_folder(&project).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(picked.projects[0].workspace, ws);
    // Inside DBeaverData but outside the workspace, and a file that does not exist yet.
    let sibling = tmp.path().join("DBeaverData").join("drivers.sqlite");
    std::fs::write(&sibling, b"").unwrap_or_default();
    let extra = format!(
        r#","sib":{{"provider":"sqlite","driver":"sqlite_jdbc","name":"sib","configuration":{{"database":"{}"}}}},
        "missing":{{"provider":"sqlite","driver":"sqlite_jdbc","name":"missing","configuration":{{"database":"{}"}}}}}}}}"#,
        sibling.display(),
        ws.join("General").join("not-yet.sqlite").display()
    );
    let ds = project.join(".dbeaver").join("data-sources.json");
    let mut text = std::fs::read_to_string(&ds).unwrap_or_default();
    let trimmed = text.trim_end().len();
    text.truncate(trimmed - 2);
    text.push_str(&extra);
    std::fs::write(&ds, text).unwrap_or_default();
    let p = import_project(&picked.projects[0]);
    let get = |id: &str| {
        p.connections
            .iter()
            .find(|c| c.dbeaver_id == id)
            .unwrap_or_else(|| panic!("{id}"))
    };
    for id in ["in", "link", "sib", "missing"] {
        let c = get(id);
        assert_eq!(c.imported().and_then(|c| c.read_only), Some(true), "{id}");
        assert!(
            c.issues
                .iter()
                .any(|i| i.kind == IssueKind::FileInsideDbeaver),
            "{id}"
        );
    }
    // The symlink is canonicalized to the real file.
    let canon = std::fs::canonicalize(&inside).unwrap_or_default();
    assert_eq!(
        get("link").imported().map(|c| c.endpoint.clone()),
        Some(Endpoint::File { path: canon })
    );
    assert_eq!(get("out").imported().and_then(|c| c.read_only), None);
    assert_eq!(get("rel").status(), Status::Failed);
    assert_eq!(get("var").status(), Status::Failed);
}

#[test]
fn per_project_limits() {
    // Data-sources files: at most 64 are read.
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let dir = common::write_project(tmp.path(), "General", r#"{"connections":{}}"#, None)
        .unwrap_or_else(|e| panic!("{e}"));
    for i in 0..70 {
        std::fs::write(
            dir.join(".dbeaver")
                .join(format!("data-sources-{i:02}.json")),
            "{}",
        )
        .unwrap_or_default();
    }
    let ws = open_folder(&dir).unwrap_or_else(|e| panic!("{e}"));
    let p = import_project(&ws.projects[0]);
    assert_eq!(p.source_files.len(), 64);
    assert!(issue(&p, IssueKind::Truncated, None));

    // Connections (10 000) and report entries (50 000).
    let mut json = String::from(r#"{"connections":{"#);
    for i in 0..10_001 {
        if i > 0 {
            json.push(',');
        }
        json.push_str(&format!(
            r#""c{i:05}":{{"provider":"postgresql","driver":"postgres-jdbc","name":"n","u1":1,"u2":1,"u3":1,"u4":1,
            "configuration":{{"host":"db.example.test"}}}}"#
        ));
    }
    json.push_str("}}");
    let p = map(&json, None);
    assert_eq!(p.connections.len(), 10_000);
    let total = p.issues.len() + p.connections.iter().map(|c| c.issues.len()).sum::<usize>();
    assert!(total <= 50_002, "{total}");
    assert_eq!(
        p.issues
            .iter()
            .filter(|i| i.kind == IssueKind::Truncated)
            .count(),
        2
    );
}

#[test]
fn init_statements_are_flagged_for_review() {
    let cfg = r#"{"host":"db.example.test","bootstrap":{"query":["SET a = 1","SET b = 2"]}}"#;
    let p = map(&one("postgresql", "postgres-jdbc", cfg), None);
    assert!(
        p.connections[0]
            .issues
            .iter()
            .any(|i| i.kind == IssueKind::InitSql && i.message.starts_with("2 init statement"))
    );
}

#[test]
fn option_like_hosts_and_users_are_rejected() {
    let cfg = r#"{"host":"-oProxyCommand=x"}"#;
    assert_eq!(
        status_of(&one("postgresql", "postgres-jdbc", cfg)),
        Status::Failed
    );
    let cfg = r#"{"host":"db.example.test","handlers":{"ssh_tunnel":{"type":"TUNNEL","enabled":true,
        "user":"-oProxyCommand=x","properties":{"host":"bastion.example.test"}}}}"#;
    assert_eq!(
        status_of(&one("postgresql", "postgres-jdbc", cfg)),
        Status::Failed
    );
    let cfg = r#"{"host":"db.example.test","handlers":{"ssh_tunnel":{"type":"TUNNEL","enabled":true,
        "user":"nine_ssh","properties":{"host":"-bastion"}}}}"#;
    assert_eq!(
        status_of(&one("postgresql", "postgres-jdbc", cfg)),
        Status::Failed
    );
}

#[test]
fn tls_review_fixes() {
    let cfg = r#"{"host":"db.example.test","properties":{"ssl":"true","verifyServerCertificate":"false"}}"#;
    assert_eq!(
        only(&map(&one("postgresql", "postgres-jdbc", cfg), None))
            .tls
            .mode,
        TlsMode::Require
    );
    // No TLS setting: verify-full, reported as a warning.
    let p = map(
        &one("mysql", "mariaDB", r#"{"host":"db.example.test"}"#),
        None,
    );
    assert_eq!(only(&p).tls.mode, TlsMode::VerifyFull);
    assert!(
        p.connections[0]
            .issues
            .iter()
            .any(|i| i.kind == IssueKind::TlsConverted
                && i.severity == ninesql_dbeaver::Severity::Warning
                && i.message.contains("MariaDB"))
    );
}

// ---- Follow-up review (N-H1, N-H2, N-M1, N-M2, N-L2) ----

#[test]
fn dbeaver_config_profile_keys_disable() {
    for cfg in [
        r#"{"host":"db.example.test","config-profile":"bastion"}"#,
        r#"{"host":"db.example.test","config-profile-source":"global"}"#,
        r#"{"host":"db.example.test","config-profile":"bastion","config-profile-source":"global"}"#,
    ] {
        let p = map(&one("postgresql", "postgres-jdbc", cfg), None);
        assert_eq!(p.connections[0].status(), Status::ImportedDisabled, "{cfg}");
        assert!(
            issue(
                &p,
                IssueKind::Disabled,
                Some("configuration.config-profile")
            ),
            "{cfg}"
        );
    }
    // In a handler too.
    let cfg = r#"{"host":"db.example.test","handlers":{"postgre_ssl":{"type":"CONFIG","enabled":true,
        "config-profile":"tls-profile","properties":{}}}}"#;
    assert_eq!(
        status_of(&one("postgresql", "postgres-jdbc", cfg)),
        Status::ImportedDisabled
    );
}

fn ssh_with(props: &str) -> String {
    one(
        "postgresql",
        "postgres-jdbc",
        &format!(
            r#"{{"host":"db.example.test","handlers":{{"ssh_tunnel":{{"type":"TUNNEL","enabled":true,
            "user":"nine_ssh","properties":{{"host":"bastion.example.test"{props}}}}}}}}}"#
        ),
    )
}

#[test]
fn ssh_jump_servers_are_mapped_or_fail_closed() {
    // Key names as DBeaver's `SSHUtils.saveHostConfigurations` writes them: the user name of a
    // jump server is `jumpServer<i>.name`.
    let p = map(
        &ssh_with(
            r#","jumpServer.count":"2",
            "jumpServer0.enabled":"true","jumpServer0.host":"jump1.example.test","jumpServer0.port":"2201",
            "jumpServer0.name":"nine_jump","jumpServer0.authType":"PUBLIC_KEY","jumpServer0.keyPath":"/home/nine_user/.ssh/id_ed25519",
            "jumpServer0.keyValue":"-----BEGIN fixture-key-not-a-secret",
            "jumpServer1.enabled":true,"jumpServer1.host":"jump2.example.test","jumpServer1.name":"nine_jump",
            "jumpServer2.enabled":false,"jumpServer2.host":"off.example.test""#,
        ),
        None,
    );
    let c = only(&p);
    let ssh = c.ssh.as_ref().unwrap_or_else(|| panic!("no ssh"));
    let hops: Vec<(&str, u16)> = ssh
        .jump_hosts
        .iter()
        .map(|h| (h.host.as_str(), h.port))
        .collect();
    assert_eq!(
        hops,
        [("jump1.example.test", 2201), ("jump2.example.test", 22)]
    );
    assert_eq!(ssh.jump_hosts[0].username, "nine_jump");
    assert_eq!(
        ssh.jump_hosts[0].auth,
        SshAuth::PublicKey {
            key_path: std::path::PathBuf::from("/home/nine_user/.ssh/id_ed25519"),
            passphrase: Some(SecretState::Prompt),
        }
    );
    assert_eq!(
        ssh.jump_hosts[1].auth,
        SshAuth::Password {
            secret: SecretState::Prompt
        }
    );
    assert!(issue(
        &p,
        IssueKind::SecretRemoved,
        Some("jumpServer0.keyValue")
    ));
    // Index 2 is past `jumpServer.count`: reported, not imported.
    assert!(issue(&p, IssueKind::Unsupported, Some("jumpServer2")));
    assert!(!format!("{p:?}").contains("not-a-secret"));
    // Unreadable or incomplete jump servers fail the connection: the hop is never skipped.
    for props in [
        r#","jumpServer0.enabled":true,"jumpServer0.host":"jump1.example.test""#,
        r#","jumpServer0.enabled":true,"jumpServer0.name":"u""#,
        r#","jumpServer0.enabled":true,"jumpServer0.host":"-x","jumpServer0.name":"u""#,
        // `user` is not the key DBeaver reads for a jump server: no user name.
        r#","jumpServer0.enabled":true,"jumpServer0.host":"jump1.example.test","jumpServer0.user":"u""#,
        r#","jumpServerX.host":"jump1.example.test""#,
        r#","jumpServer0.host":{"a":1}"#,
        r#","jumpServer.count":{"a":1}"#,
    ] {
        assert_eq!(status_of(&ssh_with(props)), Status::Failed, "{props}");
    }
}

/// A hop for `ssh_with`: `jumpServer<i>.*` with DBeaver's key names, `enabled` written as given.
fn hop(i: usize, enabled: &str) -> String {
    format!(
        r#","jumpServer{i}.enabled":{enabled},"jumpServer{i}.host":"jump{i}.example.test",
        "jumpServer{i}.name":"nine_jump","jumpServer{i}.authType":"AGENT""#
    )
}

/// The jump hosts imported for `props`, or the status when the connection is not imported.
fn jump_hosts(props: &str) -> Result<Vec<String>, Status> {
    let p = map(&ssh_with(props), None);
    match p.connections[0].imported() {
        Some(c) => Ok(c
            .ssh
            .as_ref()
            .map(|s| s.jump_hosts.iter().map(|h| h.host.clone()).collect())
            .unwrap_or_default()),
        None => Err(p.connections[0].status()),
    }
}

fn jumps(names: &[usize]) -> Vec<String> {
    names
        .iter()
        .map(|i| format!("jump{i}.example.test"))
        .collect()
}

#[test]
fn jump_servers_follow_dbeaver_selection() {
    // No `jumpServer.count`: DBeaver reads up to MAX_JUMP_SERVERS (5) indices.
    let five: String = (0..5).map(|i| hop(i, "true")).collect();
    assert_eq!(jump_hosts(&five), Ok(jumps(&[0, 1, 2, 3, 4])));
    // A sixth enabled hop in the chain: more than DBeaver supports, fails (never dropped).
    let six: String = (0..6).map(|i| hop(i, "true")).collect();
    assert_eq!(
        jump_hosts(&format!(r#","jumpServer.count":6{six}"#)),
        Err(Status::Failed)
    );
    // Without a count, index 5 is past the default count: reported, not imported (DBeaver does
    // not read it either).
    assert_eq!(jump_hosts(&six), Ok(jumps(&[0, 1, 2, 3, 4])));
    // A count larger than 5 with a shorter chain is fine.
    let two = format!("{}{}", hop(0, "true"), hop(1, "true"));
    assert_eq!(
        jump_hosts(&format!(r#","jumpServer.count":"9"{two}"#)),
        Ok(jumps(&[0, 1]))
    );
    // The count bounds the indices: number or string, as DBeaver writes it.
    for count in ["1", r#""1""#] {
        let props = format!(r#","jumpServer.count":{count}{two}"#);
        assert_eq!(jump_hosts(&props), Ok(jumps(&[0])), "{count}");
        let p = map(&ssh_with(&props), None);
        assert!(issue(&p, IssueKind::Unsupported, Some("jumpServer1")));
    }
    // Zero, negative or blank counts.
    for count in ["0", r#""0""#, "-1", r#""-3""#] {
        assert_eq!(
            jump_hosts(&format!(r#","jumpServer.count":{count}{two}"#)),
            Ok(vec![]),
            "{count}"
        );
    }
    assert_eq!(
        jump_hosts(&format!(r#","jumpServer.count":""{two}"#)),
        Ok(jumps(&[0, 1]))
    );
    // An unreadable count fails.
    for count in [r#""two""#, "1.5", r#""1.5""#, "true", r#"" 1""#] {
        assert_eq!(
            jump_hosts(&format!(r#","jumpServer.count":{count}{two}"#)),
            Err(Status::Failed),
            "{count}"
        );
    }
    // The chain stops at the first hop that is not enabled; the hops after it must not be
    // enabled.
    let off_first = format!("{}{}", hop(0, "false"), hop(1, "false"));
    assert_eq!(jump_hosts(&off_first), Ok(vec![]));
    let p = map(&ssh_with(&off_first), None);
    assert!(issue(&p, IssueKind::Unsupported, Some("jumpServer0")));
    let tail_off = format!("{}{}", hop(0, "true"), hop(1, r#""FALSE""#));
    assert_eq!(jump_hosts(&tail_off), Ok(jumps(&[0])));
    for props in [
        // Enabled after a disabled one.
        format!("{}{}", hop(0, "false"), hop(1, "true")),
        format!("{}{}{}", hop(0, "true"), hop(1, "false"), hop(2, "true")),
        // Enabled after a missing index.
        format!("{}{}", hop(0, "true"), hop(2, "true")),
        hop(1, "true"),
        // Enabled after a hop without an `enabled` flag (DBeaver reads it as not enabled).
        format!(
            r#","jumpServer0.host":"jump0.example.test","jumpServer0.name":"u"{}"#,
            hop(1, "true")
        ),
    ] {
        assert_eq!(jump_hosts(&props), Err(Status::Failed), "{props}");
    }
    // A hop without `enabled` is not used (DBeaver's `getBooleanProperty` default), and said so.
    let props = r#","jumpServer0.host":"jump0.example.test","jumpServer0.name":"u""#;
    assert_eq!(jump_hosts(props), Ok(vec![]));
    assert!(issue(
        &map(&ssh_with(props), None),
        IssueKind::Unsupported,
        Some("jumpServer0")
    ));
    // `enabled` must be a boolean or `true` / `false` (any case, not trimmed).
    assert_eq!(jump_hosts(&hop(0, r#""True""#)), Ok(jumps(&[0])));
    for enabled in [
        r#""yes""#,
        "1",
        r#""1""#,
        r#""""#,
        r#"" true""#,
        r#""true ""#,
    ] {
        assert_eq!(
            jump_hosts(&hop(0, enabled)),
            Err(Status::Failed),
            "{enabled}"
        );
    }
    // Even on a hop past the count: never read as `false`.
    assert_eq!(
        jump_hosts(&format!(r#","jumpServer.count":0{}"#, hop(0, r#""yes""#))),
        Err(Status::Failed)
    );
}

#[test]
fn jump_server_indices_must_be_canonical() {
    for index in [
        "00",
        "01",
        "+0",
        "+1",
        "-1",
        " 0",
        "0 ",
        "1x",
        "99999999999999999999999",
    ] {
        let props = format!(r#","jumpServer{index}.enabled":true"#);
        assert_eq!(jump_hosts(&props), Err(Status::Failed), "{index:?}");
    }
    // Canonical multi-digit indices are accepted (past the count: reported, not imported).
    let props = hop(10, "true");
    assert_eq!(jump_hosts(&props), Ok(vec![]));
    assert!(issue(
        &map(&ssh_with(&props), None),
        IssueKind::Unsupported,
        Some("jumpServer10")
    ));
}

// ---- ADR-0016 §1.2: RSA key files ----

fn key_type_issues(p: &ProjectImport) -> Vec<(String, String)> {
    p.connections
        .iter()
        .flat_map(|c| &c.issues)
        .filter(|i| i.kind == IssueKind::SshKeyType)
        .map(|i| (i.key.clone().unwrap_or_default(), i.message.clone()))
        .collect()
}

#[test]
fn rsa_key_files_are_flagged() {
    let with_key = |path: &str| {
        map(
            &ssh_with(&format!(r#","authType":"PUBLIC_KEY","keyPath":"{path}""#)),
            None,
        )
    };
    for path in [
        "/home/nine_user/.ssh/id_rsa",
        "/home/nine_user/.ssh/ID_RSA",
        "/home/nine_user/.ssh/id_rsa_work",
        "/home/nine_user/keys/deploy-rsa.pem",
        r"C:\\Users\\nine_user\\.ssh\\id_rsa",
    ] {
        let p = with_key(path);
        // Still imported: NineSQL refuses the key when connecting, the report warns now.
        assert_eq!(p.connections[0].status(), Status::Imported, "{path}");
        let issues = key_type_issues(&p);
        assert_eq!(issues.len(), 1, "{path}");
        assert!(issues[0].0.ends_with("properties.keyPath"), "{path}");
        assert!(
            issues[0].1.starts_with("RSA key files are not supported"),
            "{path}"
        );
        assert!(issues[0].1.contains("ADR-0016"));
        // The report never shows the path.
        assert!(!format!("{:?}", p.connections[0].issues).contains("nine_user"));
    }
    for path in [
        "/home/nine_user/.ssh/id_ed25519",
        "/home/nine_user/.ssh/id_ecdsa",
        "/home/nine_user/keys/deploy-ed25519.pem",
        // `rsa` inside a word is not a hint.
        "/home/nine_user/keys/universal_ed25519",
    ] {
        assert!(key_type_issues(&with_key(path)).is_empty(), "{path}");
    }
    // A name that says nothing: the type is not checked, the report says so (key files are
    // never read during import).
    for path in [
        "/home/nine_user/.ssh/work_key",
        "/home/nine_user/.ssh/mykey.pem",
        "/home/nine_user/.rsa/key",
        "/home/nine_user/.ssh/id_dsa",
    ] {
        let issues = key_type_issues(&with_key(path));
        assert_eq!(issues.len(), 1, "{path}");
        assert!(issues[0].1.contains("not checked"), "{path}");
        assert!(
            issues[0]
                .1
                .contains("RSA key files are not supported (ADR-0016)")
        );
    }
    // Password and agent auth: no key file, nothing to flag.
    assert!(key_type_issues(&map(&ssh_with(r#","authType":"AGENT""#), None)).is_empty());
    assert!(key_type_issues(&map(&ssh_with(""), None)).is_empty());
}

#[test]
fn rsa_key_files_of_jump_servers_are_flagged() {
    let p = map(
        &ssh_with(
            r#","authType":"PUBLIC_KEY","keyPath":"/home/nine_user/.ssh/id_ed25519",
            "jumpServer0.enabled":true,"jumpServer0.host":"jump0.example.test","jumpServer0.name":"u",
            "jumpServer0.authType":"PUBLIC_KEY","jumpServer0.keyPath":"/home/nine_user/.ssh/id_rsa",
            "jumpServer1.enabled":true,"jumpServer1.host":"jump1.example.test","jumpServer1.name":"u",
            "jumpServer1.authType":"PUBLIC_KEY","jumpServer1.keyPath":"/home/nine_user/.ssh/bastion""#,
        ),
        None,
    );
    assert_eq!(p.connections[0].status(), Status::Imported);
    let issues = key_type_issues(&p);
    assert_eq!(issues.len(), 2, "{issues:?}");
    assert!(issues[0].0.ends_with("jumpServer0.keyPath"));
    assert!(issues[0].1.starts_with("RSA key files are not supported"));
    assert!(issues[1].0.ends_with("jumpServer1.keyPath"));
    assert!(issues[1].1.contains("not checked"));
}

// ---- P2-A review L-a: blank profile values ----

#[test]
fn blank_profile_values_count_in_every_scope() {
    let handler = |extra: &str, props: &str| {
        format!(
            r#"{{"host":"db.example.test","port":"5432","handlers":{{"postgre_ssl":{{"type":"CONFIG",
            "enabled":true{extra},"properties":{{"sslmode":"verify-full"{props}}}}}}}}}"#
        )
    };
    let connection_scope = |value: &str| {
        one(
            "postgresql",
            "postgres-jdbc",
            r#"{"host":"db.example.test","port":"5432"}"#,
        )
        .replace(
            r#""name":"c","#,
            &format!(r#""name":"c","config-profile":{value},"#),
        )
    };
    let cases = |value: &str| {
        vec![
            ("connection", connection_scope(value)),
            (
                "configuration",
                one(
                    "postgresql",
                    "postgres-jdbc",
                    &format!(
                        r#"{{"host":"db.example.test","port":"5432","config-profile":{value}}}"#
                    ),
                ),
            ),
            (
                "handler",
                one(
                    "postgresql",
                    "postgres-jdbc",
                    &handler(&format!(r#","config-profile":{value}"#), ""),
                ),
            ),
            (
                "handler properties",
                one(
                    "postgresql",
                    "postgres-jdbc",
                    &handler("", &format!(r#","config-profile":{value}"#)),
                ),
            ),
        ]
    };
    // DBeaver does not trim: a blank value is a (broken) profile reference, fail closed.
    for value in [
        r#"" ""#,
        r#""\t""#,
        r#""\u00a0""#,
        r#""bastion""#,
        "false",
        "0",
        r#"["p"]"#,
        r#"{"id":"p"}"#,
    ] {
        for (scope, json) in cases(value) {
            let p = map(&json, None);
            assert_eq!(
                p.connections[0].status(),
                Status::ImportedDisabled,
                "{scope}: {value}"
            );
            assert!(
                issue(&p, IssueKind::Disabled, Some("config-profile")),
                "{scope}: {value}"
            );
        }
    }
    // Only an absent value does not: null, the empty string, an empty array or object.
    for value in ["null", r#""""#, "[]", "{}"] {
        for (scope, json) in cases(value) {
            assert_eq!(status_of(&json), Status::Imported, "{scope}: {value}");
        }
    }
}

#[test]
fn inline_ssh_key_is_a_secret() {
    let p = map(
        &ssh_with(r#","keyValue":"-----BEGIN fixture-key-not-a-secret""#),
        None,
    );
    assert!(issue(
        &p,
        IssueKind::SecretRemoved,
        Some("properties.keyValue")
    ));
    assert!(!format!("{p:?}").contains("not-a-secret"));
}

#[test]
fn many_types_with_one_base_stay_fast_and_capped() {
    let mut json = String::from(r#"{"connection-types":{"#);
    for i in 0..5_000 {
        if i > 0 {
            json.push(',');
        }
        // All sanitize to the base `x-`.
        json.push_str(&format!(
            r#""X{}":{{}}"#,
            "!".repeat(i % 7 + 1) + &" ".repeat(i / 7)
        ));
    }
    json.push_str("}}");
    let start = std::time::Instant::now();
    let p = map(&json, None);
    assert!(start.elapsed() < std::time::Duration::from_secs(20));
    assert_eq!(p.connection_types.len(), 1_000);
    assert!(issue(&p, IssueKind::Truncated, None));
    let ids: std::collections::BTreeSet<&str> =
        p.connection_types.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids.len(), 1_000, "type ids must be unique");
}

#[test]
fn unreadable_handler_data_disables() {
    for handlers in [r#"["ssh_tunnel"]"#, r#"{"ssh_tunnel":"on"}"#, r#""yes""#] {
        let cfg = format!(r#"{{"host":"db.example.test","handlers":{handlers}}}"#);
        let p = map(&one("postgresql", "postgres-jdbc", &cfg), None);
        assert_eq!(
            p.connections[0].status(),
            Status::ImportedDisabled,
            "{handlers}"
        );
    }
    let cfg = r#"{"host":"db.example.test","handlers":{}}"#;
    assert_eq!(
        status_of(&one("postgresql", "postgres-jdbc", cfg)),
        Status::Imported
    );
}

// ---- Security review of the P2-A follow-ups (Low-2, Low-3) ----

#[test]
fn skipped_jump_servers_report_secrets_and_hosts() {
    use ninesql_dbeaver::Severity;
    // Not enabled, disabled, and past the count: none is imported, each is reported.
    for props in [
        r#","jumpServer0.host":"jump0.example.test","jumpServer0.name":"u",
        "jumpServer0.keyValue":"-----BEGIN fixture-skipped-not-a-secret",
        "jumpServer0.password":"fixture-skipped-not-a-secret""#,
        r#","jumpServer0.enabled":false,"jumpServer0.host":"jump0.example.test",
        "jumpServer0.keyValue":"-----BEGIN fixture-skipped-not-a-secret",
        "jumpServer0.password":"fixture-skipped-not-a-secret""#,
        r#","jumpServer.count":0,"jumpServer0.enabled":false,"jumpServer0.host":"jump0.example.test",
        "jumpServer0.keyValue":"-----BEGIN fixture-skipped-not-a-secret",
        "jumpServer0.password":"fixture-skipped-not-a-secret""#,
        r#","jumpServer.count":0,"jumpServer0.enabled":true,"jumpServer0.host":"jump0.example.test",
        "jumpServer0.keyValue":"-----BEGIN fixture-skipped-not-a-secret",
        "jumpServer0.password":"fixture-skipped-not-a-secret""#,
    ] {
        let p = map(&ssh_with(props), None);
        assert_eq!(jump_hosts(props), Ok(vec![]), "{props}");
        assert!(
            issue(&p, IssueKind::SecretRemoved, Some("jumpServer0.keyValue")),
            "{props}"
        );
        assert!(
            issue(&p, IssueKind::SecretRemoved, Some("jumpServer0.password")),
            "{props}"
        );
        // A configured host that is not used is a warning, never only an info.
        assert!(
            p.connections[0]
                .issues
                .iter()
                .any(|i| i.kind == IssueKind::Unsupported
                    && i.severity == Severity::Warning
                    && i.key.as_deref().is_some_and(|k| k.ends_with("jumpServer0"))),
            "{props}"
        );
        let dump = format!("{p:?}");
        assert!(!dump.contains("not-a-secret"), "{props}");
        assert!(!dump.contains("jump0.example.test"), "{props}");
    }
    // Settings without a host: info only.
    let p = map(
        &ssh_with(r#","jumpServer0.enabled":false,"jumpServer0.port":"22""#),
        None,
    );
    assert!(
        p.connections[0]
            .issues
            .iter()
            .any(|i| i.kind == IssueKind::Unsupported
                && i.severity == Severity::Info
                && i.key.as_deref().is_some_and(|k| k.ends_with("jumpServer0")))
    );
}
