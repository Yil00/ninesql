//! Driver runtime and session manager (P1-D) against the mock driver: open / execute / stream /
//! cancel / `Busy` / close lifecycle, read-only and production confirmation flows, secret
//! resolution, reserved properties and the SSH wiring.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).

mod support;

use std::sync::Arc;
use std::time::Duration;

use ninesql_core::driver_api::mock::MOCK_RESERVED_PROPERTY;
use ninesql_core::driver_api::{
    DriverError, FetchBudget, ResultItem, Secret, SecurityWarning as ApiWarning, StatementClass,
};
use ninesql_core::{
    Auth, CloseReason, ConfirmReason, ConnectionError, ConnectionType, ConnectionTypeId,
    ConnectionTypes, Connections, CoreError, CoreEvent, ExecEvent, ExecuteRequest, HostKeyPolicy,
    Next, OpenOutcome, OpenRequest, SecretSlot, SecretSource, SessionWarning, SshAuth, SshHop,
    SshTunnel, Workspace,
};
use ninesql_secrets::{SecretStore as _, SecretString, StoreKind};
use ninesql_storage::{Paths, ProjectName, XdgDirs};
use support::*;

fn rows_of(events: &[ExecEvent]) -> usize {
    events
        .iter()
        .map(|e| match e {
            ExecEvent::Item(ResultItem::Rows(b)) => b.num_rows(),
            _ => 0,
        })
        .sum()
}

#[test]
fn open_execute_stream_close_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let wrap = Wrap::new(mock_driver());
    let stats = wrap.mock.stats();
    let (manager, _) = manager(Arc::new(wrap.clone()), tmp.path());
    let mut events = manager.take_events().unwrap();
    assert!(manager.take_events().is_none(), "one receiver");
    let types = ConnectionTypes::default();
    ui(async {
        let mut c = mock_connection(ConnectionTypeId::dev());
        c.properties.insert("application_name".into(), "x".into());
        c.init_sql = vec!["SET search_path TO app".into()];
        let h = open_ok(&manager, c.clone(), &types).await;
        let info = h.info();
        assert_eq!(info.connection, c.id);
        assert_eq!(info.server_version.as_deref(), Some("mock 1.0"));
        assert!(!info.read_only);
        assert!(info.auto_commit);
        assert!(info.warnings.is_empty(), "{:?}", info.warnings);
        let seen = wrap.last_seen();
        assert_eq!(
            seen.endpoint,
            ninesql_core::driver_api::Endpoint::tcp("mock.example.test", 5000)
        );
        assert_eq!(seen.server_name, None);
        assert_eq!(seen.property_keys, ["application_name"]);
        assert_eq!(seen.init_statements, 1);
        assert!(!seen.production);
        assert_eq!(manager.sessions_of(c.id).len(), 1);
        assert!(matches!(
            events.recv().await,
            Some(CoreEvent::SessionOpened { session, .. }) if session == h.id()
        ));

        let req = ExecuteRequest::sql("SELECT * FROM numbers(1000)")
            .with_budget(FetchBudget::default().with_batch_rows(100));
        let ev = collect(h.execute(req)).await;
        assert!(matches!(ev[0], ExecEvent::Started { execution: Some(_) }));
        assert!(matches!(ev[1], ExecEvent::Item(ResultItem::ResultSet(_))));
        assert_eq!(rows_of(&ev), 1000);
        assert!(matches!(
            terminal(&ev),
            ExecEvent::Finished { rows: 1000, .. }
        ));

        // Statements of a session run in order.
        let a = h.execute(ExecuteRequest::sql("SELECT * FROM numbers(3)"));
        let b = h.execute(ExecuteRequest::sql("SELECT * FROM numbers(5)"));
        assert_eq!(rows_of(&collect(a).await), 3);
        assert_eq!(rows_of(&collect(b).await), 5);

        // Transaction commands report the state.
        let st = h.set_auto_commit(false).await.unwrap().unwrap();
        assert!(!st.auto_commit);
        assert!(h.ping().await.unwrap().is_ok());

        // A query error is typed and the session goes on.
        let ev = run(&h, "SELECT error()").await;
        assert!(matches!(
            terminal(&ev),
            ExecEvent::Failed(CoreError::Driver(DriverError::Query { .. }))
        ));

        h.close().await.unwrap().unwrap();
        assert!(h.close().await.unwrap().is_ok(), "idempotent");
        // The auto-commit switch above reported its new transaction state (P3-C).
        assert!(matches!(
            events.recv().await,
            Some(CoreEvent::TransactionChanged { session, state, .. })
                if session == h.id() && !state.auto_commit
        ));
        match tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
            Ok(Some(CoreEvent::SessionClosed {
                session, reason, ..
            })) => {
                assert_eq!(session, h.id());
                assert_eq!(reason, CloseReason::Requested);
            }
            other => panic!("{other:?}"),
        }
        assert!(h.is_closed());
        assert!(manager.session(h.id()).is_none());
        let ev = run(&h, "SELECT * FROM numbers(1)").await;
        assert!(matches!(
            terminal(&ev),
            ExecEvent::Failed(CoreError::SessionClosed)
        ));
    });
    assert_eq!(stats.connects(), 1);
    manager.shutdown(Duration::from_secs(5));
}

