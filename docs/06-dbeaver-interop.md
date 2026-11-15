# DBeaver interoperability

Decision record: [ADR-0006](adr/0006-dbeaver-interoperability.md). Owner: `interop-engineer` (`crates/ninesql-dbeaver/`).

> **Verification status.** The format notes below come from DBeaver Community's public source and documentation, checked against the P0-E fixtures: synthetic workspaces written by five DBeaver CE builds, 22.3.5 to 26.2.4 ([fixtures README](../crates/ninesql-dbeaver/tests/fixtures/README.md)). Items marked *verified* were confirmed on those fixtures. Items marked *(source)* were read in DBeaver CE's public source (`dbeaver/dbeaver`, branch `devel`, checked on 2026-11-15: the plugin descriptors `plugins/org.jkiss.dbeaver.ext.*/plugin.xml`, `DataSourceSerializerModern.java` and `RegistryConstants.java`) but no fixture covers them; the importer still reports them as not verified against a DBeaver-written workspace. Items still marked *(to verify)* are covered by neither and must be confirmed before the importer relies on them. Do not "fix" the importer to match this page: fix this page to match the fixtures.

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
  - `bootstrap` (verified): `defaultSchema`, `query` (array of init queries), `autocommit` (`false` for manual commit). From the source *(source)*: `defaultCatalog` (string), `txnIsolation` (an integer, the JDBC isolation constant), `ignoreErrors` (boolean, written only when `true`) and a legacy `defaultObject`, which DBeaver reads only when `defaultSchema` is empty, as the name of another `bootstrap` key holding the schema. DBeaver writes `defaultCatalog` / `defaultSchema` only when not empty, and `autocommit` only when set;
  - `handlers` (verified for `ssh_tunnel`, `postgre_ssl`, `mysql_ssl`; from the source *(source)*: the SOCKS proxy handler `socks_proxy` of type `proxy`, and ClickHouse's TLS handler `clickhouse-ssl` of type `config`, both without a fixture): each with `type` (`TUNNEL` / `CONFIG`), `enabled`, `save-password`, `properties`. Handler property values may be strings, integers or floats (`"22"`, `22`, `22.0`): the parser accepts all three;
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
| `postgresql` / `postgres-jdbc` (verified); legacy `postgresql` / `postgresql` ("PostgreSQL (Old)", *source*) | `postgresql` | MVP |
| `postgresql` / `postgres-timescale-jdbc` (verified) | `timescaledb` | MVP |
| `postgresql` / `postgres-yugabytedb-jdbc` *(source; no fixture)* | `yugabytedb` | MVP |
| `mysql` / `mysql8` (verified), `mysql5` ("MySQL 5 (Legacy)", *source*) | `mysql` | MVP |
| `mysql` / `mariaDB` (verified) | `mariadb` | MVP |
| `tidb` / `tidb` *(source: DBeaver's TiDB plugin, MySQL Connector/J; no fixture)*. `mysql` / `tidb` and `generic` / `tidb` are also accepted, though no DBeaver build is known to write them | `tidb` | MVP |
| `generic` / `questdb` *(to verify: no DBeaver CE plugin descriptor checked declares QuestDB)* | `questdb` | MVP |
| `sqlite` / `sqlite_jdbc` (verified); legacy `generic` / `sqlite_jdbc` *(source)* | `sqlite` | MVP |
| `duckdb` / `duckdb_jdbc` *(source)*; legacy `generic` / `duckdb_jdbc` *(source)*. `duckdb` / `motherduck_jdbc` (MotherDuck, a cloud service, `jdbc:duckdb:md:`) is **not** mapped: imported disabled | `duckdb` | v0.2 |
| `clickhouse` / `com_clickhouse` (verified); legacy `clickhouse` / `yandex_clickhouse` ("ClickHouse (Legacy)", *source*) | `clickhouse` | v0.2 |
| `generic` / `trino_jdbc` *(source)* | `trino` | v0.2 |
| `jaybird` / `jaybird` *(source)*; legacy `generic` / `firebird_jaybird` and `firebird_jaybird3` *(source)*. `jaybird` / `jaybird_embedded` (an embedded engine on a local file) is **not** mapped | `firebird` | v0.2 |
| Elasticsearch: `elasticsearch` / `elastic_search_jdbc` *(source)*; Open Distro: `open-distro-elasticsearch` / `open_distro_elastic_search_jdbc` *(source)*; OpenSearch *(ids to verify)*. Imported disabled until the v0.4 drivers land | `elasticsearch`, `opensearch` | v0.4 |
| InfluxDB (PRO provider) *(ids to verify: not in the CE source)* | `influxdb` | v0.2 |
| MongoDB, Redis, Cassandra, Neo4j (PRO providers) *(ids to verify: not in the CE source)* | matching ids | v0.3 / v0.4 |
| Oracle (`oracle` / `oracle_thin`, verified), SQL Server, DB2, Snowflake, … ; also other PostgreSQL- and MySQL-plugin drivers (Greenplum, CockroachDB, Redshift, NDB Cluster `mysql_ndb`, StarRocks, libSQL `sqlite` / `libsql_jdbc`…) | — (imported disabled, ADR-0003) | after v1 |

Unknown provider / driver pairs are imported disabled with their raw values kept, never dropped.

## Field mapping rules
- **JDBC URL**: when `url` is set and host / port are empty, parse the JDBC URL per engine (`jdbc:postgresql://host:port/db?params`, `jdbc:mysql://…`, `jdbc:sqlite:/path`). Parameters go through the same rules as driver properties: TLS keys feed the TLS settings, and the rest is converted or dropped (see [Driver properties](#driver-properties)). When a URL parameter and a driver property have the same key, the one the JDBC driver uses is kept (see [precedence](#driver-properties)).
- **Connection types**: DBeaver's `dev`, `test`, `prod` map to NineSQL's Development, Test, Production; custom types (such as the fixtures' `staging`) are created with their color and flags.
- **Connection ids** are generated by DBeaver (`<driver-id>-<hex>-<hex>`) and are kept as `origin.dbeaver_id` for idempotent re-import.
- **SSH**: host, port, user, auth type (password / public key / agent), key path, saved passphrase, jump servers (`jumpServer<i>.*`, selected as DBeaver does, see [SSH jump servers](#ssh-jump-servers); their saved secrets are not imported and NineSQL asks for them). The host key is **not** trusted automatically: the first connection asks (ADR-0011). RSA key files cannot be used until RSA support is enabled ([ADR-0016](adr/0016-ssh-rsa-and-host-key-policies.md)); the import report flags them by file name (see [SSH key files](#ssh-key-files)).
- **TLS**: mode, CA / client certificate paths. Connections that do not verify certificates are flagged in the report. Defaults and conversions: see [TLS](#tls) below.
- **Scripts**: optional copy into the NineSQL project, keeping the folder structure and the connection association when it can be resolved (not done yet: the format is to verify).

## Import rules enforced by the parser (P2-A)
`crates/ninesql-dbeaver` (P2-A) parses and maps a project into a **mirror** of the `ninesql-core` connection model; it persists nothing. The core converts that model, validates it, persists it and moves the secrets ([P2-C, below](#persistence-in-the-core-p2-c)). The rules below are what the parser does.

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
- **Network profiles** (F-CON-17, not supported yet): any value other than `null`, the empty string or an empty array / object under a key containing `profile`, at the connection level, in `configuration` (DBeaver writes `config-profile` and `config-profile-source`, for example `global`: the profile may live outside the file) or in a handler's properties; and, when the file defines `network-profiles`, any such value under a key starting with `network`. The same test applies in every scope, and a blank value (`" "`) counts as a reference, because DBeaver does not trim the value it looks a profile up by.
- **Unknown network handlers**: an enabled handler other than `ssh_tunnel`, `postgre_ssl` and `mysql_ssl` may route traffic (proxy, cloud forwarding…). The only exception is a `CONFIG` handler whose id names SSL / TLS and nothing that routes traffic: its settings are not imported and the report says so.
- **Unreadable handlers**: handler data that is not a JSON object.
- **SSH forwarding target**: `remoteHost` differs from the connection host, or `remotePort` differs from the connection port (or the connection has no explicit port). NineSQL forwards to the connection's own host and port.

Engines outside the v1 set and unknown provider / driver pairs are imported disabled too (see [Engine mapping](#engine-mapping)).

An SSH tunnel the parser cannot read completely, including unreadable jump-server settings or a jump server without a valid host, port, user name or authentication type, makes the connection **failed** (not imported) rather than imported without that hop.

### SSH jump servers
DBeaver stores jump servers as `jumpServer<i>.<field>` properties of the `ssh_tunnel` handler (none in the P0-E fixtures; key names and order taken from DBeaver's source, `SSHUtils.loadHostConfigurations`). The parser selects them as DBeaver 24.3 and later does:
- `jumpServer.count`: absent or blank means 5 (DBeaver's `MAX_JUMP_SERVERS`, also the most hops accepted); a negative count means none; anything that is not an integer fails the connection.
- Indices are read from `0` to `count - 1`, stopping at the first one that is not enabled. Index 0 is the hop closest to NineSQL. Only canonical indices are accepted (`0`, or a non-zero digit followed by digits: `00`, `01`, `+0` fail).
- `enabled` must be a boolean or the string `true` / `false` (any case); absent means not enabled; any other value fails the connection rather than being read as `false`.
- The user name is `.name` (DBeaver's `ATTR_NAME`); `.host`, `.port`, `.authType` and, for public-key authentication, `.keyPath` are required.
- Fail closed (I2), the connection **fails** rather than losing a hop: unreadable settings, a non-canonical index, a non-boolean `enabled`, an enabled hop after a disabled or missing one (DBeaver would skip it silently), more than 5 hops, a hop without a valid host, port, user name or authentication type.
- Hops DBeaver does not use (not enabled, or at or past `jumpServer.count`) are reported and not imported: a configured host gets a warning, and their secret-like fields get a "removed" entry like any other secret.

### SSH key files
NineSQL refuses RSA private key files ([ADR-0016](adr/0016-ssh-rsa-and-host-key-policies.md) §1.2). DBeaver stores no key type and the importer never reads key files, so the check is a heuristic on the file name (`SshKeyType` warning in the report, without the path): a name starting with `id_rsa` or containing an `rsa` word is flagged as RSA; a name that says neither RSA nor Ed25519 / ECDSA is flagged as unchecked; `id_ed25519*`, `id_ecdsa*` and names with an `ed25519` / `ecdsa` word are not flagged. A key named `id_ed25519` may still be RSA: `ninesql-ssh` then refuses it at the first connection.

### Driver properties
DBeaver passes its driver properties (`configuration.properties`) and JDBC URL parameters to a Java JDBC driver. NineSQL's drivers accept only a short allowlist of their own property names (`ConnectCapabilities::accepted_properties`, [docs/08](08-engine-capabilities.md)) and refuse any other key at connect, so the importer converts or drops every property (code: `crates/ninesql-dbeaver/src/properties.rs`). Order of the rules, for each property:
1. TLS keys (`sslmode`, `useSSL`, `verifyServerCertificate`…) feed the TLS settings ([TLS](#tls)); `user` / `username` gives the user name when the credentials have none; `readOnly=true` sets the read-only flag.
2. A key or value that looks like a secret is removed (`SecretRemoved`).
3. A key the driver API reserves (`is_reserved_property`: endpoint, credentials, TLS, read-only, init SQL, multi-statements…) is dropped (`ReservedProperty`), except pgjdbc `options` and Connector/J `sessionVariables`, which embed several settings and are read setting by setting (below).
4. For an engine a NineSQL driver family serves, the property is **converted** to that driver's property names (`PropertyMapped`, info) or **dropped** (`PropertyDropped`, warning). For any other engine (no NineSQL driver yet), it is kept as DBeaver stored it.

Every dropped property is also listed in `ImportedConnection::dropped_properties` (DBeaver key path and reason: no equivalent, invalid value, reserved, looks like a secret), at most 1 000 entries per connection; past that, one `Truncated` report entry gives the count. The report and the list name the key, and the setting name inside `options` / `sessionVariables` only when the entry is a readable `name=value` with a non-empty value that does not start with `=`, and the name is a plain identifier that does not look like a token (16 characters or more mixing upper case, lower case and digits, with no `_`): `configuration.properties.options.geqo`. Other entries are only counted ("N entries that are not a plain `name=value` setting"). Never a value (I1). Values are checked against copies of the NineSQL drivers' own checks (allowlisted name, value shape), so the drivers' property checks accept a converted property; the server can still refuse a value at connect (an unknown time zone name, a `statement_timeout` without a valid unit…), as it would have refused it in DBeaver. A value the driver would refuse is dropped as invalid.

**Read-only intent is never lost** (fail safe, I3): a read-only setting inside `options` (`default_transaction_read_only`, `transaction_read_only`) or `sessionVariables` (`transaction_read_only`, `tx_read_only`) sets the connection's read-only flag unless its value is an explicit false (PostgreSQL: a prefix of `false` or `no`, `of` / `off`, `0`; MySQL: `0`, `OFF`, `FALSE`). An entry that cannot be read, an entry past the first 64 of the value, or an unknown setting whose name contains "read only" (after removing separators: `innodb_read_only`), also sets the flag, with a warning asking the user to check it.

Which JDBC driver's semantics apply depends on the DBeaver driver: `mariaDB` uses MariaDB Connector/J, `mysql8`, `mysql5` and `tidb` use MySQL Connector/J, the PostgreSQL-plugin drivers use pgjdbc, `sqlite_jdbc` uses xerial sqlite-jdbc. JDBC property names are matched ignoring ASCII case.

**pgjdbc → pgwire** (PostgreSQL, TimescaleDB, YugabyteDB, QuestDB; pgwire accepts `search_path`, `TimeZone`, `statement_timeout`, `lock_timeout`, `work_mem`, `client_min_messages`):

| JDBC property | NineSQL | Notes |
|---|---|---|
| `currentSchema` | `search_path` | pgjdbc sends it as the `search_path` startup parameter. Dropped if blank or if it lists `pg_catalog` after another schema (pgwire keeps `pg_catalog` first) |
| `options` | one property per allowlisted `-c name=value`, `-cname=value` or `--name=value` setting | Split like the server does (whitespace, backslash escapes) and read like its getopt (switch string `B:bC:c:D:d:EeFf:h:ijk:lN:nOPp:r:S:sTt:W:-:`): combined switches (`-ec name=value`), an argument attached or in the next word, `-` in a setting name read as `_` (`--default-transaction-read-only=on`); names compared after normalization (`TimeZone` = `timezone`). Booleans are read as PostgreSQL reads them (unique prefixes of `true` / `false` / `yes` / `no`, `on`, `off`, `1`, `0`). Read-only settings: see above. Any other setting, and any word that is not a setting, is dropped. A dedicated property (`currentSchema`) wins over the same setting in `options`, as PostgreSQL applies the startup parameters after `options` |
| anything else (`ApplicationName`, `connectTimeout`, `socketTimeout`, `prepareThreshold`, `defaultRowFetchSize`, `stringtype`, `tcpKeepAlive`, `targetServerType`…) | dropped | No pgwire equivalent. NineSQL applies its own connect timeout. A key named like a pgwire property (`statement_timeout`) is not a pgjdbc property: it had no effect in DBeaver and is dropped too |

**MySQL Connector/J and MariaDB Connector/J → mysqlwire** (MySQL, MariaDB, TiDB; mysqlwire accepts `time_zone`, `lock_wait_timeout`, `innodb_lock_wait_timeout`, `group_concat_max_len`, and `max_execution_time` on MySQL / `max_statement_time` on MariaDB):

| JDBC property | NineSQL | Notes |
|---|---|---|
| `sessionVariables` (both drivers) | one property per allowlisted `name=value` entry | Split on commas outside quotes and parentheses; a `@@`, `@@session.` or `session.` prefix is removed; values checked as mysqlwire checks them (digits for the integer variables, a decimal for `max_statement_time`, a non-empty text with its quotes removed for `time_zone`). The variable must exist on the connection's engine (`max_statement_time` is dropped on MySQL, `max_execution_time` on MariaDB). Read-only variables: see above. Any other variable (`sql_mode`…) is dropped |
| Connector/J 8 and later (`mysql8`, `tidb`): `connectionTimeZone` (or its alias `serverTimezone`) with `forceConnectionTimeZoneToSession=true` | `time_zone` | Only with that flag does Connector/J set the session time zone (Connector/J 5.1, DBeaver's `mysql5`, has no such flag: dropped); without it, the time zone only drives client-side date conversion and is dropped. `LOCAL` and `SERVER` depend on the machine or the server and are dropped. Wins over `time_zone` in `sessionVariables`. A time zone name the server does not know fails at connect, as it did in DBeaver |
| MariaDB Connector/J `timezone` | dropped | Its effect on the session time zone differs between MariaDB Connector/J versions *(to verify)*; not converted |
| anything else (`connectTimeout`, `socketTimeout`, `useCursorFetch`, `rewriteBatchedStatements`, `zeroDateTimeBehavior`, `tinyInt1isBit`…) | dropped | No mysqlwire equivalent (`characterEncoding`, `allowLoadLocalInfile`, `allowPublicKeyRetrieval`… are reserved, step 3) |

**sqlite-jdbc → sqlite** (sqlite accepts `busy_timeout`, `cache_size`, `foreign_keys`):

| JDBC property | NineSQL | Notes |
|---|---|---|
| `busy_timeout` | `busy_timeout` | Milliseconds, 0 to 600 000 |
| `cache_size` | `cache_size` | Only the KiB form (a negative number, -1 to -1 048 576). A positive value is a page count, which depends on the file's page size: dropped |
| `foreign_keys` | `foreign_keys` | `true` / `false` (also `on` / `off`, `1` / `0`, `yes` / `no`) |
| anything else (`open_mode`, `date_class`, `enable_load_extension`, `journal_mode`, `synchronous`…) | dropped | No equivalent, or a setting the sqlite driver reserves |

**Precedence.** When a URL parameter and a driver property have the same key, the importer keeps the one the JDBC driver uses and drops the other entirely (also for `options` / `sessionVariables`), with a report entry that says whether the two values differ. Checked in the public sources on 2026-11-15:

| JDBC driver | Wins | Source |
|---|---|---|
| pgjdbc | URL parameter | `org.postgresql.Driver.parseURL` (`pgjdbc/pgjdbc`, `master`): "priority 1 - URL values", the `Properties` argument only fills keys the URL does not set |
| MariaDB Connector/J | URL parameter | `org.mariadb.jdbc.Configuration.processUrlParameters` (`mariadb-corporation/mariadb-connector-j`, `develop`) writes URL parameters over the properties |
| MySQL Connector/J | driver property | `com.mysql.cj.conf.ConnectionUrl.collectProperties` (`mysql/mysql-connector-j`, `release/9.x`) puts the URL properties first, then the `Properties` argument over them |
| sqlite-jdbc | driver property | `org.sqlite.SQLiteConnection.extractPragmasFromFilename` (`xerial/sqlite-jdbc`, `master`) ignores a URL pragma already set as a property |

These are the current branches; older driver versions bundled with old DBeaver builds are not checked *(to verify)*. Engines without a NineSQL driver keep the driver property, as before.

When the same NineSQL property is produced by different keys, a dedicated property wins over a setting embedded in `options` / `sessionVariables` (PostgreSQL applies the startup parameters after `options`), then the source the JDBC driver lets win (above), then the later one. The loser is dropped with a warning, which says when the URL and the driver properties set different values (for `search_path`, `time_zone`…).

### Default schema, catalog and auto-commit
DBeaver's `bootstrap` values are exposed as they are in the mirror model, for the core to persist (P2-C): `ImportedConnection::default_schema` (`bootstrap.defaultSchema`), `default_catalog` (`bootstrap.defaultCatalog`), both trimmed, `None` when absent or blank, and `auto_commit` (`bootstrap.autocommit`: `Some(false)` manual commit, `Some(true)` auto-commit, `None` when DBeaver stored nothing and the connection type's default applies). Until the core model has fields for them, each value has a report entry saying it is not applied yet (`Unsupported`, keys `configuration.bootstrap.defaultSchema`, `.defaultCatalog`, `.autocommit`; the auto-commit entry only when the value differs from the connection type's default).

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
| `bootstrap.defaultObject` | unsupported | Legacy key; DBeaver reads it only when `defaultSchema` is empty, through an indirection NineSQL does not reproduce |
| SSH `localHost`, `localPort` | unsupported | NineSQL binds `127.0.0.1` on an ephemeral port (ADR-0011) |
| SSH `aliveInterval`, `aliveCount`, `sshConnectTimeout` | unsupported | SSH keep-alive and timeout settings are not imported |
| SSH host-key bypass keys (containing `hostverif`, `stricthost` or `hostkey`) | unsupported | NineSQL always verifies host keys; the first connection asks (ADR-0011, I5) |
| TLS `sslFactory` (PostgreSQL, MySQL) | unsupported | Custom Java socket factories |
| MySQL TLS `ssl.cipher.suites` | unsupported | rustls defaults |

## Persistence in the core (P2-C)
`ninesql-core` (`import` module, [docs/02](02-architecture.md#dbeaver-import-p2-c)) turns the parser's output into saved connections, in two steps: a preview (no secret value) and an apply that reads the source again and refuses if it changed since the preview.
- **Conversion**: each mirror connection becomes a `Connection`, then `sanitize()`, `validate()` and `DriverRegistry::validate_connection`. Driver properties the engine's NineSQL driver reserves or does not accept (pgjdbc and Connector/J properties without an equivalent, such as `prepareThreshold`, `ApplicationName`, `connectTimeout`) are dropped with a report entry. A connection its driver refuses, or whose engine has no driver in this build (TimescaleDB, ClickHouse, Oracle… today), is imported **disabled** with a persisted reason (`no-driver`, `invalid`); the parser's disabled connections keep theirs (`route`), and cannot be enabled again until the route is set up and confirmed. A connection that breaks a model rule is not imported.
- **Bootstrap**: `defaultSchema`, `defaultCatalog` and `autocommit` are kept in the connection (`default_schema`, `default_catalog`, `auto_commit`); the drivers do not apply the default schema / catalog yet. Init queries are imported with the imported init SQL marker: the first open confirms every statement whatever the type.
- **Connection types**: `dev`, `test`, `prod` use NineSQL's built-in types; the types of all imported projects are folded (the stricter rules of each), then a custom type is created, or an existing type of the same id — a built-in included — is only made stricter (never weaker; name and color stay). For example DBeaver's `test` confirms data changes, so the import makes NineSQL's Test confirm destructive statements, and the report says so.
- **Secrets**: moved to the keyring or the vault chosen in the preview (none: asked at connect), then dropped from memory. DBeaver's files are left as they are; the report recommends deleting DBeaver's saved passwords once the user has switched (ADR-0006 §4).
- **Folders**: NineSQL folders exist through their connections: empty DBeaver folders and folder descriptions are listed in the preview and the report, not created.
- **Re-import** (F-CON-14): matched on `origin.dbeaver_id`, the DBeaver project and the canonical source workspace (`origin.source_workspace`); the saved connection is updated, not duplicated. The same id from another workspace or project is a new connection, with a report entry. NineSQL keeps the user's name, description, folder, password-exchange policy, per-hop host-key policies, a stricter read-only or manual-commit setting, a stricter TLS mode and the saved CA file (unless the user accepts the change in the preview), a stricter connection type, a "disabled by the user" state, and secrets saved in NineSQL for fields DBeaver has none for **only while the server, user, TLS and SSH route they authenticate against are unchanged**. Everything else follows the DBeaver file; init SQL is marked for confirmation again. The preview lists the differences per connection.
- **Files**: the source workspace is registered as a DBeaver root for the session at preview time; file connections inside it are saved read-only.
- **Scripts**: not copied yet (`Scripts/` format to verify).

## Import report
For each connection: status (imported / imported disabled / failed), fields mapped, DBeaver keys not supported, secrets moved (count only, never values), warnings (TLS not verified, unknown driver, SSH host key to confirm). Saved in the project as `imports/dbeaver-<YYYYMMDDTHHMMSSZ>.json` (`ImportReportFile`, `schema_version` 1) and shown after the import.

## Export to DBeaver (P5-D)
Writes a **new** `data-sources.json` chosen by the user, for selected connections, without credentials and without `credentials-config.json`. NineSQL-only settings are omitted. The output is tested by importing it back with NineSQL's importer and, manually, into DBeaver.
