# Security

NineSQL holds credentials to production databases and displays their data. Its security model protects those credentials and data on the user's machine, on the network, and against the user's own mistakes.

## Assets
- Database passwords, tokens, SSH key passphrases, client certificate keys.
- Data displayed, copied, exported or kept in history.
- The integrity of production databases (accidental writes).
- The user's DBeaver workspace (must not be damaged by import).

## Threats and controls
| Threat | Control | Invariant / ADR |
|---|---|---|
| Another local process or a backup reads saved passwords | Secret Service or Argon2id + XChaCha20-Poly1305 vault ([notes below](#vault-format-v1)); workspace holds references only; `0600` / `0700`; no plaintext store at all | I1, ADR-0005, ADR-0008 |
| Secrets leak through logs, history, crash output or exports | No values or passwords in logs; history without parameters; production query text never logged | I1, ADR-0007 |
| Network attacker intercepts or impersonates a server | `verify-full` TLS by default, rustls; SSH host keys checked, changed keys blocked; non-strict host-key policies only as explicit, flagged per-hop choices | I5, ADR-0011, ADR-0016 |
| The client itself leaks data (telemetry, update checks) | No network traffic except declared connections | I2 |
| Accidental destructive statement in production | Connection types, engine-side read-only, parser-based confirmation, manual commit default | I3, ADR-0007 |
| Injection through generated SQL (edits, filters, DDL) | Dialect-aware quoting and typed literals; tests with hostile identifiers and values | ADR-0004 |
| Malicious or corrupted DBeaver files / server responses crash or exploit the parser | Total parsing, fuzzing, no panics on untrusted input | ADR-0006 |
| DBeaver workspace damaged by import | Read-only access, checksum test | I4, ADR-0006 |
| Supply-chain compromise | `cargo deny` (licenses, bans, sources, advisories), exact pins for GPUI ([ADR-0013](adr/0013-gpui-source-and-supply-chain.md)), `--locked`, patched third-party crates checked against their crates.io archive by `scripts/verify-patches.sh` in CI ([audit log](audits/gpui-snapshot.md)), signed reproducible releases, SBOM | I7, ADR-0002, ADR-0010 |
| Out-of-memory on huge results | Fetch size and byte budget per result, streamed export | I6, ADR-0004 |

## What NineSQL does not protect against
- A compromised user session (malware running as the user can read the unlocked keyring or memory).
- Database-side permissions: NineSQL can do what the database account can do; least-privilege accounts are the user's responsibility. Read-only mode reduces mistakes; it is not a privilege boundary when the account can turn it off.
- Data the user explicitly exports or copies to the clipboard.

## Vault format v1
The vault (`vault.bin`, ADR-0005 §2) is specified in the rustdoc of `ninesql_secrets::vault`. Bounds and properties:
- Key: Argon2id v0x13, 16-byte random salt, parameters in the header (default m = 64 MiB, t = 3, p = 1). New vaults and password changes accept m from 19 MiB to 1 GiB, t from 2 to 16, p from 1 to 8; reading accepts any valid parameters under the same ceilings, so a planted header cannot make NineSQL allocate more than 1 GiB.
- Sealing: XChaCha20-Poly1305 over the whole entry list, a new random 24-byte nonce on every write, the 112-byte header as associated data. A separate key-check tag tells a wrong master password from a damaged file. The payload is zero-padded to 256-byte steps, so the file size reveals little about the number and length of secrets.
- Limits: 10 000 entries, 64 KiB per secret, 16 MiB per file. Reading never writes: a wrong password, a damaged, truncated or newer file is reported and left untouched.
- Memory: the derived key and decrypted entries are zeroized on lock, idle timeout and drop. Two copies cannot be wiped from NineSQL's code: a short-lived cipher instance that Rust may move on the stack, and a 64-byte stack copy left by `argon2` 0.6 (`blake2b_long`); the Argon2 working memory is allocated and wiped by NineSQL because the crate does not wipe its own.
- Idle lock on `CLOCK_BOOTTIME` (suspended time counts). Locking on suspend and screen lock is a core follow-up ([ROADMAP](ROADMAP.md#phase-1-follow-ups)).

Known limits:
- **Offline guessing** of a copied `vault.bin` is slowed only by Argon2id: the master password must be strong. The back-off between failed unlocks (3 free attempts, then 1 s doubling up to 60 s) is **per process**: it slows guessing through a running NineSQL at an unattended desktop, not a restarted process or a copied file.
- **Rollback is not detected**: someone able to write `vault.bin` can put back an older copy (older secrets under an older master password). Detecting it needs state outside the file; it is an optional follow-up.
- For information: XChaCha20-Poly1305 is not key-committing, so a file crafted by someone who can write it could decrypt successfully under more than one key (the partitioning-oracle class of attacks, which also needs to observe many unlock outcomes). The vault does not claim to resist an attacker who can replace `vault.bin` (see rollback); this is recorded, not mitigated.

## Accepted risks
- **`secret-service` 6.0.0-rc.1 key handling** (P0-D review item L5). The crate does not zeroize the Diffie-Hellman key pair, the shared secret or the AES session key of the Secret Service transport session, and its modular exponentiation is not constant time. Leftovers only decrypt the D-Bus traffic of one short-lived session; reading them needs access to NineSQL's memory, a core dump or swap, which already exposes the secrets themselves (see above). NineSQL wraps returned secrets in zeroizing buffers at once and always asks for an encrypted session. Accepted as a residual risk; the Secret Service feature stays **off by default** until the desktop round trip of ADR-0012 passes (including the KWallet wizard that the probe may trigger through D-Bus activation) and the crate moves to a stable 6.x. Items 1–3 are to be reported upstream. Review details: rustdoc of `crates/ninesql-secrets/src/secret_service.rs`.
- **md5 password authentication on PostgreSQL** over a transport that is not protected (no `verify-ca` / `verify-full` TLS and no fully verified loopback SSH tunnel) is allowed with a logged warning, because servers still configured for md5 exist: an observer can capture the digest and crack it offline. A cleartext password is refused over such a transport. A per-connection "SCRAM only" setting is a follow-up ([docs/08](08-engine-capabilities.md#pgwire-postgresql-p1-b)).
- **Local SSH tunnel port.** While an SSH tunnel is open, its local end listens on `127.0.0.1` on an ephemeral port (ADR-0011 §5). Other local users on the same host can connect to that port and reach the database through the tunnel. Database authentication still applies; on a multi-user host, the tunnel gives other users the network path, not the credentials.

## Review process
Changes to secrets, SSH / TLS, the driver API, read-only enforcement, DBeaver import / export and packaging require a `security-reviewer` review ([AGENTS.md](../AGENTS.md)). Each ROADMAP phase ends with a security review. Vulnerability reports: [SECURITY.md](../SECURITY.md).
