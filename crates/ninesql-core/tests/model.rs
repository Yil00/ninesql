//! Connection model rules and serde round trips.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).

use std::collections::BTreeMap;
use std::path::Path;

use ninesql_core::{
    Auth, ClientCertificate, Connection, ConnectionError, ConnectionId, ConnectionType,
    ConnectionTypeId, ConnectionTypes, ConnectionsFile, Endpoint, EngineId, Origin, SecretSlot,
    SecretSource, SecurityWarning, SshAuth, SshHop, SshTunnel, TlsConfig, TlsMode,
};
use ninesql_secrets::{SecretId, SecretKind, SecretRef, StoreKind};
use ninesql_storage::document;
use proptest::prelude::*;

fn basic() -> Connection {
    Connection::new(
        "c",
        EngineId::new("postgresql").unwrap(),
        Endpoint::Tcp {
            host: "h".into(),
            port: None,
            database: None,
        },
        Auth::Username {
            username: "u".into(),
        },
        ConnectionTypeId::dev(),
    )
}

#[test]
fn engine_id_rules() {
    for ok in ["postgresql", "mysql", "sql-server", "db2_luw", "x1"] {
        assert!(EngineId::new(ok).is_ok(), "{ok}");
    }
    for bad in ["", "Postgres", "1db", "pg sql", "pg/sql", &"a".repeat(65)] {
        assert!(EngineId::new(bad).is_err(), "{bad}");
    }
}

#[test]
fn new_connection_defaults_are_safe() {
    let c = basic();
    assert_eq!(c.tls, TlsConfig::default());
    assert_eq!(c.tls.mode, TlsMode::VerifyFull);
    assert!(c.security_warnings().is_empty());
    assert!(c.enabled);
    assert_eq!(c.origin, Origin::Native);
    c.validate().unwrap();
}

#[test]
fn tls_warnings_only_for_network_endpoints() {
    let mut c = basic();
    for (mode, warning) in [
        (TlsMode::Disable, Some(SecurityWarning::TlsDisabled)),
        (
            TlsMode::Require,
            Some(SecurityWarning::TlsCertificateNotVerified),
        ),
        (
            TlsMode::VerifyCa,
            Some(SecurityWarning::TlsHostnameNotVerified),
        ),
        (TlsMode::VerifyFull, None),
    ] {
        c.tls.mode = mode;
        assert_eq!(
            c.security_warnings(),
            warning.into_iter().collect::<Vec<_>>()
        );
    }
    c.tls.mode = TlsMode::Disable;
    c.endpoint = Endpoint::File {
        path: "/tmp/x.db".into(),
    };
    assert!(c.security_warnings().is_empty());
}

type Mutation = Box<dyn Fn(&mut Connection)>;

#[test]
fn validation_rejects_bad_fields() {
    let cases: Vec<(&str, Mutation)> = vec![
        ("empty name", Box::new(|c| c.name = "  ".into())),
        (
            "control char in name",
            Box::new(|c| c.name = "a\u{7}b".into()),
        ),
        (
            "url with credentials in host",
            Box::new(|c| {
                c.endpoint = Endpoint::Tcp {
                    host: "user:pw@db".into(),
                    port: None,
                    database: None,
                }
            }),
        ),
        (
            "port zero",
            Box::new(|c| {
                c.endpoint = Endpoint::Tcp {
                    host: "h".into(),
                    port: Some(0),
                    database: None,
                }
            }),
        ),
        (
            "empty file path",
            Box::new(|c| c.endpoint = Endpoint::File { path: "".into() }),
        ),
        (
            "slash in folder",
            Box::new(|c| c.folder = vec!["a/b".into()]),
        ),
        (
            "secret-like property",
            Box::new(|c| {
                c.properties.insert("Password".into(), "x".into());
            }),
        ),
        (
            "ssh port zero",
            Box::new(|c| {
                c.ssh = Some(SshTunnel {
                    server: SshHop {
                        host: "b".into(),
                        port: 0,
                        username: "u".into(),
                        auth: SshAuth::Agent,
                    },
                    jump_hosts: vec![],
                })
            }),
        ),
        (
            "empty dbeaver id",
            Box::new(|c| {
                c.origin = Origin::Dbeaver {
                    dbeaver_id: String::new(),
                    project: None,
                }
            }),
        ),
    ];
    for (name, mutate) in cases {
        let mut c = basic();
        mutate(&mut c);
        assert!(c.validate().is_err(), "{name}");
    }
}

