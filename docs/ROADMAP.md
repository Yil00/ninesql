# ROADMAP – NineSQL

Status: `[ ]` to do · `[~]` in progress · `[x]` done (with the PR number). Each task has an id `Px-Y` used in branch names (`feat/p1-c-connection-dialog`). Owners are the subagents of [`.claude/agents/`](../.claude/agents/); see [AGENTS.md](../AGENTS.md) for the rules.

`F-…` ids refer to the [feature parity inventory](07-feature-parity.md). Engines and families follow [ADR-0003](adr/0003-supported-engines-v1.md).

**MVP = v0.1.0 = phases 0 to 6.** Phases 7 to 11 lead to DBeaver feature parity on the 20 engines (v1.0).

## Parallelism map
```
P0-A GPUI spike ──┐
P0-B driver API ──┼──► P1 (shell, connections, pg/mysql/sqlite drivers) ──► P3 (editor + grid) ──► P5 (editing, export) ──► P6 release
P0-C dev env  ────┤                      │                                   │
P0-D storage  ────┘                      └──► P2 (DBeaver import) ──────────┘──► P4 (navigator)
P0-E parity audit (docs, anytime in phase 0)
```
P0-A and P0-B gate phase 1. P2 (interop) only depends on P0-D and runs in parallel with P1 and P3.

## Phase 0 – Foundations
*Objective: decide the risky things with code, freeze the driver API, have every engine in a container.*

