//! Local port-forward tunnels through a ProxyJump chain (ADR-0011 §5).
//!
//! [`Tunnel::open`] connects to the first hop over TCP, then reaches every next hop through a
//! `direct-tcpip` channel of the previous one (the ProxyJump model: the hop itself never sees
//! the credentials of the next one). Each hop's host key is verified against
//! [`KnownHostsStore`] under that hop's [`HostKeyPolicy`] before any credential is sent. The
//! local end is a listener on `127.0.0.1` with an ephemeral port; each accepted connection gets
//! its own `direct-tcpip` channel from the last hop to the database endpoint.
//!
//! Networking (I2): the only outbound TCP connection is to the first declared hop; later hops
//! and the database are reached from inside the SSH session. No proxy support yet
//! (ADR-0011 §8: see the crate docs).
//!
//! Everything runs on the Tokio runtime given by the caller (the driver runtime, ADR-0004); no
//! call blocks a thread: file I/O goes through `spawn_blocking`.

use std::borrow::Cow;
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ninesql_driver_api::{Endpoint, TunnelInfo};
use russh::client::{self, ChannelOpenHandle, Handle, Handler, Msg, Session};
use russh::keys::{Algorithm, EcdsaCurve, PublicKeyOrCertificate};
use russh::{Channel, Disconnect, Preferred};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

use crate::auth::{HopAuth, HopRef, authenticate};
use crate::error::{SshError, UnknownHostKey};
use crate::fingerprint::Fingerprint;
use crate::known_hosts::{DEFAULT_SSH_PORT, HostKeyStatus, KnownHosts};
use crate::policy::{HostKeyDecision, HostKeyPolicy, decide};

/// Most jump hosts accepted before the tunnel server.
pub const MAX_JUMP_HOSTS: usize = 8;

/// One SSH server of the chain.
#[derive(Debug)]
#[non_exhaustive]
pub struct HopSpec {
    /// SSH host name or address.
    pub host: String,
    /// SSH port.
    pub port: u16,
    /// SSH user name.
    pub username: String,
    /// Authentication, consumed when the hop authenticates.
    pub auth: HopAuth,
    /// Host key policy of this hop; default strict.
    pub host_key_policy: HostKeyPolicy,
}

impl HopSpec {
    /// A hop on port 22 with a strict host key policy.
    #[must_use]
    pub fn new(host: impl Into<String>, username: impl Into<String>, auth: HopAuth) -> Self {
        Self {
            host: host.into(),
            port: DEFAULT_SSH_PORT,
            username: username.into(),
            auth,
            host_key_policy: HostKeyPolicy::Strict,
        }
    }

    /// Sets the port.
    #[must_use]
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Sets the host key policy (a downgrade must come from an explicit user choice).
    #[must_use]
    pub fn with_host_key_policy(mut self, policy: HostKeyPolicy) -> Self {
        self.host_key_policy = policy;
        self
    }
}

/// Timeouts and limits of a tunnel.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct TunnelOptions {
    /// Per hop: TCP / channel connection, handshake and authentication must finish within it.
    pub connect_timeout: Duration,
    /// Keepalive sent after this much silence from the server.
    pub keepalive_interval: Duration,
    /// Unanswered keepalives after which the session is considered lost.
    pub keepalive_max: usize,
    /// Concurrent forwarded connections; further local connections are refused.
    pub max_forwarded_connections: usize,
}

impl Default for TunnelOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(20),
            keepalive_interval: Duration::from_secs(30),
            keepalive_max: 3,
            max_forwarded_connections: 32,
        }
    }
}

/// A tunnel to open: jump hosts (first one first), then the server that forwards to `target`.
#[derive(Debug)]
#[non_exhaustive]
pub struct TunnelSpec {
    /// Jump hosts crossed before `server`, in ProxyJump order.
    pub jump_hosts: Vec<HopSpec>,
    /// The SSH server that connects to the database.
    pub server: HopSpec,
    /// Database host, resolved by `server`.
    pub target_host: String,
    /// Database port.
    pub target_port: u16,
    /// Timeouts and limits.
    pub options: TunnelOptions,
}

