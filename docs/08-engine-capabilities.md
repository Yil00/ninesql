# Engine capabilities

The human-readable mirror of each driver's `Capabilities` ([ADR-0004](adr/0004-driver-api-runtime-and-results.md)). The UI only offers what a driver declares (I8). **Nothing is implemented yet**: the tables below are the plan. A driver PR updates its row with what its integration tests prove.

Legend: ✅ supported · ◐ partial (see note) · ❌ not supported by the engine · 🗓 planned · — not applicable.

## Capability model
Defined in `crates/ninesql-driver-api/src/capabilities.rs` (`Capabilities`, P0-B); this section mirrors its rustdoc. A driver starts from `Capabilities::minimal(engine)` and switches on only what its integration tests prove. Anything not declared is hidden or disabled by the UI, never attempted (I8). `Session::capabilities()` gives the exact values for the connected server version; `Driver::capabilities(engine, None)` gives the pre-connect values used by the connection dialog.

| Field | Type | Meaning | UI effect when not declared |
|---|---|---|---|
| `query_languages` | set of `Sql`, `InfluxQl`, `MongoQuery`, `RedisCommand`, `SearchDsl`, `Cypher` | Languages accepted by `Session::execute` | Editor mode not offered |
| `result_kinds` | set of `Tabular` (Arrow batches), `Documents` | Kinds of result produced | Grid or document viewer not offered |
| `metadata.kinds` | set of `ObjectKind` | Object kinds `Metadata::list` returns (catalog, schema, table, view, materialized view, column, index, constraint, foreign key, trigger, routine, sequence, type, partition, user, role, collection, key, label, relationship type) | Navigator node hidden |
| `metadata.ddl` | set of `ObjectKind` | Kinds `Metadata::ddl` produces DDL for | "Generate DDL" hidden |
| `editing` | `insert`, `update`, `delete`, `requires_unique_key` | In-grid edits, rendered by `Dialect::render_edit` as parameterized DML | Grid read-only (also read-only for a result without a unique key when `requires_unique_key`) |
| `transactions` | `manual_commit`, `savepoints`, `isolation_levels` | Manual commit mode, savepoints, selectable isolation levels | Commit / rollback toolbar hidden, auto-commit forced |
| `read_only` | `ReadOnlyMechanism` (below) | How a read-only session is enforced (I3) | `ClientSideOnly` is shown in plain words with a read-only account recommendation |
| `cancel` | `ServerSide` / `LocalOnly` | Whether cancel reaches the server | Cancel still offered; `LocalOnly` is labelled "stop fetching" |
| `explain` | set of `Text`, `Json`, `Analyze` | Explain formats (`Analyze` runs the statement) | "Explain" hidden |
| `multiple_result_sets` | bool | One statement may yield several result sets | Single result tab |
| `server_side_timeout` | bool | The engine also enforces statement timeouts | Timeout enforced by the result stream only |
| `tls` | set of `TlsMode` | TLS modes offered (`disable`, `require`, `verify-ca`, `verify-full`); empty for file engines | TLS section disabled |
| `parameters` | bool | Bind parameters in statements | Parameter prompt not offered |

`ReadOnlyMechanism` values (ADR-0007): `SessionSetting` (session-level `SET … READ ONLY`), `TransactionMode` (every transaction started read-only), `OpenMode` (file opened read-only), `ServerSetting` (per-session server setting such as ClickHouse `readonly = 1`), `ReadEndpointsOnly` (only read endpoints are called), and `ClientSideOnly` (no engine-side mechanism: the driver refuses writes it can classify). Every value except `ClientSideOnly` counts as engine-side.