#[test]
fn results_stop_at_the_budget_and_backpressure_bounds_the_fetch() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = mock_driver();
    let stats = mock.stats();
    let (manager, _) = manager(Arc::new(mock), tmp.path());
    let types = ConnectionTypes::default();
    ui(async {
        let h = open_ok(&manager, mock_connection(ConnectionTypeId::dev()), &types).await;
        let budget = FetchBudget::default()
            .with_batch_rows(100)
            .with_max_rows(Some(250));
        let ev = collect(
            h.execute(ExecuteRequest::sql("SELECT * FROM numbers(1000000)").with_budget(budget)),
        )
        .await;
        assert_eq!(rows_of(&ev), 250);
        assert!(
            ev.iter()
                .any(|e| matches!(e, ExecEvent::Item(ResultItem::Truncated(_))))
        );
        assert!(
            stats.rows_generated() < 1000,
            "the fetch stopped at the budget"
        );

        // A consumer that does not read: the session task waits on the bounded channel instead
        // of fetching everything.
        let generated = stats.rows_generated();
        let mut slow = h.execute(
            ExecuteRequest::sql("SELECT * FROM numbers(1000000)")
                .with_budget(FetchBudget::default().with_batch_rows(10)),
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ahead = stats.rows_generated() - generated;
        assert!(ahead <= 200, "fetched {ahead} rows ahead of the consumer");
        assert!(matches!(
            slow.try_next(),
            Next::Ready(ExecEvent::Started { .. })
        ));
        drop(slow);
        // Dropping the execution cancels it; the session is usable again.
        let ev = run(&h, "SELECT * FROM numbers(2)").await;
        assert_eq!(rows_of(&ev), 2);
    });
}

#[test]
fn cancel_reaches_the_server_and_the_next_statement_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = mock_driver();
    let stats = mock.stats();
    let (manager, _) = manager(Arc::new(mock), tmp.path());
    let types = ConnectionTypes::default();
    ui(async {
        let h = open_ok(&manager, mock_connection(ConnectionTypeId::dev()), &types).await;
        let mut e = h.execute(
            ExecuteRequest::sql("SELECT * FROM slow_numbers(1000, 50)")
                .with_budget(FetchBudget::default().with_batch_rows(5)),
        );
        // Wait for the first rows, then stop.
        loop {
            if let Some(ExecEvent::Item(ResultItem::Rows(_))) = e.next().await {
                break;
            }
        }
        h.cancel_current();
        let rest = collect(e).await;
        assert!(
            matches!(
                terminal(&rest),
                ExecEvent::Failed(CoreError::Driver(DriverError::Cancelled))
            ),
            "{rest:?}"
        );
        assert!(stats.server_cancels() >= 1);
        let ev = run(&h, "SELECT * FROM numbers(4)").await;
        assert_eq!(rows_of(&ev), 4);

        // A timeout ends the statement the same way.
        let ev = collect(
            h.execute(
                ExecuteRequest::sql("SELECT * FROM slow_numbers(1000, 50)")
                    .with_timeout(Some(Duration::from_millis(120)))
                    .with_budget(FetchBudget::default().with_batch_rows(5)),
            ),
        )
        .await;
        assert!(matches!(
            terminal(&ev),
            ExecEvent::Failed(CoreError::Driver(DriverError::Timeout(_)))
        ));
    });
}

#[test]
fn busy_after_cancel_waits_then_reports_still_cancelling() {
    let tmp = tempfile::tempdir().unwrap();
    let wrap = Wrap::new(mock_driver());
    let (manager, _) = manager(Arc::new(wrap.clone()), tmp.path());
    let types = ConnectionTypes::default();
    ui(async {
        let h = open_ok(&manager, mock_connection(ConnectionTypeId::dev()), &types).await;
        // The server confirms the end of the cancelled statement after 150 ms: the next
        // statement waits instead of failing with `Busy`.
        wrap.busy_for(Duration::from_millis(150));
        let ev = run(&h, "SELECT * FROM numbers(3)").await;
        assert!(
            matches!(ev[0], ExecEvent::WaitingForPreviousCancel),
            "{ev:?}"
        );
        assert!(matches!(ev[1], ExecEvent::Started { .. }));
        assert_eq!(rows_of(&ev), 3);

        // Never confirmed within `busy_wait` (400 ms): a typed "still cancelling" error.
        wrap.busy_for(Duration::from_secs(30));
        let ev = run(&h, "SELECT * FROM numbers(3)").await;
        assert!(matches!(ev[0], ExecEvent::WaitingForPreviousCancel));
        assert!(matches!(
            terminal(&ev),
            ExecEvent::Failed(CoreError::StillCancelling)
        ));

        // A queued statement whose execution was dropped is never sent.
        wrap.busy_for(Duration::ZERO);
        let first = h.execute(ExecuteRequest::sql("SELECT * FROM slow_numbers(20, 20)"));
        let dropped = h.execute(ExecuteRequest::sql("INSERT INTO never VALUES (1)"));
        drop(dropped);
        assert!(matches!(
            terminal(&collect(first).await),
            ExecEvent::Finished { .. }
        ));
        assert_eq!(rows_of(&run(&h, "SELECT * FROM numbers(1)").await), 1);

        // Cancelling while waiting stops at once.
        wrap.busy_for(Duration::from_secs(30));
        let e = h.execute(ExecuteRequest::sql("SELECT * FROM numbers(3)"));
        e.cancel();
        let ev = collect(e).await;
        assert!(matches!(
            terminal(&ev),
            ExecEvent::Failed(CoreError::Driver(DriverError::Cancelled))
        ));
        wrap.busy_for(Duration::ZERO);
        assert_eq!(rows_of(&run(&h, "SELECT * FROM numbers(1)").await), 1);
    });
}

#[test]
fn read_only_connections_get_engine_side_read_only_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = mock_driver();
    let stats = mock.stats();
    let (manager, _) = manager(Arc::new(mock), tmp.path());
    let mut events = manager.take_events().unwrap();
    let types = ConnectionTypes::default();
    ui(async {
        let mut c = mock_connection(ConnectionTypeId::dev());
        c.read_only = Some(true);
        let h = open_ok(&manager, c, &types).await;
        assert!(h.info().read_only);
        let _ = events.recv().await;

        let ev = run(&h, "INSERT INTO t VALUES (1)").await;
        assert!(matches!(
            terminal(&ev),
            ExecEvent::Failed(CoreError::Driver(DriverError::ReadOnly(_)))
        ));
        // A write the classifier cannot see is rejected by the engine.
        let ev = run(&h, "SELECT mock_write()").await;
        assert!(matches!(
            terminal(&ev),
            ExecEvent::Failed(CoreError::Driver(DriverError::ReadOnly(_)))
        ));
        assert_eq!(stats.server_rejected_writes(), 1);
        assert_eq!(rows_of(&run(&h, "SELECT * FROM numbers(2)").await), 2);

        // Lifting read-only is detected by the driver; the core then drops the session.
        let _ = run(&h, "SELECT mock_lift_read_only()").await;
        let ev = run(&h, "SELECT * FROM numbers(2)").await;
        assert!(matches!(
            terminal(&ev),
            ExecEvent::Failed(CoreError::Driver(DriverError::Closed(_)))
        ));
        match tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
            Ok(Some(CoreEvent::SessionClosed {
                reason: CloseReason::Lost(DriverError::Closed(_)),
                ..
            })) => {}
            other => panic!("{other:?}"),
        }
    });
}