#[test]
fn secret_property_error_hides_the_value() {
    let mut c = basic();
    c.properties.insert("api_token".into(), "hunter2".into());
    let err = c.validate().unwrap_err();
    assert!(matches!(err, ConnectionError::SecretInProperties { .. }));
    assert!(!format!("{err} {err:?}").contains("hunter2"));
}

#[test]
fn set_secret_ref_matches_slots() {
    let r = |s: &str| SecretRef::new(StoreKind::Keyring, SecretId::new(s).unwrap());
    let mut c = basic();
    assert!(matches!(
        c.set_secret_ref(SecretSlot::Auth, r("a")),
        Err(ConnectionError::NoSuchSecretSlot(SecretSlot::Auth))
    ));
    c.auth = Auth::Token {
        username: None,
        secret: SecretSource::Prompt,
    };
    assert_eq!(
        c.set_secret_ref(SecretSlot::Auth, r("a")).unwrap(),
        SecretKind::Token
    );
    assert!(c.set_secret_ref(SecretSlot::SshServer, r("b")).is_err());
    c.ssh = Some(SshTunnel {
        server: SshHop {
            host: "b".into(),
            port: 22,
            username: "u".into(),
            auth: SshAuth::Agent,
        },
        jump_hosts: vec![],
    });
    assert!(c.set_secret_ref(SecretSlot::SshServer, r("b")).is_err());
    assert!(c.set_secret_ref(SecretSlot::SshJump(0), r("b")).is_err());
    assert!(c.set_secret_ref(SecretSlot::TlsClientKey, r("c")).is_err());
    c.tls.client_certificate = Some(ClientCertificate {
        cert_file: "c".into(),
        key_file: "k".into(),
        passphrase: None,
    });
    assert_eq!(
        c.set_secret_ref(SecretSlot::TlsClientKey, r("c")).unwrap(),
        SecretKind::ClientKeyPassphrase
    );
    assert_eq!(c.secret_refs().len(), 2);
}

#[test]
fn read_only_comes_from_type_unless_overridden() {
    let mut ro_dev = ConnectionType::development();
    ro_dev.read_only = true;
    let mut types = ConnectionTypes::default();
    types.upsert(ro_dev);
    let mut c = basic(); // type dev
    assert!(c.effective_read_only(&types));
    // Non-production types accept a weakening override.
    c.read_only = Some(false);
    assert!(!c.effective_read_only(&types));
    c.read_only = Some(true);
    assert!(c.effective_read_only(&ConnectionTypes::default()));
}

#[test]
fn production_read_only_cannot_be_weakened() {
    let mut ro_prod = ConnectionType::production();
    ro_prod.read_only = true;
    let mut types = ConnectionTypes::default();
    types.upsert(ro_prod);
    let mut c = basic();
    c.connection_type = ConnectionTypeId::prod();
    c.read_only = Some(false);
    let policy = c.policy(&types);
    assert!(policy.read_only);
    assert!(policy.production);
    assert!(!policy.log_query_text);
    // But it can be tightened on a writable production type.
    let mut c2 = basic();
    c2.connection_type = ConnectionTypeId::prod();
    assert!(!c2.effective_read_only(&ConnectionTypes::default()));
    c2.read_only = Some(true);
    assert!(c2.effective_read_only(&ConnectionTypes::default()));
}

#[test]
fn unknown_type_policy_is_production() {
    let mut c = basic();
    c.connection_type = ConnectionTypeId::new("deleted-type").unwrap();
    c.read_only = Some(false);
    let policy = c.policy(&ConnectionTypes::default());
    assert!(policy.production);
    assert!(policy.confirm_destructive);
    assert!(!policy.log_query_text);
}

