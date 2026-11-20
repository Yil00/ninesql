//! ADR-0029 §3.2 (the audit before stderr logs at `warn`): `info`, `warn` and `error` events of
//! the core, the storage, the secrets and the SSH crates carry an error kind, never an error
//! message that may quote a path under the user's home (their account name), an engine message
//! or a value. Representative failures are provoked with a marker in every place a message
//! could pick it up (the XDG directories, a user name the server echoes, a value in a file) and
//! the events at `info` and above are checked for it.
//!
//! One test per process: the capture is the global subscriber, so the runtime's worker threads
//! and the history writer are captured too.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).

mod support;

use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ninesql_core::driver_api::Secret;
use ninesql_core::{
    Auth, ConnectionTypeId, ConnectionTypes, CoreConfig, CoreServices, DriverRegistry,
    KnownHostsFiles, OpenRequest, SecretSlot, SecretSource,
};
use ninesql_secrets::SecretString;
use ninesql_storage::{Paths, ProjectName, XdgDirs};
use support::*;

/// In the XDG root: every path under it carries this marker, as a home directory carries the
/// user's account name.
const PATH_MARKER: &str = "home_marker_3f9a";
/// A user name the mock server quotes in its authentication error, as engines do.
const ENGINE_MARKER: &str = "engine_marker_b71c";
/// A value inside a damaged `settings.toml`.
const VALUE_MARKER: &str = "value_marker_d04e";

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<(tracing::Level, String)>>>);

struct Visitor<'a>(&'a mut String);

impl tracing::field::Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, " {}={:?}", field.name(), value);
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = event.metadata().target().to_owned();
        event.record(&mut Visitor(&mut line));
        self.0
            .lock()
            .unwrap()
            .push((*event.metadata().level(), line));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn config(root: &Path, driver: Arc<dyn ninesql_core::driver_api::Driver>) -> CoreConfig {
    let paths = Paths::new(&XdgDirs {
        config_home: root.join("config"),
        data_home: root.join("data"),
        state_home: root.join("state"),
    });
    let mut registry = DriverRegistry::new();
    registry.register(driver);
    let mut c = CoreConfig::new(paths.clone(), ProjectName::default_project(), registry);
    c.manager.known_hosts = KnownHostsFiles {
        own: paths.data_dir().join("known_hosts"),
        user: None,
    };
    c.manager.workers = 2;
    c.manager.dbeaver_roots = ninesql_core::DbeaverRoots::none();
    c.use_secret_service = false;
    c.watch_desktop_lock = false;
    c
}

#[test]
fn info_and_above_carry_kinds_not_messages() {
    let capture = Capture::default();
    tracing::subscriber::set_global_default(capture.clone()).unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(PATH_MARKER);
    std::fs::create_dir(&root).unwrap();
    let paths = Paths::new(&XdgDirs {
        config_home: root.join("config"),
        data_home: root.join("data"),
        state_home: root.join("state"),
    });

    // Failures at startup whose errors name a path under the root:
    // - `settings.toml` is a directory, so it cannot be read (a damaged one is read leniently);
    std::fs::create_dir_all(paths.settings_file()).unwrap();
    std::fs::set_permissions(paths.config_dir(), std::fs::Permissions::from_mode(0o755)).unwrap();
    // - `history.sqlite` is a directory, so the history cannot open;
    std::fs::create_dir_all(paths.history_file()).unwrap();
    // - `navigator.json` is damaged, cut inside a value.
    let project = paths.project(&ProjectName::default_project());
    std::fs::create_dir_all(project.dir()).unwrap();
    std::fs::write(
        project.navigator_file(),
        format!("{{\"schema_version\": 1, \"connections\": \"{VALUE_MARKER}"),
    )
    .unwrap();

    let driver = mock_driver().with_password(Secret::new("right-password".into()));
    let services = ui(async {
        tokio::time::timeout(
            Duration::from_secs(20),
            CoreServices::start(config(&root, Arc::new(driver))),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap()
    });
    assert!(services.history_error().is_some(), "the history must fail");

    // An engine message that quotes the user name: the open fails with the server's message.
    let types = ConnectionTypes::default();
    let mut c = mock_connection(ConnectionTypeId::dev());
    c.auth = Auth::Password {
        username: ENGINE_MARKER.into(),
        secret: SecretSource::Prompt,
    };
    let outcome = ui(async {
        tokio::time::timeout(
            Duration::from_secs(20),
            services.sessions().open(
                OpenRequest::new(c, &types)
                    .with_secret(SecretSlot::Auth, SecretString::from("wrong".to_owned())),
            ),
        )
        .await
        .unwrap()
        .unwrap()
    });
    let err = outcome.expect_err("a wrong password must fail");
    assert!(
        err.to_string().contains(ENGINE_MARKER),
        "the UI message keeps the engine's text: {err}"
    );
    services.shutdown(Duration::from_secs(5));

    let events = capture.0.lock().unwrap().clone();
    let shown: Vec<&String> = events
        .iter()
        .filter(|(level, _)| *level <= tracing::Level::INFO)
        .map(|(_, line)| line)
        .collect();
    // The failures were logged (the test would prove nothing otherwise).
    for expected in [
        "the query history is not available in this session",
        "settings.toml cannot be read",
        "navigator.json cannot be read",
        "connection failed",
        "tightening permissions of a NineSQL directory",
    ] {
        assert!(
            shown.iter().any(|l| l.contains(expected)),
            "no `{expected}` event in {shown:#?}"
        );
    }
    for line in &shown {
        for marker in [PATH_MARKER, ENGINE_MARKER, VALUE_MARKER] {
            assert!(!line.contains(marker), "`{marker}` leaked into: {line}");
        }
    }
}