#[test]
fn a_driver_that_ignores_read_only_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let mut wrap = Wrap::new(mock_driver());
    wrap.ignore_read_only = true;
    let (manager, _) = manager(Arc::new(wrap.clone()), tmp.path());
    let mut ro = ConnectionType::test();
    ro.id = ConnectionTypeId::new("readers").unwrap();
    ro.read_only = true;
    let mut types = ConnectionTypes::default();
    types.upsert(ro);
    ui(async {
        let c = mock_connection(ConnectionTypeId::new("readers").unwrap());
        let err = open(&manager, OpenRequest::new(c, &types))
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::ReadOnlyNotEnforced));
        assert!(wrap.last_seen().read_only, "the core asked for read-only");
    });
}

#[test]
fn production_requires_a_confirmation_token_for_destructive_statements() {
    let tmp = tempfile::tempdir().unwrap();
    let wrap = Wrap::new(mock_driver());
    let (manager, _) = manager(Arc::new(wrap.clone()), tmp.path());
    let types = ConnectionTypes::default();
    ui(async {
        let h = open_ok(&manager, mock_connection(ConnectionTypeId::prod()), &types).await;
        let seen = wrap.last_seen();
        assert!(seen.production);
        assert!(!seen.auto_commit, "production defaults to manual commit");
        assert!(h.info().policy.production);
        assert!(!h.info().policy.log_query_text);

        // Reads need no confirmation.
        assert_eq!(rows_of(&run(&h, "SELECT * FROM numbers(2)").await), 2);

        let ev = run(&h, "DROP TABLE orders").await;
        let ExecEvent::NeedsConfirmation(req) = terminal(&ev) else {
            panic!("{ev:?}");
        };
        assert_eq!(ev.len(), 1, "nothing was sent");
        assert_eq!(req.class, StatementClass::Destructive);
        assert_eq!(req.reason, ConfirmReason::Destructive);
        assert!(req.production);
        assert_eq!(req.connection_name, "Mock");
        assert_eq!(
            req.statement, "DROP TABLE orders",
            "the UI shows what will run"
        );
        assert_eq!(req.params, 0);
        let token = req.token;

        // The token is bound to the exact text.
        let ev =
            collect(h.execute(ExecuteRequest::sql("DROP TABLE customers").confirmed(token))).await;
        assert!(matches!(
            terminal(&ev),
            ExecEvent::Failed(CoreError::InvalidConfirmation)
        ));
        // ...and was consumed by that attempt.
        let ev =
            collect(h.execute(ExecuteRequest::sql("DROP TABLE orders").confirmed(token))).await;
        assert!(matches!(
            terminal(&ev),
            ExecEvent::Failed(CoreError::InvalidConfirmation)
        ));

        let ev = run(&h, "DROP TABLE orders").await;
        let ExecEvent::NeedsConfirmation(req) = terminal(&ev) else {
            panic!("{ev:?}");
        };
        let token = req.token;
        let ev =
            collect(h.execute(ExecuteRequest::sql("DROP TABLE orders").confirmed(token))).await;
        assert!(
            ev.iter()
                .any(|e| matches!(e, ExecEvent::Item(ResultItem::UpdateCount(Some(1)))))
        );
        assert!(matches!(terminal(&ev), ExecEvent::Finished { .. }));
        // Single use.
        let ev =
            collect(h.execute(ExecuteRequest::sql("DROP TABLE orders").confirmed(token))).await;
        assert!(matches!(
            terminal(&ev),
            ExecEvent::Failed(CoreError::InvalidConfirmation)
        ));

        // A token of another session is worthless here.
        let other = open_ok(&manager, mock_connection(ConnectionTypeId::prod()), &types).await;
        let ev = run(&other, "TRUNCATE orders").await;
        let ExecEvent::NeedsConfirmation(req) = terminal(&ev) else {
            panic!("{ev:?}");
        };
        let ev =
            collect(h.execute(ExecuteRequest::sql("TRUNCATE orders").confirmed(req.token))).await;
        assert!(matches!(
            terminal(&ev),
            ExecEvent::Failed(CoreError::InvalidConfirmation)
        ));

        // Unrecognised statements count as destructive (ADR-0007 §3).
        let ev = run(&h, "FROBNICATE everything").await;
        assert!(matches!(
            terminal(&ev),
            ExecEvent::NeedsConfirmation(r) if r.class == StatementClass::Unknown
        ));
        // A plain write on production is not destructive.
        let ev = run(&h, "INSERT INTO t VALUES (1)").await;
        assert!(matches!(terminal(&ev), ExecEvent::Finished { .. }));
    });
}

#[test]
fn confirm_every_write_types_confirm_inserts() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), tmp.path());
    let mut careful = ConnectionType::development();
    careful.id = ConnectionTypeId::new("careful").unwrap();
    careful.confirm_every_write = true;
    let mut types = ConnectionTypes::default();
    types.upsert(careful);
    ui(async {
        let h = open_ok(
            &manager,
            mock_connection(ConnectionTypeId::new("careful").unwrap()),
            &types,
        )
        .await;
        let ev = run(&h, "INSERT INTO t VALUES (1)").await;
        assert!(matches!(
            terminal(&ev),
            ExecEvent::NeedsConfirmation(r) if r.reason == ConfirmReason::Write && !r.production
        ));
        assert!(matches!(
            terminal(&run(&h, "SELECT * FROM numbers(1)").await),
            ExecEvent::Finished { .. }
        ));
        // A dev connection confirms nothing.
        let dev = open_ok(&manager, mock_connection(ConnectionTypeId::dev()), &types).await;
        assert!(matches!(
            terminal(&run(&dev, "DROP TABLE t").await),
            ExecEvent::Finished { .. }
        ));
    });
}