#[test]
fn reserved_and_secret_bearing_properties_are_refused() {
    for (key, value) in [
        ("sslmode", "disable"),
        ("useSSL", "false"),
        ("trustServerCertificate", "true"),
        ("host", "other.example.test"),
        ("user", "root"),
        ("readOnly", "false"),
        ("url", "postgres://app:Hunter2@db/x"),
        ("options", "Server=h;Password=Hunter2"),
    ] {
        let mut c = basic();
        c.properties.insert(key.into(), value.into());
        let err = c.validate().unwrap_err();
        assert!(!err.to_string().contains("Hunter2"), "{err}");
        assert!(
            matches!(
                err,
                ConnectionError::ReservedProperty { .. }
                    | ConnectionError::SecretInPropertyValue { .. }
            ),
            "{key}: {err}"
        );
    }
    let mut c = basic();
    c.init_sql = vec!["ALTER USER app IDENTIFIED BY Hunter2".into()];
    let err = c.validate().unwrap_err();
    assert!(matches!(err, ConnectionError::SecretInInitSql { index: 0 }));
    assert!(!err.to_string().contains("Hunter2"));
}

#[test]
fn sanitize_removes_only_the_offending_data() {
    let mut c = basic();
    c.properties
        .insert("application_name".into(), "NineSQL".into());
    c.properties.insert("sslmode".into(), "disable".into());
    c.properties.insert("password".into(), "Hunter2".into());
    c.init_sql = vec![
        "SET search_path TO app".into(),
        "ALTER ROLE app PASSWORD 'Hunter2'".into(),
    ];
    let issues = c.sanitize();
    assert_eq!(issues.len(), 3);
    assert_eq!(c.properties.len(), 1);
    assert_eq!(c.init_sql, ["SET search_path TO app"]);
    c.validate().unwrap();
    assert!(!format!("{issues:?}").contains("Hunter2"));
}

// ---- Property-based serde round trip ----

fn text() -> impl Strategy<Value = String> {
    "[A-Za-z0-9 _.-]{1,20}".prop_filter("not blank", |s| !s.trim().is_empty())
}

fn host() -> impl Strategy<Value = String> {
    "[a-z0-9][a-z0-9.-]{0,30}"
}

fn secret_source() -> impl Strategy<Value = SecretSource> {
    prop_oneof![
        Just(SecretSource::Prompt),
        (
            prop_oneof![Just(StoreKind::Keyring), Just(StoreKind::Vault)],
            "[a-z0-9:-]{1,40}"
        )
            .prop_map(|(store, id)| SecretSource::Stored(SecretRef::new(
                store,
                SecretId::new(id).unwrap()
            ))),
    ]
}

fn endpoint() -> impl Strategy<Value = Endpoint> {
    prop_oneof![
        (
            host(),
            proptest::option::of(1u16..),
            proptest::option::of(text())
        )
            .prop_map(|(host, port, database)| Endpoint::Tcp {
                host,
                port,
                database
            }),
        ("/[a-z/]{1,20}", proptest::option::of(text())).prop_map(|(p, database)| {
            Endpoint::UnixSocket {
                path: p.into(),
                database,
            }
        }),
        "/[a-z/]{1,20}".prop_map(|p| Endpoint::File { path: p.into() }),
    ]
}

fn auth() -> impl Strategy<Value = Auth> {
    prop_oneof![
        Just(Auth::None),
        text().prop_map(|username| Auth::Username { username }),
        (text(), secret_source())
            .prop_map(|(username, secret)| Auth::Password { username, secret }),
        (proptest::option::of(text()), secret_source())
            .prop_map(|(username, secret)| Auth::Token { username, secret }),
    ]
}

fn ssh_auth() -> impl Strategy<Value = SshAuth> {
    prop_oneof![
        Just(SshAuth::Agent),
        secret_source().prop_map(|secret| SshAuth::Password { secret }),
        ("/[a-z/]{1,20}", proptest::option::of(secret_source())).prop_map(|(p, passphrase)| {
            SshAuth::PublicKey {
                key_path: p.into(),
                passphrase,
            }
        }),
    ]
}

fn hop() -> impl Strategy<Value = SshHop> {
    (host(), 1u16.., text(), ssh_auth()).prop_map(|(host, port, username, auth)| SshHop {
        host,
        port,
        username,
        auth,
    })
}