impl TunnelSpec {
    /// A direct tunnel (no jump host) with default options.
    #[must_use]
    pub fn new(server: HopSpec, target_host: impl Into<String>, target_port: u16) -> Self {
        Self {
            jump_hosts: Vec::new(),
            server,
            target_host: target_host.into(),
            target_port,
            options: TunnelOptions::default(),
        }
    }

    /// Adds a jump host after the existing ones.
    #[must_use]
    pub fn via(mut self, jump: HopSpec) -> Self {
        self.jump_hosts.push(jump);
        self
    }

    /// Sets the options.
    #[must_use]
    pub fn with_options(mut self, options: TunnelOptions) -> Self {
        self.options = options;
        self
    }

    /// Downgrades configured on this tunnel, to show before connecting (I5).
    #[must_use]
    pub fn security_warnings(&self) -> Vec<SshSecurityWarning> {
        self.jump_hosts
            .iter()
            .chain(std::iter::once(&self.server))
            .enumerate()
            .filter_map(|(i, h)| SshSecurityWarning::for_policy(i, h))
            .collect()
    }

    fn validate(&self) -> Result<(), SshError> {
        if self.jump_hosts.len() > MAX_JUMP_HOSTS {
            return Err(SshError::InvalidSpec("too many jump hosts (at most 8)"));
        }
        if self.target_host.is_empty() || self.target_port == 0 {
            return Err(SshError::InvalidSpec(
                "the database host and port are required",
            ));
        }
        for h in self.jump_hosts.iter().chain(std::iter::once(&self.server)) {
            if h.host.is_empty() || h.port == 0 {
                return Err(SshError::InvalidSpec(
                    "every SSH hop needs a host and a port",
                ));
            }
            if h.username.is_empty() {
                return Err(SshError::InvalidSpec("every SSH hop needs a user name"));
            }
        }
        if self.options.connect_timeout.is_zero() || self.options.max_forwarded_connections == 0 {
            return Err(SshError::InvalidSpec(
                "timeouts and limits must be positive",
            ));
        }
        Ok(())
    }
}

/// A host key check that was weakened on one hop. The UI shows it on the connection and its
/// tabs (I5).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SshSecurityWarning {
    /// Hop index (jump hosts first, the tunnel server last).
    pub hop: usize,
    /// SSH host.
    pub host: String,
    /// SSH port.
    pub port: u16,
    /// What was weakened.
    pub kind: SshWarningKind,
}

/// Kind of [`SshSecurityWarning`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SshWarningKind {
    /// Host key verification is disabled on this hop.
    HostKeyNotVerified,
    /// Unknown host keys are accepted without confirmation on this hop.
    AcceptNewHostKeys,
    /// A key was accepted without confirmation during this connection and recorded.
    HostKeyAutoAccepted(Fingerprint),
}

impl SshSecurityWarning {
    fn for_policy(hop: usize, h: &HopSpec) -> Option<Self> {
        let kind = match h.host_key_policy {
            HostKeyPolicy::Strict => return None,
            HostKeyPolicy::AcceptNew => SshWarningKind::AcceptNewHostKeys,
            HostKeyPolicy::Disabled => SshWarningKind::HostKeyNotVerified,
        };
        Some(Self {
            hop,
            host: h.host.clone(),
            port: h.port,
            kind,
        })
    }
}

/// Shared, cheaply clonable access to the `known_hosts` files. File I/O runs on Tokio's
/// blocking pool.
#[derive(Clone)]
pub struct KnownHostsStore(Arc<Mutex<KnownHosts>>);

impl fmt::Debug for KnownHostsStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KnownHostsStore")
    }
}

impl KnownHostsStore {
    /// Wraps already loaded files.
    #[must_use]
    pub fn new(known_hosts: KnownHosts) -> Self {
        Self(Arc::new(Mutex::new(known_hosts)))
    }

    /// Loads NineSQL's file and, optionally, the user's read-only file (see [`KnownHosts::load`]).
    ///
    /// # Errors
    /// See [`KnownHosts::load`].
    pub async fn load(
        own_path: std::path::PathBuf,
        user_path: Option<std::path::PathBuf>,
    ) -> Result<Self, SshError> {
        let kh =
            tokio::task::spawn_blocking(move || KnownHosts::load(&own_path, user_path.as_deref()))
                .await
                .map_err(|_| SshError::Internal("known_hosts loading task failed"))??;
        Ok(Self::new(kh))
    }