fn workspace(root: &std::path::Path) -> Workspace {
    let xdg = XdgDirs {
        config_home: root.join("config"),
        data_home: root.join("data"),
        state_home: root.join("state"),
    };
    Workspace::open(Paths::new(&xdg), ProjectName::default_project()).unwrap()
}

#[test]
fn saved_and_prompted_secrets_reach_the_driver() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = mock_driver().with_password(Secret::new("Corr3ct-Horse".into()));
    let stats = mock.stats();
    let wrap = Wrap::new(mock);
    let (manager, keyring) = manager(Arc::new(wrap.clone()), tmp.path());
    let ws = workspace(tmp.path());
    let types = ConnectionTypes::default();
    let mut c = mock_connection(ConnectionTypeId::dev());
    c.auth = Auth::Password {
        username: "app".into(),
        secret: SecretSource::Prompt,
    };
    ui(async {
        // "Ask every time": the core asks before touching the network.
        let out = open(&manager, OpenRequest::new(c.clone(), &types))
            .await
            .unwrap();
        let OpenOutcome::NeedsSecrets(prompts) = out else {
            panic!("{out:?}");
        };
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].slot, SecretSlot::Auth);
        assert_eq!(prompts[0].label, "Password of app for Mock");
        assert_eq!(stats.connects(), 0);

        let wrong = OpenRequest::new(c.clone(), &types)
            .with_secret(SecretSlot::Auth, SecretString::from("nope".to_owned()));
        assert!(format!("{wrong:?}").contains("Auth") && !format!("{wrong:?}").contains("nope"));
        assert!(matches!(
            open(&manager, wrong).await,
            Err(CoreError::Driver(DriverError::Auth(_)))
        ));
        let right = OpenRequest::new(c.clone(), &types).with_secret(
            SecretSlot::Auth,
            SecretString::from("Corr3ct-Horse".to_owned()),
        );
        assert!(matches!(
            open(&manager, right).await,
            Ok(OpenOutcome::Opened(_))
        ));
        assert!(wrap.last_seen().has_password);

        // Saved in the keyring: no prompt.
        let mut connections = Connections::default();
        let mut stores = ninesql_secrets::SecretStores::new();
        stores.register(keyring.clone());
        let mut saved = c.clone();
        saved.id = ninesql_core::ConnectionId::new_random();
        ws.create_connection(
            &mut connections,
            saved.clone(),
            vec![(
                SecretSlot::Auth,
                SecretString::from("Corr3ct-Horse".to_owned()),
            )],
            StoreKind::Keyring,
            &stores,
        )
        .unwrap();
        let saved = connections.get(saved.id).unwrap().clone();
        assert!(matches!(
            saved.auth,
            Auth::Password {
                secret: SecretSource::Stored(_),
                ..
            }
        ));
        let h = open_ok(&manager, saved.clone(), &types).await;
        assert!(h.info().warnings.is_empty());

        // A saved secret that disappeared is a typed error naming the field.
        assert!(keyring.delete(&saved.secret_refs()[0].2.id).unwrap());
        assert!(matches!(
            open(&manager, OpenRequest::new(saved, &types)).await,
            Err(CoreError::Secret {
                slot: SecretSlot::Auth,
                ..
            })
        ));
    });
}

#[test]
fn reserved_properties_are_refused_before_save_and_before_connect() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = mock_driver();
    let stats = mock.stats();
    let (manager, _) = manager(Arc::new(mock), tmp.path());
    let types = ConnectionTypes::default();
    let mut c = mock_connection(ConnectionTypeId::dev());
    c.properties.insert("application_name".into(), "x".into());
    manager.registry().validate_connection(&c).unwrap();

    // Accepted by the shared rule, reserved by this driver only.
    assert!(ninesql_core::properties::reserved(MOCK_RESERVED_PROPERTY).is_none());
    c.properties.insert("Mock-Backdoor".into(), "1".into());
    let err = manager.registry().validate_connection(&c).unwrap_err();
    assert!(matches!(
        err,
        CoreError::InvalidConnection(ConnectionError::ReservedProperty { ref key, .. }) if key == "Mock-Backdoor"
    ));
    assert!(!err.to_string().contains("\"1\""));
    ui(async {
        let err = open(&manager, OpenRequest::new(c.clone(), &types))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            CoreError::InvalidConnection(ConnectionError::ReservedProperty { .. })
        ));
    });
    assert_eq!(stats.connects(), 0);

    // Unknown engines and disabled connections.
    let mut other = mock_connection(ConnectionTypeId::dev());
    other.engine = ninesql_core::EngineId::new("postgresql").unwrap();
    assert!(matches!(
        manager.registry().validate_connection(&other),
        Err(CoreError::NoDriver(_))
    ));
    let mut disabled = mock_connection(ConnectionTypeId::dev());
    disabled.enabled = false;
    ui(async {
        assert!(matches!(
            open(&manager, OpenRequest::new(other, &types)).await,
            Err(CoreError::NoDriver(_))
        ));
        assert!(matches!(
            open(&manager, OpenRequest::new(disabled, &types)).await,
            Err(CoreError::ConnectionDisabled)
        ));
    });
}

