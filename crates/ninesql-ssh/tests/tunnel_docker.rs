//! Integration test: a real OpenSSH jump host and SSH server in containers, with PostgreSQL 17
//! behind them (ROADMAP P1-D, ADR-0011).
//!
//! Topology (one throwaway Docker bridge network, nothing reachable from outside the host):
//!
//! ```text
//! test ──TCP 127.0.0.1:<random>──▶ jump (openssh) ──direct-tcpip──▶ sshd (openssh) ──▶ db:5432
//! ```
//!
//! Only the jump host publishes a port, on 127.0.0.1. Skipped (not failed) when Docker is not
//! available, unless `NINESQL_REQUIRE_ENGINES` lists `ssh` (AGENTS.md). Every container and the
//! network are removed at the end, also on failure.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)] // Tests may panic (AGENTS.md).

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use ninesql_driver_api::{Endpoint, HostKeyVerification};
use ninesql_ssh::{
    Fingerprint, HopAuth, HopSpec, HostKeyPolicy, KnownHostsStore, SshError, SshWarningKind,
    Tunnel, TunnelOptions, TunnelSpec, TunnelState,
};
use rand::RngExt as _;
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use secrecy::SecretString;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// `linuxserver/openssh-server`, pinned by digest (multi-arch index).
const SSH_IMAGE: &str = "linuxserver/openssh-server@sha256:161e0b670b31d9539ca1718bc5943e981779331b4e1c9d32e8929d7652a1f22f";
/// `postgres:17`, pinned by digest.
const PG_IMAGE: &str =
    "postgres:17@sha256:846a7ddaa67a4b354ae1d21087b683901aa6113a4ed626d3787cfbf6c85e3e07";

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

/// Removes the containers and the network on drop (also when a test assertion panics).
struct Lab {
    prefix: String,
    containers: Vec<String>,
    network: Option<String>,
}

impl Drop for Lab {
    fn drop(&mut self) {
        for c in &self.containers {
            let _ = docker(&["rm", "-f", "-v", c]);
        }
        if let Some(n) = &self.network {
            let _ = docker(&["network", "rm", n]);
        }
    }
}

impl Lab {
    fn name(&self, role: &str) -> String {
        format!("{}-{role}", self.prefix)
    }

    fn run(&mut self, role: &str, args: &[&str]) {
        let name = self.name(role);
        let net = self.network.clone().unwrap();
        let mut all = vec![
            "run",
            "-d",
            "--name",
            &name,
            "--network",
            &net,
            "--network-alias",
            role,
        ];
        all.extend_from_slice(args);
        docker(&all).unwrap_or_else(|e| panic!("docker run {role}: {e}"));
        self.containers.push(name);
    }

    fn exec(&self, role: &str, script: &str) -> String {
        docker(&["exec", &self.name(role), "sh", "-c", script])
            .unwrap_or_else(|e| panic!("docker exec {role}: {e}"))
    }