    fn with<T>(&self, f: impl FnOnce(&mut KnownHosts) -> T) -> Result<T, SshError> {
        let mut guard = self
            .0
            .lock()
            .map_err(|_| SshError::Internal("known_hosts lock poisoned"))?;
        Ok(f(&mut guard))
    }

    /// In-memory lookup (no I/O).
    ///
    /// # Errors
    /// Only [`SshError::Internal`] if the lock is poisoned.
    pub fn check(&self, host: &str, port: u16, key_blob: &[u8]) -> Result<HostKeyStatus, SshError> {
        self.with(|kh| kh.check(host, port, key_blob))
    }

    fn known_key_types(&self, host: &str, port: u16) -> Result<Vec<String>, SshError> {
        self.with(|kh| kh.known_key_types(host, port))
    }

    /// Records a key the user confirmed (from [`SshError::HostKeyNeedsConfirmation`]) in
    /// NineSQL's file. Call it only after the user saw and accepted the fingerprint.
    ///
    /// # Errors
    /// See [`KnownHosts::add`].
    pub async fn trust(&self, key: &UnknownHostKey) -> Result<(), SshError> {
        self.add(
            key.host.clone(),
            key.port,
            key.key_type.clone(),
            key.key_blob.clone(),
        )
        .await
    }

    async fn add(
        &self,
        host: String,
        port: u16,
        key_type: String,
        key_blob: Vec<u8>,
    ) -> Result<(), SshError> {
        let inner = Arc::clone(&self.0);
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .map_err(|_| SshError::Internal("known_hosts lock poisoned"))?
                .add(&host, port, &key_type, &key_blob)
        })
        .await
        .map_err(|_| SshError::Internal("known_hosts task failed"))?
    }

    /// Removes NineSQL's entries for a host (the explicit step required after a changed key).
    ///
    /// # Errors
    /// See [`KnownHosts::remove_host`].
    pub async fn remove_host(&self, host: String, port: u16) -> Result<usize, SshError> {
        let inner = Arc::clone(&self.0);
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .map_err(|_| SshError::Internal("known_hosts lock poisoned"))?
                .remove_host(&host, port)
        })
        .await
        .map_err(|_| SshError::Internal("known_hosts task failed"))?
    }
}

/// Host key algorithms negotiated: Ed25519 and ECDSA only. No `ssh-rsa` (SHA-1), no DSA
/// (ADR-0011 §7); RSA-SHA2 host keys need the `rsa` feature of russh, off for now.
const HOST_KEY_ALGORITHMS: &[Algorithm] = &[
    Algorithm::Ed25519,
    Algorithm::Ecdsa {
        curve: EcdsaCurve::NistP256,
    },
    Algorithm::Ecdsa {
        curve: EcdsaCurve::NistP384,
    },
    Algorithm::Ecdsa {
        curve: EcdsaCurve::NistP521,
    },
];

/// russh defaults minus anything SHA-1 or CBC based (ADR-0011 §7), host key algorithms ordered
/// so that the types on file for this host come first.
fn preferred(known_types: &[String]) -> Preferred {
    let base = Preferred::default();
    let weak = |name: &str| {
        let n = name.to_ascii_lowercase();
        n.contains("sha1") || n.contains("cbc") || n.contains("3des") || n.contains("md5")
    };
    let mut keys: Vec<Algorithm> = HOST_KEY_ALGORITHMS
        .iter()
        .filter(|a| known_types.iter().any(|t| t == a.as_str()))
        .cloned()
        .collect();
    for a in HOST_KEY_ALGORITHMS {
        if !keys.contains(a) {
            keys.push(a.clone());
        }
    }
    Preferred {
        kex: Cow::Owned(
            base.kex
                .iter()
                .filter(|k| !weak(k.as_ref()))
                .copied()
                .collect(),
        ),
        key: Cow::Owned(keys),
        host_key_certificates: Cow::Borrowed(&[]),
        cipher: Cow::Owned(
            base.cipher
                .iter()
                .filter(|c| !weak(c.as_ref()))
                .copied()
                .collect(),
        ),
        mac: Cow::Owned(
            base.mac
                .iter()
                .filter(|m| !weak(m.as_ref()))
                .copied()
                .collect(),
        ),
        compression: Cow::Owned(
            base.compression
                .iter()
                .filter(|c| c.as_ref() == "none")
                .copied()
                .collect(),
        ),
    }
}

