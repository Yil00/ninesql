# CONTEXT.md – NineSQL

Project context for every contributor, human or AI agent. Read it **before** any task.

## In one sentence
NineSQL is an open-source (Apache 2.0) native database client for Debian and Ubuntu, written in Rust with a GPU-rendered GPUI interface, that aims at DBeaver feature parity on the 20 most used open-source engines and imports DBeaver workspaces.

## Where the project stands
- **Current phase**: 1 – shell, connections, first drivers, **in progress** (see [docs/ROADMAP.md](docs/ROADMAP.md)). Phase 0 is done except the items listed below.
- Phase 0 (done): repository and settings (P0-F); GPUI spike with a GO verdict, GPUI from crates.io through `gpui-kit` 0.9.1 under supply-chain controls (P0-A, [docs/spikes/p0-a-gpui.md](docs/spikes/p0-a-gpui.md)); driver API with mock driver, security-reviewed and frozen (P0-B); dev environment with the 20 engines and the "nine" dataset (P0-C, [dev/README.md](dev/README.md)); workspace storage, connection model and secrets skeleton (P0-D); feature parity audit and synthetic DBeaver fixtures from five versions (P0-E).
- Phase 1, wave 1 (landed): the `ninesql` shell – navigator dock, tabs and splits, status bar, multiple windows, 19 actions with a command palette, key-binding overrides and light / dark / system themes from `settings.toml` (P1-A; settings read at startup, not saved yet); `ninesql-ssh` with russh tunnels, jump hosts, agent, `known_hosts` and per-hop host-key policies, RSA key files refused (P1-D, ADR-0016); driver API hardening that closes the P0-B review items; the P0-D review follow-ups (workspace lock, rewrite-on-load, scrubbed backups, project UUID scoping keyring items, wider secret detection); and the read-only DBeaver parser with fail-closed mapping (P2-A, [docs/06](docs/06-dbeaver-interop.md)). ADR-0001 to ADR-0017 are accepted.
- Still open: real-GPU measurements and screenshots on Wayland and X11 (maintainer); the X11 IME patch; the Secret Service round trip on GNOME and KDE desktops (Debian 12, Ubuntu 24.04) and the review of the `secret-service` release candidate; the rest of [Phase 0 follow-ups](docs/ROADMAP.md#phase-0-follow-ups) and [Phase 1 follow-ups](docs/ROADMAP.md#phase-1-follow-ups).
- Next: wave 2 – P1-B drivers (pg-wire, mysql-wire, SQLite) and the rest of P1-D (driver runtime bridge, vault, SSH wiring in the core); wave 3 – P1-C connections UI; then P2-B / P2-C, the DBeaver import flow and persistence.

## Invariants (non-negotiable)
| # | Invariant | Source |
|---|-----------|--------|
| I1 | No secret in clear on disk: passwords, passphrases and tokens live only in the OS keyring (Secret Service) or the encrypted vault; never in config files, logs, history, crash reports or exports | ADR-0005 |
| I2 | No network traffic except to the user's declared connections (and their SSH / proxy hops). No telemetry, no analytics, no phone-home; an update check, if ever added, is opt-in | ADR-0001, 05-security |
| I3 | A read-only connection is enforced at the session level by the driver (engine-side read-only transaction or equivalent), never only by the UI. Destructive statements on a connection typed "production" require explicit confirmation | ADR-0007 |
| I4 | NineSQL never modifies, moves or deletes DBeaver files. Import is read-only; export writes only to a new file the user chooses | ADR-0006 |
| I5 | TLS certificates and SSH host keys are verified by default (rustls, `known_hosts`). Disabling a check is an explicit, per-connection, visibly flagged choice | ADR-0011 |
| I6 | The UI thread never waits on I/O. Every query result is streamed with a memory budget; nothing loads an unbounded result into memory | ADR-0004 |
| I7 | Apache 2.0 only: no GPL / AGPL / SSPL / proprietary code linked into the binary (Zed's GPL crates included), no JVM, no webview | ADR-0001, ADR-0002 |
| I8 | Capabilities are declared honestly per engine: the UI hides or disables what a driver does not support; it never fakes a feature | ADR-0004, docs/08 |

## Glossary
| Term | Definition |
|-------|-----------|
| **Connection** | A saved set of parameters (engine, host, credentials reference, SSH, TLS, type) to reach one database server or file |
| **Connection type** | Dev / Test / Production (DBeaver-compatible), carrying color, auto-commit default, confirmation and read-only rules |
| **Driver** | Engine-specific crate implementing the `ninesql-driver-api` traits (`ninesql-driver-postgres`…) |
| **Family** | Group of engines sharing a wire protocol and therefore one driver (pg-wire, mysql-wire…) |
| **Capability** | A feature a driver declares (metadata kinds, editing, explain, transactions…), read by the UI |
| **Session** | A live connection to a database, owned by the driver runtime, with its transaction state |
| **Result stream** | Batches of rows (Arrow `RecordBatch`) or documents produced by a query, consumed by the grid under a memory budget |
| **Workspace** | NineSQL's own storage: projects, connections, scripts, history (docs/02, ADR-0008) |
| **Vault** | Encrypted local secret store used when no Secret Service is available (ADR-0005) |
| **Import** | One-way conversion of a DBeaver workspace into NineSQL connections (ADR-0006) |
| **Feature id** | `F-<area>-<nn>` identifier of a DBeaver feature in [docs/07-feature-parity.md](docs/07-feature-parity.md) |

## Documentation map
- Why / what: [docs/01-overview.md](docs/01-overview.md), [docs/04-mvp-scope.md](docs/04-mvp-scope.md)
- How: [docs/02-architecture.md](docs/02-architecture.md), [docs/03-tech-stack.md](docs/03-tech-stack.md)
- Security: [docs/05-security.md](docs/05-security.md), [SECURITY.md](SECURITY.md)
- DBeaver import / export: [docs/06-dbeaver-interop.md](docs/06-dbeaver-interop.md)
- Feature parity inventory: [docs/07-feature-parity.md](docs/07-feature-parity.md)
- Per-engine capabilities: [docs/08-engine-capabilities.md](docs/08-engine-capabilities.md)
- Working with AI agents (local and cloud): [docs/09-agent-workflow.md](docs/09-agent-workflow.md)
- Supported platforms: [docs/10-supported-platforms.md](docs/10-supported-platforms.md)
- Settled decisions: [docs/adr/](docs/adr/README.md)
- Plan: [docs/ROADMAP.md](docs/ROADMAP.md); releases: [RELEASE.md](RELEASE.md)
