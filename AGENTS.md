# AGENTS.md – rules for coding agents

This file is for every coding agent (Claude Code, Codex, Cursor…) and the humans who drive them. The domain context is in [CONTEXT.md](CONTEXT.md).

## Before each task
1. Read [CONTEXT.md](CONTEXT.md), especially the **invariants I1 to I8**.
2. Find the task in [docs/ROADMAP.md](docs/ROADMAP.md) (identifier `Px-Y`) and set it to `[~]`.
3. Read the related ADRs. **Never work around an accepted ADR**: if it blocks you, stop and propose a new ADR ([docs/adr/template.md](docs/adr/template.md)).
4. If the task implements a DBeaver feature, find its `F-…` id in [docs/07-feature-parity.md](docs/07-feature-parity.md) and quote it in the PR.

## Languages
- Everything in the repository is in **English**: documentation, code, identifiers, comments, commit messages, branch names.

## Tree and owners
| Path | Content | Main owner |
|--------|---------|------------------------|
| `crates/ninesql-ui/`, `crates/ninesql-grid/`, `crates/ninesql-editor/` | GPUI application, results grid, SQL editor | `ui-engineer` |
| `crates/ninesql-core/`, `crates/ninesql-storage/`, `crates/ninesql-transfer/` | Application state, workspace storage, import / export of data | `core-engineer` |
| `crates/ninesql-secrets/`, `crates/ninesql-ssh/` | Keyring / vault, SSH tunnels, TLS settings | `core-engineer`, `security-reviewer` review **required** |
| `crates/ninesql-driver-api/` | Driver traits, capability model, result types | `driver-engineer`, `security-reviewer` review **required** |
| `crates/drivers/` | One crate per driver family | `driver-engineer` |
| `crates/ninesql-dbeaver/` | DBeaver workspace import / export | `interop-engineer`, `security-reviewer` review **required** |
| `dev/` | Test databases in containers, seed data | `driver-engineer` |
| `packaging/` | `.deb`, APT repository, Flatpak | `core-engineer` |
| `docs/`, root `*.md`, `.github/` | Documentation, ROADMAP, ADRs, CI | `docs-keeper` |

An agent does **not** modify files belonging to another owner, unless the task explicitly requires it. If it needs to, it says so in its task report.

## Conventions – Rust (whole workspace)
- Stable Rust (`rust-toolchain.toml`), edition 2024, `#![forbid(unsafe_code)]` in every crate.
  - The only accepted exception is a crate that must call a C library (for example SQLite or DuckDB bundled builds): the `unsafe` stays inside the upstream crate, never in ours.