fn client_config(options: &TunnelOptions, known_types: &[String]) -> client::Config {
    client::Config {
        preferred: preferred(known_types),
        keepalive_interval: Some(options.keepalive_interval),
        keepalive_max: options.keepalive_max,
        inactivity_timeout: None,
        nodelay: true,
        ..client::Config::default()
    }
}

/// Outcome of the host key check, read after the handshake.
#[derive(Debug)]
enum HostKeyOutcome {
    Accepted { record: Option<(String, Vec<u8>)> },
    Rejected(SshError),
}

/// russh handler of one hop: verifies the host key, refuses every server-initiated channel.
struct HopHandler {
    hop: usize,
    host: String,
    port: u16,
    policy: HostKeyPolicy,
    store: KnownHostsStore,
    /// Key accepted at the first key exchange: re-keys must present the same one.
    accepted: Option<Vec<u8>>,
    outcome: Arc<Mutex<Option<HostKeyOutcome>>>,
}

impl HopHandler {
    fn set_outcome(&self, o: HostKeyOutcome) {
        if let Ok(mut slot) = self.outcome.lock() {
            *slot = Some(o);
        }
    }

    fn verify(&mut self, key: &PublicKeyOrCertificate) -> bool {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            self.set_outcome(HostKeyOutcome::Rejected(SshError::UnsupportedHostKey {
                host: self.host.clone(),
                port: self.port,
                detail: "host certificate".into(),
            }));
            return false;
        };
        let Ok(blob) = key.to_bytes() else {
            self.set_outcome(HostKeyOutcome::Rejected(SshError::InvalidHostKey));
            return false;
        };
        if let Some(accepted) = &self.accepted {
            // Re-key: the server must keep the key it proved at the first exchange.
            let same = *accepted == blob;
            if !same {
                tracing::warn!(hop = self.hop, host = %self.host, "SSH host key changed during re-key");
            }
            return same;
        }
        let key_type = key.algorithm().as_str().to_owned();
        let fingerprint = Fingerprint::of_blob(&blob);
        let status = match self.store.check(&self.host, self.port, &blob) {
            Ok(s) => s,
            Err(e) => {
                self.set_outcome(HostKeyOutcome::Rejected(e));
                return false;
            }
        };
        let decision = decide(self.policy, &status);
        let host = self.host.clone();
        let port = self.port;
        let hop = Some(self.hop);
        let outcome = match (decision, status) {
            (HostKeyDecision::Accept, _) => HostKeyOutcome::Accepted { record: None },
            (HostKeyDecision::AcceptUnverified, _) => {
                tracing::warn!(hop = self.hop, host = %host, port, %fingerprint, "SSH host key NOT verified (disabled for this connection)");
                HostKeyOutcome::Accepted { record: None }
            }
            (HostKeyDecision::AcceptAndRecord, _) => {
                tracing::warn!(hop = self.hop, host = %host, port, %fingerprint, "unknown SSH host key accepted without confirmation (accept-new policy)");
                HostKeyOutcome::Accepted {
                    record: Some((key_type, blob.clone())),
                }
            }
            (HostKeyDecision::NeedsConfirmation, HostKeyStatus::Unknown { other_key_types }) => {
                HostKeyOutcome::Rejected(SshError::HostKeyNeedsConfirmation(Box::new(
                    UnknownHostKey {
                        hop: self.hop,
                        host,
                        port,
                        key_type,
                        fingerprint,
                        key_blob: blob.clone(),
                        other_key_types,
                    },
                )))
            }
            (
                HostKeyDecision::RejectChanged,
                HostKeyStatus::Changed {
                    file,
                    line,
                    known_fingerprint,
                },
            ) => HostKeyOutcome::Rejected(SshError::HostKeyChanged {
                hop,
                host,
                port,
                file,
                line,
                known_fingerprint,
                presented_fingerprint: fingerprint,
            }),
            (HostKeyDecision::RejectRevoked, HostKeyStatus::Revoked { file, line }) => {
                HostKeyOutcome::Rejected(SshError::HostKeyRevoked {
                    hop,
                    host,
                    port,
                    file,
                    line,
                })
            }
            // Any other pairing is impossible by construction of `decide`; refuse.
            _ => HostKeyOutcome::Rejected(SshError::Internal("inconsistent host key decision")),
        };
        let ok = matches!(outcome, HostKeyOutcome::Accepted { .. });
        if ok {
            self.accepted = Some(blob);
        }
        self.set_outcome(outcome);
        ok
    }
}

