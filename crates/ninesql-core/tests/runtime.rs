//! Driver runtime and session manager (P1-D) against the mock driver: open / execute / stream /
//! cancel / `Busy` / close lifecycle, read-only and production confirmation flows, secret
//! resolution, reserved properties and the SSH wiring.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).

mod support;

use std::sync::Arc;
use std::time::Duration;

use ninesql_core::driver_api::mock::{MOCK_RESERVED_PROPERTY, MockDriver};
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
    let wrap = Wrap::new(MockDriver::new());
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
    let mock = MockDriver::new();
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
    let mock = MockDriver::new();
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
    let wrap = Wrap::new(MockDriver::new());
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
    let mock = MockDriver::new();
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
    let mut wrap = Wrap::new(MockDriver::new());
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
    let wrap = Wrap::new(MockDriver::new());
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
    let (manager, _) = manager(Arc::new(MockDriver::new()), tmp.path());
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
    let mock = MockDriver::new().with_password(Secret::new("Corr3ct-Horse".into()));
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
    let mock = MockDriver::new();
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
    let mock = MockDriver::new();
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
    let mock = MockDriver::new();
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
    let mut caps = ninesql_core::driver_api::mock::full_capabilities();
    caps.read_only = ninesql_core::driver_api::ReadOnlyMechanism::ClientSideOnly;
    let (manager, _) = manager(
        Arc::new(MockDriver::new().with_capabilities(caps)),
        tmp.path(),
    );
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
    let (manager, _) = manager(Arc::new(MockDriver::new()), tmp.path());
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