/// M2: settings the driver declares it refuses are refused when the connection is saved, and
/// again before any connect.
#[test]
fn settings_the_driver_refuses_fail_validation() {
    use ninesql_core::driver_api::{AuthKind, ConnectCapabilities};
    use ninesql_core::{ClientCertificate, TlsMode};
    let tmp = tempfile::tempdir().unwrap();
    let mut caps = mock_capabilities();
    let mut connect = ConnectCapabilities::default();
    connect.accepted_properties = Some(["application_name".to_owned()].into());
    connect.auth = [AuthKind::Password].into();
    connect.client_certificates = true;
    connect.encrypted_client_keys = false;
    caps.connect = connect;
    let mock = mock_driver().with_capabilities(caps);
    let stats = mock.stats();
    let (manager, _) = manager(Arc::new(mock), tmp.path());
    let types = ConnectionTypes::default();
    let valid = || {
        let mut c = mock_connection(ConnectionTypeId::dev());
        c.auth = Auth::Username {
            username: "nine".into(),
        };
        c.properties.insert("Application-Name".into(), "x".into());
        c
    };
    manager.registry().validate_connection(&valid()).unwrap();

    let field_of = |c: &ninesql_core::Connection| match manager.registry().validate_connection(c) {
        Err(CoreError::InvalidConnection(ConnectionError::InvalidField { field, .. })) => field,
        other => panic!("expected an invalid field, got {other:?}"),
    };
    // A property off the allowlist; its value is never echoed.
    let mut c = valid();
    c.properties.insert("fetch_size".into(), "value-xyz".into());
    assert_eq!(field_of(&c), "properties.fetch_size");
    assert!(
        !manager
            .registry()
            .validate_connection(&c)
            .unwrap_err()
            .to_string()
            .contains("value-xyz")
    );
    // An authentication kind the driver does not list.
    let mut c = valid();
    c.auth = Auth::Token {
        username: None,
        secret: SecretSource::Prompt,
    };
    assert_eq!(field_of(&c), "auth");
    let mut c = valid();
    c.auth = Auth::None;
    assert_eq!(field_of(&c), "auth");
    // verify-ca without a CA file (ADR-0019).
    let mut c = valid();
    c.tls.mode = TlsMode::VerifyCa;
    assert_eq!(field_of(&c), "tls.ca_file");
    c.tls.ca_file = Some("/etc/ninesql/ca.pem".into());
    manager.registry().validate_connection(&c).unwrap();
    // An encrypted client key.
    let mut c = valid();
    c.tls.client_certificate = Some(ClientCertificate {
        cert_file: "/c.pem".into(),
        key_file: "/k.pem".into(),
        passphrase: Some(SecretSource::Prompt),
    });
    assert_eq!(field_of(&c), "tls.client_certificate.passphrase");
    // TLS disabled: TLS files are not looked at.
    c.tls.mode = TlsMode::Disable;
    manager.registry().validate_connection(&c).unwrap();

    // The connect flow refuses before reaching the driver.
    let mut c = valid();
    c.properties.insert("fetch_size".into(), "1".into());
    ui(async {
        assert!(matches!(
            open(&manager, OpenRequest::new(c, &types)).await,
            Err(CoreError::InvalidConnection(
                ConnectionError::InvalidField { .. }
            ))
        ));
    });
    assert_eq!(stats.connects(), 0);
}

fn ssh_connection(port: u16) -> ninesql_core::Connection {
    let mut c = mock_connection(ConnectionTypeId::dev());
    let mut server = SshHop::new(
        "127.0.0.1",
        port,
        "deploy",
        SshAuth::Password {
            secret: SecretSource::Prompt,
        },
    );
    server.host_key_policy = HostKeyPolicy::Strict;
    c.ssh = Some(SshTunnel {
        server,
        jump_hosts: vec![],
    });
    c
}

/// A local port with nothing listening.
fn closed_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

#[test]
fn ssh_tunnel_is_opened_before_the_driver_and_its_failures_are_typed() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = mock_driver();
    let stats = mock.stats();
    let (manager, _) = manager(Arc::new(mock), tmp.path());
    let types = ConnectionTypes::default();
    let port = closed_port();
    let c = ssh_connection(port);
    ui(async {
        // The SSH password prompt comes first; nothing touches the network.
        let out = open(&manager, OpenRequest::new(c.clone(), &types))
            .await
            .unwrap();
        assert!(matches!(
            out,
            OpenOutcome::NeedsSecrets(ref p) if p.len() == 1 && p[0].slot == SecretSlot::SshServer
        ));
        let req = OpenRequest::new(c.clone(), &types).with_secret(
            SecretSlot::SshServer,
            SecretString::from("ssh-pw".to_owned()),
        );
        let err = open(&manager, req).await.unwrap_err();
        assert!(
            matches!(&err, CoreError::Ssh(ninesql_core::ssh::SshError::Connect { hop: 0, port: p, .. }) if *p == port),
            "{err:?}"
        );
        assert!(!err.to_string().contains("ssh-pw"));
    });
    assert_eq!(
        stats.connects(),
        0,
        "the driver is never reached without the tunnel"
    );
}

/// A fake SSH server that sends a banner and then closes: proves the tunnel is attempted on
/// the driver runtime with the connection's hop, and that a protocol failure is typed.
#[test]
fn ssh_protocol_failures_are_typed() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = mock_driver();
    let stats = mock.stats();
    let (manager, _) = manager(Arc::new(mock), tmp.path());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        use std::io::Write as _;
        if let Ok((mut s, _)) = listener.accept() {
            let _ = s.write_all(b"SSH-2.0-fake\r\n");
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    let types = ConnectionTypes::default();
    let mut c = ssh_connection(port);
    c.ssh.as_mut().unwrap().server.auth = SshAuth::Agent;
    ui(async {
        let err = open(&manager, OpenRequest::new(c, &types))
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::Ssh(_)), "{err:?}");
    });
    server.join().unwrap();
    assert_eq!(stats.connects(), 0);
}

#[test]
fn session_warnings_report_weakened_settings() {
    let tmp = tempfile::tempdir().unwrap();
    let mut caps = mock_capabilities();
    caps.read_only = ninesql_core::driver_api::ReadOnlyMechanism::ClientSideOnly;
    let (manager, _) = manager(Arc::new(mock_driver().with_capabilities(caps)), tmp.path());
    let types = ConnectionTypes::default();
    ui(async {
        let mut c = mock_connection(ConnectionTypeId::dev());
        c.tls.mode = ninesql_core::TlsMode::Disable;
        c.read_only = Some(true);
        c.auth = Auth::Username {
            username: "u".into(),
        };
        let h = open_ok(&manager, c, &types).await;
        let w = &h.info().warnings;
        assert!(
            w.contains(&SessionWarning::Connection(ApiWarning::TlsDowngraded(
                ninesql_core::driver_api::TlsMode::Disable
            )))
        );
        assert!(w.contains(&SessionWarning::Connection(ApiWarning::CredentialsInClear)));
        assert!(w.contains(&SessionWarning::ReadOnlyClientSideOnly));
    });
}