impl Handler for HopHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(self.verify(key))
    }

    // Server-initiated channels are never wanted (no agent / X11 / remote forwarding is
    // requested): dropping the reply rejects them.
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        _channel: Channel<Msg>,
        _connected_address: &str,
        _connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        drop(reply);
        Ok(())
    }

    async fn server_channel_open_forwarded_streamlocal(
        &mut self,
        _channel: Channel<Msg>,
        _socket_path: &str,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        drop(reply);
        Ok(())
    }

    async fn server_channel_open_agent_forward(
        &mut self,
        _channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        drop(reply);
        Ok(())
    }

    async fn server_channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        drop(reply);
        Ok(())
    }

    async fn server_channel_open_direct_tcpip(
        &mut self,
        _channel: Channel<Msg>,
        _host_to_connect: &str,
        _port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        drop(reply);
        Ok(())
    }

    async fn server_channel_open_direct_streamlocal(
        &mut self,
        _channel: Channel<Msg>,
        _socket_path: &str,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        drop(reply);
        Ok(())
    }

    async fn server_channel_open_x11(
        &mut self,
        _channel: Channel<Msg>,
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        drop(reply);
        Ok(())
    }
}

/// Lifecycle state of an open tunnel.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TunnelState {
    /// Accepting local connections.
    Running,
    /// Closed on request ([`Tunnel::close`] or drop).
    Closed,
    /// An SSH session of the chain ended (network loss, keepalive timeout, server shutdown).
    Lost,
}

type SshHandle = Handle<HopHandler>;

/// An open tunnel. Drivers receive only [`Tunnel::endpoint`] and [`Tunnel::info`]; they never
/// see SSH.
///
/// Dropping the handle shuts the tunnel down (in the background, on the runtime);
/// [`Tunnel::close`] does it and waits for the end.
pub struct Tunnel {
    local_addr: SocketAddr,
    info: TunnelInfo,
    warnings: Vec<SshSecurityWarning>,
    shutdown: watch::Sender<bool>,
    state: watch::Receiver<TunnelState>,
    task: Option<JoinHandle<()>>,
}

impl fmt::Debug for Tunnel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tunnel")
            .field("local_addr", &self.local_addr)
            .field("info", &self.info)
            .field("state", &*self.state.borrow())
            .finish_non_exhaustive()
    }
}

