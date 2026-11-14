# ADR-0002: Licensing and dependency policy

- **Status**: Accepted. Partially superseded by [ADR-0013](0013-gpui-source-and-supply-chain.md) for Decision §6 (git source allow-list: only sources actually used; GPUI is no longer a git dependency).
- **Date**: 2026-11-14

## Context
NineSQL is Apache 2.0 (I7). A database client pulls many dependencies: GPU stack, TLS, SSH, one client per engine. Some engines' servers are under copyleft or source-available licenses (Neo4j Community: GPL-3.0; MongoDB: SSPL; Elasticsearch: AGPL / ELv2 / SSPL; Redis 8: AGPL / RSAL / SSPL). Their **client libraries** are usually permissive.

## Decision
1. NineSQL is licensed **Apache-2.0**. Contributions are accepted under the DCO, without a CLA.
2. Every crate linked into the shipped binary must be under a license in the `deny.toml` allow-list: Apache-2.0, MIT, BSD-2/3-Clause, ISC, Zlib, Unicode-3.0, CC0-1.0, MPL-2.0 (file-level copyleft, unmodified use only). Anything else needs a new ADR.
3. **Linking** is what matters: a database **server** under GPL / AGPL / SSPL used in a test container is not linked and is allowed in `dev/` and CI. Its **client** crate must pass rule 2.
4. Explicitly banned in `deny.toml`: Zed's GPL crates, `openssl`, `openssl-sys`, `native-tls` (rustls only), webview crates.
5. C libraries bundled through a crate (SQLite, DuckDB, zstd…) are allowed when their license passes rule 2; their notices go in `NOTICE` / the `.deb` copyright file.
6. `cargo deny check licenses bans sources` runs in the required CI. Git dependencies are allowed only from `github.com/zed-industries/zed` and `github.com/longbridge/gpui-component` (ADR-0001).

## Consequences
- An engine whose only Rust client has an incompatible license cannot be supported until a compatible client exists or we write one.
- The `.deb` and Flatpak ship a generated third-party license file (`cargo about` or equivalent, P6).
- Reading DBeaver's file formats is interoperability with an Apache 2.0 project; we do not copy DBeaver code (ADR-0006).

## Rejected alternatives
- **GPL-3.0 for NineSQL** (to reuse Zed's editor): closes the door to integrations and goes against the project's intent.
- **Allowing LGPL crates**: static linking of Rust crates makes LGPL compliance awkward; avoided.