fn tls() -> impl Strategy<Value = TlsConfig> {
    (
        prop_oneof![
            Just(TlsMode::Disable),
            Just(TlsMode::Require),
            Just(TlsMode::VerifyCa),
            Just(TlsMode::VerifyFull)
        ],
        proptest::option::of("/[a-z/]{1,20}"),
        proptest::option::of(proptest::option::of(secret_source())),
    )
        .prop_map(|(mode, ca, cert)| TlsConfig {
            mode,
            ca_file: ca.map(Into::into),
            client_certificate: cert.map(|passphrase| ClientCertificate {
                cert_file: "/c.pem".into(),
                key_file: "/c.key".into(),
                passphrase,
            }),
        })
}

prop_compose! {
    fn connection()(
        name in text(),
        engine in "[a-z][a-z0-9-]{0,15}",
        endpoint in endpoint(),
        auth in auth(),
        ssh in proptest::option::of((hop(), proptest::collection::vec(hop(), 0..3))),
        tls in tls(),
        ty in prop_oneof![Just("dev"), Just("test"), Just("prod"), Just("staging")],
        read_only in proptest::option::of(any::<bool>()),
        folder in proptest::collection::vec(text(), 0..3),
        properties in proptest::collection::btree_map("[a-z_]{1,10}", text(), 0..3),
        init_sql in proptest::collection::vec(text(), 0..2),
        enabled in any::<bool>(),
        dbeaver in proptest::option::of(("[a-z0-9-]{1,30}", proptest::option::of(text()))),
    ) -> Connection {
        let mut c = Connection::new(
            name,
            EngineId::new(engine).unwrap(),
            endpoint,
            auth,
            ConnectionTypeId::new(ty).unwrap(),
        );
        c.ssh = ssh.map(|(server, jump_hosts)| SshTunnel { server, jump_hosts });
        c.tls = tls;
        c.read_only = read_only;
        c.folder = folder;
        c.properties = properties
            .into_iter()
            .filter(|(k, _)| {
                !ninesql_storage::hygiene::is_secret_like_key(k)
                    && ninesql_core::properties::reserved(k).is_none()
            })
            .collect::<BTreeMap<_, _>>();
        c.init_sql = init_sql;
        c.enabled = enabled;
        if let Some((dbeaver_id, project)) = dbeaver {
            c.origin = Origin::Dbeaver { dbeaver_id, project };
        }
        c
    }
}

proptest! {
    #[test]
    fn connections_file_round_trip(connections in proptest::collection::vec(connection(), 0..4)) {
        for c in &connections {
            prop_assert!(c.validate().is_ok(), "{:?}", c.validate());
        }
        let file = ConnectionsFile { connections };
        let bytes = document::encode(&file).unwrap();
        let back = document::decode::<ConnectionsFile>(Path::new("mem"), &bytes).unwrap();
        prop_assert!(back.secret_like_keys.is_empty());
        prop_assert_eq!(back.document, file);
    }

    #[test]
    fn connection_id_round_trip(bytes in any::<[u8; 16]>()) {
        let id = ConnectionId::from_uuid(uuid::Uuid::from_bytes(bytes));
        let json = serde_json::to_string(&id).unwrap();
        prop_assert_eq!(json, format!("\"{id}\""));
        let back: ConnectionId = serde_json::from_str(&format!("\"{id}\"")).unwrap();
        prop_assert_eq!(back, id);
    }
}

/// Common driver options that are kept as plain properties: neither secret-like, nor reserved by
/// the driver API rule, nor carrying a credential value.
const ACCEPTED_KEYS: &[&str] = &[
    // PostgreSQL / pgjdbc
    "application_name",
    "connect_timeout",
    "search_path",
    "currentSchema",
    "statement_timeout",
    "lock_timeout",
    "idle_in_transaction_session_timeout",
    "keepalives",
    "keepalives_idle",
    "tcpKeepAlive",
    "prepareThreshold",
    "defaultRowFetchSize",
    "targetServerType",
    "assumeMinServerVersion",
    "escapeSyntaxCallMode",
    // MySQL / MariaDB / Connector/J
    "connectTimeout",
    "socketTimeout",
    "characterEncoding",
    "charset",
    "useUnicode",
    "serverTimezone",
    "zeroDateTimeBehavior",
    "rewriteBatchedStatements",
    "useServerPrepStmts",
    "cachePrepStmts",
    "prepStmtCacheSize",
    "useCursorFetch",
    "tinyInt1isBit",
    "yearIsDateType",
    "useCompression",
    "nullCatalogMeansCurrent",
    // Others (ClickHouse, MongoDB, Trino, generic)
    "compress",
    "max_result_rows",
    "session_timezone",
    "replicaSet",
    "appName",
    "retryWrites",
    "readPreference",
    "directConnection",
    "serverSelectionTimeoutMS",
    "maxPoolSize",
    "clientTags",
    "fetchSize",
    "loginTimeout",
    "ApplicationIntent",
];