Rules every driver follows whatever its mechanism (rustdoc of `Driver::connect` and `Session::execute`):
- read-only is applied **before** `connect` returns and before init statements run; if it cannot be applied, connect fails;
- in a read-only session, statements the dialect classifies as writes are refused client-side as well, and statements that may lift the access mode (`SET … READ WRITE`, `default_transaction_read_only = off`, `PRAGMA query_only = OFF`…, class `ChangesAccessMode`) are always refused;
- every result goes through `ResultStream`, which enforces the caller's `FetchBudget` (`batch_rows` default 200, `max_bytes` default 256 MiB, optional `max_rows`), the timeout and the cancel token, and ends with `Truncated` at the budget (I6).

The mock driver (feature `mock` of `ninesql-driver-api`, engine id `mock`) declares every capability by default and accepts overrides so UI tests can check that undeclared features are hidden.

## Engines, families and test containers
| Engine id | Engine | Family crate | Default port | Official image (pinned by digest in `dev/`) | Profile | Phase |
|---|---|---|---|---|---|---|
| `postgresql` | PostgreSQL | pgwire | 5432 | `postgres` | core | MVP |
| `mysql` | MySQL | mysqlwire | 3306 | `mysql` | core | MVP |
| `mariadb` | MariaDB | mysqlwire | 3306 | `mariadb` | core | MVP |
| `sqlite` | SQLite | sqlite | — (file) | — | core | MVP |
| `timescaledb` | TimescaleDB | pgwire | 5432 | `timescale/timescaledb` | sql | MVP |
| `yugabytedb` | YugabyteDB (YSQL) | pgwire | 5433 | `yugabytedb/yugabyte` | sql | MVP |
| `questdb` | QuestDB | pgwire | 8812 | `questdb/questdb` | sql | MVP |
| `tidb` | TiDB | mysqlwire | 4000 | `pingcap/tidb` | sql | MVP |
| `duckdb` | DuckDB | duckdb | — (file) | — | sql | v0.2 |
| `clickhouse` | ClickHouse | clickhouse | 8123 (HTTP) | `clickhouse/clickhouse-server` | sql | v0.2 |
| `trino` | Trino | trino | 8080 | `trinodb/trino` | sql | v0.2 |
| `firebird` | Firebird | firebird | 3050 | `firebirdsql/firebird` | sql | v0.2 |
| `influxdb` | InfluxDB 3 Core | influxdb | 8181 | `influxdb` (3-core tag) | sql | v0.2 |
| `mongodb` | MongoDB | mongodb | 27017 | `mongo` | nosql | v0.3 |
| `redis` | Redis | redis | 6379 | `redis` | nosql | v0.3 |
| `valkey` | Valkey | redis | 6379 | `valkey/valkey` | nosql | v0.3 |
| `elasticsearch` | Elasticsearch | search | 9200 | `docker.elastic.co/elasticsearch/elasticsearch` | heavy | v0.4 |
| `opensearch` | OpenSearch | search | 9200 | `opensearchproject/opensearch` | heavy | v0.4 |
| `cassandra` | Apache Cassandra | cassandra | 9042 | `cassandra` | heavy | v0.4 |
| `neo4j` | Neo4j Community | neo4j | 7687 (Bolt) | `neo4j` | heavy | v0.4 |

Ports in `dev/` are remapped to avoid clashes; `dev/engines.json` (P0-C) is authoritative. Image names, ports and the InfluxDB 3 tag are verified in P0-C.