impl Tunnel {
    /// Opens the tunnel: connects and authenticates every hop in order, then listens on
    /// `127.0.0.1:<ephemeral>`. Must run on a Tokio runtime (the driver runtime).
    ///
    /// Cancelling: drop the returned future; every session opened so far is closed.
    ///
    /// # Errors
    /// [`SshError::HostKeyNeedsConfirmation`] for an unknown host key under the strict policy
    /// (confirm with [`KnownHostsStore::trust`] and call again), [`SshError::HostKeyChanged`] /
    /// [`SshError::HostKeyRevoked`] (hard errors), connection, timeout, protocol and
    /// authentication errors, each naming the hop.
    pub async fn open(spec: TunnelSpec, known_hosts: KnownHostsStore) -> Result<Self, SshError> {
        spec.validate()?;
        let mut warnings = spec.security_warnings();
        let TunnelSpec {
            jump_hosts,
            server,
            target_host,
            target_port,
            options,
        } = spec;
        let jump_count = u8::try_from(jump_hosts.len())
            .map_err(|_| SshError::InvalidSpec("too many jump hosts (at most 8)"))?;
        let mut weakest = HostKeyPolicy::Strict;
        let (ssh_host, ssh_port) = (server.host.clone(), server.port);

        let mut sessions: Vec<Arc<SshHandle>> = Vec::with_capacity(jump_hosts.len() + 1);
        for (index, hop) in jump_hosts
            .into_iter()
            .chain(std::iter::once(server))
            .enumerate()
        {
            weakest = weakest.weakest(hop.host_key_policy);
            let previous = sessions.last().cloned();
            let (session, auto_accepted) =
                open_hop(index, hop, previous, &options, &known_hosts).await?;
            if let Some(w) = auto_accepted {
                warnings.push(w);
            }
            sessions.push(Arc::new(session));
        }

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|e| SshError::Listener(e.to_string()))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| SshError::Listener(e.to_string()))?;

        let mut info = TunnelInfo::new(ssh_host, ssh_port);
        info.jump_hosts = jump_count;
        info.host_key = weakest.to_verification();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (state_tx, state_rx) = watch::channel(TunnelState::Running);
        let task = tokio::spawn(serve(
            listener,
            sessions,
            target_host,
            target_port,
            options.max_forwarded_connections,
            shutdown_rx,
            state_tx,
        ));
        tracing::info!(
            local = %local_addr,
            hops = usize::from(jump_count) + 1,
            "SSH tunnel open"
        );
        Ok(Self {
            local_addr,
            info,
            warnings,
            shutdown: shutdown_tx,
            state: state_rx,
            task: Some(task),
        })
    }

    /// Local address of the tunnel (`127.0.0.1:<ephemeral>`).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Endpoint to hand to the driver ([`ConnectParams`](ninesql_driver_api::ConnectParams)).
    /// The core also sets `TlsConfig::server_name` to the real database host so `verify-full`
    /// still checks the server's name.
    #[must_use]
    pub fn endpoint(&self) -> Endpoint {
        Endpoint::Tcp {
            host: self.local_addr.ip().to_string(),
            port: self.local_addr.port(),
        }
    }

    /// Route description for [`ConnectParams::tunnel`](ninesql_driver_api::ConnectParams).
    /// `host_key` is the weakest policy across hops.
    #[must_use]
    pub fn info(&self) -> &TunnelInfo {
        &self.info
    }

    /// Host key downgrades of this tunnel (configured policies and keys auto-accepted while
    /// opening it).
    #[must_use]
    pub fn security_warnings(&self) -> &[SshSecurityWarning] {
        &self.warnings
    }

    /// Current state.
    #[must_use]
    pub fn state(&self) -> TunnelState {
        self.state.borrow().clone()
    }

    /// Waits until the tunnel is no longer running and returns the final state.
    pub async fn wait_closed(&self) -> TunnelState {
        let mut rx = self.state.clone();
        loop {
            let s = rx.borrow_and_update().clone();
            if s != TunnelState::Running {
                return s;
            }
            if rx.changed().await.is_err() {
                return TunnelState::Closed;
            }
        }
    }

    /// Closes the tunnel: stops accepting, ends forwarded connections, disconnects every SSH
    /// session (last hop first) and waits for that to finish.
    pub async fn close(mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        // The serving task notices and disconnects in the background.
        let _ = self.shutdown.send(true);
    }
}