#[test]
fn closing_all_sessions_and_dropping_the_manager() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), tmp.path());
    let types = ConnectionTypes::default();
    let c = mock_connection(ConnectionTypeId::dev());
    let (a, running) = ui(async {
        let a = open_ok(&manager, c.clone(), &types).await;
        let _b = open_ok(&manager, c.clone(), &types).await;
        assert_eq!(manager.sessions_of(c.id).len(), 2);
        let running = a.execute(
            ExecuteRequest::sql("SELECT * FROM slow_numbers(1000, 50)")
                .with_budget(FetchBudget::default().with_batch_rows(5)),
        );
        tokio::time::sleep(Duration::from_millis(80)).await;
        manager.close_all().await.unwrap();
        assert!(
            manager.sessions_of(c.id).is_empty() || {
                // The supervisor removes them right after the close reply.
                tokio::time::sleep(Duration::from_millis(100)).await;
                manager.sessions_of(c.id).is_empty()
            }
        );
        (a, running)
    });
    // The running statement was cancelled by the close.
    let ev = ui(collect(running));
    assert!(matches!(terminal(&ev), ExecEvent::Failed(_)), "{ev:?}");
    assert!(a.is_closed());
    // Dropping the last manager clone from a plain thread stops the runtime.
    drop(manager);
}

#[test]
fn a_consumer_that_stops_reading_never_blocks_the_session() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), tmp.path());
    let types = ConnectionTypes::default();
    let big = || {
        ExecuteRequest::sql("SELECT * FROM numbers(1000000)")
            .with_budget(FetchBudget::default().with_batch_rows(10))
    };
    ui(async {
        // Held but not read, then cancelled: the terminal event does not wait for a slot, and
        // the queued command behind it is served.
        let h = open_ok(&manager, mock_connection(ConnectionTypeId::dev()), &types).await;
        let held = h.execute(big());
        tokio::time::sleep(Duration::from_millis(200)).await;
        let ping = h.ping();
        held.cancel();
        let pong = tokio::time::timeout(Duration::from_secs(5), ping)
            .await
            .expect("queued command not served");
        assert!(pong.unwrap().is_ok());
        let closed = tokio::time::timeout(Duration::from_secs(5), h.close())
            .await
            .expect("close blocked by a consumer that stopped reading");
        assert!(closed.unwrap().is_ok());
        let ev = collect(held).await;
        assert!(
            matches!(
                terminal(&ev),
                ExecEvent::Failed(CoreError::Driver(DriverError::Cancelled))
            ),
            "{ev:?}"
        );

        // Held, not read, not cancelled: close() cancels it and finishes.
        let h = open_ok(&manager, mock_connection(ConnectionTypeId::dev()), &types).await;
        let held = h.execute(big());
        tokio::time::sleep(Duration::from_millis(200)).await;
        let closed = tokio::time::timeout(Duration::from_secs(5), h.close())
            .await
            .expect("close blocked by a consumer that stopped reading");
        assert!(closed.unwrap().is_ok());
        assert!(matches!(
            terminal(&collect(held).await),
            ExecEvent::Failed(_)
        ));
    });
    // Shutdown is not held up either.
    let ui_manager = manager.clone();
    let h = ui(open_ok(
        &ui_manager,
        mock_connection(ConnectionTypeId::dev()),
        &types,
    ));
    let _held = h.execute(big());
    std::thread::sleep(Duration::from_millis(200));
    let start = std::time::Instant::now();
    manager.shutdown(Duration::from_secs(10));
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "shutdown waited on the consumer"
    );
    while !h.is_closed() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "session not closed"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(ui_manager);
}

