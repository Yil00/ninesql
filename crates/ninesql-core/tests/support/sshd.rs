//! A throwaway OpenSSH server in Docker with password authentication and TCP forwarding, for
//! the SSH tests of the connect flow. `start` returns `None` (test skipped) when Docker is not
//! available, unless `NINESQL_REQUIRE_ENGINES` lists `ssh` (AGENTS.md). The container is removed
//! on drop, also when a test panics.
#![allow(clippy::print_stderr)]

use std::io::Read as _;
use std::process::Command;
use std::time::{Duration, Instant};

use ninesql_core::driver_api::Endpoint as ApiEndpoint;

/// `linuxserver/openssh-server`, pinned by digest (same image as `ninesql-ssh`'s test).
const SSH_IMAGE: &str = "linuxserver/openssh-server@sha256:161e0b670b31d9539ca1718bc5943e981779331b4e1c9d32e8929d7652a1f22f";

/// User created in the container.
pub const USER: &str = "tunnel";

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

/// A running server; removed on drop.
pub struct Sshd {
    name: String,
    /// Published port on 127.0.0.1.
    pub port: u16,
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", "-v", &self.name]);
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
pub fn banner(endpoint: &ApiEndpoint) -> Option<String> {
    let ApiEndpoint::Tcp { host, port } = endpoint else {
        return None;
    };
    let mut s = std::net::TcpStream::connect((host.as_str(), *port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buf = [0u8; 16];
    let n = s.read(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf[..n]).into_owned())
}

/// Starts a server where [`USER`] logs in with `password`.
pub fn start(password: &str) -> Option<Sshd> {
    if docker(&["info", "--format", "{{.ServerVersion}}"]).is_err() {
        assert!(
            !required(),
            "Docker is required (NINESQL_REQUIRE_ENGINES) but unavailable"
        );
        eprintln!("skipped: Docker is not available");
        return None;
    }
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
        &format!("USER_NAME={USER}"),
        "-e",
        "PASSWORD_ACCESS=true",
        "-e",
        &format!("USER_PASSWORD={password}"),
        "-p",
        "127.0.0.1::2222",
        SSH_IMAGE,
    ])
    .unwrap_or_else(|e| panic!("docker run: {e}"));
    let mut sshd = Sshd { name, port: 0 };
    wait_log(&sshd.name, "[ls.io-init] done.");
    // The image ships `AllowTcpForwarding no`; a tunnel endpoint needs it.
    docker(&[
        "exec",
        &sshd.name,
        "sh",
        "-c",
        "sed -i 's/^AllowTcpForwarding no/AllowTcpForwarding yes/' /config/sshd/sshd_config \
         && grep -q '^AllowTcpForwarding yes' /config/sshd/sshd_config \
         && s6-svc -r /run/service/svc-openssh-server",
    ])
    .unwrap();
    let published = docker(&["port", &sshd.name, "2222/tcp"]).unwrap();
    sshd.port = published
        .lines()
        .next()
        .and_then(|l| l.rsplit(':').next())
        .and_then(|p| p.parse().ok())
        .unwrap();
    let start = Instant::now();
    while banner(&ApiEndpoint::tcp("127.0.0.1", sshd.port)).is_none_or(|b| !b.starts_with("SSH-")) {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "sshd never answered"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
    Some(sshd)
}

/// Starts a server on the **host network**, listening on `127.0.0.1` only, on a free port, where
/// [`USER`] logs in with `password`. From inside it, `localhost` is the host's loopback, so a
/// tunnel can reach the `dev/` engines on their published ports with the host names their test
/// certificates carry (`localhost`), and `verify-full` works through the tunnel.
pub fn start_host_network(password: &str) -> Option<Sshd> {
    if docker(&["info", "--format", "{{.ServerVersion}}"]).is_err() {
        assert!(
            !required(),
            "Docker is required (NINESQL_REQUIRE_ENGINES) but unavailable"
        );
        eprintln!("skipped: Docker is not available");
        return None;
    }
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let name = format!("ninesql-core-ssh-host-it-{}", uuid::Uuid::new_v4().simple());
    docker(&[
        "run",
        "-d",
        "--name",
        &name,
        "--network",
        "host",
        "-e",
        "PUID=1000",
        "-e",
        "PGID=1000",
        "-e",
        &format!("USER_NAME={USER}"),
        "-e",
        "PASSWORD_ACCESS=true",
        "-e",
        &format!("USER_PASSWORD={password}"),
        SSH_IMAGE,
    ])
    .unwrap_or_else(|e| panic!("docker run: {e}"));
    let sshd = Sshd { name, port };
    wait_log(&sshd.name, "[ls.io-init] done.");
    // Forwarding on, and only the loopback of the host on a port of our own.
    docker(&[
        "exec",
        &sshd.name,
        "sh",
        "-c",
        &format!(
            "sed -i -e 's/^AllowTcpForwarding no/AllowTcpForwarding yes/' \
               -e 's/^Port 2222$/Port {port}\\nListenAddress 127.0.0.1/' /config/sshd/sshd_config \
             && grep -q '^AllowTcpForwarding yes' /config/sshd/sshd_config \
             && grep -q '^Port {port}$' /config/sshd/sshd_config \
             && s6-svc -r /run/service/svc-openssh-server"
        ),
    ])
    .unwrap();
    let start = Instant::now();
    while banner(&ApiEndpoint::tcp("127.0.0.1", port)).is_none_or(|b| !b.starts_with("SSH-")) {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "sshd never answered"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
    Some(sshd)
}

/// A dev mock connection through the server, with a prompted SSH password. The database "as
/// seen from the SSH server" is the server's own sshd, so a test can check forwarding by reading
/// its banner through the tunnel.
pub fn connection(port: u16, policy: ninesql_core::HostKeyPolicy) -> ninesql_core::Connection {
    use ninesql_core::{Endpoint, SecretSource, SshAuth, SshHop, SshTunnel};
    let mut c = super::mock_connection(ninesql_core::ConnectionTypeId::dev());
    c.endpoint = Endpoint::Tcp {
        host: "localhost".into(),
        port: Some(2222),
        database: None,
    };
    let mut server = SshHop::new(
        "127.0.0.1",
        port,
        USER,
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