/// Connects, verifies and authenticates one hop, directly (first hop) or through `previous`.
async fn open_hop(
    index: usize,
    hop: HopSpec,
    previous: Option<Arc<SshHandle>>,
    options: &TunnelOptions,
    known_hosts: &KnownHostsStore,
) -> Result<(SshHandle, Option<SshSecurityWarning>), SshError> {
    let HopSpec {
        host,
        port,
        username,
        auth,
        host_key_policy,
    } = hop;
    let timeout_err = || SshError::Timeout {
        hop: index,
        host: host.clone(),
        port,
    };
    let fut = async {
        let known_types = known_hosts.known_key_types(&host, port)?;
        let config = Arc::new(client_config(options, &known_types));
        let outcome = Arc::new(Mutex::new(None));
        let handler = HopHandler {
            hop: index,
            host: host.clone(),
            port,
            policy: host_key_policy,
            store: known_hosts.clone(),
            accepted: None,
            outcome: Arc::clone(&outcome),
        };
        let connect_err = |detail: String| SshError::Connect {
            hop: index,
            host: host.clone(),
            port,
            detail,
        };
        let result = match previous {
            None => {
                let tcp = TcpStream::connect((host.as_str(), port))
                    .await
                    .map_err(|e| connect_err(e.to_string()))?;
                let _ = tcp.set_nodelay(true);
                handshake(config, tcp, handler).await
            }
            Some(prev) => {
                let channel = prev
                    .channel_open_direct_tcpip(host.as_str(), u32::from(port), "127.0.0.1", 0)
                    .await
                    .map_err(|e| {
                        connect_err(format!("the previous hop refused to forward ({e})"))
                    })?;
                handshake(config, channel.into_stream(), handler).await
            }
        };
        let taken = outcome.lock().ok().and_then(|mut o| o.take());
        let mut session = match (result, taken) {
            (_, Some(HostKeyOutcome::Rejected(e))) => return Err(e),
            (Ok(s), Some(HostKeyOutcome::Accepted { record })) => (s, record),
            (Ok(_), None) => {
                return Err(SshError::Internal("host key was not checked"));
            }
            (Err(e), _) => {
                return Err(SshError::Protocol {
                    hop: index,
                    host: host.clone(),
                    port,
                    detail: e.to_string(),
                });
            }
        };
        let mut warning = None;
        if let Some((key_type, blob)) = session.1.take() {
            let fp = Fingerprint::of_blob(&blob);
            known_hosts.add(host.clone(), port, key_type, blob).await?;
            warning = Some(SshSecurityWarning {
                hop: index,
                host: host.clone(),
                port,
                kind: SshWarningKind::HostKeyAutoAccepted(fp),
            });
        }
        let mut handle = session.0;
        let hop_ref = HopRef {
            index,
            host: &host,
            port,
            user: &username,
        };
        authenticate(&mut handle, &hop_ref, auth).await?;
        tracing::debug!(hop = index, host = %host, port, "SSH hop authenticated");
        Ok((handle, warning))
    };
    tokio::time::timeout(options.connect_timeout, fut)
        .await
        .map_err(|_| timeout_err())?
}

async fn handshake<R>(
    config: Arc<client::Config>,
    stream: R,
    handler: HopHandler,
) -> Result<SshHandle, russh::Error>
where
    R: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    client::connect_stream(config, stream, handler).await
}

/// Accept loop: one `direct-tcpip` channel per local connection, until shutdown or loss of a
/// session of the chain.
async fn serve(
    listener: TcpListener,
    sessions: Vec<Arc<SshHandle>>,
    target_host: String,
    target_port: u16,
    max_connections: usize,
    mut shutdown: watch::Receiver<bool>,
    state: watch::Sender<TunnelState>,
) {
    let mut conns = JoinSet::new();
    let mut health = tokio::time::interval(Duration::from_millis(500));
    health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let Some(last) = sessions.last().cloned() else {
        let _ = state.send(TunnelState::Lost);
        return;
    };
    let final_state = loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break TunnelState::Closed;
                }
            }
            _ = health.tick() => {
                if sessions.iter().any(|s| s.is_closed()) {
                    tracing::warn!("SSH tunnel lost: a session of the chain ended");
                    break TunnelState::Lost;
                }
            }
            accepted = listener.accept() => {
                let (socket, peer) = match accepted {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::debug!(error = %e, "tunnel accept failed");
                        continue;
                    }
                };
                if !peer.ip().is_loopback() {
                    continue;
                }
                if conns.len() >= max_connections {
                    tracing::warn!(max_connections, "tunnel connection limit reached; refusing a local connection");
                    continue;
                }
                let last = Arc::clone(&last);
                let host = target_host.clone();
                conns.spawn(forward(socket, peer, last, host, target_port));
            }
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
        }
    };
    drop(listener);
    conns.abort_all();
    while conns.join_next().await.is_some() {}
    for s in sessions.iter().rev() {
        let _ = s
            .disconnect(Disconnect::ByApplication, "tunnel closed", "en")
            .await;
    }
    drop(sessions);
    let _ = state.send(final_state);
    tracing::info!("SSH tunnel closed");
}

