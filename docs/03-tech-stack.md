# Tech stack

Starting choices. Driver client crates are confirmed (or replaced) by their driver task and recorded in [docs/08-engine-capabilities.md](08-engine-capabilities.md). Every crate must pass the license policy of [ADR-0002](adr/0002-licensing-and-dependency-policy.md).

| Area | Choice | Notes |
|---|---|---|
| Language | Rust stable, edition 2024, minimum 1.89 | `rust-toolchain.toml`; `rust-version` in the root `Cargo.toml` ([ADR-0012](adr/0012-secret-service-client-and-msrv.md)) |
| UI | GPUI and `gpui-component` from crates.io through `gpui-kit` (exact pin; GPUI is the `gpui-pre` snapshot of `zed-industries/zed`) | ADR-0001, [ADR-0013](adr/0013-gpui-source-and-supply-chain.md) (supply-chain controls); never Zed's GPL crates |
| Syntax | `tree-sitter` through gpui-component's code editor, with one SQL grammar (MIT `tree-sitter-sequel`) for every dialect so far; `lsp-types` and `anyhow` only at gpui-component's completion-provider boundary ([ADR-0025](adr/0025-phase-3-dialect-and-editor-decisions.md) §6, accepted) | Splitting, parameters and completion keywords follow the session dialect; object completion from our own metadata cache (phase 4) |
| Async runtime | `tokio` (multi-thread, bounded workers) for driver and network I/O | ADR-0004 |
| Results | `arrow-array`, `arrow-schema` (not the full `arrow` crate) | Tabular batches; documents as an ordered value tree |
| TLS | `rustls` 0.23 with the `ring` crypto provider, `tokio-rustls`, `rustls-native-certs` for the platform trust store | No OpenSSL, no `aws-lc-rs` (ADR-0011). `rustls-native-certs` pulls `openssl-probe`, which only locates the system CA files |
| SSH | `russh` 0.65 (`ring` feature) | Jump hosts, agent, `known_hosts`; RSA disabled ([ADR-0016](adr/0016-ssh-rsa-and-host-key-policies.md)) |
| Secrets | `secret-service` 6 (pure Rust, `zbus` 6, no `libsecret` / `libdbus` / OpenSSL; pinned `=6.0.0-rc.1` until 6.x is stable; feature off by default) instead of `keyring`; vault: `argon2` 0.6 (Argon2id) and `chacha20poly1305` 0.11 (XChaCha20-Poly1305), both with their `zeroize` feature; `secrecy`, `zeroize` | ADR-0005, [ADR-0012](adr/0012-secret-service-client-and-msrv.md) |
| Storage | `serde`, `serde_json`, `toml`, `toml_edit` (writes `settings.toml` back keeping comments and unknown keys), `directories` (XDG), `rusqlite` for history | ADR-0008 |
| HTTP (for HTTP-based engines) | `reqwest` with `rustls-tls`, no default features | ClickHouse, Trino, InfluxDB, Elasticsearch / OpenSearch |
| Export formats | `csv`, `serde_json`, `parquet` (phase 10), `rust_xlsxwriter` (phase 10) | Streamed |
| Errors / logs | `thiserror`, `tracing`, `tracing-subscriber`, `tracing-appender` | No values in logs |
| Testing | `proptest`, `insta` (snapshots without secrets), `cargo-fuzz` for parsers | |
| Supply chain | `cargo-deny` (licenses, bans, sources, advisories) | Required CI check |
| Packaging | `cargo-deb`, Flatpak, cosign, CycloneDX SBOM | ADR-0010 |

## Driver clients
Confirmed by a driver task when marked so; the others are the initial plan.

| Family | Crate |
|---|---|
| pg-wire (confirmed, P1-B) | `postgres-protocol` (message codec only) with `tokio` and `tokio-rustls`; SCRAM-SHA-256 and md5 implemented in the driver with `ring` and `md-5`. Not `tokio-postgres`: the driver needs `ParameterStatus` after every statement, full control of portals and cancel requests, and value cutting while rows are read ([docs/08](08-engine-capabilities.md#pgwire-postgresql-p1-b)) |
| mysql-wire (confirmed, P1-B) | No protocol crate: the client/server protocol (packets, handshake, text protocol) is implemented in the driver on top of `tokio` and `tokio-rustls`; `caching_sha2_password` and `mysql_native_password` scrambles with `ring`. Not `mysql_async`: it reads each row whole (no value cutting while rows are read), and its protocol crate `mysql_common` brings a large tree (compression, big-number and date crates) for features the driver does not use. The subset needed (one text-protocol statement per call, no prepared statements, no compression, no `LOAD DATA LOCAL`) is small, needs no new dependency and keeps control of `KILL QUERY` on a control connection, the access-mode read-back and the session's lexical modes ([docs/08](08-engine-capabilities.md#mysqlwire-mysql-and-mariadb-p1-b)) |
| SQLite (confirmed, P1-B) | `rusqlite` 0.42 (MIT) with the `bundled` feature: `libsqlite3-sys` (MIT) compiles the SQLite 3.54 amalgamation (public domain) into the binary, so no system library and no runtime download; features `column_decltype`, `hooks` (authorizer, progress handler) and `limits` only, default features off (no `hashlink` cache, no wasm backend), never `load_extension`. The C compiler of the build host is needed (`cc`); the `unsafe` code stays inside `rusqlite` / `libsqlite3-sys`. The connection runs on a dedicated thread per session ([docs/08](08-engine-capabilities.md#sqlite-sqlite-p1-b)) |
| DuckDB | `duckdb` (bundled) |
| ClickHouse | `clickhouse` or HTTP via `reqwest` |
| Trino | HTTP via `reqwest` (client protocol) |
| Firebird | `rsfbclient` (pure-Rust feature) |
| InfluxDB 3 | HTTP via `reqwest` |
| MongoDB | `mongodb` |
| Redis / Valkey | `redis` |
| Elasticsearch / OpenSearch | HTTP via `reqwest` |
| Cassandra | `scylla` |
| Neo4j | `neo4rs` |
