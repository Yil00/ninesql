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

use std::io::Read as _;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ninesql_core::driver_api::mock::MockDriver;
use ninesql_core::driver_api::{Endpoint as ApiEndpoint, SecurityWarning as ApiWarning};
use ninesql_core::ssh::{SshError, SshWarningKind};
use ninesql_core::{
    ConnectionTypeId, ConnectionTypes, CoreError, Endpoint, HostKeyPolicy, OpenOutcome,
    OpenRequest, SecretSlot, SecretSource, SessionWarning, SshAuth, SshHop, SshTunnel,
};
use ninesql_secrets::SecretString;
use support::*;

/// `linuxserver/openssh-server`, pinned by digest (same image as `ninesql-ssh`'s test).
const SSH_IMAGE: &str = "linuxserver/openssh-server@sha256:161e0b670b31d9539ca1718bc5943e981779331b4e1c9d32e8929d7652a1f22f";

fn docker(args: &[&str]) -> Result<String, String> {
    let out = Command::new("docker")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

fn required() -> bool {
    std::env::var("NINESQL_REQUIRE_ENGINES")
        .map(|v| v.split([',', ' ']).any(|e| e == "ssh" || e == "all"))
        .unwrap_or(false)
}

struct Container(String);

impl Drop for Container {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", "-v", &self.0]);
    }
}

fn wait_log(name: &str, needle: &str) {
    let start = Instant::now();
    loop {
        let out = Command::new("docker")
            .args(["logs", name])
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if text.contains(needle) {
            return;
        }
        assert!(start.elapsed() < Duration::from_secs(90), "sshd not ready");
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Reads the SSH banner of whatever answers at `endpoint`.
fn banner(endpoint: &ApiEndpoint) -> Option<String> {
    let ApiEndpoint::Tcp { host, port } = endpoint else {
        return None;
    };
    let mut s = std::net::TcpStream::connect((host.as_str(), *port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buf = [0u8; 16];
    let n = s.read(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf[..n]).into_owned())
}

fn wait_banner(port: u16) {
    let start = Instant::now();
    while banner(&ApiEndpoint::tcp("127.0.0.1", port)).is_none_or(|b| !b.starts_with("SSH-")) {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "sshd never answered"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn connection(port: u16, policy: HostKeyPolicy) -> ninesql_core::Connection {
    let mut c = mock_connection(ConnectionTypeId::dev());
    // The database as seen from the SSH server: here the server's own sshd, so the test can
    // check forwarding by reading its banner through the tunnel.
    c.endpoint = Endpoint::Tcp {
        host: "localhost".into(),
        port: Some(2222),
        database: None,
    };
    let mut server = SshHop::new(
        "127.0.0.1",
        port,
        "tunnel",
        SshAuth::Password {
            secret: SecretSource::Prompt,
        },
    );
    server.host_key_policy = policy;
    c.ssh = Some(SshTunnel {
        server,
        jump_hosts: vec![],
    });
    c
}

#[test]
fn ssh_tunnel_through_the_core_connect_flow() {
    if docker(&["info", "--format", "{{.ServerVersion}}"]).is_err() {
        assert!(
            !required(),
            "Docker is required (NINESQL_REQUIRE_ENGINES) but unavailable"
        );
        eprintln!("skipped: Docker is not available");
        return;
    }
    let password = format!(
        "pw-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    );
    let name = format!("ninesql-core-ssh-it-{}", uuid::Uuid::new_v4().simple());
    docker(&[
        "run",
        "-d",
        "--name",
        &name,
        "-e",
        "PUID=1000",
        "-e",
        "PGID=1000",
        "-e",
        "USER_NAME=tunnel",
        "-e",
        "PASSWORD_ACCESS=true",
        "-e",
        &format!("USER_PASSWORD={password}"),
        "-p",
        "127.0.0.1::2222",
        SSH_IMAGE,
    ])
    .unwrap_or_else(|e| panic!("docker run: {e}"));
    let container = Container(name.clone());
    wait_log(&container.0, "[ls.io-init] done.");
    docker(&[
        "exec",
        &name,
        "sh",
        "-c",
        "sed -i 's/^AllowTcpForwarding no/AllowTcpForwarding yes/' /config/sshd/sshd_config \
         && grep -q '^AllowTcpForwarding yes' /config/sshd/sshd_config \
         && s6-svc -r /run/service/svc-openssh-server",
    ])
    .unwrap();
    let published = docker(&["port", &name, "2222/tcp"]).unwrap();
    let port: u16 = published
        .lines()
        .next()
        .and_then(|l| l.rsplit(':').next())
        .and_then(|p| p.parse().ok())
        .unwrap();
    wait_banner(port);

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
        let OpenOutcome::HostKeyNeedsConfirmation(key) = out else {
            panic!("{out:?}");
        };
        assert_eq!(
            (key.hop, key.host.as_str(), key.port),
            (0, "127.0.0.1", port)
        );
        assert!(
            wrap.seen.lock().unwrap().is_empty(),
            "the driver was not called"
        );

        // A wrong password once the key is trusted: typed authentication error.
        manager.trust_host_key(*key).await.unwrap().unwrap();
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
    drop(container);
}
