# ADR-0011: TLS and SSH defaults

- **Status**: Accepted. Partially amended by [ADR-0016](0016-ssh-rsa-and-host-key-policies.md) for Decision §5–§7 (RSA keys disabled for now, opt-in `AcceptNew` / `Disabled` host-key policies) and a clarification of §4 (TLS of imported DBeaver connections). Clarified by [ADR-0019](0019-tls-ca-file-semantics.md) for Decision §1–§2: a per-connection CA file is the only trust anchor (system roots not added), `verify-ca` requires a CA file.
- **Date**: 2026-11-14

## Context
Database clients often default to "prefer TLS without verification" and auto-accept SSH host keys, which hides man-in-the-middle attacks. NineSQL replaces DBeaver's SSH (JSch / SSHJ) and TLS (JSSE) stacks with Rust ones.

## Decision
### TLS
1. **rustls only** (I5, ADR-0002), with the platform trust store (`rustls-native-certs` / `rustls-platform-verifier`) plus an optional per-connection CA file.
2. New connections default to **`verify-full`** (certificate chain and host name) where the engine supports TLS. The modes offered map to the engine's terms: `disable`, `require` (encrypted, no verification), `verify-ca`, `verify-full`. Any mode below `verify-full` shows a visible warning on the connection and in its tab.
3. Client certificates (mTLS) are supported; the key's passphrase goes to the secret store (ADR-0005).
4. Imported DBeaver connections keep their effective TLS mode, and the import report flags those that do not verify certificates.

### SSH
5. Tunnels use **`russh`** (pure Rust): local port forwarding bound to `127.0.0.1` on an ephemeral port, one tunnel per session, jump hosts (ProxyJump chains), auth by password, private key (OpenSSH format, with passphrase), or `ssh-agent` (`SSH_AUTH_SOCK`).
6. **Host keys are checked** against NineSQL's `known_hosts` file and the user's `~/.ssh/known_hosts` (read-only). An unknown host shows its fingerprint and asks the user; a **changed** key blocks the connection with a clear warning and no "connect anyway" button in the same dialog (the user must remove the old key explicitly).
7. Algorithms: russh defaults minus SHA-1-based and CBC algorithms; RSA keys with SHA-2 signatures only.
8. HTTP CONNECT and SOCKS5 proxies are supported for TLS / TCP connections, configured per connection.

## Consequences
- Some legacy servers (old SSH daemons, self-signed certificates) need an explicit, visible downgrade; that friction is intended.
- `russh` and rustls are security-critical dependencies: advisories are checked daily (`cargo deny check advisories`).

## Rejected alternatives
- **`ssh2` (libssh2 bindings)**: C dependency with OpenSSL, against ADR-0002.
- **Calling the system `ssh` binary**: depends on the user's setup, hard to control and to sandbox in Flatpak; may be added later as an opt-in "use system SSH" mode.
