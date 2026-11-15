# ADR-0021: Phase 1 core, secrets and storage decisions

- **Status**: Accepted (maintainer, 2026-11-15)
- **Date**: 2026-11-15

## Context
Phase 1 (P1-D, P1-C and the L5 wiring of wave 3) landed the session manager, the secret backends, the startup assembly (`CoreServices`) and the connections UI. The security reviews of those waves, and the P1-E end review, listed several decisions that were made in code and documented in [docs/02](../02-architecture.md), [docs/05](../05-security.md) and the crates' rustdoc, but recorded in no ADR ([ROADMAP](../ROADMAP.md#phase-1-follow-ups), row "ADR questions"). This ADR records them as they are implemented; it changes no behaviour. The driver-side decisions are in [ADR-0020](0020-phase-1-driver-decisions.md).

Invariants involved: I1 (no secret in clear), I4 (DBeaver files never modified), I6 (the UI never waits on I/O). ADRs involved: [ADR-0005](0005-secrets-storage.md), [ADR-0006](0006-dbeaver-interoperability.md), [ADR-0008](0008-workspace-and-config-storage.md), [ADR-0012](0012-secret-service-client-and-msrv.md), [ADR-0016](0016-ssh-rsa-and-host-key-policies.md).

## Decision
### 1. Bounded secret reads: 60 s per open, at most 8 pending reads
A Secret Service unlock prompt is not a D-Bus method call and can wait indefinitely. The core bounds secret resolution itself (`crates/ninesql-core/src/connect.rs`, [docs/05](../05-security.md#bounded-secret-reads)):
1. All store calls of one open share one deadline, `ManagerConfig::secret_timeout`, default **60 s** (`DEFAULT_SECRET_TIMEOUT`). Past it, the field fails with `SecretError::Unavailable`, as does a panicking store task.
2. Reads are **single-flight** per secret reference: an open that needs a secret already being read joins that read.
3. At most **8** store reads (`MAX_PENDING_SECRET_READS`) may run or hang on the runtime's blocking pool at once. Further reads of other secrets fail with `SecretError::Unavailable` until some end, so a store that stops answering cannot take every blocking thread.
4. An open whose caller went away (its `Reply` dropped) stops at once. A store call already running ends on the store's own limits, and its value is dropped (zeroized) unused.

### 2. Vault idle lock: 15 min, checked every 30 s
1. The vault locks itself after **15 min** without use by default (`DEFAULT_VAULT_IDLE_TIMEOUT`, `CoreConfig::vault_idle_timeout`; `None` means never). Idle time is measured on `CLOCK_BOOTTIME`, so suspended time counts.
2. The timeout is enforced at every vault access, and by an idle timer on the driver runtime that runs every **30 s** (`DEFAULT_LOCK_CHECK_INTERVAL`, `CoreConfig::lock_check_interval`, at least 100 ms): `Vault::lock_if_idle`, and the expiry of cached "ask every time" answers.
3. These timers apply whether or not the desktop lock watcher runs ([ADR-0005 amendment](0005-secrets-storage.md#amendment-p1-d-wiring-2026-11-15)).

### 3. `connections.json` schema 3
1. `connections.json` moves to `schema_version` **3**, which adds the optional `password_exchange` of a connection: `any` when absent, or `scram-only` (`Connection::password_exchange`, passed to `ConnectParams::password_exchange`).
2. Schema history: version 2 added the optional per-hop `host_key_policy` of SSH hops (ADR-0016, strict when absent).
3. A version 2 file is migrated on load and the original kept as `connections.json.bak` (scrubbed, [ADR-0017](0017-workspace-lock-rewrite-on-load-and-project-identity.md) §3), per ADR-0008 §2.
4. An older NineSQL **refuses** a newer file instead of dropping a field it does not know and saving the connection weaker than the user set it. Downgrading means restoring `connections.json.bak` and losing the changes made since.

### 4. Detecting files inside a DBeaver workspace
`ninesql-core` computes `ConnectParams::inside_dbeaver_workspace` for every file endpoint (`DbeaverRoots`, `crates/ninesql-core/src/dbeaver_roots.rs`, [docs/05](../05-security.md#files-inside-a-dbeaver-workspace)). After symbolic links are resolved (the file, or its nearest existing ancestor followed by the missing names), a path counts as inside when:
1. it lies under the `DBeaverData` directory of a known install location (native, Flatpak, Snap), whether that directory exists or not, or under a folder the user picked for an import;
2. or one of its components is named `DBeaverData`;
3. or one of its ancestors holds a DBeaver workspace marker (`.metadata/dbeaver-workspace.properties`) or a `.dbeaver` directory with a project file (`data-sources*.json`, `project-settings.json`, `credentials-config.json`).

Rules:
4. The ancestor walk **stops before the home directory**: `$HOME`, its ancestors and `/` are never taken for a workspace or project.
5. A **dangling** symbolic link is followed to its target (at most 40 links deep), where a creating open would write.
6. Every check is read-only (`stat`, `canonicalize`, `read_dir`, I4) and runs on the blocking pool.
7. **Fail closed**: a path that cannot be resolved (any error but "not found"), a marker whose presence cannot be checked, or a `.dbeaver` directory that cannot be listed or holds more than 4096 entries counts as inside.
8. The canonical path that was checked is the one handed to the driver, and `SessionInfo::inside_dbeaver_workspace` lets the UI say why the file opened read-only. What the driver then does is in ADR-0020 §8.

### 5. Driver crates as dependencies of the `ninesql-ui` package
1. UI crates never use a driver crate directly: `ninesql-core` registers drivers and exposes services ([docs/02](../02-architecture.md#crates)).
2. The one exception is the `ninesql` binary (`crates/ninesql-ui/src/main.rs`), which registers the drivers NineSQL ships (pgwire, mysqlwire, sqlite) in the `DriverRegistry` at startup. Cargo has no binary-only dependencies, so the driver crates are listed in the `ninesql-ui` manifest; the library code of `ninesql-ui` never names them.
3. Adding a shipped driver means one line in `main.rs` and one in the `ninesql-ui` manifest, owned by `ui-engineer`, an exception to the owner split of [AGENTS.md](../../AGENTS.md) for these two places only.

### 6. Retry back-off of the sleep delay inhibitor
`DesktopLockWatcher` holds a logind sleep delay inhibitor ([ADR-0005 amendment](0005-secrets-storage.md#amendment-p1-d-wiring-2026-11-15) §4). When logind refuses the `Inhibit` call or does not answer:
1. the watcher retries with a back-off that starts at **1 s** and doubles up to **60 s** (`INHIBIT_RETRY_FIRST`, `INHIBIT_RETRY_MAX` in `crates/ninesql-secrets/src/lock_signals.rs`);
2. it retries only while logind is on the bus, no inhibitor is held and no sleep is under way;
3. the back-off resets to 1 s once an inhibitor is held;
4. signals are handled while a retry is pending, so a sleep or lock that arrives meanwhile still locks the secrets (without the delay).

## Consequences
- A keyring that never answers costs one open at most 60 s and at most 8 blocking threads; the user sees `Unavailable` rather than a hang. A store call that hangs keeps its blocking thread until the store's own limits end it.
- A vault left unlocked locks within 15 min 30 s of its last use (plus the time to the next access check). The timeout is a `CoreConfig` field; the UI offers no setting for it in phase 1.
- Opening a schema 3 file with an older NineSQL fails with a clear refusal. Every later schema bump follows the same rule and adds a fixture.
- A DBeaver workspace in a custom location, without its markers and never picked for an import, is not recognised. A `.dbeaver` directory with a project file placed by another user in a shared directory such as `/tmp` makes files below it open read-only: it fails safe and shows through the badge. Between the check and the open, a process with write access to the file's directory can swap a path component (TOCTOU residual, accepted for now; ROADMAP follow-up).
- Every build of `ninesql-ui` compiles the three drivers, tests and examples included.
- A logind that keeps refusing the inhibitor is asked at most once a minute; while no inhibitor is held, a suspend may happen before the lock runs.
- Tests to keep: the secret timeout and the pending-read cap, the vault idle lock, the versioned `connections.json` fixtures (`crates/ninesql-core/tests/fixtures/connections/v1.json` to `v3.json`) and the refusal of a newer schema, the `DbeaverRoots` cases (known roots, markers, the `$HOME` stop, dangling links, fail-closed errors), the inhibitor retry in `lock_signals.rs`.

## Rejected alternatives
- **No bound on secret reads, relying on the store's timeouts**: an unlock prompt has no timeout and would pin a blocking thread and the open forever.
- **One store call per open, without single-flight**: retries after a timeout would pile up calls on a store that does not answer.
- **No idle lock, only sleep / screen-lock locking**: default builds without logind (or a Flatpak without the permission) would keep the vault unlocked for the whole session.
- **Keeping schema 2 and adding `password_exchange` as an unknown key**: an older NineSQL would drop it and save the connection without `scram-only`.
- **Detecting only the known install locations**: misses workspaces in other places that still carry DBeaver's markers. **Walking up to `/`**: a stray `~/.dbeaver` would make every home file read-only.
- **Failing open when a check errors**: a file inside a DBeaver workspace could be opened writable (I4).
- **A separate binary crate that depends on `ninesql-ui` and the drivers**: one more crate for two lines of code; may be revisited if the binary grows.
- **A fixed retry interval for the inhibitor**: too short loads logind, too long leaves suspends unprotected after a brief refusal.
