# Run the checks before a PR

CI runs these checks on every pull request ([`.github/workflows/ci.yml`](../../.github/workflows/ci.yml)); running them first saves a round trip. The rules they enforce are in [AGENTS.md](../../AGENTS.md#definition-of-done) and [CONTRIBUTING.md](../../CONTRIBUTING.md).

## 1. Everything that needs no database
```bash
make check
```
`make check` runs, in order, and stops at the first failure:

| Target | Command | CI job |
|---|---|---|
| `fmt-check` | `cargo fmt --all --check` | Rust |
| `clippy` | `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | Rust |
| `test` | `cargo test --workspace --locked` | Rust |
| `deny` | `cargo deny --locked check` (licenses, bans, sources, advisories) | cargo-deny |
| `verify-patches` | `scripts/verify-patches.sh` (patched crates under `crates/patches/`) | cargo-deny |
| `forbidden-apis` | refuses GPUI's HTTP and credential APIs under `crates/` ([ADR-0013](../adr/0013-gpui-source-and-supply-chain.md)) | cargo-deny |
| `md-links` | `python3 scripts/check-md-links.py` | Documentation links |
| `dev-check` | `python3 dev/ready.py check` | Dev environment |

Each target also runs alone (`make clippy`, `make md-links`…). To fix formatting, `make fmt`.

Requirements: the [build requirements](build-and-run.md#1-install-the-toolchain) (clippy and the tests build GPUI), [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) (`cargo install cargo-deny --locked`; CI uses 0.20.2), Python 3.11 or newer for `verify-patches`. The long cargo commands stop after `TIMEOUT` seconds (900 by default; `make check TIMEOUT=1800` on a slow machine).

In `make test`, the integration tests of PostgreSQL, MySQL, MariaDB, TimescaleDB, YugabyteDB, QuestDB, TiDB and SSH **skip themselves** when their engine is not reachable. That is why the next step exists.

## 2. The driver integration tests
```bash
make test-engines
```
This does what the `driver-integration` CI job does:
1. pulls the images of the `core` profile and of the four wire-compatible engines, then starts PostgreSQL, MySQL and MariaDB (`dev/ready.py up core`) and TimescaleDB, YugabyteDB, QuestDB and TiDB (`dev/ready.py up sql --engines timescaledb,yugabytedb,questdb,tidb`; about 2.3 GB of memory together);
2. runs `cargo test --locked` for `ninesql-driver-pgwire`, `ninesql-driver-mysqlwire`, `ninesql-driver-sqlite`, `ninesql-core` and `ninesql-ssh` with `NINESQL_REQUIRE_ENGINES=postgresql,mysql,mariadb,ssh,timescaledb,yugabytedb,questdb,tidb`, so an unreachable engine **fails** the run instead of skipping. The SSH tests start their own OpenSSH containers with `docker run`;
3. stops the engines and deletes their volumes (`dev/ready.py down`), whether the tests passed or not.

It needs Docker ([Start the dev databases](dev-databases.md#1-requirements)) and none of the GPUI system packages. If you already started the engines, run only the tests with `make test-engines-run` and stop the engines yourself with `make dev-down`. With only the `core` profile up (`make dev-up`), narrow the required list: `make test-engines-run REQUIRE_ENGINES=postgresql,mysql,mariadb,ssh` (the tests of the other engines then skip). Note that `make test-engines` stops them at the end too.

## 3. Optional checks
- `make verify-gpui-snapshot`: compares the GPUI crates in Cargo's cache with their upstream sources ([ADR-0013](../adr/0013-gpui-source-and-supply-chain.md)). It needs network access to GitHub and crates.io. Its output is required in a PR that bumps GPUI.
- `make release`: the optimised `ninesql` binary.

## 4. Before you open the PR
- The [PR template](../../.github/pull_request_template.md) checklist: task report, invariants, documentation, screenshots for visible UI changes.
- If your change alters what a user sees or types, update the matching page of these tutorials ([index](README.md)); a new user-facing feature gets a tutorial section.
- Commits are signed off (`git commit -s`) and follow Conventional Commits (`feat(ui): …`, `fix(pgwire): …`).

## What CI runs that `make` does not
- The **macOS build** (`cargo build`, `cargo test` and `cargo clippy` of `ninesql-ui` on Apple Silicon). It is required to pass; you cannot reproduce it from Linux, because cross-compiling needs the Apple SDK.
- The dev-environment workflow, which brings up every profile (`make dev-up PROFILE=sql`, `nosql`, `heavy` are the local equivalents), and CodeQL.
