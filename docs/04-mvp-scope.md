# MVP scope (v0.1.0)

The MVP is phases 0 to 6 of the [ROADMAP](ROADMAP.md). It is deliberately narrower than DBeaver: it covers the daily loop (connect, browse, query, read, edit, export) on 8 engines, plus the DBeaver import. Everything else in the [feature parity inventory](07-feature-parity.md) follows in phases 7 to 11.

## In scope
**Engines (8)**: PostgreSQL, MySQL, MariaDB, SQLite, TimescaleDB, YugabyteDB (YSQL), QuestDB, TiDB (the pg-wire, mysql-wire and SQLite families).

**Features**:
- Connections: manager, folders, types with production safety, keyring secrets with vault fallback, SSH tunnels with jump hosts, TLS, URL, properties, read-only (F-CON-01 to F-CON-06, F-CON-08 to F-CON-12).
- DBeaver import with report and secrets moved to the keyring (F-CON-14); export back to DBeaver format without passwords (F-CON-15).
- SQL editor: highlighting, completion, execute statement / selection / script, parameters, cancel, transactions, history, scripts, error positions, formatter, snippets, destructive-statement confirmation (F-SQL-01 to F-SQL-12, F-SQL-15, F-SQL-16, F-SQL-18).
- Results: virtualized grid with streaming and memory budget, multiple result tabs, record / text views, value viewers, sort / filter, in-cell editing with DML preview, FK navigation, aggregate and metadata panels, advanced copy (F-DATA-01 to F-DATA-07, F-DATA-09, F-DATA-10).
- Navigator: tree, all object kinds, filters, search, object editor (read), DDL and SQL generation (F-NAV-01 to F-NAV-07).
- Transfer: streamed export (CSV, TSV, JSON, SQL, Markdown, HTML) and CSV import (F-XFER-01, F-XFER-03).
- Workspace: tabs and splits, command palette, themes, preferences, session restore (F-WS-01 to F-WS-04, F-WS-06).
- Packaging: a cosign-signed `.deb` attached to the GitHub release (decided by the maintainer, 2026-11-19; see the note under the exit criterion). The signed APT repository and the Flatpak on Flathub follow after v0.1.0.

## Out of scope for the MVP
The other 12 engines; ER diagrams; visual explain plans; schema editing forms; admin tools; XLSX / Parquet export; cross-connection transfer; multiple projects. They are planned, not dropped.

## Targets to measure
These are targets for the reference machine (4-core x86-64 laptop, integrated GPU, 16 GB RAM, Ubuntu 24.04, Wayland). They become published measurements in P6-C; the README never states them as facts before that.

| Metric | Target |
|---|---|
| Cold start to interactive window | < 300 ms |
| Idle memory (RSS) with one open connection | < 150 MB |
| `SELECT` of 1 M rows: time to first rows (local PostgreSQL) | < 300 ms |
| Scrolling 1 M rows | no dropped frames at the display refresh rate |
| Memory with a truncated 1 M-row result | ≤ budget (256 MiB default) + 100 MB |
| Import of a 100-connection DBeaver workspace | < 2 s |

## Exit criterion
v0.1.0 can be released when all of these hold:
1. The exit criteria of phases 1 to 5 are met in CI or in a documented manual test.
2. On a fresh Ubuntu 24.04 and Debian 12, downloading the `.deb` from the GitHub release, checking its cosign signature, installing it, importing a DBeaver workspace and running a query over SSH against PostgreSQL takes under 10 minutes for a new user following the user guide.
3. No password is found under any NineSQL directory after the full test scenario (I1 test).
4. The measurements table above is filled with real numbers (whatever they are), with method and commit.
5. The end-of-MVP security review (P6-E) says "ready". Its verdict (2026-11-19) is "ready for v0.1.0 with conditions"; the conditions are listed in the [ROADMAP](ROADMAP.md#phase-6--hardening-and-v010-release) (P6-E row).
6. The final tests (P6-F) are done on Linux, with the tags and releases: a visual pass of the interface with screenshots on Wayland and X11, and a manual run of the exit scenarios of phases 1 to 5. Visual checks of earlier phases (P1-C) are deferred to it and do not block their phases.

**Note (2026-11-19, maintainer decision).** v0.1.0 is published as a cosign-signed `.deb` attached to the GitHub release, with its `SHA256SUMS` and CycloneDX SBOM ([RELEASE.md](../RELEASE.md#3-artifacts-from-v010), [SECURITY.md](../SECURITY.md#release-signing)). The signed APT repository and the Flatpak on Flathub come in a later release. Criterion 2 was amended accordingly: it used to read "installing the `.deb` from the signed APT repository". The rest of the criterion is unchanged.