## Read-only mechanism (ADR-0007, I3)
| Engine | Planned engine-side mechanism | Status |
|---|---|---|
| PostgreSQL, TimescaleDB | `SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY` (+ `default_transaction_read_only`) | 🗓 |
| YugabyteDB (YSQL) | Same as PostgreSQL | 🗓 verify |
| QuestDB | No read-only transaction; to investigate (read-only user / `pg.readonly` server option) — may be `ClientSideOnly` | 🗓 verify |
| MySQL, MariaDB, TiDB | `SET SESSION TRANSACTION READ ONLY` | 🗓 verify TiDB |
| SQLite | Open with `SQLITE_OPEN_READ_ONLY` + `PRAGMA query_only = ON` | 🗓 |
| DuckDB | `access_mode = READ_ONLY` at open | 🗓 |
| ClickHouse | `readonly = 1` setting per session / query | 🗓 |
| Trino | No session-level read-only; access control is server-side → `ClientSideOnly` unless a read-only catalog / user is configured | 🗓 verify |
| Firebird | Read-only transactions (`isc_tpb_read`) | 🗓 |
| InfluxDB 3 | Query API is read-only; writes go through a separate API the driver will not expose in read-only mode | 🗓 |
| MongoDB | No session read-only; driver refuses write commands, recommend a read-only role → `ClientSideOnly` | 🗓 |
| Redis, Valkey | `READONLY` only applies to cluster replicas → `ClientSideOnly` via command classification, recommend an ACL user with read commands | 🗓 |
| Elasticsearch, OpenSearch | No session read-only → `ClientSideOnly` (only search / SQL endpoints called), recommend a read-only role | 🗓 |
| Cassandra | No session read-only → `ClientSideOnly`, recommend a role with `SELECT` only | 🗓 |
| Neo4j | Read access mode on sessions / transactions (`READ`) — routes to readers; to verify that writes are rejected on a single instance | 🗓 verify |

## Feature capabilities (planned)
| Engine | Metadata tree | DDL | Edit data | Transactions | Server-side cancel | Explain | Multiple result sets |
|---|---|---|---|---|---|---|---|
| PostgreSQL | 🗓 | 🗓 | 🗓 | 🗓 | 🗓 `pg_cancel_backend` / cancel request | 🗓 | 🗓 |
| MySQL / MariaDB | 🗓 | 🗓 `SHOW CREATE` | 🗓 | 🗓 | 🗓 `KILL QUERY` | 🗓 | 🗓 |
| SQLite | 🗓 | 🗓 `sqlite_master` | 🗓 | 🗓 | 🗓 interrupt | 🗓 `EXPLAIN QUERY PLAN` | ❌ |
| TimescaleDB | 🗓 + hypertables | 🗓 | 🗓 | 🗓 | 🗓 | 🗓 | 🗓 |
| YugabyteDB | 🗓 (catalog differences) | 🗓 | 🗓 | 🗓 | 🗓 | 🗓 | 🗓 |
| QuestDB | 🗓 (tables, partitions) | ◐ | ◐ (designated timestamp rules) | ❌ | 🗓 | 🗓 | ❌ |
| TiDB | 🗓 | 🗓 | 🗓 | 🗓 | 🗓 `KILL TIDB QUERY` | 🗓 | 🗓 |
| DuckDB | 🗓 | 🗓 | 🗓 | 🗓 | 🗓 interrupt | 🗓 | ❌ |
| ClickHouse | 🗓 | 🗓 `SHOW CREATE` | ◐ (mutations) | ❌ | 🗓 `KILL QUERY` | 🗓 | ❌ |
| Trino | 🗓 (catalogs) | ◐ | ◐ (connector-dependent) | ◐ | 🗓 | 🗓 | ❌ |
| Firebird | 🗓 | 🗓 | 🗓 | 🗓 | 🗓 | 🗓 plan | ❌ |
| InfluxDB 3 | 🗓 (databases, tables) | ❌ | ❌ | ❌ | ◐ | 🗓 | ❌ |
| MongoDB | 🗓 | — | 🗓 | ◐ (replica sets) | 🗓 `killOp` | 🗓 `explain` | — |
| Redis / Valkey | 🗓 (keyspace by pattern) | — | 🗓 | ◐ (`MULTI`) | ❌ | — | — |
| Elasticsearch / OpenSearch | 🗓 (indices, mappings) | — | 🗓 | ❌ | 🗓 tasks API | 🗓 profile | — |
| Cassandra | 🗓 | 🗓 `DESCRIBE` | 🗓 | ❌ (LWT only) | ❌ | 🗓 tracing | — |
| Neo4j | 🗓 (labels, types, indexes) | 🗓 | 🗓 | 🗓 | 🗓 | 🗓 `EXPLAIN` / `PROFILE` | — |
