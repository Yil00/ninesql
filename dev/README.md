# Development environment

Test databases for NineSQL's drivers ([ADR-0009](../docs/adr/0009-container-first-test-environment.md)): one container per server engine of the v1 set ([ADR-0003](../docs/adr/0003-supported-engines-v1.md)), each loaded with the fake "nine" sample dataset.

| File | Content |
|---|---|
| [`compose.yaml`](compose.yaml) | One service per server engine, plus one-shot `<engine>-seed` services for images without an init directory |
| [`engines.json`](engines.json) | Source of truth for driver integration tests: engine id, service, host port, image, credentials, seed files, readiness |
| [`ready.py`](ready.py) | Starts a profile and waits until every engine is healthy and seeded; `check` validates `engines.json` against `compose.yaml` |
| [`seed/`](seed/) | The "nine" dataset, one directory per engine (or per family when the SQL is shared) |

## Start and stop
From the repository root (Docker Engine with the Compose v2 plugin, Python 3.9+):

```bash
python3 dev/ready.py up core     # PostgreSQL, MySQL, MariaDB
python3 dev/ready.py up sql      # core + TimescaleDB, YugabyteDB, QuestDB, TiDB, ClickHouse, Trino, Firebird, InfluxDB 3
python3 dev/ready.py up nosql    # MongoDB, Redis, Valkey
python3 dev/ready.py up heavy    # Elasticsearch, OpenSearch, Cassandra, Neo4j (about 8 GB of RAM)
python3 dev/ready.py up all      # everything

python3 dev/ready.py down        # = docker compose -f dev/compose.yaml --profile all down -v
python3 dev/ready.py check       # engines.json <-> compose.yaml consistency, no container started
```

`ready.py up` runs `docker compose --profile <p> up -d`, then waits (default 15 min, `--timeout`) until every engine container is **healthy** and every seed container has **exited with 0**. On failure it prints the last log lines of the failing service and exits non-zero. Plain Compose also works (`docker compose -f dev/compose.yaml --profile core up -d --wait`): the healthchecks query the dataset's `nine_meta` marker, so "healthy" means "seeded". But `--wait` does not report a failed seed container, so prefer `ready.py`.

Seeding happens on the first start of a container. It takes a few seconds (PostgreSQL, ClickHouse) to a few minutes (YugabyteDB, Cassandra, Elasticsearch). Data lives in anonymous volumes: `down -v` deletes it, `stop` / `start` keeps it. Trino's `memory` catalog is rebuilt by `trino-seed` on every `up`.

To run a single engine: `docker compose -f dev/compose.yaml --profile sql up -d tidb tidb-seed`.

Resources: the `sql` profile needs about 6 GB of RAM and 12 GB of disk for images. YugabyteDB refuses writes when the disk is nearly full (a few GB free), and the heavy JVM engines have fixed 1 GB heaps.

## Engines, ports and dev credentials
Every port is bound to `127.0.0.1`. Host ports are remapped so they do not clash with locally installed servers.

> **These credentials are fake, dev-only and public.** They protect nothing. Never reuse them for anything else, and never point NineSQL tests at a real server.

| Engine id | Profile | Host port(s) | User | Password / token | Database |
|---|---|---|---|---|---|
| `postgresql` | core | 15432 | `nine` | `nine_dev_only` | `nine` |
| `mysql` | core | 13306 | `nine` (root: `nine_dev_only_root`) | `nine_dev_only` | `nine` |
| `mariadb` | core | 13307 | `nine` (root: `nine_dev_only_root`) | `nine_dev_only` | `nine` |
| `sqlite` | core | file | — | — | `seed/sqlite/nine.sql` applied to a temporary file |
| `timescaledb` | sql | 15433 | `nine` | `nine_dev_only` | `nine` |
| `yugabytedb` | sql | 15434 (YSQL) | `nine` | `nine_dev_only` | `nine` |
| `questdb` | sql | 18812 (pg-wire), 19000 (HTTP console, no auth) | `nine`; read-only `nine_ro` | `nine_dev_only` | `qdb` |
| `tidb` | sql | 14000, 14080 (status) | `nine` (root: `nine_dev_only_root`) | `nine_dev_only` | `nine` |
| `duckdb` | sql | file | — | — | `seed/duckdb/nine.sql` applied to a temporary file |
| `clickhouse` | sql | 18123 (HTTP) | `nine` | `nine_dev_only` | `nine` |
| `trino` | sql | 18080 | any (no authentication over plain HTTP) | — | catalog `memory`, schema `nine`; catalog `tpch` |
| `firebird` | sql | 13050 | `nine` (SYSDBA: `nine_dev_only_root`) | `nine_dev_only` | `/var/lib/firebird/data/nine.fdb` |
| `influxdb` | sql | 18181 | — | token `apiv3_nine-dev-only-token` | `nine` |
| `mongodb` | nosql | 27117 | `nine` (root, `authSource=admin`) | `nine_dev_only` | `nine` |
| `redis` | nosql | 16379 | `default`; read-only ACL user `nine_ro` | `nine_dev_only` | 0 |
| `valkey` | nosql | 16380 | `default`; read-only ACL user `nine_ro` | `nine_dev_only` | 0 |
| `elasticsearch` | heavy | 19200 (HTTP, basic auth, no TLS) | `elastic` | `nine_dev_only` | — |
| `opensearch` | heavy | 19201 (HTTP, security plugin disabled) | — | — | — |
| `cassandra` | heavy | 19042 | none (`AllowAllAuthenticator`) | — | keyspace `nine` |
| `neo4j` | heavy | 17687 (Bolt), 17474 (HTTP) | `neo4j` | `nine_dev_only` | `neo4j` |

