# Start the dev databases

NineSQL's drivers are tested against real engines running in containers, each loaded with the fake "nine" dataset. You can use the same containers to try NineSQL. The reference is [dev/README.md](../../dev/README.md); this page is the short path.

> The credentials below are **fake, dev-only and public**. Every port is bound to `127.0.0.1`. Never reuse these passwords, and never point the tests at a real server.

## 1. Requirements
- Linux: Docker Engine with the Compose v2 plugin (`docker compose version` must answer).
- macOS: Docker Desktop or OrbStack.
- Python 3.9 or newer (`dev/ready.py` uses only the standard library).
- RAM and disk: the `core` profile is light; the `sql` profile needs about 6 GB of RAM and 12 GB of disk for images; `heavy` needs about 8 GB of RAM.

## 2. Start the core engines
```bash
make dev-up               # = python3 dev/ready.py up core --timeout 900
```
This starts PostgreSQL, MySQL and MariaDB and waits until every container is healthy **and seeded** (seeding happens on the first start of a container). It prints one `ready` line per engine, then `profile 'core' ready`. On failure it prints the last log lines of the failing service and exits non-zero.

Other profiles:
```bash
make dev-up PROFILE=sql   # core + TimescaleDB, YugabyteDB, QuestDB, TiDB, ClickHouse, Trino, Firebird, InfluxDB 3
make dev-up PROFILE=nosql # MongoDB, Redis, Valkey
make dev-up PROFILE=heavy # Elasticsearch, OpenSearch, Cassandra, Neo4j
make dev-up-all           # everything
```
NineSQL can connect to PostgreSQL, MySQL, MariaDB, SQLite, TimescaleDB, YugabyteDB, QuestDB and TiDB today; the other engines are there for the drivers still to come ([ROADMAP](../ROADMAP.md)).

To start only some engines of a profile, list them with `--engines` (with `up` and `wait`; their seed containers come along, and an id outside the profile is an error). The driver integration tests use it for the four wire-compatible engines, which need about 2.3 GB of memory together:
```bash
python3 dev/ready.py up sql --engines timescaledb,yugabytedb,questdb,tidb --timeout 900
```

Slow machines or slow pulls: `make dev-up DEV_TIMEOUT=1800`.

## 3. Check what runs
```bash
make dev-ps               # = docker compose -f dev/compose.yaml --profile all ps --all
make dev-check            # engines.json against compose.yaml; starts nothing
```

## 4. Connection details
| Engine | Host | Port | User | Password | Database |
|---|---|---|---|---|---|
| PostgreSQL | `127.0.0.1` | 15432 | `nine` | `nine_dev_only` | `nine` |
| MySQL | `127.0.0.1` | 13306 | `nine` | `nine_dev_only` | `nine` |
| MariaDB | `127.0.0.1` | 13307 | `nine` | `nine_dev_only` | `nine` |
| SQLite | — | — | — | — | a file you create, see below |
| TimescaleDB | `127.0.0.1` | 15433 | `nine` | `nine_dev_only` | `nine` |
| YugabyteDB | `127.0.0.1` | 15434 | `nine` | `nine_dev_only` | `nine` |
| QuestDB | `127.0.0.1` | 18812 | `nine`; read-only user `nine_ro` | `nine_dev_only` | `qdb` |
| TiDB | `127.0.0.1` | 14000 | `nine` | `nine_dev_only` | `nine` |

The ports are remapped so they do not clash with servers installed on your machine. TimescaleDB, YugabyteDB, QuestDB and TiDB are in the `sql` profile. TimescaleDB and YugabyteDB also accept TLS with a throwaway test CA (for the certificate-verification tests); QuestDB and TiDB have no TLS. NineSQL refuses a direct connection to QuestDB (it only accepts a password sent in clear) and a read-only connection to the dev TiDB (the instance is writable); see [Create your first connection](first-connection.md#8-timescaledb-yugabytedb-questdb-and-tidb). The other engines are listed in [dev/README.md](../../dev/README.md#engines-ports-and-dev-credentials).

### TLS on the dev engines
PostgreSQL, MySQL and MariaDB also accept TLS. At their first start, each container generates a throwaway certificate authority (CA) and a server certificate for `localhost` and `127.0.0.1`; no key is committed or leaves the container. Your system does not trust that CA, so to try NineSQL's verified TLS modes, copy the public CA certificate to a file:
```bash
# PostgreSQL
docker compose -f dev/compose.yaml exec -T postgresql psql -U nine -d nine -Atc \
  "SELECT pg_read_file(current_setting('data_directory') || '/ninesql-tls/ca.crt')" > ~/nine-pg-ca.crt
# MySQL
docker compose -f dev/compose.yaml exec -T mysql mysql -unine -pnine_dev_only --raw -N -B \
  -e "SELECT ca_pem FROM nine.ninesql_tls" > ~/nine-mysql-ca.crt
# MariaDB
docker compose -f dev/compose.yaml exec -T mariadb mariadb -unine -pnine_dev_only --raw -N -B \
  -e "SELECT ca_pem FROM nine.ninesql_tls" > ~/nine-mariadb-ca.crt
```
A new CA is generated whenever a container is created again (after `make dev-down`), so copy it again then. How to use the file: [Create your first connection](first-connection.md#3-tls).

### A SQLite file
SQLite needs no container. With the `sqlite3` command-line tool installed:
```bash
sqlite3 ~/nine.db < dev/seed/sqlite/nine.sql
```
NineSQL opens an existing file by its absolute path; it does not create a missing one.

## 5. Stop
```bash
make dev-down             # = python3 dev/ready.py down
```
This stops every dev container **and deletes their data volumes** (`docker compose … down -v`); the next `make dev-up` seeds fresh data. To keep the data between sessions, use `docker compose -f dev/compose.yaml --profile core stop`, then `start`.

## Troubleshooting
- **Port already allocated**: another container or a previous run holds a port. `make dev-ps` shows it; `make dev-down` stops the NineSQL ones.
- **Docker Hub rate limit**: log in (`docker login`) or pull through a registry mirror; the image digests stay the same.
- **A heavy engine never gets healthy**: YugabyteDB and Elasticsearch stop working when the disk is nearly full. Free a few GB.

## Next
- [Create your first connection](first-connection.md) to one of these engines.
- [Browse a database](browse-database.md) in the navigator, on the "nine" dataset.
- [Run the checks before a PR](checks-before-a-pr.md): `make test-engines` runs the driver integration tests against the `core` profile and the four wire-compatible engines.
