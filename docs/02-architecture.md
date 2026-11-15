# Architecture

## Crates
```
crates/
├── ninesql-ui/          # binary `ninesql`: GPUI app, windows, docks, dialogs, actions
├── ninesql-grid/        # virtualized Arrow-backed results grid (GPUI element)
├── ninesql-editor/      # SQL editor integration (gpui-component editor + tree-sitter + completion)
├── ninesql-core/        # app state, connection model, services, driver runtime bridge
├── ninesql-storage/     # XDG workspace files, schema versions, migrations, history.sqlite
├── ninesql-secrets/     # Secret Service backend, encrypted vault, secret handles
├── ninesql-ssh/         # russh tunnels, jump hosts, known_hosts
├── ninesql-transfer/    # streamed export / import (CSV, JSON, SQL, Parquet…)
├── ninesql-dbeaver/     # DBeaver workspace import / export
├── ninesql-driver-api/  # driver traits, capabilities, dialect, result types
└── drivers/             # one crate per family (ADR-0003), added from P1-B
    ├── ninesql-driver-pgwire/
    ├── ninesql-driver-mysqlwire/
    ├── ninesql-driver-sqlite/
    └── …
```

Dependency direction (no cycles, enforced by the crate graph):
```
ninesql-ui ──► ninesql-grid, ninesql-editor ──► ninesql-core ──► ninesql-driver-api ◄── drivers/*
                                                  │
                                                  ├──► ninesql-storage
                                                  ├──► ninesql-secrets
                                                  ├──► ninesql-ssh
                                                  ├──► ninesql-transfer
                                                  └──► ninesql-dbeaver
```
- UI crates never depend on a driver crate directly: `ninesql-core` registers drivers and exposes services.
- Drivers depend only on `ninesql-driver-api` (and their client crates). They never read workspace files or secrets; they receive resolved parameters.
- `ninesql-dbeaver` does not depend on `ninesql-core` (the core depends on it). It outputs a **mirror** of the core connection model, with the decrypted secrets in zeroized buffers; `ninesql-core` converts that model, runs its own validation, persists the connections and moves the secrets to `ninesql-secrets` (P2-C).

## Threads and data flow
```
GPUI foreground (UI)                Tokio runtime (driver I/O)                 Database
────────────────────                ─────────────────────────                 ────────
user runs a query ──► core service ──► spawn: session.execute() ──────────────► server
                                        │   batches (Arrow / documents)
grid entity  ◄── channel ◄─────────────┘   stop at fetch_size / max_bytes
 renders visible rows only                  cancel token ──► server-side cancel
```
- The foreground never awaits I/O (I6). Services return handles; results arrive through channels polled by GPUI.
- Each open result keeps its batches in an `Arc` shared with the grid; memory is accounted per result tab.
- SSH tunnels are tasks on the same runtime; a session holds its tunnel alive and closing the session closes it.

## Startup (`CoreServices`)
The application starts the core with `CoreServices::start(CoreConfig)` (`services` module of `ninesql-core`), which runs on a background thread and answers through a `Reply` (I6). It:
1. opens the project's `Workspace` (directories, workspace lock, `project.json`);
2. selects the secret backend, **keyring → vault → ask every time**, never plaintext: a `SecretServiceStore` when a Secret Service answers (feature `secret-service`, off by default), the vault always registered (opened locked) when it can be opened, and the "ask every time" `SessionSecretCache`. `secret_backend()` says where new secrets go (`Keyring`, `Vault` or `AskEveryTime`);
3. registers the vault and the cache in one `LockHooks`, together with a hook that emits `CoreEvent::SecretsLocked`;
4. starts the `SessionManager` with the `DriverRegistry` the application filled (the application registers the drivers it ships; the UI never depends on a driver crate);
5. starts `DesktopLockWatcher` (feature `desktop-lock`, **on by default**); failing to start it (no system bus) is not an error, `lock_watcher_status()` reports it;
6. runs the idle timer on the driver runtime: `Vault::lock_if_idle` and `SessionSecretCache::purge_expired` every 30 s (vault idle timeout 15 min by default, both in `CoreConfig`).

