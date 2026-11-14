//! Property-based fuzzing of both decoders and of the mapping (ADR-0006 consequences: total
//! parsing, no panic on untrusted input). Runs on stable in CI; the cargo-fuzz targets in
//! `fuzz/` drive the same entry points with coverage guidance where a nightly toolchain exists.
//!
//! Inputs: raw bytes, random JSON, and JSON shaped like `data-sources.json` / the decrypted
//! credentials (known keys with random or plausible values) so the mapping code is reached.

mod common;

use ninesql_dbeaver::mapping::map_sources;
use ninesql_dbeaver::{decrypt_credentials, parse_data_sources};
use proptest::prelude::*;
use serde_json::{Map, Value, json};

fn leaf() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::from),
        any::<f64>()
            .prop_map(|f| serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)),
        "\\PC{0,24}".prop_map(Value::String),
        prop::sample::select(vec![
            "",
            "true",
            "false",
            "22",
            "22.0",
            "-1",
            "65536",
            "PASSWORD",
            "PUBLIC_KEY",
            "AGENT",
            "TUNNEL",
            "PROXY",
            "CONFIG",
            "URL",
            "MANUAL",
            "dev",
            "prod",
            "test",
            "verify-full",
            "prefer",
            "disable",
            "VERIFY_CA",
            ":memory:",
            "db.example.test",
            "a@b",
            "/home/nine_user/x",
            "jdbc:postgresql://db.example.test:5432/nine?sslmode=require",
            "jdbc:mysql://u:p@[::1]:x,h2/db?useSSL=true&&=&%zz",
            "jdbc:sqlite:file:",
            "jdbc:clickhouse:http://",
            "jdbc:",
            "Team A/Production",
            "org.jkiss.dbeaver.color.connectionType.prod.background",
            "255,0,0",
            "1,2,3,4,5",
        ])
        .prop_map(|s| Value::String(s.to_owned())),
    ]
}