TLS is off everywhere in this environment. TLS and certificate-verification tests (ADR-0011) will add dedicated services when the drivers need them. The read-only users (`nine_ro`) are for engines whose read-only mode is client-side or user-based ([docs/08](../docs/08-engine-capabilities.md#read-only-mechanism-adr-0007-i3)). Session-level read-only tests on the other engines use the normal user.

## The "nine" dataset
Fake data only: names like `Customer 42`, e-mails at the reserved `example.com` domain, documentation IP ranges. It is generated **inside the engine** (`generate_series`, `numbers()`, `long_sequence()`, `range()`, recursive CTEs, Lua, `UNWIND`). Engines without a generator (Elasticsearch, OpenSearch, Cassandra, InfluxDB) get it from `awk` in the seed container at startup. Nothing large is committed.

| Object | Rows | Content |
|---|---|---|
| `customers`, `products`, `orders` | 1 000 / 100 / 10 000 | Relational sample with foreign keys and an index (documents, hashes, a stream or a graph on NoSQL engines) |
| `all_types` | 3 | One column per common type of the engine: typical values, edge values (min / max, empty, Unicode), all NULL |
| `big_events` | 1 000 000 | `id`, `event_time`, `kind`, `value`, `payload` (NULL every 10th row), for streaming, paging and memory-budget tests |
| `customer_order_totals` | view | Where the engine has views |
| `nine_meta` | 2 | Seed marker (`seed_version`, `big_events_rows`); readiness checks look for it |

Engine-specific extras:
- TimescaleDB: a hypertable and a continuous aggregate (`sensor_readings`).
- QuestDB: designated timestamps and partitions.
- Redis / Valkey: one key of every data type (`nine:type:*`) and 1 000 000 string keys (`nine:big:*`).
- Neo4j: `Event` nodes, and `PLACED` / `CONTAINS` relationships.

## Without Docker (some cloud agent sandboxes)
When Docker is not available ([docs/09-agent-workflow.md](../docs/09-agent-workflow.md)):
- Work on unit tests and the file engines. SQLite and DuckDB need no server: apply `seed/sqlite/nine.sql` or `seed/duckdb/nine.sql` to a temporary database file in the test.
- Server-engine integration tests skip themselves. CI runs them (`.github/workflows/dev-env.yml`, and the driver jobs that set `NINESQL_REQUIRE_ENGINES`).
- Say in the task report that the CI-only path was used.

When Docker works but Docker Hub rate-limits anonymous pulls, log in (`docker login`) or pull through a registry mirror. The digests stay the same.

## Integration test switches
- Tests read host, port and credentials from `engines.json` and skip an unreachable engine by default.
- `NINESQL_REQUIRE_ENGINES=postgresql,mysql` turns skips into failures for the listed engines (CI sets it per job).

## Rules
- Official images, pinned by digest (`name:tag@sha256:…`), and every port bound to `127.0.0.1`. `ready.py check` enforces both and runs in CI.
- To bump an image, change the tag and digest in both `compose.yaml` and `engines.json`. Use the multi-arch index digest from `docker buildx imagetools inspect <name:tag>`. Then run `ready.py check` and bring the profile up.
- Dependabot proposes digest bumps weekly (`docker-compose` ecosystem on `/dev`, [`.github/dependabot.yml`](../.github/dependabot.yml)), but it only edits `compose.yaml`. Copy the new tag and digest to `engines.json` in the same PR: `python3 dev/ready.py check` (run by the dev-env workflow) fails while the two files disagree.
- Fake data only. Never commit dumps, real hosts or real credentials.