The UI keeps the `CoreServices` (cheap to clone) and uses `sessions()`, `take_events()`, `validate_connection()`, `unlock_vault()` / `create_vault()` / `vault_status()` / `lock_now()` (all on the blocking pool, answered through a `Reply`) and `lock_watcher_status()`. `shutdown(timeout)` at exit locks the secrets, stops the watcher and the timer and closes every session.

**Vault unlock flow.** When a saved secret of the connection being opened is in the locked vault, `SessionManager::open` answers `OpenOutcome::NeedsVaultUnlock { connection, slot }` before anything is dialled. The UI asks for the master password into a `SecretString`, calls `CoreServices::unlock_vault` (the password is moved to the blocking pool and zeroized right after the key derivation) and opens again, giving prompted secrets again.

## Driver runtime and session manager (P1-D)
The API lives in `ninesql-core` (`runtime`, `registry`, `session`, `connect` modules); the rustdoc is the reference.
- **`DriverRuntime`**: one multi-thread Tokio runtime owned by the core (4 workers by default, at most 16), for sessions, SSH tunnels and, on its blocking pool, secret-store calls and `known_hosts` I/O. The UI never runs on it.
- **`DriverRegistry`**: engine id → driver. It answers capabilities and dialects without a session, and `validate_connection` is **the** validation entry point of the save (P1-C) and import (P2-C) flows: the model rules, then every driver rule (reserved and unknown properties, authentication kind, password-exchange policy, TLS mode against `Capabilities::tls` — a driver without TLS accepts only `disable` on network endpoints —, the `verify-ca` CA file, client certificates). `Workspace::save_connections_checked` and `create_connection_checked` call it for every connection (disabled connections of engines without a driver get the model rules only); the connect flow calls it again.
- **`SessionManager::open`** runs the connect flow on the runtime and answers through a `Reply` with an `OpenOutcome`: `Opened(SessionHandle)`, `NeedsSecrets` (fields set to "ask every time"; nothing reached the network), `NeedsVaultUnlock` (a saved secret is in the locked vault; nothing reached the network), `HostKeyNeedsConfirmation` (unknown key under the strict policy; no credential sent to that hop) or `InitSqlNeedsConfirmation`. The UI answers and calls `open` again.
- **Sessions**: each session is owned by one task, driven through a bounded command channel. `SessionHandle::execute` returns an `Execution` whose `ExecEvent`s carry the result items one by one through a small bounded channel, so a slow consumer slows the fetch instead of filling memory (I6). The terminal event goes through a slot reserved when the statement starts, and every send gives up when the statement is cancelled or the `Execution` dropped, so a consumer that stopped reading never blocks the session.
- **Cancel**: `Execution::cancel` triggers the driver's execution-scoped server-side cancel. The session stays busy until the server confirms the end; a statement sent meanwhile reports `WaitingForPreviousCancel` and waits up to `ManagerConfig::busy_wait`, then fails with `StillCancelling`.
- **Events**: session-level events (opened, closed, tunnel lost) and `SecretsLocked` (sleep, screen lock, idle, requested) go to one bounded `CoreEvents` channel, taken once with `take_events`. When it is full, events are dropped and counted (`dropped_events`), so the runtime never waits on the UI; `SessionClosed` is never dropped. The UI takes the receiver at startup and resynchronizes when the counter grows (P1-C).
- `Reply` and `Execution` are executor-agnostic: GPUI awaits them on its own executor or polls them with `try_take` / `try_next`.

