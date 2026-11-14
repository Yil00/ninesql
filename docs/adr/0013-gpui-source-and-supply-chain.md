# ADR-0013: GPUI source (gpui-kit / gpui-pre from crates.io) and supply-chain controls

- **Status**: Accepted (maintainer, 2026-11-14). Supersedes [ADR-0001](0001-native-stack-rust-gpui.md) Decision §2 (git pin of `zed-industries/zed` and `gpui-component`). Amends [ADR-0002](0002-licensing-and-dependency-policy.md) Decision §6 (git source allow-list). Every other point of both ADRs stands. Partially amended by [ADR-0018](0018-patched-third-party-crates.md): a control for patched third-party crates.
- **Date**: 2026-11-14

## Context
[ADR-0001](0001-native-stack-rust-gpui.md) §2 pins GPUI to a git revision of `zed-industries/zed` together with a matching `gpui-component` revision. §3 allows a later switch to crates.io releases. [ADR-0002](0002-licensing-and-dependency-policy.md) §6 allows git dependencies only from those two repositories.

The P0-A spike (branch `feat/p0-a-gpui-spike`, `docs/spikes/p0-a-gpui.md`) and its security review found:

- **The git pin no longer works.** Since 0.8, `gpui-component` depends on exact-pinned `gpui-pre-*` crates, a crates.io republication of Zed's GPUI crates, not on Zed's git repository. Pinning `zed` by git would add a second, incompatible `gpui` crate next to the one `gpui-component` uses. Zed's own crates.io `gpui` releases (0.2.x) are too old for `gpui-component`.
- **Chosen pin.** `gpui-kit = "=0.9.1"`: same source as `longbridge/gpui-component` tag `v0.9.1`, commit `39ab849a93b5f15507b969164d49891db8d5b470`. It pins `gpui-pre =0.3.12`, a snapshot of `zed-industries/zed@d2a4fb6`, recorded in the crate's `[package.metadata.gpui-pre] zed-rev`.
- **Publisher.** All `gpui-pre*`, `gpui-kit`, `gpui-component` and `gpui-base` crates are owned by a single personal crates.io account (`huacnlee`). They are published by a weekly scheduled workflow (`release-gpui.yml`, `script/bump-gpui.ts`) with a long-lived token and tag-pinned actions, without crates.io Trusted Publishing. Zed's own crates are owned by the `zed-industries` team. Trust therefore moves from "Zed's git repository" to "a third-party snapshot of it".
- **Review of the pinned versions.**
  - A diff of the 20 `gpui-pre-*` crates against `zed@d2a4fb6` shows only packaging patches, each marked `// Modified for gpui-pre`: facade path rewrites in macros, vendored `build.rs` paths, `wgpu` 30 API adjustments.
  - `gpui-pre-reqwest` is identical to `zed-reqwest` and is only in the wasm part of our graph.
  - The `gpui-kit` crate family is identical to the `v0.9.1` tag. Checksums match.
  - No Zed GPL crate, and no HTTP client, TLS stack or telemetry linked on Linux: I2 and I7 hold.
  - `cargo deny` needs 8 advisory ignores, all informational "unmaintained" notices: `instant`, `paste`, `rustls-pemfile` (wasm only), `rustybuzz`, `ttf-parser`, `zbus_names`, `zvariant`, `zvariant_derive`.