#[test]
fn init_sql_that_needs_confirmation_is_confirmed_before_connecting() {
    let tmp = tempfile::tempdir().unwrap();
    let wrap = Wrap::new(mock_driver());
    let stats = wrap.mock.stats();
    let (manager, _) = manager(Arc::new(wrap.clone()), tmp.path());
    let types = ConnectionTypes::default();
    let mut c = mock_connection(ConnectionTypeId::prod());
    c.init_sql = vec![
        "SELECT * FROM numbers(1)".into(),
        "DROP TABLE scratch".into(),
    ];
    ui(async {
        let out = open(&manager, OpenRequest::new(c.clone(), &types))
            .await
            .unwrap();
        let OpenOutcome::InitSqlNeedsConfirmation(ic) = out else {
            panic!("{out:?}");
        };
        assert_eq!(
            ic.statements, c.init_sql,
            "every statement, exactly as it will run"
        );
        assert_eq!(ic.flagged.len(), 1);
        assert_eq!(ic.flagged[0].index, 1);
        assert_eq!(ic.flagged[0].class, StatementClass::Destructive);
        assert_eq!(ic.flagged[0].reason, ConfirmReason::Destructive);
        assert!(ic.production);
        assert_eq!((ic.connection, ic.connection_name.as_str()), (c.id, "Mock"));
        assert_eq!(stats.connects(), 0, "nothing was dialled");

        // Bound to the exact statements: a changed init SQL is refused, and the token is spent.
        let mut changed = c.clone();
        changed.init_sql[1] = "DROP TABLE everything".into();
        let err = open(
            &manager,
            OpenRequest::new(changed, &types).with_init_sql_confirmation(ic.token),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, CoreError::InvalidConfirmation));
        let err = open(
            &manager,
            OpenRequest::new(c.clone(), &types).with_init_sql_confirmation(ic.token),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, CoreError::InvalidConfirmation));

        // Bound to the connection.
        let OpenOutcome::InitSqlNeedsConfirmation(ic) =
            open(&manager, OpenRequest::new(c.clone(), &types))
                .await
                .unwrap()
        else {
            panic!("no confirmation");
        };
        let mut twin = c.clone();
        twin.id = ninesql_core::ConnectionId::new_random();
        assert!(matches!(
            open(
                &manager,
                OpenRequest::new(twin, &types).with_init_sql_confirmation(ic.token)
            )
            .await,
            Err(CoreError::InvalidConfirmation)
        ));

        // Confirmed: opens, the driver gets both statements; the token is single-use.
        let OpenOutcome::InitSqlNeedsConfirmation(ic) =
            open(&manager, OpenRequest::new(c.clone(), &types))
                .await
                .unwrap()
        else {
            panic!("no confirmation");
        };
        let confirmed = OpenRequest::new(c.clone(), &types).with_init_sql_confirmation(ic.token);
        let out = open(&manager, confirmed).await;
        assert!(matches!(out, Ok(OpenOutcome::Opened(_))));
        assert_eq!(wrap.last_seen().init_statements, 2);
        assert!(matches!(
            open(
                &manager,
                OpenRequest::new(c.clone(), &types).with_init_sql_confirmation(ic.token)
            )
            .await,
            Err(CoreError::InvalidConfirmation)
        ));

        // A secret prompt in between does not spend the token.
        let mut prompted = c.clone();
        prompted.auth = Auth::Password {
            username: "app".into(),
            secret: SecretSource::Prompt,
        };
        let OpenOutcome::InitSqlNeedsConfirmation(ic) =
            open(&manager, OpenRequest::new(prompted.clone(), &types))
                .await
                .unwrap()
        else {
            panic!("init SQL is confirmed before secrets are asked");
        };
        let out = open(
            &manager,
            OpenRequest::new(prompted.clone(), &types).with_init_sql_confirmation(ic.token),
        )
        .await
        .unwrap();
        assert!(matches!(out, OpenOutcome::NeedsSecrets(_)));
        let req = OpenRequest::new(prompted, &types)
            .with_init_sql_confirmation(ic.token)
            .with_secret(SecretSlot::Auth, SecretString::from("pw".to_owned()));
        assert!(matches!(
            open(&manager, req).await,
            Ok(OpenOutcome::Opened(_))
        ));

        // An entry with several statements counts as unrecognised.
        let mut multi = c.clone();
        multi.init_sql = vec!["SELECT 1; SELECT 2".into()];
        let OpenOutcome::InitSqlNeedsConfirmation(ic) =
            open(&manager, OpenRequest::new(multi, &types))
                .await
                .unwrap()
        else {
            panic!("no confirmation");
        };
        assert_eq!(ic.flagged[0].class, StatementClass::Unknown);

        // Reads only on production, or anything on dev: no confirmation.
        let mut reads = c.clone();
        reads.init_sql = vec!["SELECT * FROM numbers(1)".into()];
        assert!(matches!(
            open(&manager, OpenRequest::new(reads, &types)).await,
            Ok(OpenOutcome::Opened(_))
        ));
        let mut dev = c.clone();
        dev.connection_type = ConnectionTypeId::dev();
        assert!(matches!(
            open(&manager, OpenRequest::new(dev, &types)).await,
            Ok(OpenOutcome::Opened(_))
        ));
    });
}

#[test]
fn core_events_are_bounded_and_drops_are_counted() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager_with(Arc::new(mock_driver()), tmp.path(), |c| {
        c.event_queue = 2;
    });
    let types = ConnectionTypes::default();
    ui(async {
        // Nobody reads the events: opens and closes go on, extra events are dropped.
        for _ in 0..3 {
            let h = open_ok(&manager, mock_connection(ConnectionTypeId::dev()), &types).await;
            h.close().await.unwrap().unwrap();
        }
    });
    // Event queue of 2: Opened 1 and Closed 1 fit; Opened 2 and 3 are dropped and counted;
    // Closed 2 and 3 are never dropped.
    let start = std::time::Instant::now();
    while manager.dropped_events() < 2 {
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(manager.dropped_events(), 2);
    let mut events = manager.take_events().unwrap();
    assert!(matches!(
        events.try_recv(),
        Next::Ready(CoreEvent::SessionOpened { .. })
    ));
    let mut closed = std::collections::BTreeSet::new();
    while closed.len() < 3 {
        assert!(start.elapsed() < Duration::from_secs(5), "{closed:?}");
        match events.try_recv() {
            Next::Ready(CoreEvent::SessionClosed { session, .. }) => {
                assert!(closed.insert(session));
            }
            Next::Ready(other) => panic!("{other:?}"),
            Next::Pending => std::thread::sleep(Duration::from_millis(20)),
            Next::Ended => panic!("ended"),
        }
    }
    // The receiver also wakes up for a backlogged close.
    ui(async {
        let h = open_ok(&manager, mock_connection(ConnectionTypeId::dev()), &types).await;
        assert!(matches!(
            events.recv().await,
            Some(CoreEvent::SessionOpened { .. })
        ));
        h.close().await.unwrap().unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(5), events.recv()).await;
        assert!(
            matches!(ev, Ok(Some(CoreEvent::SessionClosed { .. }))),
            "{ev:?}"
        );
    });
    assert_eq!(
        manager.dropped_events(),
        2,
        "SessionClosed is never counted as dropped"
    );
}