## Connection lifecycle
1. The user opens a connection → `SessionManager::open`. The core validates it against its driver and, for init SQL that needs confirmation under the connection type, answers `InitSqlNeedsConfirmation` before anything else happens.
2. Saved secrets are read from `ninesql-secrets` on the blocking pool; fields set to "ask every time" come from the UI (`NeedsSecrets`). Secrets travel as zeroizing values and are moved into the SSH hops and the driver parameters; nothing is kept once the driver connected.
3. If SSH is configured, `ninesql-ssh` opens the tunnel hop by hop under each hop's host-key policy (`connections.json` schema 2) and returns a local `127.0.0.1:<port>`. TLS `server_name` is set to the real database host, so `verify-full` still checks the right name behind the tunnel.
4. The driver connects with TLS settings (ADR-0011), the connection's password-exchange policy (`password_exchange`, `connections.json` schema 3) and read-only taken from the connection type's policy (ADR-0007), applies init SQL, and returns a `Session` with its `Capabilities` for that server version. If the policy asks for read-only and the session does not report it, the core closes the session (`ReadOnlyNotEnforced`, I3).
5. The UI enables actions from the capabilities (I8).

For a file endpoint, the core sets `ConnectParams::inside_dbeaver_workspace` (I4) from `ManagerConfig::dbeaver_roots` (`DbeaverRoots`), on the blocking pool: after resolving symbolic links, a file is inside when it lies under the `DBeaverData` directory of a known install location (native, Flatpak, Snap) or under a folder added with `DbeaverRoots::add` (a workspace picked for an import, at runtime through `SessionManager::dbeaver_roots()`), when a component of its path is `DBeaverData`, or when an ancestor holds `.metadata/dbeaver-workspace.properties` or a `.dbeaver` directory. Every check is read-only; a failed check counts as inside. File drivers then open the file read-only.

## Confirmations (ADR-0007, ADR-0011)
Everything the user must confirm goes through a single-use token issued by the core, bound to what was shown, expiring after ten minutes:
| Token | Issued with | Bound to |
|---|---|---|
| `ConfirmationToken` | `ExecEvent::NeedsConfirmation` (destructive statement, or any write when the type confirms writes; always on production) | The session, the exact statement text, its language and its parameters |
| `InitSqlToken` | `OpenOutcome::InitSqlNeedsConfirmation` | The connection and its exact init statements |
| `HostKeyToken` | `OpenOutcome::HostKeyNeedsConfirmation` | The key the core kept: the UI cannot get a different key trusted |

Init SQL is classified with the engine's dialect before anything is dialled. Entries the classifier cannot vouch for are flagged too: an entry holding several statements, an entry that changes how the following statements are lexed (for example `sql_mode`, `standard_conforming_strings`), and every entry after the first session-level, access-mode or unrecognised one. Init SQL runs after the read-only mechanism is applied. Saving such init SQL stays allowed (DBeaver imports carry bootstrap queries); it is confirmed when it would run.

## Secrets (ADR-0005, P1-D)
`ninesql-secrets` offers three ways to hold a secret, never plaintext on disk (I1):
- **Secret Service** (`SecretServiceStore`, feature `secret-service`, **off by default** until the desktop round trip of ADR-0012 passes): items scoped by the project UUID. The startup probe (`detect_secret_service`) is bounded to 5 s and every D-Bus method call to 25 s; an unlock prompt is not a method call and can wait indefinitely, so the core bounds secret resolution itself: all store calls of one open get `ManagerConfig::secret_timeout` together (default 60 s), after which the field fails with `SecretError::Unavailable`, as does a panicking store task. An open whose `Reply` is dropped stops at once; a store call already running on a blocking thread still ends on the store's own limits, and its value is dropped unused.
- **Vault** (`Vault`, `VaultStore`): `$XDG_DATA_HOME/ninesql/vault.bin`, `0600`, written atomically under the workspace lock. Argon2id (default m = 64 MiB, t = 3, p = 1, stored in the header) and XChaCha20-Poly1305 over the whole entry list with the header as associated data; a key-check tag tells a wrong master password from a damaged file; the payload is padded to 256-byte steps; limits of 10 000 entries, 64 KiB per secret and 16 MiB per file. It starts locked, locks again after an idle time measured on `CLOCK_BOOTTIME` (suspended time counts), slows repeated failed unlocks within the process (`UnlockBackoff`), and can change its master password. Reading never writes. Format v1 is specified in the rustdoc of `ninesql_secrets::vault`.
- **Ask every time** (`SessionSecretCache`): the answer to a prompt stays in memory only, for the session or for a set duration, and is zeroized when removed or expired.

