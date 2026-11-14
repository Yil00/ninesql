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
- ADR-0012 (Secret Service client and Rust 1.89 floor), ADR-0013 (GPUI source and supply-chain controls), ADR-0014 (dev profiles).
- Dependabot updates for the dev container images.
- `ninesql` application shell (P1-A): navigator dock, editor tabs and splits, status bar, multiple windows; 19 actions with default key bindings and a command palette; key-binding overrides, light / dark / system themes, fonts and zoom read from `settings.toml` at startup. Settings are not saved from the UI yet.
- SSH tunnels (`ninesql-ssh`, P1-D): russh with `ring`, jump hosts, `ssh-agent`, Ed25519 / ECDSA key files, `known_hosts` checks and per-hop host-key policies (ADR-0016). RSA key files are refused until RSA support is enabled. Not wired into the core yet.
- DBeaver workspace parser (`ninesql-dbeaver`, P2-A): read-only detection of native, Flatpak, Snap and custom workspaces; `data-sources*.json` model; `credentials-config.json` decryption into zeroized buffers; mapping to a mirror of the core connection model with fail-closed rules (network profiles, unknown or unreadable handlers, SSH forwarding target mismatch), per-project limits and a `verify-full` TLS default; cargo-fuzz targets for both decoders (not run yet). Import flow and persistence are not done.
- Driver API: execution-scoped cancel with a busy-until-confirmed rule, timeout watchdog, per-row and header budget accounting, document truncation and depth limit, `extra_reserved_properties`, `retains_secret_after_connect`, wider side-effect classification and SQL-form secret redaction, new conformance helpers.
- Workspace storage: single-writer lock `$XDG_DATA_HOME/ninesql/ninesql.lock`, immediate rewrite of files holding a plaintext secret, scrubbed migration backups, directory ownership and symlink checks; `project.json` v2 with a stable project UUID that scopes Secret Service items, with duplicate resolution (ADR-0017).
- GPUI snapshot audit log (`docs/audits/gpui-snapshot.md`).
- ADR-0015 (0BSD license allowed), ADR-0016 (SSH RSA keys and host-key policies); ADR-0017 (workspace lock, rewrite-on-load and project identity) accepted.

### Changed
- Minimum Rust version raised to 1.89 (ADR-0012).
- Feature parity inventory audited against DBeaver 26.2.4: 22 rows corrected, 14 added, 2 unverified.