fn any_json() -> impl Strategy<Value = Value> {
    leaf().prop_recursive(4, 48, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::btree_map("\\PC{0,12}", inner, 0..6)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

/// An object with some of `keys`, each with a random value or the one from `special`.
fn obj_with(
    keys: &'static [&'static str],
    special: impl Strategy<Value = Map<String, Value>>,
) -> impl Strategy<Value = Value> {
    (
        prop::collection::btree_map(prop::sample::select(keys), any_json(), 0..keys.len()),
        special,
    )
        .prop_map(|(random, special)| {
            let mut m: Map<String, Value> =
                random.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
            m.extend(special);
            Value::Object(m)
        })
}

fn handler() -> impl Strategy<Value = Value> {
    let props = obj_with(
        &[
            "host",
            "port",
            "authType",
            "keyPath",
            "user",
            "implementation",
            "sslMode",
            "ssl.ca.cert",
            "ssl.client.cert",
            "ssl.client.key",
            "ssl.require",
            "ssl.verify.server",
            "ssl.public.key.retrieve",
            "localPort",
            "password",
        ],
        Just(Map::new()),
    );
    obj_with(
        &[
            "type",
            "enabled",
            "save-password",
            "user",
            "password",
            "properties",
        ],
        (any::<bool>(), props).prop_map(|(enabled, props)| {
            let mut m = Map::new();
            m.insert("enabled".into(), Value::Bool(enabled));
            m.insert("properties".into(), props);
            m
        }),
    )
}

fn configuration() -> impl Strategy<Value = Value> {
    let handlers = prop::collection::btree_map(
        prop::sample::select(vec![
            "ssh_tunnel",
            "postgre_ssl",
            "mysql_ssl",
            "socks_proxy",
            "x",
        ]),
        handler(),
        0..4,
    );
    let props = prop::collection::btree_map(
        prop::sample::select(vec![
            "sslmode",
            "ssl",
            "useSSL",
            "requireSSL",
            "verifyServerCertificate",
            "user",
            "password",
            "readOnly",
            "ApplicationName",
            "sslrootcert",
            "sslcert",
            "sslkey",
            "",
        ]),
        any_json(),
        0..6,
    );
    let bootstrap = obj_with(
        &["defaultSchema", "defaultCatalog", "autocommit", "query"],
        Just(Map::new()),
    );
    obj_with(
        &[
            "host",
            "port",
            "server",
            "database",
            "url",
            "configurationType",
            "type",
            "auth-model",
            "user",
            "password",
            "provider-properties",
            "closeIdleConnection",
        ],
        (handlers, props, bootstrap).prop_map(|(h, p, b)| {
            let mut m = Map::new();
            m.insert(
                "handlers".into(),
                Value::Object(h.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()),
            );
            m.insert(
                "properties".into(),
                Value::Object(p.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()),
            );
            m.insert("bootstrap".into(), b);
            m
        }),
    )
}

fn connection() -> impl Strategy<Value = Value> {
    let pair = prop::sample::select(vec![
        ("postgresql", "postgres-jdbc"),
        ("postgresql", "postgres-timescale-jdbc"),
        ("mysql", "mysql8"),
        ("mysql", "mariaDB"),
        ("sqlite", "sqlite_jdbc"),
        ("clickhouse", "com_clickhouse"),
        ("oracle", "oracle_thin"),
        ("", ""),
    ]);
    obj_with(
        &[
            "provider",
            "driver",
            "name",
            "description",
            "save-password",
            "read-only",
            "folder",
        ],
        (pair, configuration()).prop_map(|((p, d), cfg)| {
            let mut m = Map::new();
            m.insert("provider".into(), json!(p));
            m.insert("driver".into(), json!(d));
            m.insert("configuration".into(), cfg);
            m
        }),
    )
}

fn data_sources() -> impl Strategy<Value = Value> {
    let folders = prop::collection::btree_map(
        prop::sample::select(vec!["A", "B", "A/B", "", "/", "C"]),
        obj_with(&["parent", "description"], Just(Map::new())),
        0..4,
    );
    let types = prop::collection::btree_map(
        prop::sample::select(vec!["dev", "prod", "test", "staging", "Weird Id", ""]),
        obj_with(
            &[
                "name",
                "color",
                "colorDark",
                "auto-commit",
                "confirm-execute",
                "confirm-data-change",
                "smart-commit",
                "auto-close-connections",
                "security",
            ],
            Just(Map::new()),
        ),
        0..4,
    );
    let conns = prop::collection::btree_map(prop::sample::select(IDS), connection(), 0..4);
    (folders, types, conns, any_json()).prop_map(|(f, t, c, extra)| {
        let to_obj = |m: std::collections::BTreeMap<&str, Value>| {
            Value::Object(m.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
        };
        json!({
            "folders": to_obj(f),
            "connection-types": to_obj(t),
            "connections": to_obj(c),
            "extra": extra,
        })
    })
}

/// Connection ids shared by the data-sources and credentials strategies, so they match.
const IDS: &[&str] = &["c1", "c2", "postgres-jdbc-1-2", "", "\u{7f}"];

fn credentials_json() -> impl Strategy<Value = Value> {
    let section = obj_with(&["user", "password", "other"], Just(Map::new()));
    let sections = prop::collection::btree_map(
        prop::sample::select(vec!["#connection", "network/ssh_tunnel", "network/x"]),
        section,
        0..3,
    );
    prop::collection::btree_map(prop::sample::select(IDS), sections, 0..4).prop_map(|m| {
        Value::Object(
            m.into_iter()
                .map(|(k, v)| {
                    let v = v.into_iter().map(|(a, b)| (a.to_owned(), b)).collect();
                    (k.to_owned(), Value::Object(v))
                })
                .collect(),
        )
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn data_sources_decoder_never_panics_on_bytes(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        if let Ok(ds) = parse_data_sources(&bytes) {
            let _ = map_sources("General", &[ds], None);
        }
    }

    #[test]
    fn data_sources_decoder_never_panics_on_json(value in any_json()) {
        let bytes = serde_json::to_vec(&value).unwrap_or_default();
        if let Ok(ds) = parse_data_sources(&bytes) {
            let _ = map_sources("General", &[ds], None);
        }
    }

    #[test]
    fn mapping_never_panics_on_shaped_input(
        value in data_sources(),
        creds in credentials_json(),
        iv in any::<[u8; 16]>(),
    ) {
        let bytes = serde_json::to_vec(&value).unwrap_or_default();
        let ds = parse_data_sources(&bytes);
        prop_assert!(ds.is_ok());
        let Ok(ds) = ds else { return Ok(()) };
        let plain = serde_json::to_vec(&creds).unwrap_or_default();
        let enc = common::encrypt_like_dbeaver(&plain, iv);
        let p = map_sources("General", &[ds], Some(decrypt_credentials(&enc)));
        prop_assert_eq!(p.connections.len(), value["connections"].as_object().map_or(0, Map::len));
    }

    #[test]
    fn credentials_decoder_never_panics_on_bytes(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
        let _ = decrypt_credentials(&bytes);
    }

    #[test]
    fn credentials_decoder_never_panics_on_encrypted_bytes(
        plain in prop::collection::vec(any::<u8>(), 0..256),
        iv in any::<[u8; 16]>(),
    ) {
        let _ = decrypt_credentials(&common::encrypt_like_dbeaver(&plain, iv));
    }

    #[test]
    fn credentials_decoder_never_panics_on_encrypted_json(value in any_json(), iv in any::<[u8; 16]>()) {
        let plain = serde_json::to_vec(&value).unwrap_or_default();
        let result = decrypt_credentials(&common::encrypt_like_dbeaver(&plain, iv));
        // Any JSON object decodes; anything else is a structure error, never a panic.
        prop_assert_eq!(result.is_ok(), value.is_object());
    }

    #[test]
    fn credentials_round_trip(user in "[a-z_]{1,12}", password in "\\PC{1,40}", iv in any::<[u8; 16]>()) {
        use ninesql_secrets::ExposeSecret;
        let plain = serde_json::to_vec(&json!({"id": {"#connection": {"user": user, "password": password}}}))
            .unwrap_or_default();
        let creds = decrypt_credentials(&common::encrypt_like_dbeaver(&plain, iv));
        prop_assert!(creds.is_ok());
        let Ok(creds) = creds else { return Ok(()) };
        let section = &creds.connections["id"].sections["#connection"];
        prop_assert_eq!(section.user.as_deref(), Some(user.as_str()));
        prop_assert!(section.password.as_ref().is_some_and(|p| p.expose_secret() == password));
    }
}

#[test]
fn deep_nesting_is_rejected_without_overflow() {
    let deep = "[".repeat(100_000);
    assert!(parse_data_sources(deep.as_bytes()).is_err());
    let deep_obj = format!("{}{}", r#"{"a":"#.repeat(100_000), "1");
    assert!(parse_data_sources(deep_obj.as_bytes()).is_err());
    let enc = common::encrypt_like_dbeaver(deep_obj.as_bytes(), [0; 16]);
    assert!(decrypt_credentials(&enc).is_err());
}
