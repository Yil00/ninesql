//! SSH tunnels for database connections (ADR-0011 §5–7, invariants I1, I2, I5, I6). ROADMAP: P1-D.
//! Security review required for every change.
//!
//! - [`Tunnel::open`] opens a local port forward `127.0.0.1:<ephemeral>` → database endpoint
//!   through a ProxyJump chain of [`HopSpec`]s, with `russh` (pure Rust, `ring` crypto
//!   provider, no OpenSSL). Drivers only receive [`Tunnel::endpoint`] and [`Tunnel::info`].
//! - Host keys ([`known_hosts`]) are checked on every hop against NineSQL's own
//!   `$XDG_DATA_HOME/ninesql/known_hosts` and, read-only, the user's `~/.ssh/known_hosts`.
//!   An unknown key yields [`SshError::HostKeyNeedsConfirmation`] with its SHA-256
//!   [`Fingerprint`]; a changed or revoked key is a hard error. [`HostKeyPolicy::AcceptNew`]
//!   and [`HostKeyPolicy::Disabled`] are explicit per-hop downgrades reported as
//!   [`SshSecurityWarning`]s.
//! - Authentication ([`HopAuth`]): password, private key (Ed25519 / ECDSA, optional
//!   passphrase), `ssh-agent`. Secrets are [`secrecy::SecretString`]s consumed by the hop's
//!   authentication and zeroized on drop; nothing secret is logged.
//!
//! Not supported yet (see the P1-D task report):
//! - RSA private keys and RSA host keys: russh's `rsa` feature pins `rsa =0.10.0-rc.18`,
//!   flagged by RUSTSEC-2023-0071. RSA keys held by `ssh-agent` are still offered (SHA-2 only).
//! - HTTP CONNECT / SOCKS5 proxy in front of the first hop (ADR-0011 §8).
//! - Keyboard-interactive and multi-factor authentication, host certificates.
#![forbid(unsafe_code)]

mod auth;
mod error;
mod fingerprint;
pub mod known_hosts;
mod policy;
mod tunnel;

pub use auth::HopAuth;
pub use error::{KeyFileError, SshError, UnknownHostKey};
pub use fingerprint::Fingerprint;
pub use known_hosts::{HostKeyStatus, KnownHosts, KnownHostsSource};
pub use policy::{HostKeyDecision, HostKeyPolicy, decide};
pub use tunnel::{
    HopSpec, KnownHostsStore, MAX_JUMP_HOSTS, SshSecurityWarning, SshWarningKind, Tunnel,
    TunnelOptions, TunnelSpec, TunnelState,
};