- **Residual risks.**
  - Compromise of the single publishing account, or a malicious minor release.
  - `gpui-kit` itself uses caret requirements on `gpui-base`, `gpui-component`, `gpui-kit-assets` and `gpui-component-macros`, so a `cargo update` can move them.
  - The snapshot's dependency graph differs from the one Zed tests: crates.io `calloop`, `async-task` and `oo7` instead of Zed's forks, and `wgpu` 30.0.x (the surface-drop fix gfx-rs/wgpu#10230 is to be watched).
  - Opt-in GPUI features reach the network or load code at run time: `hot-reload` (pulls `subsecond`, the Dioxus devtools crates, a websocket client) and `tracy`.
  - GPUI links `oo7` and exposes `read_credentials` / `write_credentials`, a second keyring path that would bypass `ninesql-secrets` ([ADR-0005](0005-secrets-storage.md), [ADR-0012](0012-secret-service-client-and-msrv.md)).
  - Build-time environment variables (`GPUI_LEAK_DETECTION`, `ZTRACING`) change the compiled code.

## Decision
NineSQL takes GPUI and `gpui-component` from crates.io through **`gpui-kit` / `gpui-pre`**, under the following mandatory controls.

1. **Exact pins.** The whole family is pinned with `=` in `[workspace.dependencies]` of the root `Cargo.toml`: `gpui-kit`, `gpui-base`, `gpui-component`, `gpui-component-macros`, `gpui-kit-assets`. CI builds with `--locked`. The GPUI version moves only in a dedicated `chore(ui): bump gpui` PR.
2. **Snapshot verification.** A script, `scripts/verify-gpui-snapshot.sh`, runs on every bump and its output is attached to the bump PR. It diffs:
   - each `gpui-pre-*` crate against `zed-industries/zed` at the `zed-rev` recorded in the crate metadata;
   - each `gpui-kit` family crate against the matching `longbridge/gpui-component` tag;
   - `gpui-pre-reqwest` against `zed-reqwest`.

   Any hunk that is not under a `// Modified for gpui-pre` header blocks the bump until it is explained and reviewed by the `security-reviewer`.
3. **Owners.** Each bump PR records the crates.io owners of every crate in the family and flags any change since the previous bump.
4. **Audit trail.** Adopt `cargo vet`, importing the audits published by Mozilla, Google and the Bytecode Alliance. Until it is in place, each bump adds an entry to a checked-in audit log with the diff summary from point 2 and the checksums of the pinned crates.
5. **cargo-deny.**
   - The dependency graph is restricted to the shipped Linux targets, `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`.
   - Banned crates: `subsecond`, `dioxus-cli-config`, `dioxus-devtools-types`, `async-tungstenite`, `tracy-client`, `tracing-tracy`, `scap`, `gpui-pre-reqwest`. GPUI's `hot-reload` and `tracy` features are therefore never enabled.
   - No unused `allow-git` entry. This amends ADR-0002 §6: the git allow-list holds only sources actually used; re-adding a git source for GPUI needs a new ADR.
   - NineSQL code never calls GPUI's `set_http_client` / `with_http_client`.
6. **Release builds.** `.deb` and Flatpak release builds ([ADR-0010](0010-packaging-and-distribution.md)) use vendored dependencies (`cargo vendor`) in a clean build environment where `GPUI_LEAK_DETECTION` and `ZTRACING` are not set.
7. **Secrets.** UI code never uses GPUI's credentials API (`read_credentials`, `write_credentials` and related calls). `ninesql-secrets` is the only path to the keyring (ADR-0005, ADR-0012, I1).
8. **Exit plan.** If upstream stops publishing or trust in the publisher is lost, NineSQL regenerates the snapshot from Zed with the Apache-2.0 `bump-gpui.ts` script under a crate name NineSQL controls, or forks `gpui-component`. Either path goes through a new ADR.

**When the controls apply.** Points 1 to 5 and 7 must be in place before `gpui-kit` becomes a normal (non-dev) dependency of `ninesql-ui`, `ninesql-grid` or `ninesql-editor` in phase 1. Point 6 applies from the first release build.

**Rust version.** GPUI needs Rust 1.88 (let-chains). The workspace floor of 1.89 set by ADR-0012 already covers it.

## Consequences
- The UI builds against one consistent GPUI lineage, the one `gpui-component` is developed and tested against.
- Each bump costs more: the verification script, an owners check, an audit entry and a security review. Bumps are expected to be infrequent.
- Trust in a single publisher is mitigated, not removed: exact pins and `Cargo.lock` checksums fix the bytes, and the diff against Zed and the `gpui-component` tag shows what changed. A malicious release would still have to be caught at review time.
- The graph we ship is not the graph Zed tests. GPUI regressions specific to the snapshot (for example in `wgpu` 30.0.x or the crates.io `calloop` / `async-task` / `oo7`) are ours to detect; the spike app and its CI job stay the regression gate.
- Follow-up tasks:
  - write `scripts/verify-gpui-snapshot.sh`;
  - set up `cargo vet` or the audit log;
  - apply the exact pins, `--locked`, target restriction and bans in `Cargo.toml`, `deny.toml` and CI;
  - add a vendored, clean-environment release build to the packaging tasks.
- The 8 "unmaintained" advisory ignores are re-evaluated on every bump.

## Rejected alternatives
- **Keep the git pin of `zed` + `gpui-component` (ADR-0001 §2)**: no longer buildable without two incompatible `gpui` crates in the graph.
- **Zed's own crates.io `gpui` releases**: published by the `zed-industries` team, but too old for any current `gpui-component`.
- **Switch to crates.io without an ADR, as ADR-0001 §3 allowed**: §3 assumed releases from the same publisher as the git source. A third-party, single-account publisher changes the trust model, so the controls above must be recorded as a decision.
- **Fork `gpui-component` and regenerate GPUI ourselves now**: removes the third-party publisher, but means maintaining a snapshot pipeline and a widget library before phase 1 starts. Kept as the exit plan (point 8).
- **Another UI toolkit**: already rejected in ADR-0001. The P0-A spike gave a GO for GPUI and `gpui-component`.
