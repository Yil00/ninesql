//! SSH wiring of the connect flow against a real OpenSSH server in a container (P1-D, ADR-0011,
//! ADR-0016): an unknown host key under the strict policy asks for confirmation before any
//! credential is sent; once trusted, the driver gets the tunnel's local end, `server_name` set to
//! the real database host and the route; the tunnel forwards; closing the session closes it.
//! `AcceptNew` connects at once and reports the auto-accepted key.
//!
//! Skipped (not failed) when Docker is not available, unless `NINESQL_REQUIRE_ENGINES` lists
//! `ssh` (AGENTS.md). The container is removed at the end, also on failure.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)] // Tests may panic (AGENTS.md).

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use ninesql_core::driver_api::mock::MockDriver;
use ninesql_core::driver_api::{Endpoint as ApiEndpoint, SecurityWarning as ApiWarning};
use ninesql_core::ssh::{SshError, SshWarningKind};
use ninesql_core::{
    ConnectionTypes, CoreError, HostKeyPolicy, OpenOutcome, OpenRequest, SecretSlot, SessionWarning,
};
use ninesql_secrets::SecretString;
use support::sshd::{banner, connection};
use support::*;

#[test]
fn ssh_tunnel_through_the_core_connect_flow() {
    let password = format!(
        "pw-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    );
    let Some(sshd) = support::sshd::start(&password) else {
        return;
    };
    let port = sshd.port;

    let tmp = tempfile::tempdir().unwrap();
    let wrap = Wrap::new(MockDriver::new());
    let (manager, _) = manager(Arc::new(wrap.clone()), tmp.path());
    let types = ConnectionTypes::default();
    let secret = || SecretString::from(password.clone());
    ui(async {
        // Strict: an unknown host key stops the flow before any credential is sent.
        let c = connection(port, HostKeyPolicy::Strict);
        let req = OpenRequest::new(c.clone(), &types).with_secret(SecretSlot::SshServer, secret());
        let out = open(&manager, req).await.unwrap();
        let OpenOutcome::HostKeyNeedsConfirmation(confirm) = out else {
            panic!("{out:?}");
        };
        let key = &confirm.key;
        assert_eq!(
            (key.hop, key.host.as_str(), key.port),
            (0, "127.0.0.1", port)
        );
        assert!(
            wrap.seen.lock().unwrap().is_empty(),
            "the driver was not called"
        );

        // A wrong password once the key is trusted: typed authentication error.
        manager
            .trust_host_key(confirm.token)
            .await
            .unwrap()
            .unwrap();
        // Single use.
        assert!(matches!(
            manager.trust_host_key(confirm.token).await.unwrap(),
            Err(CoreError::InvalidConfirmation)
        ));
        let req = OpenRequest::new(c.clone(), &types).with_secret(
            SecretSlot::SshServer,
            SecretString::from("wrong".to_owned()),
        );
        let err = open(&manager, req).await.unwrap_err();
        assert!(
            matches!(err, CoreError::Ssh(SshError::AuthFailed { hop: 0, .. })),
            "{err:?}"
        );

        let req = OpenRequest::new(c.clone(), &types).with_secret(SecretSlot::SshServer, secret());
        let OpenOutcome::Opened(h) = open(&manager, req).await.unwrap() else {
            panic!("not opened");
        };
        let seen = wrap.last_seen();
        assert_ne!(seen.endpoint, ApiEndpoint::tcp("127.0.0.1", port));
        assert!(matches!(&seen.endpoint, ApiEndpoint::Tcp { host, .. } if host == "127.0.0.1"));
        assert_eq!(
            seen.server_name.as_deref(),
            Some("localhost"),
            "verify-full checks the real host"
        );
        let route = seen.tunnel.clone().unwrap();
        assert_eq!(
            (route.ssh_host.as_str(), route.ssh_port),
            ("127.0.0.1", port)
        );
        assert_eq!(h.info().tunnel.as_ref(), Some(&route));
        assert!(h.info().warnings.is_empty(), "{:?}", h.info().warnings);
        // The tunnel forwards to the target (the server's own sshd).
        let through = banner(&seen.endpoint).unwrap();
        assert!(through.starts_with("SSH-"), "{through}");

        // Closing the session closes the tunnel.
        h.close().await.unwrap().unwrap();
        let start = Instant::now();
        while banner(&seen.endpoint).is_some_and(|b| b.starts_with("SSH-")) {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "tunnel still open"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });

    // AcceptNew, with an empty known_hosts: connects at once, and says so.
    let tmp2 = tempfile::tempdir().unwrap();
    let (manager2, _) = support::manager(Arc::new(wrap.clone()), tmp2.path());
    ui(async {
        let c = connection(port, HostKeyPolicy::AcceptNew);
        let req = OpenRequest::new(c, &types).with_secret(SecretSlot::SshServer, secret());
        let OpenOutcome::Opened(h) = open(&manager2, req).await.unwrap() else {
            panic!("not opened");
        };
        let w = &h.info().warnings;
        assert!(w.contains(&SessionWarning::Connection(
            ApiWarning::SshHostKeyNotVerified
        )));
        assert!(w.iter().any(|w| matches!(
            w,
            SessionWarning::Ssh(s) if s.kind == SshWarningKind::AcceptNewHostKeys
        )));
        assert!(w.iter().any(|w| matches!(
            w,
            SessionWarning::Ssh(s) if matches!(s.kind, SshWarningKind::HostKeyAutoAccepted(_))
        )));
        h.close().await.unwrap().unwrap();
    });
    let known = std::fs::read_to_string(tmp2.path().join("known_hosts")).unwrap();
    assert!(known.contains(&format!("[127.0.0.1]:{port}")));
    assert!(!known.contains(&password));
    drop(sshd);
}
