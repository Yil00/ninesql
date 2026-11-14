//! I1: decrypted DBeaver secrets never appear in `Debug` / `Display` output, report entries,
//! errors or logs. Logs are captured with a recording `tracing` subscriber at TRACE level.
//!
//! Every fake secret of the fixtures contains [`common::SECRET_MARKER`]; synthetic inputs below
//! use the same marker for secrets planted in odd places (URL user info, driver properties,
//! plaintext legacy fields, unknown keys).

mod common;

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use common::SECRET_MARKER;
use ninesql_dbeaver::mapping::{ProjectImport, map_sources};
use ninesql_dbeaver::{decrypt_credentials, import_project, open_folder, parse_data_sources};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

/// Records every event and span field as text.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<String>>);

struct Text<'a>(&'a mut String);

impl Visit for Text<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, " {}={:?}", field.name(), value);
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        let _ = write!(self.0, " {}={}", field.name(), value);
    }
}

impl Recorder {
    fn text(&self) -> String {
        self.0.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

impl Subscriber for Recorder {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, span: &Attributes<'_>) -> Id {
        if let Ok(mut s) = self.0.lock() {
            s.push_str(span.metadata().name());
            span.record(&mut Text(&mut s));
            s.push('\n');
        }
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, values: &Record<'_>) {
        if let Ok(mut s) = self.0.lock() {
            values.record(&mut Text(&mut s));
            s.push('\n');
        }
    }
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        if let Ok(mut s) = self.0.lock() {
            s.push_str(event.metadata().target());
            event.record(&mut Text(&mut s));
            s.push('\n');
        }
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

/// Everything a user or a log could see of an import result.
fn visible_text(p: &ProjectImport) -> String {
    let mut out = format!("{p:?}\n");
    for i in &p.issues {
        let _ = writeln!(out, "{i} {i:?}");
    }
    for c in &p.connections {
        let _ = writeln!(out, "{c:?}");
        for i in &c.issues {
            let _ = writeln!(out, "{i} {i:?}");
        }
    }
    out
}

#[test]
fn fixture_secrets_never_leak() {
    let recorder = Recorder::default();
    let mut seen = String::new();
    tracing::subscriber::with_default(recorder.clone(), || {
        for (v, ws) in common::fixture_workspaces() {
            let w = open_folder(&ws).unwrap_or_else(|e| panic!("[{v}] {e}"));
            let _ = write!(seen, "{w:?}");
            let p = import_project(&w.projects[0]);
            assert_eq!(p.secret_count(), 8, "[{v}]");
            seen.push_str(&visible_text(&p));

            // The decrypted file itself.
            let bytes = std::fs::read(
                w.projects[0]
                    .credentials_file
                    .as_ref()
                    .unwrap_or_else(|| panic!("[{v}] no credentials file")),
            )
            .unwrap_or_default();
            let creds = decrypt_credentials(&bytes);
            let _ = write!(seen, "{creds:?}");
        }
    });
    let logs = recorder.text();
    assert!(
        logs.contains("DBeaver project mapped"),
        "the recorder did not capture the importer's logs"
    );
    assert!(
        !seen.contains(SECRET_MARKER),
        "a secret reached Debug / Display output"
    );
    assert!(!logs.contains(SECRET_MARKER), "a secret reached the logs");
    // User names are not secrets, but the logs should not carry connection details either.
    assert!(!logs.contains("nine_user") && !logs.contains("example.invalid"));
}

const PLANTED: &str = r##"{
  "connections": {
    "postgres-jdbc-1-a": {
      "provider": "postgresql", "driver": "postgres-jdbc", "name": "planted",
      "save-password": true,
      "secret-unknown-key": "fixture-unknown-not-a-secret",
      "configuration": {
        "url": "jdbc:postgresql://nine_user:fixture-url-not-a-secret@db.example.test:5432/nine?password=fixture-param-not-a-secret&sslpassword=fixture-ssl-not-a-secret",
        "configurationType": "URL",
        "user": "nine_user",
        "password": "fixture-plain-not-a-secret",
        "auth-token": "fixture-token-not-a-secret",
        "properties": {
          "password": "fixture-prop-not-a-secret",
          "apiKey": "fixture-apikey-not-a-secret",
          "nested": {"x": "fixture-nested-not-a-secret"}
        },
        "handlers": {
          "ssh_tunnel": {
            "type": "TUNNEL", "enabled": true, "save-password": true,
            "user": "nine_ssh", "password": "fixture-sshplain-not-a-secret",
            "properties": {"host": "bastion.example.test", "authType": "PASSWORD",
                           "passphrase": "fixture-sshprop-not-a-secret"}
          },
          "postgre_ssl": {
            "type": "CONFIG", "enabled": true,
            "properties": {"sslMode": "require", "ssl.key.password": "fixture-tls-not-a-secret"}
          }
        }
      }
    },
    "broken": "fixture-broken-not-a-secret"
  },
  "top-secret-password": "fixture-top-not-a-secret"
}"##;

#[test]
fn planted_secrets_never_leak() {
    let recorder = Recorder::default();
    let mut seen = String::new();
    tracing::subscriber::with_default(recorder.clone(), || {
        let ds = parse_data_sources(PLANTED.as_bytes()).unwrap_or_else(|e| panic!("{e}"));
        let _ = write!(seen, "{ds:?}");
        let creds_plain = br##"{"postgres-jdbc-1-a":{"#connection":{"password":"fixture-c-not-a-secret","odd":"fixture-odd-not-a-secret"},"network/postgre_ssl":{"password":"fixture-h-not-a-secret"}},"ghost":{"#connection":{"password":"fixture-g-not-a-secret"}}}"##;
        let creds = decrypt_credentials(&common::encrypt_like_dbeaver(creds_plain, [7; 16]));
        let _ = write!(seen, "{creds:?}");
        let p = map_sources("General", &[ds], Some(creds));
        seen.push_str(&visible_text(&p));
        // Only the saved password and the plaintext SSH password are moved.
        let c = p.connections[0]
            .imported()
            .unwrap_or_else(|| panic!("planted connection not imported: {:?}", p.connections[0]));
        assert_eq!(c.secrets.len(), 2, "{:?}", c.secrets);
        assert!(c.properties.is_empty(), "{:?}", c.properties.keys());

        // Errors of every decoder.
        for bad in [
            &b"{\"a\": fixture-err-not-a-secret"[..],
            b"[\"fixture-arr-not-a-secret\"]",
        ] {
            let _ = write!(seen, "{:?}", parse_data_sources(bad).err());
            let _ = write!(seen, "{:?}", decrypt_credentials(bad).err());
        }
        let wrong_key = common::encrypt_with_key(creds_plain, [1; 16], [2; 16]);
        let _ = write!(seen, "{:?}", decrypt_credentials(&wrong_key).err());
    });
    let logs = recorder.text();
    assert!(
        !seen.contains(SECRET_MARKER),
        "a planted secret reached Debug / Display output"
    );
    assert!(
        !logs.contains(SECRET_MARKER),
        "a planted secret reached the logs"
    );
}
