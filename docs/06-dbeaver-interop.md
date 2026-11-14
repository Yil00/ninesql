# DBeaver interoperability

Decision record: [ADR-0006](adr/0006-dbeaver-interoperability.md). Owner: `interop-engineer` (`crates/ninesql-dbeaver/`).

> **Verification status.** The format notes below come from DBeaver Community's public source and documentation, checked against the P0-E fixtures: synthetic workspaces written by five DBeaver CE builds, 22.3.5 to 26.2.4 ([fixtures README](../crates/ninesql-dbeaver/tests/fixtures/README.md)). Items marked *verified* were confirmed on those fixtures. Items still marked *(to verify)* are not covered by them and must be confirmed before the importer relies on them. Do not "fix" the importer to match this page: fix this page to match the fixtures.

## Where DBeaver keeps its data on Linux
| Install | Default workspace |
|---|---|
| `.deb` / tarball | `~/.local/share/DBeaverData/workspace6/` (verified: the fixtures keep this layout) |
| Flatpak (`io.dbeaver.DBeaverCommunity`) | `~/.var/app/io.dbeaver.DBeaverCommunity/data/DBeaverData/workspace6/` *(to verify)* |
| Snap (`dbeaver-ce`) | `~/snap/dbeaver-ce/current/.local/share/DBeaverData/workspace6/` *(to verify)* |

The Flatpak and Snap paths are not covered by the fixtures.

Inside a workspace, each project is a folder (`General/` by default) with:
| File | Content |
|---|---|
| `.dbeaver/data-sources.json` (and `data-sources-*.json`) | Connections, folders, connection types, network handlers (SSH, proxy, TLS) |
| `.dbeaver/credentials-config.json` | Saved user names and passwords, encrypted |
| `.dbeaver/project-settings.json` | Project-level settings *(to verify: DBeaver did not create it in the fixture flows)* |
| `Scripts/` | SQL scripts (`.sql`), with their connection association in project metadata *(to verify: not created in the fixture flows)* |
| `.dbeaver/.data-sources.json.bak`, `.dbeaver/.credentials-config.json.bak` | Backups DBeaver writes next to the real files. The importer ignores these dot-files (verified) |

## `data-sources.json` (main keys)
- `folders`: map of folder name → `{ parent?, description? }`. Nested folders differ by version (verified): up to 25.x, `"Production": { "parent": "Team A" }`; in 26.x, a folder created from the command line is keyed by its full path, `"Team A/Production": {}`, with no `parent` (UI-created nested folders in 26.x not verified). The parser accepts both forms.
- `connection-types`: map of type id → object (verified). Keys in every version: `name`, `color`, `description`, `auto-commit`, `confirm-execute`, `confirm-data-change`, `auto-close-transactions`. From 23.x: `smart-commit`, `smart-commit-recover`, `close-transactions-period`, `auto-close-connections`, `close-connections-period`. From 26.x: `colorDark` and `security.permission-restrictions` (array). Built-in type colors are theme keys (`org.jkiss.dbeaver.color.connectionType.prod.background`) up to 25.x and plain `R,G,B` in 26.x; the parser accepts both.
- `connections`: map of connection id → object with:
  - `provider` (e.g. `postgresql`, `mysql`, `sqlite`, `generic`) and `driver` (e.g. `postgres-jdbc`, `mysql8`, `mariaDB`) → NineSQL engine id (table below);
  - `name`, `folder`, `save-password`, `read-only`;
  - `configuration`: `host`, `port` (a string, verified), `database`, `server`, `url`, `configurationType` (`MANUAL` in the fixtures), `type` (connection type id), `auth-model`, `user` (when not in credentials), `bootstrap`, `provider-properties`, driver `properties`, `handlers`;
  - `bootstrap` (verified): `defaultSchema`, `query` (array of init queries), `autocommit` (`false` for manual commit); default catalog and isolation keys *(to verify)*;
  - `handlers` (verified for `ssh_tunnel`, `postgre_ssl`, `mysql_ssl`; proxy handlers *to verify*): each with `type` (`TUNNEL` / `CONFIG`), `enabled`, `save-password`, `properties`. Handler property values may be strings, integers or floats (`"22"`, `22`, `22.0`): the parser accepts all three;
  - `configuration.closeIdleConnection` appears in 23.x to 25.x only (verified).
- Unknown keys are preserved in the parsed model's `extra` map and listed in the import report.

## `credentials-config.json`
- Verified on all five fixture versions (same scheme in each): **AES-128-CBC** with PKCS#5/7 padding and a **fixed key present in DBeaver Community's source code**, identical for every installation; the file is a 16-byte IV followed by the ciphertext. This is obfuscation, not protection: anyone who can read the file can decrypt it, which is why NineSQL moves the secrets to the keyring ([ADR-0005](adr/0005-secrets-storage.md)).
- Decrypted content (verified): compact JSON, connection id → `{ "#connection": { "user", "password" }, "network/<handler-id>": { "user", "password" } }`. Keys are omitted when no value is saved, and a connection with nothing saved has no entry. An SSH key passphrase is stored as the `password` of `network/ssh_tunnel`. Only `network/ssh_tunnel` is covered by the fixtures.
- DBeaver PRO can protect credentials with a master password / secure storage; NineSQL detects that the file is not in the Community format and imports connections without passwords.
- Decrypted values live only in `secrecy` / `zeroize` buffers and go straight to `ninesql-secrets` (I1).