    fn wait_log(&self, role: &str, needle: &str, timeout: Duration) {
        let start = Instant::now();
        loop {
            let out = Command::new("docker")
                .args(["logs", &self.name(role)])
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
            assert!(start.elapsed() < timeout, "{role} not ready: {text}");
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// Starts an OpenSSH container with TCP forwarding enabled and returns its Ed25519 host key.
    fn ssh_server(&mut self, role: &str, extra_env: &[String], publish: bool) -> PublicKey {
        let mut args: Vec<String> = vec![
            "-e".into(),
            "PUID=1000".into(),
            "-e".into(),
            "PGID=1000".into(),
        ];
        for e in extra_env {
            args.push("-e".into());
            args.push(e.clone());
        }
        if publish {
            args.push("-p".into());
            args.push("127.0.0.1::2222".into());
        }
        args.push(SSH_IMAGE.into());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(role, &refs);
        self.wait_log(role, "[ls.io-init] done.", Duration::from_secs(90));
        // The image ships `AllowTcpForwarding no`; a tunnel endpoint needs it.
        self.exec(
            role,
            "sed -i 's/^AllowTcpForwarding no/AllowTcpForwarding yes/' /config/sshd/sshd_config \
             && grep -q '^AllowTcpForwarding yes' /config/sshd/sshd_config \
             && s6-svc -r /run/service/svc-openssh-server",
        );
        let pub_line = self.exec(role, "cat /config/ssh_host_keys/ssh_host_ed25519_key.pub");
        PublicKey::from_openssh(&pub_line).unwrap()
    }
}

fn random_token(rng: &mut impl rand::Rng) -> String {
    (0..24)
        .map(|_| char::from(b"abcdefghijkmnpqrstuvwxyz23456789"[rng.random_range(0..32)]))
        .collect()
}

async fn wait_ssh_banner(port: u16) {
    let start = Instant::now();
    loop {
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)).await {
            let mut buf = [0u8; 8];
            if let Ok(Ok(n)) = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf)).await
                && n >= 4
                && &buf[..4] == b"SSH-"
            {
                return;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "jump host never answered"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// Sends a PostgreSQL `SSLRequest` through the tunnel; the server answers one byte (`N` / `S`).
async fn postgres_answers(endpoint: &Endpoint) -> bool {
    let Endpoint::Tcp { host, port } = endpoint else {
        return false;
    };
    let Ok(mut s) = TcpStream::connect((host.as_str(), *port)).await else {
        return false;
    };
    let mut req = Vec::new();
    req.extend_from_slice(&8u32.to_be_bytes());
    req.extend_from_slice(&80_877_103u32.to_be_bytes());
    if s.write_all(&req).await.is_err() {
        return false;
    }
    let mut b = [0u8; 1];
    matches!(
        tokio::time::timeout(Duration::from_secs(10), s.read_exact(&mut b)).await,
        Ok(Ok(_))
    ) && (b[0] == b'N' || b[0] == b'S')
}

struct Creds {
    jump_port: u16,
    jump_password: String,
    key_path: std::path::PathBuf,
    key_passphrase: String,
}

fn spec(c: &Creds, jump_policy: HostKeyPolicy, server_policy: HostKeyPolicy) -> TunnelSpec {
    let jump = HopSpec::new(
        "127.0.0.1",
        "hop",
        HopAuth::Password(SecretString::from(c.jump_password.clone())),
    )
    .with_port(c.jump_port)
    .with_host_key_policy(jump_policy);
    let server = HopSpec::new(
        "sshd",
        "tunnel",
        HopAuth::PublicKey {
            key_path: c.key_path.clone(),
            passphrase: Some(SecretString::from(c.key_passphrase.clone())),
        },
    )
    .with_port(2222)
    .with_host_key_policy(server_policy);
    let mut options = TunnelOptions::default();
    options.connect_timeout = Duration::from_secs(30);
    options.keepalive_interval = Duration::from_secs(2);
    options.keepalive_max = 2;
    TunnelSpec::new(server, "db", 5432)
        .via(jump)
        .with_options(options)
}

fn fp(key: &PublicKey) -> Fingerprint {
    Fingerprint::of_blob(&key.to_bytes().unwrap())
}

fn file_contains_no(path: &Path, needles: &[&str]) {
    if let Ok(text) = std::fs::read_to_string(path) {
        for n in needles {
            assert!(!text.contains(n), "a secret reached {}", path.display());
        }
    }
}

#[test]
fn tunnel_through_jump_host_to_postgres() {
    if docker(&["info", "--format", "{{.ServerVersion}}"]).is_err() {
        assert!(
            !required(),
            "Docker is required (NINESQL_REQUIRE_ENGINES) but unavailable"
        );
        eprintln!("skipped: Docker is not available");
        return;
    }
    let mut rng = rand::rng();
    let prefix = format!(
        "ninesql-ssh-it-{}-{}",
        std::process::id(),
        random_token(&mut rng)
    );
    let mut lab = Lab {
        prefix: prefix.clone(),
        containers: Vec::new(),
        network: None,
    };
    docker(&["network", "create", &prefix]).expect("network create");
    lab.network = Some(prefix.clone());

    // Credentials generated for this run only.
    let jump_password = random_token(&mut rng);
    let pg_password = random_token(&mut rng);
    let key_passphrase = random_token(&mut rng);
    let client_key = PrivateKey::random(&mut rng, Algorithm::Ed25519).unwrap();
    let client_pub = client_key.public_key().to_openssh().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("id_ed25519");
    let encrypted = client_key
        .encrypt(&mut rng, key_passphrase.as_str())
        .unwrap();
    std::fs::write(
        &key_path,
        encrypted.to_openssh(LineEnding::LF).unwrap().as_bytes(),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    lab.run(
        "db",
        &["-e", &format!("POSTGRES_PASSWORD={pg_password}"), PG_IMAGE],
    );
    let server_key = lab.ssh_server(
        "sshd",
        &[
            "USER_NAME=tunnel".into(),
            format!("PUBLIC_KEY={client_pub}"),
            "PASSWORD_ACCESS=false".into(),
        ],
        false,
    );
    let jump_key = lab.ssh_server(
        "jump",
        &[
            "USER_NAME=hop".into(),
            "PASSWORD_ACCESS=true".into(),
            format!("USER_PASSWORD={jump_password}"),
        ],
        true,
    );
    let jump_rsa_line = lab.exec("jump", "cat /config/ssh_host_keys/ssh_host_rsa_key.pub");
    lab.wait_log(
        "db",
        "database system is ready to accept connections",
        Duration::from_secs(90),
    );
    let mapped = docker(&["port", &lab.name("jump"), "2222/tcp"]).unwrap();
    let jump_port: u16 = mapped
        .lines()
        .find(|l| l.starts_with("127.0.0.1:"))
        .and_then(|l| l.rsplit(':').next())
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("unexpected port mapping {mapped:?}"));
    let creds = Creds {
        jump_port,
        jump_password: jump_password.clone(),
        key_path,
        key_passphrase: key_passphrase.clone(),
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        wait_ssh_banner(jump_port).await;
        let own = dir.path().join("data").join("ninesql").join("known_hosts");

        // 1. Strict, nothing on file: the first hop needs confirmation; nothing is written.
        let store = KnownHostsStore::load(own.clone(), None).await.unwrap();
        let err = Tunnel::open(spec(&creds, HostKeyPolicy::Strict, HostKeyPolicy::Strict), store.clone())
            .await
            .unwrap_err();
        let unknown = err.unknown_host_key().cloned().unwrap_or_else(|| panic!("{err:?}"));
        assert_eq!(unknown.hop, 0);
        assert_eq!(unknown.port, jump_port);
        assert_eq!(unknown.key_type, "ssh-ed25519");
        assert_eq!(unknown.fingerprint, fp(&jump_key));
        assert!(err.to_string().contains(&fp(&jump_key).to_string()));
        assert!(!own.exists(), "nothing may be written before the user confirms");

        // 2. The user confirms hop 0; then hop 1 (reached through hop 0) needs confirmation.
        store.trust(&unknown).await.unwrap();
        let err = Tunnel::open(spec(&creds, HostKeyPolicy::Strict, HostKeyPolicy::Strict), store.clone())
            .await
            .unwrap_err();
        let unknown = err.unknown_host_key().cloned().unwrap_or_else(|| panic!("{err:?}"));
        assert_eq!((unknown.hop, unknown.host.as_str(), unknown.port), (1, "sshd", 2222));
        assert_eq!(unknown.fingerprint, fp(&server_key));
        store.trust(&unknown).await.unwrap();

        // 3. Both hops known: the tunnel opens and reaches PostgreSQL.
        let tunnel = Tunnel::open(spec(&creds, HostKeyPolicy::Strict, HostKeyPolicy::Strict), store.clone())
            .await
            .unwrap();
        let endpoint = tunnel.endpoint();
        assert!(matches!(&endpoint, Endpoint::Tcp { host, .. } if host == "127.0.0.1"));
        assert!(tunnel.local_addr().ip().is_loopback());
        assert_eq!(tunnel.info().ssh_host, "sshd");
        assert_eq!(tunnel.info().ssh_port, 2222);
        assert_eq!(tunnel.info().jump_hosts, 1);
        assert_eq!(tunnel.info().host_key, HostKeyVerification::Strict);
        assert!(tunnel.security_warnings().is_empty());
        assert_eq!(tunnel.state(), TunnelState::Running);
        let (a, b) = tokio::join!(postgres_answers(&endpoint), postgres_answers(&endpoint));
        assert!(a && b, "PostgreSQL did not answer through the tunnel");
        let local = tunnel.local_addr();
        tunnel.close().await;
        assert!(
            TcpStream::connect(local).await.is_err(),
            "the local port must be closed after close()"
        );

        // known_hosts: 0600, two lines, and no credential anywhere in it (I1).
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&own).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert_eq!(std::fs::read_to_string(&own).unwrap().lines().count(), 2);
        file_contains_no(&own, &[&jump_password, &key_passphrase, &pg_password]);

        // 4. A changed key on file for the jump host is a hard error (no credential is sent).
        let changed = dir.path().join("changed_known_hosts");
        let other = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        std::fs::write(
            &changed,
            format!("[127.0.0.1]:{jump_port} {}\n", other.public_key().to_openssh().unwrap()),
        )
        .unwrap();
        let store_changed = KnownHostsStore::load(dir.path().join("unused"), Some(changed.clone()))
            .await
            .unwrap();
        let err = Tunnel::open(spec(&creds, HostKeyPolicy::Strict, HostKeyPolicy::Strict), store_changed.clone())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, SshError::HostKeyChanged { hop: Some(0), presented_fingerprint, .. } if *presented_fingerprint == fp(&jump_key)),
            "{err:?}"
        );
        // Accept-new does not override a changed key either.
        let err = Tunnel::open(spec(&creds, HostKeyPolicy::AcceptNew, HostKeyPolicy::AcceptNew), store_changed)
            .await
            .unwrap_err();
        assert!(matches!(err, SshError::HostKeyChanged { .. }), "{err:?}");