| Workstream | Owner | Tasks |
|----------|--------------|--------|
| P0-A GPUI spike | `ui-engineer` | [x] Pin GPUI and `gpui-component` in the workspace `Cargo.toml`: done as an exact pin of `gpui-kit` 0.9.1 from crates.io instead of a git revision of `zed-industries/zed` ([ADR-0013](adr/0013-gpui-source-and-supply-chain.md) supersedes ADR-0001 §2) · [x] `crates/ninesql-ui/examples/spike.rs`: a window with tabs, a dock, a text input with tree-sitter SQL highlighting, and a virtual table of 1 000 000 synthetic rows · [x] Builds in CI on `ubuntu-24.04` with the system packages listed in `docs/10-supported-platforms.md` (`gpui-spike.yml`, software-rendered smoke test under Xvfb) · [ ] Measured locally on a real GPU and written in the PR: cold start to first frame, idle RSS, RSS with 1 M rows, scroll smoothness (frame times) on Wayland and X11. *Not done: only software-rendered numbers (llvmpipe, cloud sandbox) exist; the maintainer runs `crates/ninesql-ui/examples/spike-measure.sh` on a real GPU* · [x] Go / no-go note: **GO with workarounds** ([docs/spikes/p0-a-gpui.md](spikes/p0-a-gpui.md)); X11 IME crash to patch in P1, Orca not tested |
| P0-B Driver API | `driver-engineer` | [x] `ninesql-driver-api`: `Driver`, `Session`, `Metadata`, `Dialect`, `Capabilities`, `ConnectParams` with a secret handle, `ResultStream` (Arrow batches, documents, update counts, notices, `Truncated`), errors (ADR-0004) · [x] A mock driver used by UI and core tests (feature `mock`, with conformance helpers) · [x] Capability model documented in rustdoc and mirrored in `docs/08-engine-capabilities.md` · [x] **Freeze**: `security-reviewer` review approved; from now on the API changes only through reviewed PRs. Deferred review items: [Phase 0 follow-ups](#phase-0-follow-ups) |
| P0-C Dev environment | `driver-engineer` | [x] `dev/compose.yaml` with all 20 engines, official images pinned by digest, `127.0.0.1` only, profiles `core` / `sql` / `nosql` / `heavy` / `all` (ADR-0009; the ADR-0003 / ADR-0009 profile wording is reconciled by [ADR-0014](adr/0014-dev-environment-profiles.md)). *In the cloud sandbox YugabyteDB and Elasticsearch needed a local disk override; on GitHub runners every profile, heavy included, came up healthy and seeded without it ([#2](https://github.com/Yil00/ninesql/pull/2))* · [x] `dev/engines.json` (id, service, port, image, readiness, profile) · [x] "nine" sample dataset per engine, fake data only, including a 1 M-row table and an all-types table. *The wide table of ADR-0009 is not built yet* · [x] `dev/README.md`: start / stop, ports, dev credentials, the no-Docker path for cloud sandboxes · [x] CI job that brings up each profile and checks readiness (`heavy` nightly): `.github/workflows/dev-env.yml`, first green run on [#2](https://github.com/Yil00/ninesql/pull/2) (core, sql, nosql and heavy) |
| P0-D Storage | `core-engineer` | [x] `ninesql-storage`: XDG paths, `schema_version`, atomic writes, `0700` / `0600`, migrations with fixtures (ADR-0008) · [x] Connection model in `ninesql-core` (engine id, endpoint, auth with secret reference, SSH, TLS, type, folder, properties, `origin`) · [x] `ninesql-secrets` skeleton: trait + in-memory test store; Secret Service backend written on `secret-service` 6 ([ADR-0012](adr/0012-secret-service-client-and-msrv.md)) · [ ] Secret Service backend validated on Debian 12 and Ubuntu 24.04 (GNOME and KDE). *Not done: only checked on a private session bus without a provider; the real round trip is an ignored test left to the maintainer* |
| P0-E Parity audit | `docs-keeper` | [x] Re-check every row of `docs/07-feature-parity.md` (edition column, missing features) against the current DBeaver documentation and release notes (DBeaver 26.2.4: 91 rows checked, 22 corrected, 14 added, 2 unverified) · [x] DBeaver workspaces from at least three DBeaver versions as interop fixtures (with `interop-engineer`): synthetic workspaces written by five DBeaver CE builds, 22.3.5 to 26.2.4, fake content only ([fixtures README](../crates/ninesql-dbeaver/tests/fixtures/README.md)) |
| P0-F Repository | `docs-keeper` | [x] Documentation, ADR-0001 to ADR-0011, agent definitions, CI (fmt, clippy, tests, `cargo deny`, links), crate skeleton · [x] Repository settings (maintainer, see [RELEASE.md](../RELEASE.md#5-repository-configuration-one-time-maintainer)): `dev` as default branch, branch rulesets on `main` / `dev` with the `CI result` required check, squash into `dev` / merge commit into `main`, private vulnerability reporting enabled; CodeQL workflow added |

**Exit criterion**: the GPUI spike shows 1 M rows scrolling smoothly on Ubuntu 24.04 (Wayland and X11) with measured numbers; the driver API is frozen; `docker compose --profile all up` brings all 20 engines to ready in CI.

**Exit status** (2026-11-14): the driver API is frozen. The dev-environment CI job brought every profile to ready on [#2](https://github.com/Yil00/ninesql/pull/2). Not met yet: real-GPU measurements of the spike (only software-rendered numbers exist). Phase 1 starts in parallel; both items stay open above.

### Phase 0 follow-ups
Items deferred by the phase 0 security reviews and spikes. The deadline is when the item must have landed. Items that have landed are listed after the table.

| Origin | Item | Must land |
|---|---|---|
| P0-A | Real-GPU measurements on Wayland and X11 (`spike-measure.sh`, maintainer) | Before the phase 0 exit criterion is declared met |
| P0-A | Test accessibility with Orca | Before P6 |
| P0-A | Trim the build packages of `docs/10-supported-platforms.md`; check Ubuntu 22.04 and Debian 12 builds and Vulkan adapters | P1 |
| P0-B | Per-engine SQL that lifts read-only (liftable mechanisms), with tests | P1-B, per engine |
| P0-C | Build the wide table of the "nine" dataset (ADR-0009) | Before P3-B grid work relies on it |
| P0-D | Secret Service round trip on GNOME Keyring and KWallet, Debian 12 and Ubuntu 24.04 ([ADR-0012](adr/0012-secret-service-client-and-msrv.md)) | Before P1-D ships the Secret Service backend |
| ADR-0012 | Move `secret-service` from `=6.0.0-rc.1` to a stable 6.x; revisit the `ctutils` lock (RUSTSEC-2026-0385) | Stable release available; checked at each phase-end review |

Moved to [Phase 1 follow-ups](#phase-1-follow-ups): the X11 IME patch (P0-A, now a P1-A follow-up) and review item L5 of P0-D (`secret-service` release-candidate internals).

**Landed in phase 1, wave 1** (2026-11-14):
- P0-B review, in the driver API hardening: M6 (execution-scoped cancel handles, session `Busy` until the server confirms the end), L1 (weakest policy across hops plus per-hop list in `TunnelInfo`), L2 (timeout watchdog), L3 (copy-on-slice), L4 (classifier limit documented), L5 (`retains_secret_after_connect`, metadata connections under the session's TLS / read-only / property rules, bound `PageRequest.filter`), F1 (`U&"…"` decoded), F2 (wider side-effect function list), F4 (`DocValue::Truncated`), F5 (`max_row_bytes`, result-set headers counted), F7 (SQL-form secret redaction), and the missing conformance helpers (verify-full against an untrusted certificate, batch budgets, identifier quoting, engine-neutral `WRITES`, non-exhaustive `Document`). Details in [docs/08](08-engine-capabilities.md).
- Integration review: the core property test alphabet covers digits and non-ASCII and refuses `replication`; drivers expose their extra reserved keys through `Capabilities::extra_reserved_properties`. Core validation against them is a [phase 1 follow-up](#phase-1-follow-ups).
- P0-D review, in the storage / core / secrets follow-ups: N1 (redaction rule), N3 (wider secret detectors), N6 (keyring items scoped by the project UUID of `project.json` v2), N7.2 (immediate rewrite when a plaintext secret is found on load), N7.4 (removed property values zeroized), L2 (directory ownership and symlink checks), L6 (structured config documented as a driver contract in docs/08), L8 (workspace lock `ninesql.lock`), L-B (duplicate resolution), L-C (value-shape checks; `PENDING_RESERVED_KEYS` removed), and the `.bak` scrub (JSON migration backups re-encoded from the scrubbed value). These change ADR-0008: see [ADR-0017](adr/0017-workspace-lock-rewrite-on-load-and-project-identity.md).
- ADR-0013: the checked-in audit log is [docs/audits/gpui-snapshot.md](audits/gpui-snapshot.md), started when `gpui-kit` became a normal dependency of `ninesql-ui` (P1-A). `cargo vet` is not set up.

## Phase 1 – Application shell, connections, first drivers
*Objective: connect to PostgreSQL, MySQL, MariaDB and SQLite (directly, over TLS and over SSH) from a real NineSQL window, with secrets in the keyring.*

| Workstream | Owner | Tasks |
|----------|--------------|--------|
| P1-A Shell | `ui-engineer` | [x] Main window: navigator dock, editor area with tabs and splits, status bar, multiple windows (F-WS-01): `ninesql` binary, wave 1 · [x] Command palette, actions and default key bindings, overridable in `settings.toml` (F-WS-02): 19 actions, each with a binding and a palette entry; `settings.toml` is read at startup only, and a symlinked `settings.toml` is refused · [x] Light / dark / system themes, fonts, zoom; fractional scaling through GPUI's window scale factor (F-WS-03). *Still open: settings are not persisted from the UI yet; the X11 IME crash is not patched; screenshots and checks on a real GPU (Wayland, X11, fractional scaling) are pending. See [Phase 1 follow-ups](#phase-1-follow-ups)* |
| P1-B Drivers (MVP families) | `driver-engineer` | [ ] `ninesql-driver-pgwire`: PostgreSQL connect, TLS modes, execute, stream, cancel, read-only (engine-side), capabilities · [ ] `ninesql-driver-mysqlwire`: MySQL and MariaDB, same scope · [ ] `ninesql-driver-sqlite`: file open (read-only mode), same scope · [ ] Integration tests against `dev/` including the read-only write-must-fail test |
| P1-C Connections UI | `ui-engineer` | [ ] Connection dialog: engine picker, endpoint / URL (F-CON-12), auth, TLS (F-CON-06), SSH (F-CON-05), properties (F-CON-08), type (F-CON-03), test connection (F-CON-01) · [ ] Connection list with folders (F-CON-02), duplicate / delete · [ ] Production visuals: colored frame, type badge, read-only badge (ADR-0007) |
| P1-D Core services | `core-engineer` | [ ] Driver runtime (Tokio) and the bridge to GPUI entities (ADR-0004) · [ ] `ninesql-secrets`: Secret Service backend, vault fallback with Argon2id + XChaCha20-Poly1305, "ask every time" (F-CON-04, ADR-0005). *Secret Service items are now scoped by the project UUID with duplicate resolution (P0-D follow-ups); the vault and "ask every time" are open* · [x] `ninesql-ssh`: russh tunnels, jump hosts, agent, `known_hosts` checks (ADR-0011), per-hop host-key policies ([ADR-0016](adr/0016-ssh-rsa-and-host-key-policies.md)), wave 1. *RSA key files are refused (ADR-0016); no HTTP CONNECT / SOCKS5 proxy in front of the first hop yet; not wired into `ninesql-core` yet (wave 2)* · [ ] Test: no secret in any workspace file after create / edit / connect (I1) |
| P1-E Review | `security-reviewer` | [ ] Review of secrets, SSH, TLS defaults and read-only enforcement · [ ] End-of-phase review |

**Exit criterion**: from a clean install, create a PostgreSQL connection over SSH with `verify-full` TLS, password in the keyring; connect; restart NineSQL; reconnect without retyping; `grep` finds no password under `$XDG_*/ninesql`. Same for MySQL, MariaDB and SQLite. A read-only session rejects a write engine-side.

**Status** (2026-11-14): phase 1 in progress. Wave 1 landed the P1-A shell, `ninesql-ssh` (P1-D), the driver API hardening and the P0-D follow-ups, together with the P2-A parser. Wave 2: the P1-B drivers and the rest of P1-D (driver runtime bridge, vault, SSH wiring in the core). Wave 3: P1-C. The exit criterion is not met.

### Phase 1 follow-ups
Items deferred by phase 1 decisions and reviews. The deadline is when the item must have landed.

| Origin | Item | Must land |
|---|---|---|
| ADR-0016 | Enable RSA support (russh `rsa` feature) once a russh release depends on `rsa` ≥ 0.10.0 (RUSTSEC-2023-0071), with a `security-reviewer` review | When such a russh release exists; checked at each phase-end review |
| ADR-0016 | Add a `HostKeyVerification::AcceptNew` variant to the driver API so `TunnelInfo` stops reporting `AcceptNew` as `Disabled` (frozen API: own reviewed PR) | Before P1-C exposes host-key policies |
| ADR-0016 | Per-hop host-key policy field in the `ninesql-core` connection model (`SshHop`) | P1-D |
| ADR-0015 | Consider `include-dev = true` in the `[licenses]` section of `deny.toml` so test-only crates are license-checked | P1 |
| P1 review | SHA-pin the actions used in `.github/workflows/codeql.yml` | P1 |
| P0-A / P1-A | Patch the X11 IME crash (`xim-ctext` / `zed-xim`, `[patch.crates-io]` under ADR-0013's rules) and report it upstream | P1 (P1-A follow-up) |
| P1-A | Persist settings changed from the UI (theme, fonts, zoom, key bindings) to `settings.toml` with atomic writes; today the file is only read at startup | P1 |
| P1-A | Decide whether a symlinked `settings.toml` (dotfile managers) stays refused or is followed under ownership checks; record the choice | P1 |
| P1-A | Screenshots and checks on a real GPU: Wayland and X11, fractional scaling (maintainer, local) | Before the P1-A items are reviewed at phase end |
| P0-D review (L5) | Review the internals of `secret-service` 6.0.0-rc.1 (key material not zeroized) before the Secret Service feature is on by default | Before P1-D enables the backend by default |
| P1-D (SSH) | Cross-process lock on `known_hosts` writes (reuse `ninesql_storage::DirLock`) | Before P1-C lets users confirm host keys |
| P1-D (SSH) | Supply-chain tracking of the `ssh-key` release candidate pulled by russh (pin, advisories, move to a stable release) | Checked at each phase-end review |
| P1-D (core) | Validate connection properties with `Capabilities::is_reserved_property`, so each driver's `extra_reserved_properties` are refused at save and import time, not only at connect | With the first P1-B driver |
| P1-D (core) | Handle `Busy` after a cancel: the session stays busy until the server confirms the end; the core waits or reports it instead of failing the next statement | With the driver runtime bridge |
| P1-B (pgwire) | Close an idle cursor on cancel, set `idle_in_transaction_session_timeout`, and test that a paused cursor's locks are released after the deadline (docs/08 idle-cursor contract); settle `krbsrvname` and `target_session_attrs` as extra reserved keys | With `ninesql-driver-pgwire` |
| P1-B (mysqlwire) | Decide between `retains_secret_after_connect = true` and a control connection opened during `connect` for `KILL QUERY` | With `ninesql-driver-mysqlwire` |
| P1-C (UI) | Show `LoadIssues` (rewritten files, refused properties) and the reason a property was refused; show `SshHopHostKeyNotVerified`, `other_key_types` and `Tunnel::security_warnings`; keep truncated documents and columns non-editable | P1-C |
| P2-A | Run both cargo-fuzz targets (time-boxed) and record the result | Before the P2-D review |
| ADR-0016 / P2-A | Flag imported SSH connections that use an RSA key file in the import report (ADR-0016 §1.2) | Before P2-B shows the preview |
| P2-A review (L-a) | A blank (whitespace-only) `config-profile` value: treat it consistently in every scope and test it | Before P2-C |
| P2-A review (L-b) | Jump-host selection: iterate indices `0..count`, stop at the first disabled hop, use canonical indices, fail on a non-boolean `enabled` | Before P2-C |
| P2-C | Convert the mirror model to `ninesql_core::Connection` and run `sanitize()` / `validate()` before persisting, with an I1 test (no imported secret in any workspace file) | P2-C |
| P2-C | Persist the disabled reason, so that a connection disabled for a missing hop, profile or handler cannot simply be re-enabled without fixing it | P2-C |
| P2-C | Imported connection types never overwrite a built-in type and never weaken an existing type (confirmations, auto-commit, read-only rules) | P2-C |
| P2-C | Move imported secrets to the keyring and drop the imported values (zeroized); use `import_project`, not `map_sources` | P2-C |
| P2-C / P1-B | Drivers open files flagged as inside `DBeaverData` read-only (I4) | P2-C |
| P2-C (core) | Core fields for the default schema / catalog and per-connection auto-commit, so the mapped DBeaver `bootstrap` values are not lost | P2-C |
| P3-C | Init SQL (F-CON-09): either run it before read-only is enforced, or re-assert and verify read-only after it ran; apply the production confirmation to it | P3-C |

## Phase 2 – DBeaver import
*Objective: a DBeaver user switches in one click.*

| Workstream | Owner | Tasks |
|----------|--------------|--------|
| P2-A Parser | `interop-engineer` | [x] Workspace detection: native, Flatpak, Snap, custom folder (ADR-0006). *The Flatpak and Snap paths are not verified on a real install* · [x] `data-sources*.json` model with an `extra` map for unknown keys; folders; connection types · [x] `credentials-config.json` decryption into zeroized buffers; master-password-protected files detected and skipped · [x] Mapping to NineSQL connections: engines in the v1 set, the others as disabled connections; JDBC URL and driver properties → NineSQL options; SSH and TLS handlers; fail-closed rules ([docs/06](06-dbeaver-interop.md#import-rules-enforced-by-the-parser-p2-a)). *Output is a mirror model, not yet persisted (P2-C); the engine mappings marked "to verify" in docs/06 have no fixture* · [x] Fixtures from P0-E, property tests, fuzz targets for both decoders. *The cargo-fuzz targets exist but have not been run yet* |
| P2-B Import flow | `ui-engineer` | [ ] First-run "Import from DBeaver" screen and a menu entry · [ ] Preview: connections, what maps, what does not, secrets to move · [ ] Import report saved in the project; idempotent re-import by `origin.dbeaver_id` (F-CON-14) |
| P2-C Core | `core-engineer` | [ ] Persist imported connections and move secrets to the store; optional copy of DBeaver SQL scripts into the project |
| P2-D Review | `security-reviewer` | [ ] Review: I1 and I4 (no write to DBeaver files, no secret on disk or in logs), parser robustness |

**Exit criterion**: importing each fixture workspace yields the expected connections, folders, types and secrets (snapshot tests without secret values); DBeaver files are byte-identical before and after (checksum test); a 100-connection workspace imports in under 2 s.

## Phase 3 – SQL editor and results grid
*Objective: the daily loop. Write SQL, run it, read large results without lag.*

| Workstream | Owner | Tasks |
|----------|--------------|--------|
| P3-A Editor | `ui-engineer` | [ ] Editor with tree-sitter highlighting per dialect (F-SQL-01), find / replace, multi-cursor, folding (F-SQL-15) · [ ] Autocompletion from metadata: keywords, objects, columns, aliases, FK joins (F-SQL-02) · [ ] Execute statement / selection / script (F-SQL-03), parameters prompt (F-SQL-06), error position (F-SQL-12) · [ ] Script files linked to a connection (F-SQL-11), session restore (F-WS-06) |
| P3-B Grid | `ui-engineer` | [ ] `ninesql-grid`: virtualized Arrow-backed grid, column resize / reorder, selection, copy, NULL and type-aware rendering (F-DATA-01) · [ ] Fetch next page / fetch all under the memory budget, `Truncated` banner (I6) · [ ] Multiple result tabs, update counts, notices (F-SQL-04) |
| P3-C Execution services | `core-engineer` | [ ] Query execution service: cancel (F-SQL-05), timeouts, transaction modes and pending-transaction tracking (F-SQL-07), keep-alive / reconnect (F-CON-10), init SQL (F-CON-09) · [ ] History in `history.sqlite`, no parameter values, retention (F-SQL-08) · [ ] Preferences UI backend (F-WS-04) |
| P3-D Dialects | `driver-engineer` | [ ] Statement splitting and destructive-statement classification for pg-wire, mysql-wire and SQLite, with a corpus of tricky cases and property tests (ADR-0007, F-SQL-18) |
| P3-E Performance | `ui-engineer` | [ ] Benchmarks in CI (headless parts) and a documented local protocol: 1 M rows from PostgreSQL, time to first rows, memory at budget, scroll frame times |

**Exit criterion**: `SELECT * FROM big_table` (1 M rows) shows first rows in under 300 ms on the dev PostgreSQL, scrolls without dropped frames on the reference machine, stays under the memory budget, and can be cancelled; a `DELETE` without `WHERE` on a production connection asks for confirmation.

## Phase 4 – Navigator and metadata
*Objective: browse and understand a database.*

| Workstream | Owner | Tasks |
|----------|--------------|--------|
| P4-A Metadata (MVP families) | `driver-engineer` | [ ] `Metadata` for pg-wire (PostgreSQL first, then per-engine overrides), mysql-wire, SQLite: every object kind of F-NAV-01 and F-NAV-02 the engine has · [ ] DDL generation where the engine provides it (F-NAV-07) |
| P4-B Navigator UI | `ui-engineer` | [ ] Lazy async tree with cancel and per-node refresh (F-NAV-03), filters (F-NAV-04), fuzzy object search (F-NAV-05) · [ ] Object editor: properties, columns, keys, indexes, DDL tab (F-NAV-06) · [ ] "Generate SQL" actions (F-NAV-07) |
| P4-C Wire-compatible engines | `driver-engineer` | [ ] TimescaleDB, YugabyteDB, QuestDB (pg-wire) and TiDB (mysql-wire): engine ids, capability overrides, metadata differences, dev containers green in CI |

**Exit criterion**: the 8 MVP engines browse their "nine" dataset fully in the navigator; each engine's capability row in docs/08 matches what its integration tests prove.

## Phase 5 – Data editing and transfer
*Objective: change data safely and move it in and out.*

| Workstream | Owner | Tasks |
|----------|--------------|--------|
| P5-A Editing | `ui-engineer` | [ ] In-cell edit, insert / duplicate / delete rows, save with DML preview (F-DATA-04); requires a key, otherwise read-only with an explanation · [ ] Record and text views (F-DATA-02), value viewers JSON / XML / binary / image (F-DATA-05) · [ ] Sort / filter client- and server-side, filter history (F-DATA-03); FK navigation (F-DATA-07); aggregate and metadata panels (F-DATA-09, F-DATA-10); advanced copy (F-DATA-06) |
| P5-B Dialect DML | `driver-engineer` | [ ] DML generation with dialect quoting and typed literals for every MVP engine; injection tests on identifiers and values |
| P5-C Transfer | `core-engineer` | [ ] `ninesql-transfer`: streamed export to CSV, TSV, JSON, SQL `INSERT`, Markdown, HTML (F-XFER-01) · [ ] CSV import with mapping and preview (F-XFER-03) · [ ] Script execution with continue / stop on error and summary (F-SQL-16) · [ ] SQL formatter (F-SQL-09) and snippets (F-SQL-10) |
| P5-D Export to DBeaver | `interop-engineer` | [ ] Write a new `data-sources.json` for selected connections, without passwords (F-CON-15) |
| P5-E Review | `security-reviewer` | [ ] Review of generated SQL (edits, filters, import), file paths in import / export, production guards |

**Exit criterion**: edit, insert and delete rows on each MVP engine with a correct DML preview; export 1 M rows to CSV in bounded memory; import a 100 000-line CSV; a DBeaver export of NineSQL connections imports back into DBeaver.

## Phase 6 – Hardening and v0.1.0 release
*Objective: a signed, installable, honest first release.*

| Workstream | Owner | Tasks |
|----------|--------------|--------|
| P6-A Packaging | `core-engineer` | [ ] `.deb` (ADR-0010): desktop file, icons, AppStream, MIME, man page, third-party licenses · [ ] Install tests in Debian 12 / 13 and Ubuntu 22.04 / 24.04 containers · [ ] Flatpak manifest with minimal permissions · [ ] Reproducible build check, SBOM, cosign signatures from the tag-push workflow |
| P6-B Release infrastructure | maintainer | [ ] APT signing key, signed repository on GitHub Pages, fingerprint in SECURITY.md · [ ] Flathub submission · [x] Branch protection (done in P0-F) · [ ] Release settings (tag-push signing, Pages) ([RELEASE.md](../RELEASE.md)) |
| P6-C Measurements | `ui-engineer` | [ ] Publish measured cold start, idle memory and 1 M-row behavior, with method, machine and commit, in docs/04 — replacing every target with a measurement |
| P6-D Documentation | `docs-keeper` | [ ] User guide (install, import from DBeaver, connections, editor, safety model), known limitations, capability matrix final pass, CHANGELOG |
| P6-E Review | `security-reviewer` | [ ] End-of-MVP security review: verdict ready / not ready for v0.1.0 |

**Exit criterion (MVP)**: see [docs/04-mvp-scope.md](04-mvp-scope.md#exit-criterion).

## Phase 7 – Analytical SQL engines (v0.2)
| Workstream | Owner | Tasks |
|----------|--------------|--------|
| P7-A Embedded / analytical | `driver-engineer` | [ ] DuckDB (file and in-memory, read-only open, `spawn_blocking`) · [ ] ClickHouse (HTTP, TLS, query cancel via `KILL QUERY` / query id) · [ ] Trino (client protocol, paging via `nextUri`, cancel via `DELETE`) · [ ] Firebird (pure-Rust wire, read-only transactions) · [ ] InfluxDB 3 (SQL over HTTP, token in secret store) (F-NOSQL-06) |
| P7-B UI | `ui-engineer` | [ ] Engine-specific connection fields, file pickers for DuckDB, metadata views for catalogs (Trino), databases / tables (ClickHouse) |

## Phase 8 – Document and key-value engines (v0.3)
| Workstream | Owner | Tasks |
|----------|--------------|--------|
| P8-A Drivers | `driver-engineer` | [ ] MongoDB: documents stream, metadata (databases, collections, indexes, validators), find / aggregate, edits (F-NOSQL-01) · [ ] Redis and Valkey: SCAN-based browsing, typed reads / writes, TTL, command execution with destructive-command classification (F-NOSQL-02) |
| P8-B UI | `ui-engineer` | [ ] Document grid and JSON tree editor; Redis key browser and typed value editors; command console |
| P8-C Proxy | `core-engineer` | [ ] SOCKS5 / HTTP CONNECT proxy support for all TCP drivers (F-CON-07) |

## Phase 9 – Search, wide-column and graph engines (v0.4)
| Workstream | Owner | Tasks |
|----------|--------------|--------|
| P9-A Drivers | `driver-engineer` | [ ] Elasticsearch and OpenSearch: indices, mappings, SQL endpoint, query DSL, documents (F-NOSQL-03) · [ ] Cassandra: keyspaces, tables, CQL, paging state (F-NOSQL-04) · [ ] Neo4j: labels, relationship types, Cypher, typed results (F-NOSQL-05) |
| P9-B UI | `ui-engineer` | [ ] Query DSL editor with JSON schema completion; graph result view (nodes / relationships canvas) |

## Phase 10 – Advanced parity
| Workstream | Owner | Tasks |
|----------|--------------|--------|
| P10-A Schema tools | `ui-engineer` + `driver-engineer` | [ ] Create / alter / drop forms with script preview (F-NAV-08), dependencies (F-NAV-09), users / roles / grants (F-NAV-10), data search (F-NAV-11), schema compare (F-NAV-12) |
| P10-B ER diagrams | `ui-engineer` | [ ] GPU canvas, auto layout, custom diagrams, PNG / SVG export (F-ERD-01 to F-ERD-03) |
| P10-C Explain and outline | `ui-engineer` + `driver-engineer` | [ ] Visual explain plans per engine (F-SQL-13), outline and go to definition (F-SQL-14) |
| P10-D Data | `ui-engineer` + `core-engineer` | [ ] FK lookups (F-DATA-08), grouping / color rules (F-DATA-11), GIS viewer (F-DATA-12), auto-refresh (F-DATA-13), virtual keys and virtual columns (F-DATA-16) · [ ] XLSX / XML / Parquet export (F-XFER-02), table-to-table transfer (F-XFER-04), saved tasks (F-XFER-05) |
| P10-E Admin tools | `driver-engineer` + `ui-engineer` | [ ] Native backup / restore (F-TOOL-01), sessions (F-TOOL-02), locks (F-TOOL-03), maintenance (F-TOOL-04), dashboards (F-TOOL-05), multi-connection scripts (F-TOOL-06), script execution through the native client (F-TOOL-07) |
| P10-F Workspace | `core-engineer` | [ ] Multiple projects (F-CON-13), local engine detection (F-CON-16), network profiles (F-CON-17), bookmarks (F-WS-05), git-friendly projects (F-WS-07) |

## Phase 11 – v1.0 parity completion
| Workstream | Owner | Tasks |
|----------|--------------|--------|
| P11-A Beyond CE | per item | [ ] Visual query builder (F-SQL-17), data compare (F-DATA-14), scheduling (F-XFER-06), mock data (F-XFER-07), schema edit from ERD (F-ERD-04), charts from results (F-DATA-15), XLSX / XML import (F-XFER-08) |
| P11-B Ecosystem | per item | [ ] Opt-in AI assistant with a user-provided endpoint (F-WS-08, needs an ADR on data sent), translations (F-WS-09) |
| P11-C Parity audit | `docs-keeper` + `security-reviewer` | [ ] Every `F-…` row done or `won't do` with a reason; end-of-v1 security review |

## After v1
- Engines outside the v1 set through new ADRs (SQL Server and Oracle if a compatible client exists, cloud warehouses, generic ODBC driver).
- Extension API.
- Upstream Debian packaging.
