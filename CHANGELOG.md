# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses [Semantic Versioning](https://semver.org/). Released sections are generated at release time from Conventional Commits ([RELEASE.md](RELEASE.md)).

## [Unreleased]
### Added
- Repository bootstrap: documentation, ADR-0001 to ADR-0011, ROADMAP, agent definitions, CI, Rust workspace skeleton.
- GPUI spike (`crates/ninesql-ui/examples/spike.rs`): dock, tabbed SQL editors with tree-sitter highlighting, virtual table of 1 000 000 rows; go / no-go note in `docs/spikes/p0-a-gpui.md`. GPUI and `gpui-component` come from crates.io through an exact pin of `gpui-kit` 0.9.1, with a snapshot verification script (`scripts/verify-gpui-snapshot.sh`) and `cargo deny` bans (ADR-0013).
- Driver API (`ninesql-driver-api`): driver, session, metadata, dialect and capability traits, connection parameters with secret handles, Arrow result streams under a fetch budget, engine-side read-only rules, a mock driver and conformance helpers. Reviewed and frozen.
- Dev environment (`dev/`): the 20 engines in containers pinned by digest, bound to `127.0.0.1`, in `core` / `sql` / `nosql` / `heavy` / `all` profiles, with the fake "nine" dataset, `engines.json`, `ready.py` and a CI workflow.
- Workspace storage (`ninesql-storage`), connection model and workspace service (`ninesql-core`), secret store trait with an in-memory store and a Secret Service backend on `secret-service` 6 (`ninesql-secrets`, ADR-0012).
- Synthetic DBeaver workspace fixtures written by five DBeaver CE versions (22.3.5 to 26.2.4), with tests that decrypt and audit their fake credentials.
- ADR-0012 (Secret Service client and Rust 1.89 floor), ADR-0013 (GPUI source and supply-chain controls); ADR-0014 (dev profiles) proposed.
- Dependabot updates for the dev container images.

### Changed
- Minimum Rust version raised to 1.89 (ADR-0012).
- Feature parity inventory audited against DBeaver 26.2.4: 22 rows corrected, 14 added, 2 unverified.