        // 5. Wrong password: typed authentication error naming the hop.
        let mut bad = spec(&creds, HostKeyPolicy::Strict, HostKeyPolicy::Strict);
        bad.jump_hosts[0].auth = HopAuth::Password(SecretString::from("wrong-password"));
        let err = Tunnel::open(bad, store.clone()).await.unwrap_err();
        assert!(matches!(err, SshError::AuthFailed { hop: 0, method: "password", .. }), "{err:?}");

        // 6. Accept-new on an empty file: opens, records both keys, reports warnings.
        let own2 = dir.path().join("accept-new").join("known_hosts");
        let store2 = KnownHostsStore::load(own2.clone(), None).await.unwrap();
        let tunnel = Tunnel::open(spec(&creds, HostKeyPolicy::AcceptNew, HostKeyPolicy::AcceptNew), store2)
            .await
            .unwrap();
        assert_eq!(tunnel.info().host_key, HostKeyVerification::Disabled);
        let auto: Vec<_> = tunnel
            .security_warnings()
            .iter()
            .filter(|w| matches!(w.kind, SshWarningKind::HostKeyAutoAccepted(_)))
            .collect();
        assert_eq!(auto.len(), 2);
        assert!(postgres_answers(&tunnel.endpoint()).await);
        drop(tunnel);
        assert_eq!(std::fs::read_to_string(&own2).unwrap().lines().count(), 2);

