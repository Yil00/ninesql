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
- SSH tunnels are tasks on the same runtime; a session holds its tunnel alive.

## Connection lifecycle
1. The user opens a connection → `ninesql-core` resolves the secret through `ninesql-secrets` (keyring, vault or prompt).
2. If SSH is configured, `ninesql-ssh` opens the tunnel (host key check) and returns a local `127.0.0.1:<port>`.
3. The driver connects with TLS settings (ADR-0011), applies init SQL, read-only mode (ADR-0007), and returns a `Session` with its `Capabilities` for that server version.
4. The UI enables actions from the capabilities (I8).

## Storage
See [ADR-0008](adr/0008-workspace-and-config-storage.md): settings in `$XDG_CONFIG_HOME/ninesql`, projects in `$XDG_DATA_HOME/ninesql/workspace`, history and logs in `$XDG_STATE_HOME/ninesql`. No secrets in any of them. Amended by [ADR-0017](adr/0017-workspace-lock-rewrite-on-load-and-project-identity.md):
- **Single writer**: an open workspace holds an exclusive lock on `$XDG_DATA_HOME/ninesql/ninesql.lock`, released by the kernel when the process ends. A second NineSQL process is refused; a file system without locks is refused with an explanation.
- **Rewrite on load**: a secret-looking value found in a workspace file is removed and the file is rewritten at once, atomically; migration backups (`.bak`) are built from the scrubbed content.
- **Project id**: `project.json` (schema version 2) carries a stable UUID, which scopes the project's Secret Service items, so renaming a project keeps its secrets.

## Error handling and logging
- Typed errors per crate (`thiserror`), mapped by `ninesql-core` to user-facing messages with an optional technical detail panel.
- `tracing` to a rotated file in `$XDG_STATE_HOME/ninesql/`; no values, no passwords, no query text on production connections.
- A panic in a driver task fails that query or session, never the application.
