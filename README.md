<br />
<div align="center">
  <img src="docs/assets/logo.svg" alt="NineSQL" width="120" height="120">

<h3 align="center">NineSQL</h3>

  <p align="center">
  <i>A native, GPU-rendered database client for Debian and Ubuntu. Fast, secure by default, and able to import your DBeaver workspace.</i>
  </p>

[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)
[![Status: pre-alpha](https://img.shields.io/badge/status-pre--alpha-orange.svg)](docs/ROADMAP.md)
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

> ⚠️ **Pre-alpha.** Nothing is released yet and nothing is usable end to end: the application shell, the PostgreSQL driver, SSH tunnels, the secret stores and the DBeaver parser exist as crates, but there is no connection UI yet. See the [roadmap](docs/ROADMAP.md) for the MVP plan.

## Why NineSQL
DBeaver is the de facto universal database client, but it runs on the JVM and Eclipse RCP: high memory use, slow cold start, and uneven HiDPI / Wayland behavior on Linux.

NineSQL keeps the **feature set** DBeaver users rely on and rebuilds it as a **native Rust application** rendered on the GPU with [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui):
- **Native and lean**: no JVM, no webview. Startup time and memory use are measured in CI and published, not promised ([docs/04-mvp-scope.md](docs/04-mvp-scope.md#exit-criterion)).
- **Large results stay fluid**: a virtualized, GPU-rendered grid streams results in bounded memory.
- **Secure by default**: passwords live in the desktop keyring (Secret Service) or an encrypted local vault, never in config files; TLS verified by default; production connections guarded by a real read-only mode, not just a red banner ([docs/05-security.md](docs/05-security.md)).
- **Offline-first**: no telemetry, no account, no network traffic except to the databases you declare.
- **Switch in one click**: imports DBeaver connections, folders, connection types, SSH tunnels and saved passwords ([docs/06-dbeaver-interop.md](docs/06-dbeaver-interop.md)).

## Supported databases
The 20 most used **open-source, self-hostable** engines. Cloud-only services (Snowflake, BigQuery…) and proprietary engines (Oracle, SQL Server, DB2…) are out of scope for v1 ([ADR-0003](docs/adr/0003-supported-engines-v1.md)).

| Engine | Family | Planned in | Status |
|:--|:--|:--|:--|
| **PostgreSQL** | SQL · pg-wire | MVP (v0.1) | 🚧 Driver landed (P1-B), not usable from the UI yet |
| **MySQL** | SQL · mysql-wire | MVP (v0.1) | 🗓 Planned |
| **MariaDB** | SQL · mysql-wire | MVP (v0.1) | 🗓 Planned |
| **SQLite** | SQL · embedded | MVP (v0.1) | 🗓 Planned |
| **TimescaleDB** | SQL · pg-wire | MVP (v0.1) | 🗓 Planned |
| **YugabyteDB** (YSQL) | SQL · pg-wire | MVP (v0.1) | 🗓 Planned |
| **QuestDB** | SQL · pg-wire | MVP (v0.1) | 🗓 Planned |
| **TiDB** | SQL · mysql-wire | MVP (v0.1) | 🗓 Planned |
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
No release yet. The v0.1.0 MVP will ship as:
- a signed **`.deb`** for Debian 12 / 13 and Ubuntu 22.04 / 24.04, from a signed APT repository;
- a **Flatpak** (Flathub).

There is deliberately no `curl … | bash` installer ([ADR-0010](docs/adr/0010-packaging-and-distribution.md)).

## Development
```bash
# Rust stable, 1.89 or newer (see rust-toolchain.toml)
cargo build --workspace
cargo test --workspace
```
The test databases run in containers: see [dev/README.md](dev/README.md). AI coding agents (Claude Code, Codex…) start with [AGENTS.md](AGENTS.md) and [CONTEXT.md](CONTEXT.md).

## Contributing
Contributions are welcome: see [CONTRIBUTING.md](CONTRIBUTING.md). PRs target the `dev` branch.
Releases: [RELEASE.md](RELEASE.md) · [CHANGELOG.md](CHANGELOG.md).
Vulnerabilities: **do not open a public issue**, see [SECURITY.md](SECURITY.md).

## License
NineSQL is distributed under the [Apache 2.0 license](LICENSE). The "NineSQL" name and logo are not covered by this license: see [TRADEMARKS.md](TRADEMARKS.md).

NineSQL is an independent project. It is not affiliated with or endorsed by DBeaver Corp; "DBeaver" is used only to describe interoperability.
