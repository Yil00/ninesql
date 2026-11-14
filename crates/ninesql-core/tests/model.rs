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

#[test]
fn benign_look_alike_properties_survive_sanitize() {
    let mut c = basic();
    for key in [
        "passwordCharacterEncoding",
        "allowPublicKeyRetrieval",
        "gssencmode",
        "rewriteBatchedStatements",
    ] {
        c.properties.insert(key.into(), "true".into());
    }
    assert!(c.sanitize().is_empty());
    assert_eq!(c.properties.len(), 4);
    c.validate().unwrap();
    // The value check still applies to allowlisted keys.
    c.properties.insert(
        "passwordCharacterEncoding".into(),
        "jdbc:mysql://root:Hunter2@db/x".into(),
    );
    assert!(matches!(
        c.validate(),
        Err(ConnectionError::SecretInPropertyValue { .. })
    ));
}
