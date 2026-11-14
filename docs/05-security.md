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
| Another local process or a backup reads saved passwords | Secret Service or Argon2id + XChaCha20-Poly1305 vault; workspace holds references only; `0600` / `0700` | I1, ADR-0005, ADR-0008 |
| Secrets leak through logs, history, crash output or exports | No values or passwords in logs; history without parameters; production query text never logged | I1, ADR-0007 |
| Network attacker intercepts or impersonates a server | `verify-full` TLS by default, rustls; SSH host keys checked, changed keys blocked; non-strict host-key policies only as explicit, flagged per-hop choices | I5, ADR-0011, ADR-0016 |
| The client itself leaks data (telemetry, update checks) | No network traffic except declared connections | I2 |
| Accidental destructive statement in production | Connection types, engine-side read-only, parser-based confirmation, manual commit default | I3, ADR-0007 |
| Injection through generated SQL (edits, filters, DDL) | Dialect-aware quoting and typed literals; tests with hostile identifiers and values | ADR-0004 |
| Malicious or corrupted DBeaver files / server responses crash or exploit the parser | Total parsing, fuzzing, no panics on untrusted input | ADR-0006 |
| DBeaver workspace damaged by import | Read-only access, checksum test | I4, ADR-0006 |
| Supply-chain compromise | `cargo deny` (licenses, bans, sources, advisories), pinned git revisions, `--locked`, signed reproducible releases, SBOM | I7, ADR-0002, ADR-0010 |
| Out-of-memory on huge results | Fetch size and byte budget per result, streamed export | I6, ADR-0004 |

## What NineSQL does not protect against
- A compromised user session (malware running as the user can read the unlocked keyring or memory).
- Database-side permissions: NineSQL can do what the database account can do; least-privilege accounts are the user's responsibility. Read-only mode reduces mistakes; it is not a privilege boundary when the account can turn it off.
- Data the user explicitly exports or copies to the clipboard.

## Accepted risks
- **Local SSH tunnel port.** While an SSH tunnel is open, its local end listens on `127.0.0.1` on an ephemeral port (ADR-0011 §5). Other local users on the same host can connect to that port and reach the database through the tunnel. Database authentication still applies; on a multi-user host, the tunnel gives other users the network path, not the credentials.

## Review process
Changes to secrets, SSH / TLS, the driver API, read-only enforcement, DBeaver import / export and packaging require a `security-reviewer` review ([AGENTS.md](../AGENTS.md)). Each ROADMAP phase ends with a security review. Vulnerability reports: [SECURITY.md](../SECURITY.md).