- `cargo fmt --all`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace` must pass.
- `cargo deny check` must pass (licenses, bans, sources). Lifting a ban needs a `security-reviewer` review, and an ADR when it touches an invariant.
- The GPUI version moves only in a dedicated `chore(ui): bump gpui` PR with the output of `scripts/verify-gpui-snapshot.sh` attached ([ADR-0013](docs/adr/0013-gpui-source-and-supply-chain.md)).
- TLS: **rustls only** (no OpenSSL, no `native-tls`).
- No `println!` / `eprintln!` in library crates: `tracing`. **Never** log a password, a query parameter value or a result value. Query text is logged only at `trace` level and never for connections typed "production".
- Errors: `thiserror` in libraries, typed and user-presentable; no `unwrap()` / `expect()` outside tests and `main` setup.
- Async: driver I/O runs on the driver runtime (Tokio), never on the GPUI foreground executor ([ADR-0004](docs/adr/0004-driver-api-runtime-and-results.md)).
- Every query has a timeout and a cancel path. Every result is fetched in batches under a memory budget (I6).

## Conventions – UI (`ninesql-ui`, `ninesql-grid`, `ninesql-editor`)
- GPUI pinned to the exact version in the workspace `Cargo.toml` ([ADR-0001](docs/adr/0001-native-stack-rust-gpui.md), [ADR-0013](docs/adr/0013-gpui-source-and-supply-chain.md)). Bumping it is its own PR.
- Never use GPUI's credentials API (`read_credentials` / `write_credentials`) or HTTP client API (`set_http_client` / `with_http_client`); secrets go only through `ninesql-secrets` ([ADR-0013](docs/adr/0013-gpui-source-and-supply-chain.md)).
- Never depend on Zed's GPL crates (`editor`, `language`, `project`, `workspace`…). `cargo deny` enforces it.
- Every user-facing action is a GPUI `Action` with a key binding and an entry in the command palette.
- UI code never formats a connection's password, even masked; it shows "saved in keyring" / "not saved".
- Screenshots for PRs: run locally (macOS works for UI iteration) and attach them; cloud sandboxes have no display ([docs/09-agent-workflow.md](docs/09-agent-workflow.md)).

## Conventions – Drivers (`crates/drivers/`)
- One crate per **family**, not per engine: `ninesql-driver-pgwire` serves PostgreSQL, TimescaleDB, YugabyteDB and QuestDB, with per-engine dialect and capability overrides ([ADR-0003](docs/adr/0003-supported-engines-v1.md)).
- A driver declares its capabilities; the UI reads them (I8). Unsupported means "not offered", not "fails at runtime".
- Read-only sessions are enforced engine-side (I3): see the per-engine mechanism in [docs/08-engine-capabilities.md](docs/08-engine-capabilities.md).
- Integration tests run against the containers of `dev/` and are skipped (not failed) when the engine is not reachable, unless `NINESQL_REQUIRE_ENGINES` lists it (the `driver-integration` job of `.github/workflows/ci.yml` sets it to `postgresql,mysql,mariadb,ssh,timescaledb,yugabytedb,questdb,tidb`, and `make test-engines` uses the same list). A run that brings up its own containers sets its own `COMPOSE_PROJECT_NAME`, so it never stops another run's containers; the host ports are fixed, so runs on one Docker host are serialised.

## Useful commands (root)
- `make help`: the root `Makefile` wraps the commands below and the dev-engine scripts (`make check`, `make test-engines`, `make dev-up`…)
- `cargo build --workspace` / `cargo test --workspace`
- `cargo deny check`
- `python3 scripts/check-md-links.py`: internal documentation links
- `pre-commit install --hook-type pre-commit --hook-type commit-msg`: gitleaks + commit format hooks

## Required tests
- Every crate: unit tests; parsers (DBeaver files, SQL splitting, connection URLs) get property tests or fuzz targets.
- Drivers: integration tests against `dev/` containers, including the read-only enforcement test (a write in a read-only session must fail engine-side).
- DBeaver import: tests on anonymized real-world fixtures in `crates/ninesql-dbeaver/tests/fixtures/` (never real hosts or passwords).
- Secrets: a test that no password reaches any file under the workspace directory after create / import / export flows (I1).
- UI: GPUI `TestAppContext` tests for view models and actions; no pixel tests in CI.

## Multi-agent work
- **One task = one branch = one worktree** (`git worktree`), created from `dev`, named `<type>/<roadmap-id>-<slug>` (e.g. `feat/p1-c-connection-dialog`). PRs target `dev`.
- Parallel tasks do not touch the same directories. The ROADMAP's split into workstreams is designed for this.
- Single synchronization point: `crates/ninesql-driver-api/`. The API is frozen at the end of P0-B **before** UI and drivers are built in parallel. Changing it later is its own PR with a `security-reviewer` review.
- **Task report** (in the PR description):
  1. What was done (ROADMAP tasks and `F-…` ids checked off)
  2. What was not done, and why
  3. Decisions made that would warrant an ADR
  4. Files from other owners that should be modified
- The `docs-keeper` updates `docs/ROADMAP.md`, `docs/07-feature-parity.md` and `CONTEXT.md` (current phase) after each merge.

## Long-running tasks
- Every task has a time budget, set by whoever launches it. When it runs out: stop, commit what is consistent, and report what is left.
- Wrap potentially long commands in `timeout` (e.g. `timeout 900 cargo build --workspace`: the first GPUI build is long).
- No watch modes (`cargo watch`, `bacon`) left running.
- Stop any container started for a test before ending the task (`docker compose -f dev/compose.yaml down`).
- If a command keeps failing, report the partial result instead of retrying in a loop.
- Every CI job sets `timeout-minutes`.

## Definition of Done
- [ ] fmt + clippy + tests + `cargo deny` green for the affected crates
- [ ] Invariants I1–I8 respected (state it explicitly in the PR if the task touches secrets, the network, the driver API or DBeaver files)
- [ ] Capability matrix updated if a driver changed ([docs/08-engine-capabilities.md](docs/08-engine-capabilities.md))
- [ ] Feature parity inventory updated if a DBeaver feature landed ([docs/07-feature-parity.md](docs/07-feature-parity.md))
- [ ] Documentation updated if visible behavior changes; ROADMAP updated
- [ ] Tutorials in [docs/tutorials/](docs/tutorials/README.md) updated when user-visible behavior or commands change; new user-facing features get a tutorial section

## Git
- Branches, tags and releases: [RELEASE.md](RELEASE.md). Never commit directly to `main` or `dev`; never create a release tag.
- Conventional Commits: `feat(ui): …`, `fix(pgwire): …`, `docs(adr): …`
- DCO-signed commits (`git commit -s`)
- **No AI tool attribution** in commits or PRs: no assistant `Co-Authored-By` trailer, no "Generated with …" mention. Only the git identity of the maintainer or contributor appears.
- Do not edit `CHANGELOG.md` by hand (generated at release), except for the "Unreleased" section.
- Never commit secrets, real DBeaver workspaces, real connection details, or database dumps.
- Never push or open a PR without an explicit request from the maintainer.