Sleep and screen lock: the vault and the "ask every time" cache are `LockHook`s. Registered in one `LockHooks` (by `CoreServices` at startup), they are locked (vault) and cleared (cache) together through `DesktopLockWatcher` (feature `desktop-lock`, on by default and implied by `secret-service`: zbus 6 only, its own system-bus connection):
- on logind's `PrepareForSleep(true)`. The watcher holds a logind sleep **delay inhibitor**, so the suspend waits (up to logind's `InhibitDelayMaxSec`) until the hook ran; it is released right after and taken again on resume, and dropped when logind leaves the bus and taken again from the new logind when it (re)appears;
- when the current logind session locks: its `Lock` signal, or its `LockedHint` property turning true (what GNOME and KDE lockers set). The session is found by PID, then `$XDG_SESSION_ID`, then the user's display session (`GetUser` → `Display`, for applications in an `app-*.scope`).

The watcher only calls logind (session lookup, `Inhibit`) and the bus (`GetNameOwner`). Match rules alone do not filter senders (a signal addressed to the watcher by name is delivered whatever the rules say), so each signal is checked on receipt: no destination header, and the sender must be the current unique owner of `org.freedesktop.login1`, followed through `NameOwnerChanged`, whose stream is polled before the others so a restarted logind's first signal is not dropped. A forged signal therefore cannot release the sleep delay or force a lock. A panicking hook is logged and the watcher goes on; if the bus connection is lost, it locks once and stops (`is_running()`; it is not restarted). `CoreServices` starts it at startup; without the feature, or without a system bus, the idle timers still apply and `CoreServices::lock_now` locks on demand.

Backend selection at startup: the keyring if a Secret Service answers, otherwise the vault (created on first use with a master password); with neither, only "ask every time". Saved references keep resolving in the vault whatever the keyring does, and there is no silent fallback at runtime: if the keyring fails, the user decides whether to move the secret to the vault. Store calls are blocking and run on the runtime's blocking pool, never on the GPUI thread.

## Storage
See [ADR-0008](adr/0008-workspace-and-config-storage.md): settings in `$XDG_CONFIG_HOME/ninesql`, projects in `$XDG_DATA_HOME/ninesql/workspace`, history and logs in `$XDG_STATE_HOME/ninesql`. No secrets in any of them. Amended by [ADR-0017](adr/0017-workspace-lock-rewrite-on-load-and-project-identity.md):
- **Single writer**: an open workspace holds an exclusive lock on `$XDG_DATA_HOME/ninesql/ninesql.lock`, released by the kernel when the process ends. A second NineSQL process is refused; a file system without locks is refused with an explanation. Writes to NineSQL's `known_hosts` take the same lock behind a process-wide writer mutex and re-read both `known_hosts` files under it, so concurrent writers never drop each other's entries and a key revoked meanwhile is honoured. A lock held by another process is waited for at most 2 s: that only helps against brief holders (a writer without an open workspace); another NineSQL with an open workspace holds the lock for its lifetime, so the write fails.
- **Rewrite on load**: a secret-looking value found in a workspace file is removed and the file is rewritten at once, atomically; migration backups (`.bak`) are built from the scrubbed content.
- **Settings**: `settings.toml` is outside the workspace lock. Its writes are serialized within one process only; with two NineSQL processes the last write wins (each write atomic, no secrets in the file).
- **Project id**: `project.json` (schema version 2) carries a stable UUID, which scopes the project's Secret Service items, so renaming a project keeps its secrets.

## Error handling and logging
- Typed errors per crate (`thiserror`), mapped by `ninesql-core` to user-facing messages with an optional technical detail panel.
- `tracing` to a rotated file in `$XDG_STATE_HOME/ninesql/`; no values, no passwords, no query text on production connections.
- A panic in a driver task fails that query or session, never the application.
