# Engine capabilities

The human-readable mirror of each driver's `Capabilities` ([ADR-0004](adr/0004-driver-api-runtime-and-results.md)). The UI only offers what a driver declares (I8). **Nothing is implemented yet**: the tables below are the plan. A driver PR updates its row with what its integration tests prove.

Legend: ✅ supported · ◐ partial (see note) · ❌ not supported by the engine · 🗓 planned · — not applicable.

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
| `influxdb` | InfluxDB 3 Core | influxdb | 8181 | `influxdb` (`3.x-core` tags) | sql | v0.2 |
| `mongodb` | MongoDB | mongodb | 27017 | `mongo` | nosql | v0.3 |
| `redis` | Redis | redis | 6379 | `redis` | nosql | v0.3 |
| `valkey` | Valkey | redis | 6379 | `valkey/valkey` | nosql | v0.3 |
| `elasticsearch` | Elasticsearch | search | 9200 | `elasticsearch` (Docker Official Image, maintained by Elastic) | heavy | v0.4 |
| `opensearch` | OpenSearch | search | 9200 | `opensearchproject/opensearch` | heavy | v0.4 |
| `cassandra` | Apache Cassandra | cassandra | 9042 | `cassandra` | heavy | v0.4 |
| `neo4j` | Neo4j Community | neo4j | 7687 (Bolt) | `neo4j` | heavy | v0.4 |

Ports in `dev/` are remapped to avoid clashes; [`dev/engines.json`](../dev/engines.json) is authoritative for host ports, pinned images and dev credentials. Image names, default ports and the InfluxDB 3 tag were verified in P0-C: every image above starts and loads the "nine" dataset ([dev/README.md](../dev/README.md)).

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