#[test]
fn init_sql_that_changes_the_lexical_mode_or_follows_a_setting_is_confirmed() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), tmp.path());
    let types = ConnectionTypes::default();
    let flagged = |init: &[&str]| {
        let mut c = mock_connection(ConnectionTypeId::prod());
        c.init_sql = init.iter().map(|s| (*s).to_owned()).collect();
        let out = ui(open(&manager, OpenRequest::new(c, &types))).unwrap();
        match out {
            OpenOutcome::InitSqlNeedsConfirmation(ic) => ic
                .flagged
                .iter()
                .map(|f| (f.index, f.class))
                .collect::<Vec<_>>(),
            OpenOutcome::Opened(h) => {
                let _ = ui(h.close());
                Vec::new()
            }
            other => panic!("{other:?}"),
        }
    };
    // Lexical mode changes are unrecognised.
    for setting in [
        "SET sql_mode = 'ANSI_QUOTES'",
        "SET standard_conforming_strings = off",
        "SET client_encoding TO 'SJIS'",
        "SET NAMES 'gbk'",
        "SET CHARACTER SET big5",
    ] {
        // Flagged: unrecognised, or already access-mode changing for the dialect.
        let f = flagged(&[setting]);
        assert!(
            matches!(
                f.as_slice(),
                [(
                    0,
                    StatementClass::Unknown | StatementClass::ChangesAccessMode
                )]
            ),
            "{setting}: {f:?}"
        );
    }
    // A plain setting is fine on its own, but what follows it is unrecognised.
    assert!(flagged(&["SET search_path TO app"]).is_empty());
    assert_eq!(
        flagged(&["SET search_path TO app", "SELECT * FROM numbers(1)"]),
        [(1, StatementClass::Unknown)]
    );
    assert!(flagged(&["SELECT * FROM numbers(1)", "SELECT * FROM numbers(2)"]).is_empty());
}

/// P1-E review (for P2-C): imported init SQL is confirmed once whatever the connection type, so
/// a crafted DBeaver workspace cannot skip the confirmation by typing its connection "dev".
#[test]
fn imported_init_sql_is_confirmed_once_whatever_the_type() {
    let tmp = tempfile::tempdir().unwrap();
    let wrap = Wrap::new(mock_driver());
    let stats = wrap.mock.stats();
    let (manager, _) = manager(Arc::new(wrap.clone()), tmp.path());
    let mut events = manager.take_events().unwrap();
    let types = ConnectionTypes::default();
    ui(async {
        // Dev, harmless statements: no confirmation without the marker.
        let mut c = mock_connection(ConnectionTypeId::dev());
        c.init_sql = vec![
            "SELECT * FROM numbers(1)".into(),
            "SET search_path TO app".into(),
        ];
        let h = open_ok(&manager, c.clone(), &types).await;
        h.close().await.unwrap().unwrap();

        // With the marker: every statement is flagged, as imported, before anything is dialled.
        c.init_sql_needs_confirmation = true;
        let connects = stats.connects();
        let out = open(&manager, OpenRequest::new(c.clone(), &types))
            .await
            .unwrap();
        let OpenOutcome::InitSqlNeedsConfirmation(ic) = out else {
            panic!("{out:?}");
        };
        assert!(ic.imported);
        assert!(!ic.production);
        assert_eq!(ic.statements, c.init_sql);
        let flagged: Vec<_> = ic.flagged.iter().map(|f| (f.index, f.reason)).collect();
        assert_eq!(
            flagged,
            [(0, ConfirmReason::Imported), (1, ConfirmReason::Imported)]
        );
        assert_eq!(stats.connects(), connects, "nothing was dialled");

        // Confirmed: opens, and the core reports the confirmation (no workspace here).
        let out = open(
            &manager,
            OpenRequest::new(c.clone(), &types).with_init_sql_confirmation(ic.token),
        )
        .await
        .unwrap();
        let OpenOutcome::Opened(h) = out else {
            panic!("{out:?}");
        };
        assert_eq!(wrap.last_seen().init_statements, 2);
        let mut confirmed = false;
        while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
            if ev
                == (CoreEvent::ImportedInitSqlConfirmed {
                    connection: c.id,
                    statements: c.init_sql.clone(),
                    cleared: None,
                })
            {
                confirmed = true;
                break;
            }
        }
        assert!(confirmed, "no ImportedInitSqlConfirmed event");
        h.close().await.unwrap().unwrap();

        // An empty init SQL needs nothing, marker or not.
        let mut empty = c.clone();
        empty.init_sql.clear();
        let h = open_ok(&manager, empty, &types).await;
        h.close().await.unwrap().unwrap();

        // Production keeps its own rule: the destructive entry stays `Destructive`, the others
        // are flagged as imported; without the marker only the destructive one is flagged.
        let mut prod = mock_connection(ConnectionTypeId::prod());
        prod.init_sql = vec![
            "SELECT * FROM numbers(1)".into(),
            "DROP TABLE scratch".into(),
        ];
        prod.init_sql_needs_confirmation = true;
        let OpenOutcome::InitSqlNeedsConfirmation(ic) =
            open(&manager, OpenRequest::new(prod.clone(), &types))
                .await
                .unwrap()
        else {
            panic!("no confirmation");
        };
        assert!(ic.imported && ic.production);
        let flagged: Vec<_> = ic.flagged.iter().map(|f| (f.index, f.reason)).collect();
        assert_eq!(
            flagged,
            [
                (0, ConfirmReason::Imported),
                (1, ConfirmReason::Destructive)
            ]
        );
        // L1: a token issued with the imported notice does not open the connection once the
        // request says otherwise, and the reverse.
        let imported_token = ic.token;
        let mut unmarked = prod.clone();
        unmarked.init_sql_needs_confirmation = false;
        assert!(matches!(
            open(
                &manager,
                OpenRequest::new(unmarked.clone(), &types)
                    .with_init_sql_confirmation(imported_token)
            )
            .await,
            Err(CoreError::InvalidConfirmation)
        ));
        let OpenOutcome::InitSqlNeedsConfirmation(plain) =
            open(&manager, OpenRequest::new(unmarked, &types))
                .await
                .unwrap()
        else {
            panic!("no confirmation");
        };
        assert!(!plain.imported);
        assert!(matches!(
            open(
                &manager,
                OpenRequest::new(prod.clone(), &types).with_init_sql_confirmation(plain.token)
            )
            .await,
            Err(CoreError::InvalidConfirmation)
        ));

        prod.init_sql_needs_confirmation = false;
        let OpenOutcome::InitSqlNeedsConfirmation(ic) =
            open(&manager, OpenRequest::new(prod, &types))
                .await
                .unwrap()
        else {
            panic!("no confirmation");
        };
        assert!(!ic.imported);
        let flagged: Vec<_> = ic.flagged.iter().map(|f| (f.index, f.reason)).collect();
        assert_eq!(flagged, [(1, ConfirmReason::Destructive)]);
    });
}