## Engine mapping
| DBeaver `provider` / `driver` (examples) | NineSQL engine id | Phase |
|---|---|---|
| `postgresql` / `postgres-jdbc` (verified) | `postgresql` | MVP |
| `postgresql` / `postgres-timescale-jdbc` (verified) | `timescaledb` | MVP |
| `postgresql` / `postgres-yugabytedb-jdbc` *(to verify: no fixture)* | `yugabytedb` | MVP |
| `mysql` / `mysql8` (verified), `mysql5` | `mysql` | MVP |
| `mysql` / `mariaDB` (verified) | `mariadb` | MVP |
| `generic` / `tidb` *(to verify)* | `tidb` | MVP |
| `generic` / `questdb` *(to verify)* | `questdb` | MVP |
| `sqlite` / `sqlite_jdbc` (verified) | `sqlite` | MVP |
| `duckdb` *(to verify)* | `duckdb` | v0.2 |
| `clickhouse` / `com_clickhouse` (verified); legacy `yandex_clickhouse` driver of older workspaces *(to verify: no fixture)* | `clickhouse` | v0.2 |
| `generic` / `trino_jdbc` *(to verify)* | `trino` | v0.2 |
| `jaybird` / `firebird` *(to verify)* | `firebird` | v0.2 |
| Elasticsearch, OpenSearch (CE providers, enhanced in PRO) *(ids to verify)* | `elasticsearch`, `opensearch` | v0.4 |
| InfluxDB (PRO provider) *(ids to verify)* | `influxdb` | v0.2 |
| MongoDB, Redis, Cassandra, Neo4j (PRO providers) *(ids to verify)* | matching ids | v0.3 / v0.4 |
| Oracle (`oracle` / `oracle_thin`, verified), SQL Server, DB2, Snowflake, … | — (imported disabled, ADR-0003) | after v1 |

Unknown provider / driver pairs are imported disabled with their raw values kept, never dropped.

