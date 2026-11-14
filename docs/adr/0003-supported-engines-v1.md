# ADR-0003: Supported engines for v1 and driver families

- **Status**: Accepted; partially superseded by [ADR-0014](0014-dev-environment-profiles.md) for the dev environment profiles
- **Date**: 2026-11-14

## Context
DBeaver reaches 100+ databases through JDBC. NineSQL has no JVM (ADR-0001), so each engine needs a native Rust client. The maintainer's scope for v1: the **20 most used engines that are open source or free to self-host**, excluding cloud-only services and proprietary engines, so that every one of them can run in a container in CI and in Claude Code cloud sandboxes.

## Decision
### Selection criteria
An engine is in the v1 set when all of these hold:
1. Free to self-host, with an official container image (no account, no license key).
2. Among the most used engines of its category (DB-Engines ranking and DBeaver usage, late 2026).
3. A Rust client exists under an ADR-0002 license, or the protocol is simple enough to implement (HTTP / JSON).

Excluded for v1: cloud-only services (Snowflake, BigQuery, Redshift, Azure SQL, DynamoDB…); proprietary engines (Oracle, SQL Server, DB2, SAP HANA, Teradata…); CockroachDB (enterprise license since 2024); Greenplum (closed source since 2024). They may come back after v1 through a new ADR.

### The 20 engines, grouped by driver family
| Family crate | Engines | Client |
|---|---|---|
| `ninesql-driver-pgwire` | PostgreSQL, TimescaleDB, YugabyteDB (YSQL), QuestDB | `tokio-postgres` + `tokio-postgres-rustls` |
| `ninesql-driver-mysqlwire` | MySQL, MariaDB, TiDB | `mysql_async` (rustls feature) |
| `ninesql-driver-sqlite` | SQLite | `rusqlite` (bundled) |
| `ninesql-driver-duckdb` | DuckDB | `duckdb` (bundled) |
| `ninesql-driver-clickhouse` | ClickHouse | HTTP interface (`clickhouse` crate or `reqwest` + rustls) |
| `ninesql-driver-trino` | Trino | Trino client REST protocol over `reqwest` |
| `ninesql-driver-firebird` | Firebird | `rsfbclient` pure-Rust wire implementation |
| `ninesql-driver-influxdb` | InfluxDB 3 (Core) | SQL over the v3 HTTP API |
| `ninesql-driver-mongodb` | MongoDB | `mongodb` (official, Apache-2.0) |
| `ninesql-driver-redis` | Redis, Valkey | `redis` (redis-rs) |
| `ninesql-driver-search` | Elasticsearch, OpenSearch | REST over `reqwest` (SQL endpoints + query DSL) |
| `ninesql-driver-cassandra` | Apache Cassandra | `scylla` driver (CQL protocol) |
| `ninesql-driver-neo4j` | Neo4j (Community) | `neo4rs` (Bolt) |

The client choices are the starting point; the driver task (P1-B, P7, P8, P9) confirms each one (license, rustls support, maintenance, cancel support) and records any change in [docs/08-engine-capabilities.md](../08-engine-capabilities.md). Replacing a client does not need a new ADR; dropping an engine does.

### Delivery order
- **MVP (v0.1)**: pg-wire and mysql-wire families and SQLite: PostgreSQL, MySQL, MariaDB, SQLite, TimescaleDB, YugabyteDB, QuestDB, TiDB.
- **v0.2**: DuckDB, ClickHouse, Trino, Firebird, InfluxDB 3.
- **v0.3**: MongoDB, Redis, Valkey.
- **v0.4**: Elasticsearch, OpenSearch, Cassandra, Neo4j.

### Wire-compatible engines
A wire-compatible engine is a separate **engine id** with its own dialect and capability overrides, tested against its own container. "Speaks pg-wire" never means "supports everything PostgreSQL supports": QuestDB, for example, has no transactions in the PostgreSQL sense, and YugabyteDB lacks some catalog views.

## Consequences
- 13 driver crates instead of 20: shared code for wire-compatible engines, but a capability matrix per engine (I8).
- NoSQL engines need dedicated editors (document, key browser, graph results), not just the SQL grid. Feature parity for them is defined against DBeaver PRO's NoSQL support, as DBeaver Community does not cover them ([docs/07-feature-parity.md](../07-feature-parity.md)).
- All 20 engines have containers in `dev/` (ADR-0009). Heavy engines (Elasticsearch, OpenSearch, Cassandra, Neo4j, Trino, YugabyteDB) are in a separate compose profile.

## Rejected alternatives
- **JDBC bridge** (embedded JVM): instant breadth, but defeats the purpose (ADR-0001).
- **ODBC as the main path**: requires system drivers and their configuration, uneven quality on Linux. Kept as a possible post-v1 "generic" driver for engines outside the set.
- **One crate per engine**: duplicates protocol code across wire-compatible engines.