async fn forward(
    mut socket: TcpStream,
    peer: SocketAddr,
    last: Arc<SshHandle>,
    host: String,
    port: u16,
) {
    let _ = socket.set_nodelay(true);
    let channel = match last
        .channel_open_direct_tcpip(host, u32::from(port), "127.0.0.1", u32::from(peer.port()))
        .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "the SSH server refused to forward to the database endpoint");
            return;
        }
    };
    let mut stream = channel.into_stream();
    if let Err(e) = tokio::io::copy_bidirectional(&mut socket, &mut stream).await {
        tracing::debug!(error = %e, "tunnel connection ended with an error");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;
    use secrecy::SecretString;

    fn hop(host: &str) -> HopSpec {
        HopSpec::new(host, "u", HopAuth::Password(SecretString::from("x")))
    }

    #[test]
    fn preferred_has_no_weak_algorithms() {
        let p = preferred(&[]);
        for n in p
            .kex
            .iter()
            .map(AsRef::as_ref)
            .chain(p.cipher.iter().map(AsRef::as_ref))
            .chain(p.mac.iter().map(AsRef::as_ref))
        {
            assert!(!n.contains("sha1") && !n.contains("cbc"), "{n}");
        }
        assert!(p.key.iter().all(|a| !a.clone().is_rsa()));
        assert_eq!(p.key.first(), Some(&Algorithm::Ed25519));
        assert!(p.compression.iter().all(|c| c.as_ref() == "none"));
        assert!(p.host_key_certificates.is_empty());
    }

    #[test]
    fn preferred_puts_known_types_first() {
        let p = preferred(&["ecdsa-sha2-nistp256".to_owned()]);
        assert_eq!(
            p.key.first(),
            Some(&Algorithm::Ecdsa {
                curve: EcdsaCurve::NistP256
            })
        );
        assert_eq!(p.key.len(), HOST_KEY_ALGORITHMS.len());
    }

    #[test]
    fn spec_validation_and_warnings() {
        let spec = TunnelSpec::new(hop("s"), "db", 5432);
        assert!(spec.validate().is_ok());
        assert!(spec.security_warnings().is_empty());

        let spec = TunnelSpec::new(
            hop("s").with_host_key_policy(HostKeyPolicy::Disabled),
            "db",
            5432,
        )
        .via(hop("j").with_host_key_policy(HostKeyPolicy::AcceptNew));
        let w = spec.security_warnings();
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].hop, 0);
        assert_eq!(w[0].kind, SshWarningKind::AcceptNewHostKeys);
        assert_eq!(w[1].hop, 1);
        assert_eq!(w[1].kind, SshWarningKind::HostKeyNotVerified);

        let mut spec = TunnelSpec::new(hop("s"), "db", 0);
        assert!(spec.validate().is_err());
        spec.target_port = 1;
        for _ in 0..=MAX_JUMP_HOSTS {
            spec.jump_hosts.push(hop("j"));
        }
        assert!(spec.validate().is_err());
        let spec = TunnelSpec::new(hop(""), "db", 1);
        assert!(spec.validate().is_err());
    }

    #[tokio::test]
    async fn unreachable_first_hop_is_a_typed_error() {
        // Bind then drop to get a closed local port: nothing outside 127.0.0.1 is contacted.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let dir = tempfile::tempdir().unwrap();
        let store = KnownHostsStore::load(dir.path().join("kh"), None)
            .await
            .unwrap();
        let spec = TunnelSpec::new(hop("127.0.0.1").with_port(port), "db", 5432);
        let err = Tunnel::open(spec, store).await.unwrap_err();
        assert!(matches!(err, SshError::Connect { hop: 0, .. }), "{err:?}");
    }

    #[tokio::test]
    async fn silent_server_times_out() {
        // A listener that accepts but never speaks SSH.
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let _keep = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((s, _)) = l.accept().await {
                held.push(s);
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let store = KnownHostsStore::load(dir.path().join("kh"), None)
            .await
            .unwrap();
        let opts = TunnelOptions {
            connect_timeout: Duration::from_millis(300),
            ..TunnelOptions::default()
        };
        let spec = TunnelSpec::new(hop("127.0.0.1").with_port(port), "db", 5432).with_options(opts);
        let err = Tunnel::open(spec, store).await.unwrap_err();
        assert!(matches!(err, SshError::Timeout { hop: 0, .. }), "{err:?}");
    }
}