## Field mapping rules
- **JDBC URL**: when `url` is set and host / port are empty, parse the JDBC URL per engine (`jdbc:postgresql://host:port/db?params`, `jdbc:mysql://…`, `jdbc:sqlite:/path`). Parameters with a NineSQL equivalent (`sslmode`, `connectTimeout`, `currentSchema`…) are mapped; the others are kept in properties and reported.
- **Connection types**: DBeaver's `dev`, `test`, `prod` map to NineSQL's Development, Test, Production; custom types (such as the fixtures' `staging`) are created with their color and flags.
- **Connection ids** are generated by DBeaver (`<driver-id>-<hex>-<hex>`) and are kept as `origin.dbeaver_id` for idempotent re-import.
- **SSH**: host, port, user, auth type (password / public key / agent), key path, saved passphrase, jump servers (`jumpServer<i>.*`, mapped in index order; their saved secrets are not imported and NineSQL asks for them). The host key is **not** trusted automatically: the first connection asks (ADR-0011). RSA key files cannot be used until RSA support is enabled ([ADR-0016](adr/0016-ssh-rsa-and-host-key-policies.md)); the import report does not flag them yet.
- **TLS**: mode, CA / client certificate paths. Connections that do not verify certificates are flagged in the report. Defaults and conversions: see [TLS](#tls) below.
- **Scripts**: optional copy into the NineSQL project, keeping the folder structure and the connection association when it can be resolved.

## Import rules enforced by the parser (P2-A)
`crates/ninesql-dbeaver` (P2-A) parses and maps a project into a **mirror** of the `ninesql-core` connection model; it persists nothing. The core converts that model, validates it and moves the secrets (P2-C, not done yet). The rules below are what the parser does today.

### Files read
- Every DBeaver file is opened read-only through one function; nothing under a DBeaver directory is created, written, renamed, deleted or locked (I4).
- `.bak` files and other dot-files (`.data-sources.json.bak`…) are never listed or read.
- A `credentials-config.json` that does not decrypt to the Community structure (DBeaver PRO master password or secure storage, corruption) is skipped: the connections are imported without saved credentials, and the report says so.
- Size limits per file: `data-sources*.json` 32 MiB, `credentials-config.json` 8 MiB.

### Per-project limits
Past a limit, the rest is not imported and the report says so.

| Item | Limit |
|---|---|
| `data-sources*.json` files read | 64 |
| Bytes of `data-sources*.json` read | 64 MiB |
| Connections | 10 000 |
| Folders | 10 000 (nesting depth 32) |
| Connection types | 1 000 |
| Report entries (project and connections together) | 50 000 |

### Connections imported disabled (fail closed, I2)
A connection whose route NineSQL cannot reproduce is imported **disabled** with the reason in the report, so that it never connects without a hop DBeaver used:
- **Network profiles** (F-CON-17, not supported yet): any non-empty value under a key containing `profile`, at the connection level, in `configuration` (DBeaver writes `config-profile` and `config-profile-source`, for example `global`: the profile may live outside the file) or in a handler's properties; and, when the file defines `network-profiles`, any non-empty key starting with `network`.
- **Unknown network handlers**: an enabled handler other than `ssh_tunnel`, `postgre_ssl` and `mysql_ssl` may route traffic (proxy, cloud forwarding…). The only exception is a `CONFIG` handler whose id names SSL / TLS and nothing that routes traffic: its settings are not imported and the report says so.
- **Unreadable handlers**: handler data that is not a JSON object.
- **SSH forwarding target**: `remoteHost` differs from the connection host, or `remotePort` differs from the connection port (or the connection has no explicit port). NineSQL forwards to the connection's own host and port.

Engines outside the v1 set and unknown provider / driver pairs are imported disabled too (see [Engine mapping](#engine-mapping)).

An SSH tunnel the parser cannot read completely, including unreadable jump-server settings or a jump server without a valid host, port, user name or authentication type, makes the connection **failed** (not imported) rather than imported without that hop.

### Database files inside `DBeaverData`
A file endpoint (SQLite and other file engines) is canonicalized. If it lies inside the DBeaver workspace, or inside the `DBeaverData` directory that contains it (or the parent of a workspace named `workspace6`), the connection is forced **read-only** with a warning (I4: NineSQL never modifies a file under a DBeaver directory). Drivers must honor that flag by opening the file read-only (P2-C follow-up).

### TLS
- No TLS setting at all in DBeaver: depending on the JDBC driver, DBeaver connected with opportunistic TLS without verification (PostgreSQL, MySQL) or without TLS (MariaDB, ClickHouse…). NineSQL applies **`verify-full`** and adds a warning: if the server has no TLS or no trusted certificate, the user chooses a weaker mode explicitly (ADR-0016 §4 clarifies ADR-0011 §4).
- `prefer` / `allow` (opportunistic TLS) have no NineSQL equivalent and become **`require`** (encrypted, certificate not verified), with a note.
- An explicit "do not verify" flag (`verifyServerCertificate=false`, `trustServerCertificate=true`) caps the mode at `require`.
- `disable`, `require` and `verify-ca` are kept as explicit, flagged downgrades.

### Keys not imported
Every `data-sources.json` key is either mapped, **ignored** (no report entry: nothing the user relies on is lost), **unsupported** (reported with a reason) or **unknown** (reported as not imported). The classification lives in `crates/ninesql-dbeaver/src/coverage.rs`; keep this table in step with it.

| Key | Class | Reason |
|---|---|---|
| `configuration.closeIdleConnection` | ignored | Written on every connection by DBeaver 23.x to 25.x; no per-connection idle-close setting yet (F-CON-10) |
| SSH `implementation` | ignored | DBeaver's SSH library choice; NineSQL uses its own client (ADR-0011) |
| PostgreSQL TLS `ssl.method` | ignored | Selects certificate files in DBeaver's dialog; the paths are imported directly |
| `network-profiles` (top level) | unsupported | Network profiles not supported yet (F-CON-17); connections that use one are disabled (above) |
| `virtual-models` | unsupported | Virtual keys and references have no NineSQL equivalent |
| `navigator-*` (connection) | unsupported | Navigator display settings |
| `configuration.events` | unsupported | Shell commands on connection events: NineSQL never runs commands from an imported file |
| `configuration.home` | unsupported | Local client home (native dump / restore tools) |
| `configuration.auth-properties` | unsupported | Authentication model properties |
| `bootstrap.txnIsolation` | unsupported | No default isolation level yet |
| `bootstrap.ignoreErrors` | unsupported | NineSQL stops at the first failing init statement |
| SSH `localHost`, `localPort` | unsupported | NineSQL binds `127.0.0.1` on an ephemeral port (ADR-0011) |
| SSH `aliveInterval`, `aliveCount`, `sshConnectTimeout` | unsupported | SSH keep-alive and timeout settings are not imported |
| SSH host-key bypass keys (containing `hostverif`, `stricthost` or `hostkey`) | unsupported | NineSQL always verifies host keys; the first connection asks (ADR-0011, I5) |
| TLS `sslFactory` (PostgreSQL, MySQL) | unsupported | Custom Java socket factories |
| MySQL TLS `ssl.cipher.suites` | unsupported | rustls defaults |

## Import report
For each connection: status (imported / imported disabled / failed), fields mapped, DBeaver keys not supported, secrets moved (count only, never values), warnings (TLS not verified, unknown driver, SSH host key to confirm). Saved in the project as `imports/dbeaver-<timestamp>.json` and shown after the import.

## Export to DBeaver (P5-D)
Writes a **new** `data-sources.json` chosen by the user, for selected connections, without credentials and without `credentials-config.json`. NineSQL-only settings are omitted. The output is tested by importing it back with NineSQL's importer and, manually, into DBeaver.
