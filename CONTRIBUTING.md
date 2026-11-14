# Contributing to NineSQL

Thank you for your interest! The project is in its foundations phase: design discussions are as useful as code.

## Before you start
1. Read [CONTEXT.md](CONTEXT.md) (especially the invariants) and the [architecture decisions](docs/adr/README.md).
2. Find or open an issue for any non-trivial change. ROADMAP tasks ([docs/ROADMAP.md](docs/ROADMAP.md)) are the preferred entry points.
3. A change that contradicts an accepted ADR goes through a **new ADR**, not a code PR.

## Invariants to respect
Any PR that violates them will be rejected:
- No secret in clear on disk; no telemetry or unexpected network traffic.
- Read-only enforced engine-side; production guards cannot be bypassed.
- DBeaver files are never modified.
- TLS and SSH host keys verified by default.
- Apache 2.0-compatible dependencies only (`cargo deny check`).

## Development setup
Rust 1.89 or newer through `rustup` (the `stable` channel of `rust-toolchain.toml` is selected automatically).
```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo deny check
```
System packages for the UI build: [docs/10-supported-platforms.md](docs/10-supported-platforms.md). Test databases: [dev/README.md](dev/README.md).

## Workflow
- **PRs target the `dev` branch**, never `main` (`main` = released code).
- Branch from `dev`, named `<type>/<roadmap-id>-<slug>` when there is a ROADMAP id: `feat/p1-b-pgwire-driver`, `fix/…`, `docs/…`, `chore/…`, `ci/…`, `refactor/…`, `perf/…`, `test/…`.
- Commits and PR titles in [Conventional Commits](https://www.conventionalcommits.org/) format. Scopes: `ui`, `grid`, `editor`, `core`, `storage`, `secrets`, `ssh`, `transfer`, `dbeaver`, `driver-api`, `pgwire`, `mysqlwire`, `sqlite`, `duckdb`, `clickhouse`, `trino`, `firebird`, `influxdb`, `mongodb`, `redis`, `search`, `cassandra`, `neo4j`, `dev`, `packaging`, `docs`, `adr`.
- Breaking change: `feat!:` or a `BREAKING CHANGE: …` footer.
- Recommended hooks: `pre-commit install --hook-type pre-commit --hook-type commit-msg` (gitleaks + commit format).
- One PR = one topic; it is squashed on merge. Fill in the task report of the PR template.

Full rules for branches, tags and releases: [RELEASE.md](RELEASE.md).

## AI-assisted contributions
Welcome, under the same rules: the contributor is the author and answers for every line. Agents follow [AGENTS.md](AGENTS.md). No AI tool attribution in commits or PRs.

## Developer Certificate of Origin (DCO)
Every commit must be signed off, which certifies that you have the right to submit this code under the Apache 2.0 license ([developercertificate.org](https://developercertificate.org/)):

```bash
git commit -s -m "feat(pgwire): stream results as Arrow batches"
```

No CLA is required.

## License of contributions
Every contribution is published under the [Apache 2.0 license](LICENSE) (section 5 of the license).