        // 7. Disabled on an empty file: opens, writes nothing, flagged.
        let own3 = dir.path().join("disabled").join("known_hosts");
        let store3 = KnownHostsStore::load(own3.clone(), None).await.unwrap();
        let tunnel = Tunnel::open(spec(&creds, HostKeyPolicy::Disabled, HostKeyPolicy::Strict), store3.clone())
            .await;
        // Hop 1 is still strict and unknown in this file.
        assert!(tunnel.unwrap_err().unknown_host_key().is_some_and(|k| k.hop == 1));
        let tunnel = Tunnel::open(spec(&creds, HostKeyPolicy::Disabled, HostKeyPolicy::Disabled), store3)
            .await
            .unwrap();
        assert!(tunnel
            .security_warnings()
            .iter()
            .all(|w| w.kind == SshWarningKind::HostKeyNotVerified));
        assert_eq!(tunnel.security_warnings().len(), 2);
        drop(tunnel);
        assert!(!own3.exists());

        // 8. The user's known_hosts is honored read-only.
        let user_kh = dir.path().join("user_known_hosts");
        let user_text = format!(
            "[127.0.0.1]:{jump_port} {}\n[sshd]:2222 {}\n",
            jump_key.to_openssh().unwrap(),
            server_key.to_openssh().unwrap()
        );
        std::fs::write(&user_kh, &user_text).unwrap();
        let store4 = KnownHostsStore::load(dir.path().join("empty-own"), Some(user_kh.clone()))
            .await
            .unwrap();
        let tunnel = Tunnel::open(spec(&creds, HostKeyPolicy::Strict, HostKeyPolicy::Strict), store4.clone())
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&user_kh).unwrap(), user_text);
        assert!(!dir.path().join("empty-own").exists());

        // 8b. A key revoked in the user's file after the store was loaded is seen at the next
        // open (the files are re-read per open).
        let revoked_kh = dir.path().join("revoked_user_known_hosts");
        std::fs::write(&revoked_kh, &user_text).unwrap();
        let store5 = KnownHostsStore::load(dir.path().join("own5"), Some(revoked_kh.clone()))
            .await
            .unwrap();
        std::fs::write(
            &revoked_kh,
            format!("{user_text}@revoked * {}\n", jump_key.to_openssh().unwrap()),
        )
        .unwrap();
        let err = Tunnel::open(spec(&creds, HostKeyPolicy::Strict, HostKeyPolicy::Strict), store5)
            .await
            .unwrap_err();
        assert!(matches!(err, SshError::HostKeyRevoked { hop: Some(0), .. }), "{err:?}");

        // 8c. Accept-new never silently accepts a host already known under another key type
        // (here the jump host's real RSA key, which NineSQL does not negotiate).
        let rsa_kh = dir.path().join("rsa_user_known_hosts");
        let rsa_key = jump_rsa_line.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        std::fs::write(&rsa_kh, format!("[127.0.0.1]:{jump_port} {rsa_key}\n")).unwrap();
        let own6 = dir.path().join("own6").join("known_hosts");
        let store6 = KnownHostsStore::load(own6.clone(), Some(rsa_kh)).await.unwrap();
        let err = Tunnel::open(spec(&creds, HostKeyPolicy::AcceptNew, HostKeyPolicy::AcceptNew), store6)
            .await
            .unwrap_err();
        let unknown = err.unknown_host_key().unwrap_or_else(|| panic!("{err:?}"));
        assert_eq!(unknown.hop, 0);
        assert_eq!(unknown.other_key_types, vec!["ssh-rsa".to_owned()]);
        assert!(!own6.exists(), "nothing may be recorded without confirmation");

        // 9. Losing the jump host ends the tunnel (keepalive / connection loss).
        docker(&["kill", &lab.name("jump")]).unwrap();
        let state = tokio::time::timeout(Duration::from_secs(30), tunnel.wait_closed())
            .await
            .expect("tunnel did not notice the lost hop");
        assert_eq!(state, TunnelState::Lost);
    });
    drop(rt);
    drop(lab);
}
