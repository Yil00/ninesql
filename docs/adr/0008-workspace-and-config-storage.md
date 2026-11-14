# ADR-0008: Workspace and configuration storage

- **Status**: Accepted. Partially amended by [ADR-0017](0017-workspace-lock-rewrite-on-load-and-project-identity.md) (proposed): workspace lock, rewrite on load, scrubbed migration backups, project id in `project.json`.
- **Date**: 2026-11-14

## Context
NineSQL stores settings, connections, scripts and query history. Users want to version and share connection definitions (without secrets), back them up, and recover from crashes.

## Decision
1. **Locations** (XDG Base Directory):
   - `$XDG_CONFIG_HOME/ninesql/settings.toml`: user preferences (theme, key bindings overrides, defaults).
   - `$XDG_DATA_HOME/ninesql/workspace/<project>/`: `project.json`, `connections.json`, `connection-types.json`, `scripts/` (plain `.sql` files), `diagrams/`.
   - `$XDG_STATE_HOME/ninesql/`: `history.sqlite` (query history), window layout, logs (`ninesql.log`, rotated).
   - `$XDG_DATA_HOME/ninesql/vault.bin`: the encrypted vault (ADR-0005), if used.
2. **Formats**: JSON for machine-owned files, TOML for hand-edited settings. Every file has a top-level `"schema_version"`; `ninesql-storage` migrates older versions forward on load and keeps a `.bak` of the previous file.
3. **Writes are atomic**: write to a temp file in the same directory, `fsync`, rename. Directories are `0700`, files `0600`.
4. **No secret in any of these files** (I1): connections hold a secret reference only. History stores query text but never bind parameter values, and never stores anything for connections whose type disables query logging (ADR-0007).
5. **Portability**: "Export connections" writes a `connections.json` without secrets that another NineSQL can import; projects can live in a git repository.
6. History retention: 90 days or 10 000 entries per connection by default, configurable, purgeable.

## Consequences
- Connection files can be committed and reviewed; they are safe to share by design.
- Migrations are code we must test: every `schema_version` bump ships with a fixture of the previous version.

## Rejected alternatives
- **A single SQLite file for everything**: opaque to users, impossible to diff or version.
- **Reusing DBeaver's on-disk format natively**: ties NineSQL to a format we do not control (ADR-0006 keeps import / export instead).