/// Keys refused before and after the move to the driver API rule.
const STILL_REFUSED_KEYS: &[&str] = &[
    "sslmode",
    "sslrootcert",
    "useSSL",
    "requireSSL",
    "verifyServerCertificate",
    "trustServerCertificate",
    "hostNameInCertificate",
    "trustStoreType",
    "tlsAllowInvalidCertificates",
    "secure",
    "encrypt",
    "keystore",
    "host",
    "hostaddr",
    "port",
    "server",
    "user",
    "username",
    "uid",
    "userid",
    "readOnly",
    "default_transaction_read_only",
    // Secret-like keys (I1).
    "password",
    "sslpassword",
    "pwd",
    "apiKey",
    // `gsslib` contains `ssl`: refused before as TLS, now as GSSAPI (fails closed).
    "gsslib",
];

/// Keys the core accepted before and now refuses because the driver API reserves them. Each one
/// would have been stored and then refused by the driver at connect time.
const NEWLY_REFUSED_KEYS: &[&str] = &[
    // Was benign in core. MySQL: lets a MITM on a non-TLS connection obtain the password.
    "allowPublicKeyRetrieval",
    // Were benign in core. Matched by the driver API `password` fragment.
    "passwordCharacterEncoding",
    "password_encryption",
    // Was benign in core. GSSAPI transport encryption bypasses the TLS settings; reserved by the
    // driver API.
    "gssencmode",
    // PostgreSQL walsender mode: replication commands bypass the read-only transaction (I3);
    // reserved by the driver API.
    "replication",
    // Session-level settings that can change lexical or access behavior.
    "options",
    "sessionVariables",
    "sql_mode",
    "standard_conforming_strings",
    // Multi-statements.
    "allowMultiQueries",
    "multiStatements",
    // Init SQL: use `init_sql`, which is checked for password literals.
    "initSql",
    "init_command",
    // Endpoint shadows: socket, service file, URLs, extra hosts, fail-over, reconnect.
    "socket",
    "unix_socket",
    "socketFactory",
    "service",
    "servername",
    "url",
    "jdbcUrl",
    "uri",
    "connectionString",
    "hosts",
    "alt_hosts",
    "failoverHosts",
    "loadBalanceHosts",
    "nodes",
    "contactPoints",
    "autoReconnect",
    "maxReconnects",
    // Credentials / authentication options (no typed field yet: fails closed).
    "passfile",
    "authSource",
    "authMechanism",
    "authenticationPlugins",
    "authentication",
    "token_type",
    // TLS-adjacent: channel binding, Firebird wire encryption.
    "channel_binding",
    "wire_crypt",
    // Proxies: use the SSH / proxy settings.
    "proxy",
    "http_proxy",
    "socksProxyHost",
    // Local file access.
    "allowLoadLocalInfile",
    "allowLocalInfile",
    "localInfile",
];

fn with_property(key: &str) -> Connection {
    let mut c = basic();
    c.properties.insert(key.into(), "x-value".into());
    c
}

