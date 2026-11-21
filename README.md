<br />
<div align="center">
  <img src="docs/assets/logo.svg" alt="NineSQL" width="120" height="120">

<h3 align="center">NineSQL</h3>

  <p align="center">
  <i>A native, GPU-rendered database client for Debian and Ubuntu. Fast, secure by default, and able to import your DBeaver workspace.</i>
  </p>

[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)
[![Status: paused](https://img.shields.io/badge/status-paused-lightgrey.svg)](#project-status-paused)
[![Stage: pre-alpha](https://img.shields.io/badge/stage-pre--alpha-orange.svg)](docs/ROADMAP.md)
[![Platform](https://img.shields.io/badge/platform-Debian%20%7C%20Ubuntu-lightgrey)](docs/10-supported-platforms.md)
[![Rust](https://img.shields.io/badge/Rust-stable-000000?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![GPUI](https://img.shields.io/badge/UI-GPUI-4b5dff)](https://github.com/zed-industries/zed/tree/main/crates/gpui)

[![PostgreSQL](https://img.shields.io/badge/PostgreSQL-336791?logo=postgresql&logoColor=white)](https://www.postgresql.org/)
[![MySQL](https://img.shields.io/badge/MySQL-4479A1?logo=mysql&logoColor=white)](https://www.mysql.com/)
[![MariaDB](https://img.shields.io/badge/MariaDB-003545?logo=mariadb&logoColor=white)](https://mariadb.org/)
[![SQLite](https://img.shields.io/badge/SQLite-003B57?logo=sqlite&logoColor=white)](https://sqlite.org/)
[![MongoDB](https://img.shields.io/badge/MongoDB-13aa52?logo=mongodb&logoColor=white)](https://www.mongodb.com/)
[![Redis](https://img.shields.io/badge/Redis-DC382D?logo=redis&logoColor=white)](https://redis.io/)
[![Valkey](https://img.shields.io/badge/Valkey-6284fc)](https://valkey.io/)
[![ClickHouse](https://img.shields.io/badge/ClickHouse-FFCC01?logo=clickhouse&logoColor=black)](https://clickhouse.com/)
[![DuckDB](https://img.shields.io/badge/DuckDB-FFF000?logo=duckdb&logoColor=black)](https://duckdb.org/)
[![+11 more](https://img.shields.io/badge/+11%20engines-see%20matrix-lightgrey)](docs/08-engine-capabilities.md)

  <p>
    <strong>
        <a href="docs/01-overview.md">Overview</a> •
        <a href="docs/ROADMAP.md">Roadmap</a> •
        <a href="docs/07-feature-parity.md">Feature parity</a> •
        <a href="docs/06-dbeaver-interop.md">DBeaver import</a> •
        <a href="https://github.com/Yil00/ninesql/issues/new?template=bug_report.md">Report Bug</a> •
        <a href="https://github.com/Yil00/ninesql/issues/new?template=feature_request.md">Request Feature</a>
    </strong>
  </p>
</div>

> ⚠️ **Pre-alpha.** Nothing is released yet. A development build can create, test and open connections to PostgreSQL, MySQL, MariaDB and SQLite (directly, over TLS and over SSH), and to TimescaleDB, YugabyteDB, QuestDB and TiDB with per-engine limits, from the connection dialog, and import connections from a DBeaver workspace from the UI (preview, saved passwords moved to the vault, re-import; [tutorial](docs/tutorials/import-from-dbeaver.md)), and run SQL in an editor with a results grid (statements, selections and scripts, parameters, production confirmations, cancel, commit and roll back; [tutorial](docs/tutorials/run-sql.md)). It can also browse schemas and objects in the navigator, with an object editor, DDL, SQL templates and object completion ([tutorial](docs/tutorials/browse-database.md)). It can edit the rows of a table with a key, with a preview of the generated SQL and a confirmation on production ([tutorial](docs/tutorials/edit-data.md)), export results and tables to files, import CSV files, and export connections to a new DBeaver file without passwords ([tutorial](docs/tutorials/export-import.md)); cells are edited through a dialog, not in place. It saves and reopens script files and restores them after a restart, without ever writing unsaved editor text. The interface, navigator, editor and grid included, awaits its visual check and its performance measurements on real desktops, planned with the final MVP tests (P6-F). Saved passwords go to the encrypted vault: the keyring backend is off by default until it is validated on GNOME and KDE. The [user guide](docs/user-guide/README.md) describes what works today and its [known limitations](docs/user-guide/known-limitations.md); see the [roadmap](docs/ROADMAP.md) for the MVP plan.

## Project status: paused

**Development is paused since 2026-11-21.** Nothing is released yet. The code on `dev` is consistent, CI is green, and every accepted decision is recorded (ADR-0001 to ADR-0029). The project can resume from here at any time.

**Where it stopped:** the MVP (phases 0 to 5) is built and tested in CI against the 8 MVP engines. Phase 6 (hardening and v0.1.0) is in progress: packaging, documentation, the end-of-MVP security review, application logging and the automated UI end-to-end tests (`make e2e-local` with its scenarios) have landed. [CONTEXT.md](CONTEXT.md) gives the full state; [docs/ROADMAP.md](docs/ROADMAP.md) the plan.

**To finish v0.1.0**, in this order:
1. Run `make e2e-local E2E_ARGS="--screenshot"` on a Linux desktop (it has not run on a real display yet) and fix what it reports ([tutorial](docs/tutorials/local-e2e.md)).
2. Release settings (P6-B): private vulnerability reporting, a tag ruleset on `refs/tags/v*`, the `release` environment with a required reviewer, the `.deb` `Maintainer` address, then a full `.deb` rebuild and the Debian 12 / 13 install tests.
3. A rehearsal with the tag `v0.1.0-rc.1` (first run of `release.yml`), then the version bump to 0.1.0 in the release PR `dev` → `main` ([RELEASE.md](RELEASE.md)).
4. The visual review of the scenario screenshots and the manual checks they do not cover (P6-F), the GPU measurements (P6-C) and the Orca accessibility test.
5. The tag `v0.1.0`: a cosign-signed `.deb` on the GitHub release (APT repository and Flathub later).

**Open decision:** the dated note on [ADR-0029](docs/adr/0029-automated-ui-end-to-end-tests-and-application-logging.md) (implementation choices of the test build) is proposed and awaits the maintainer.

**Development items ready to pick up** (none blocks v0.1.0): sturdier scenarios (dialog fields targeted directly instead of by Tab counts; the DBeaver import applied; production confirmations, "Insert row" and script restore in the scenarios); production confirmations for a trivial `WHERE` (`WHERE 1=1`) and `MERGE … WHEN MATCHED THEN DELETE`; the ADR-0016 re-evaluation against russh 0.65.2; the completion snapshot rebuilt off the UI thread and a per-load byte cap in the metadata service; the remaining items of the [known limitations](docs/user-guide/known-limitations.md).

## Why NineSQL
DBeaver is the de facto universal database client, but it runs on the JVM and Eclipse RCP: high memory use, slow cold start, and uneven HiDPI / Wayland behavior on Linux.

NineSQL keeps the **feature set** DBeaver users rely on and rebuilds it as a **native Rust application** rendered on the GPU with [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui):
- **Native and lean**: no JVM, no webview. Startup time and memory use will be measured and published with the method, not promised; no numbers exist yet ([docs/04-mvp-scope.md](docs/04-mvp-scope.md#exit-criterion)).
- **Large results in bounded memory**: a virtualized, GPU-rendered grid streams results under a memory budget and builds only the visible rows; its frame times will be measured on real hardware and published with the method, not promised.
- **Secure by default**: passwords live in the desktop keyring (Secret Service) or an encrypted local vault, never in config files; TLS verified by default; production connections guarded by a real read-only mode, not just a red banner ([docs/05-security.md](docs/05-security.md)).
- **Offline-first**: no telemetry, no account, no network traffic except to the databases you declare.
- **Switch in one click**: imports DBeaver connections, folders, connection types, SSH tunnels and saved passwords ([docs/06-dbeaver-interop.md](docs/06-dbeaver-interop.md)).

## Supported databases
The 20 most used **open-source, self-hostable** engines. Cloud-only services (Snowflake, BigQuery…) and proprietary engines (Oracle, SQL Server, DB2…) are out of scope for v1 ([ADR-0003](docs/adr/0003-supported-engines-v1.md)).

| Engine | Family | Planned in | Status |
|:--|:--|:--|:--|
| **PostgreSQL** | SQL · pg-wire | MVP (v0.1) | 🚧 Driver landed (P1-B); connect from the connection dialog (P1-C) or import from DBeaver (P2); run SQL in the editor and read results in the grid (P3); browse objects and DDL in the navigator (P4); edit rows, export and import CSV (P5, closed 2026-11-18) |
| **MySQL** | SQL · mysql-wire | MVP (v0.1) | 🚧 Driver landed (P1-B); connect from the connection dialog (P1-C) or import from DBeaver (P2); run SQL in the editor and read results in the grid (P3); browse objects and DDL in the navigator (P4); edit rows, export and import CSV (P5, closed 2026-11-18) |
| **MariaDB** | SQL · mysql-wire | MVP (v0.1) | 🚧 Driver landed (P1-B); connect from the connection dialog (P1-C) or import from DBeaver (P2); run SQL in the editor and read results in the grid (P3); browse objects and DDL in the navigator (P4); edit rows, export and import CSV (P5, closed 2026-11-18; CSV import engine-tested since 2026-11-18) |
| **SQLite** | SQL · embedded | MVP (v0.1) | 🚧 Driver landed (P1-B); connect from the connection dialog (P1-C) or import from DBeaver (P2); run SQL in the editor and read results in the grid (P3); browse objects and DDL in the navigator (P4); edit rows, export and import CSV (P5, closed 2026-11-18) |
| **TimescaleDB** | SQL · pg-wire | MVP (v0.1) | 🚧 Served by the pgwire driver (P4-C): connect (the `timescaledb` extension is required), run SQL, browse hypertables and continuous aggregates (chunks hidden); read-only as PostgreSQL; TLS integration-tested against a dev test CA; row edits and exports as PostgreSQL (P5: generated SQL engine-tested in the driver; editing, export and CSV import not engine-tested in the core) |
| **YugabyteDB** (YSQL) | SQL · pg-wire | MVP (v0.1) | 🚧 Served by the pgwire driver (P4-C): connect, run SQL, browse as PostgreSQL; read-only as PostgreSQL; TLS integration-tested against a dev test CA; row edits, export and CSV import as PostgreSQL (P5: generated SQL, editing, the 1 M-row export and the 100 000-line import engine-tested since 2026-11-18) |
| **QuestDB** | SQL · pg-wire | MVP (v0.1) | 🚧 Served by the pgwire driver (P4-C): run SQL and browse tables, views and partitions; no TLS, so the password needs a strict SSH tunnel, ideally to the QuestDB host itself; no transactions; read-only only through QuestDB's read-only user or a read-only server (open-source QuestDB tested); no row editing (P5); exported to DBeaver as PostgreSQL |
| **TiDB** | SQL · mysql-wire | MVP (v0.1) | 🚧 Served by the mysqlwire driver (P4-C): connect, run SQL, browse; no TLS yet (plaintext); read-only only on a read-only TiDB instance, for an account without `RESTRICTED_REPLICA_WRITER_ADMIN`, `SUPER` or `ALL PRIVILEGES ON *.*` (so not `root`); row edits and exports as MySQL, but exports run without an engine-side read-only transaction (TiDB has none: refused on production unless the connection is read-only, warned otherwise) (P5: generated SQL and exports engine-tested; editing and import not engine-tested) |
| **DuckDB** | SQL · embedded | v0.2 | 🗓 Planned |
| **ClickHouse** | SQL · HTTP | v0.2 | 🗓 Planned |
| **Trino** | SQL · HTTP | v0.2 | 🗓 Planned |
| **Firebird** | SQL · native wire | v0.2 | 🗓 Planned |
| **InfluxDB 3** | SQL · HTTP | v0.2 | 🗓 Planned |
| **MongoDB** | Document | v0.3 | 🗓 Planned |
| **Redis** | Key-value | v0.3 | 🗓 Planned |
| **Valkey** | Key-value | v0.3 | 🗓 Planned |
| **Elasticsearch** | Search | v0.4 | 🗓 Planned |
| **OpenSearch** | Search | v0.4 | 🗓 Planned |
| **Apache Cassandra** | Wide-column · CQL | v0.4 | 🗓 Planned |
| **Neo4j** | Graph · Bolt | v0.4 | 🗓 Planned |

What each engine supports (metadata, editing, explain plans…) is listed in the [capability matrix](docs/08-engine-capabilities.md).

## Installation
No release yet. The v0.1.0 MVP will ship as a **`.deb`** for Debian 12 / 13 and Ubuntu 22.04 / 24.04 (amd64), attached to the GitHub release with its SBOM and cosign signatures ([verification](SECURITY.md#release-signing)). A signed APT repository and a Flatpak on Flathub come in a later release.

There is deliberately no `curl … | bash` installer ([ADR-0010](docs/adr/0010-packaging-and-distribution.md)).

## Development
```bash
# Rust stable, 1.89 or newer (see rust-toolchain.toml)
make help                 # list the targets of the root Makefile
make build                # = cargo build --workspace --locked
make run                  # start the ninesql window
make check                # what CI checks, without database engines
```
The [user guide](docs/user-guide/README.md) explains each part of the application for end users (install, secrets, DBeaver import, connections, the SQL editor, results and editing, the navigator, the safety model, keyboard shortcuts, troubleshooting, known limitations). Step-by-step guides are in [docs/tutorials/](docs/tutorials/README.md): build and run from source (Linux, and macOS for UI work), start the dev databases, create a first connection, run SQL and read results, browse a database, edit data, export and import, import from DBeaver, secrets, and the checks to run before a PR. The test databases run in containers: see [dev/README.md](dev/README.md). AI coding agents (Claude Code, Codex…) start with [AGENTS.md](AGENTS.md) and [CONTEXT.md](CONTEXT.md).

## Contributing
Contributions are welcome: see [CONTRIBUTING.md](CONTRIBUTING.md). PRs target the `dev` branch.
Releases: [RELEASE.md](RELEASE.md) · [CHANGELOG.md](CHANGELOG.md).
Vulnerabilities: **do not open a public issue**, see [SECURITY.md](SECURITY.md).

## License
NineSQL is distributed under the [Apache 2.0 license](LICENSE). The "NineSQL" name and logo are not covered by this license: see [TRADEMARKS.md](TRADEMARKS.md).

NineSQL is an independent project. It is not affiliated with or endorsed by DBeaver Corp; "DBeaver" is used only to describe interoperability.