#[test]
fn common_driver_properties_follow_the_driver_api_rule() {
    for key in ACCEPTED_KEYS {
        with_property(key)
            .validate()
            .unwrap_or_else(|e| panic!("{key}: {e}"));
        assert!(!ninesql_driver_api::is_reserved_property(key), "{key}");
    }
    for key in STILL_REFUSED_KEYS.iter().chain(NEWLY_REFUSED_KEYS) {
        let err = with_property(key).validate().unwrap_err();
        assert!(
            matches!(
                err,
                ConnectionError::ReservedProperty { .. }
                    | ConnectionError::SecretInProperties { .. }
            ),
            "{key}: {err}"
        );
        assert!(!err.to_string().contains("x-value"), "{err}");
    }
    assert!(ACCEPTED_KEYS.len() + STILL_REFUSED_KEYS.len() + NEWLY_REFUSED_KEYS.len() >= 60);
}

#[test]
fn reserved_keys_win_over_benign_lists() {
    // `passwordCharacterEncoding` is on the storage benign list (not secret-like) but the driver
    // API reserves it: it is refused as reserved, not accepted.
    assert!(!ninesql_storage::hygiene::is_secret_like_key(
        "passwordCharacterEncoding"
    ));
    let mut c = basic();
    c.properties
        .insert("passwordCharacterEncoding".into(), "UTF-8".into());
    assert!(matches!(
        c.validate(),
        Err(ConnectionError::ReservedProperty { .. })
    ));
    // With a value that is not a charset it is treated as a misplaced secret.
    assert!(matches!(
        with_property("passwordCharacterEncoding").validate(),
        Err(ConnectionError::SecretInProperties { .. })
    ));
}

/// Imported (or hand-edited) connections holding keys that are now reserved are cleaned on load
/// and every removal is reported; nothing is silently kept.
#[test]
fn sanitize_reports_and_removes_now_reserved_keys() {
    let mut c = basic();
    c.properties
        .insert("application_name".into(), "NineSQL".into());
    for key in NEWLY_REFUSED_KEYS {
        c.properties.insert((*key).into(), "true".into());
    }
    let issues = c.sanitize();
    assert_eq!(issues.len(), NEWLY_REFUSED_KEYS.len(), "{issues:?}");
    assert_eq!(
        c.properties.keys().collect::<Vec<_>>(),
        ["application_name"]
    );
    c.validate().unwrap();
    // The value check still applies to accepted keys.
    c.properties
        .insert("charset".into(), "jdbc:mysql://root:Hunter2@db/x".into());
    assert!(matches!(
        c.validate(),
        Err(ConnectionError::SecretInPropertyValue { .. })
    ));
}

proptest! {
    /// Whatever the key (ASCII letters, digits, separators and non-ASCII letters, which the
    /// driver API's normalization drops), a property the core keeps is one the drivers accept.
    #[test]
    fn kept_properties_pass_the_driver_check(key in "[A-Za-z0-9_.\\-éèüßçøåıİΣσжЖ中ﬁ]{1,24}") {
        let mut c = with_property(&key);
        let kept = c.validate().is_ok();
        let issues = c.sanitize();
        prop_assert_eq!(kept, issues.is_empty());
        if kept {
            let props = std::iter::once((
                key.clone(),
                ninesql_driver_api::PropertyValue::Plain("x".into()),
            ))
            .collect();
            prop_assert!(ninesql_driver_api::check_properties(&props, &[]).is_ok(), "{}", key);
        }
    }
}

/// L6: host / user / database names may hold characters that matter in DSNs; the model accepts
/// them (drivers must pass structured config, never build a connection string from them), and
/// still refuses the ones that could hide user info in the host.
#[test]
fn dsn_significant_characters_are_data_not_syntax() {
    let mut c = basic();
    c.endpoint = Endpoint::Tcp {
        host: "db.example.test".into(),
        port: Some(5432),
        database: Some("app;sslmode=disable host='evil' ?x=&y%20".into()),
    };
    c.auth = Auth::Username {
        username: "u;password=x' or '1'='1 @:{}".into(),
    };
    c.validate().unwrap();
    for host in ["u:p@db", "db/x", "db x", "db\\x"] {
        c.endpoint = Endpoint::Tcp {
            host: host.into(),
            port: None,
            database: None,
        };
        assert!(c.validate().is_err(), "{host}");
    }
    for host in ["db;sslmode=disable", "fe80::1%eth0", "db?x=1"] {
        c.endpoint = Endpoint::Tcp {
            host: host.into(),
            port: None,
            database: None,
        };
        c.validate().unwrap();
    }
}
