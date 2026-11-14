# GPUI snapshot audit log

Checked-in audit trail required by [ADR-0013](../adr/0013-gpui-source-and-supply-chain.md) control 4 until `cargo vet` is in place. Every `chore(ui): bump gpui` PR, and any change that makes the `gpui-kit` family a normal dependency of a NineSQL crate, adds an entry **at the top**: the `scripts/verify-gpui-snapshot.sh` result with the summary of changed files, the Cargo.lock checksums of the pinned crates, the crates.io owners (with any change since the previous entry flagged), and the date.

## 2026-11-14 – gpui-kit 0.9.1 / gpui-pre 0.3.12 becomes a normal dependency of `ninesql-ui` (P1-A)

- **Pin**: `gpui-kit =0.9.1` (= `longbridge/gpui-component` tag `v0.9.1`, commit `39ab849a93b5f15507b969164d49891db8d5b470`), which pins `gpui-pre =0.3.12` (snapshot of `zed-industries/zed@d2a4fb6ded7c470be25d1541281da3882495b65b`). Same versions as the P0-A spike ([docs/spikes/p0-a-gpui.md](../spikes/p0-a-gpui.md)); no version moved.
- **Change**: `gpui-kit` (features `component`, `assets`, `tree-sitter-sql`) and its exact-pinned siblings move from `[dev-dependencies]` to `[dependencies]` of `ninesql-ui`, which now builds the `ninesql` binary. Tests use GPUI's own `test-support` feature through a dev-dependency on `gpui-pre =0.3.12` (also exact-pinned in the workspace), not `gpui-kit/test-support`, which would add the optional `gpui-fast` family to Cargo.lock through weak features.
- **New in Cargo.lock**: `gpui-pre-bench-metrics 0.3.12` and its dependencies (`criterion`, `perf-event2`…), pulled by the weak `bench_metrics?/test-support` feature of `gpui-pre`. Dev / test only: not linked into the `ninesql` binary.
- **ADR-0013 controls in place**: 1 (exact pins, `--locked` in CI), 2 (this verification), 3 (owners below), 4 (this log), 5 (`deny.toml`: Linux targets only, bans, no git source; CI grep for `set_http_client`, `with_http_client`, `read_credentials`, `write_credentials`, `delete_credentials`), 7 (no GPUI credentials API in NineSQL code). Control 6 (vendored, clean-environment release builds) applies from the first release build.

### `scripts/verify-gpui-snapshot.sh` (run on 2026-11-14)

Result: **all 27 crates match their sources** (`verify-gpui-snapshot: all crates match their sources`, exit 0). Every Cargo.lock checksum matched its `.crate` archive. Changed files, each carrying a `Modified for gpui-pre` marker:

| Crate | Source | Changed files |
|---|---|---|
| `gpui-pre` | `zed@d2a4fb6` `crates/gpui` | 1 (`src/action.rs`: `actions!` derive paths made crate-relative) |
| `gpui-pre-macros` | `crates/gpui_macros` | 2 (`src/gpui_macros.rs`, added `src/gpui_pre_facade_paths.rs`: macros resolve `gpui` through a facade) |
| `gpui-pre-wgpu` | `crates/gpui_wgpu` | 3 (`wgpu_atlas.rs`, `wgpu_context.rs`, `wgpu_renderer.rs`: adapted to `wgpu` 30.0.1) |
| `gpui-pre-apple` | `crates/gpui_apple` | 1 (`build.rs` reads the vendored GPUI sources, which are identical to `crates/gpui/src`) |
| every other `gpui-pre-*` (incl. `gpui-pre-bench-metrics` vs `crates/bench_metrics`) | `zed@d2a4fb6` | 0 |
| `gpui-kit`, `gpui-base`, `gpui-component`, `gpui-component-macros`, `gpui-kit-assets` | `gpui-component` tag `v0.9.1` | 0 |
| `gpui-pre-reqwest` | crates.io `zed-reqwest 0.12.15-zed` | 0 (wasm only, banned from the Linux graph by `deny.toml`) |

### Cargo.lock checksums

| Crate | Version | SHA-256 |
|---|---|---|
| `gpui-base` | 0.9.1 | `f3e9ae5237737cfd110d0ff513fcd9c20f97d98ebabd0a73164068177f0aca4f` |
| `gpui-component` | 0.9.1 | `e155c4914bd809d966660610d596f7b57c5cd38979ca2c132215057a91f23f03` |
| `gpui-component-macros` | 0.9.1 | `9676a5d12c59ce295b5c82cc18365ad8b96cb683682e2e74668a56fb92544300` |
| `gpui-kit` | 0.9.1 | `aca1dedfea73645b60b5893c9c16de0b1396f2cf4251d19e5f03a295c1b61879` |
| `gpui-kit-assets` | 0.9.1 | `cb5abe0bbd8f9226170c8726e529f6574519e297e9ed1bde86d5fd4d3536079a` |
| `gpui-pre` | 0.3.12 | `29db89613527079737f53752eab27fb65c9a7dd94979256c65a0e590c9fd34d8` |
| `gpui-pre-apple` | 0.3.12 | `a460605ec4e8cbcc9b213462a84b1d6cc3efe4ed8f0e18669659f07b2d5c6aec` |
| `gpui-pre-bench-metrics` | 0.3.12 | `db6a8ff719fdc3da4f5f497b5d39c5545634424073f94943b003c818fc95e1d5` |
| `gpui-pre-collections` | 0.3.12 | `cb60ece401b399fd5ec13911915b407b0c6efa1f6c9a9f66201aaac56c7ec39d` |
| `gpui-pre-derive-refineable` | 0.3.12 | `aeb2a3f5e6c9f300f432ae7989a81b262953ccfb3f9385be51cdc3303aef8858` |
| `gpui-pre-http-client` | 0.3.12 | `55245de7e3b6e219596fd98bb29555d6e3fe44c055d025960affeb11b894c048` |
| `gpui-pre-ios` | 0.3.12 | `10656f6db4bbedb60bd7fcbb8a70f7c7eeee45bc4582fb8933ca807d2ddf52ad` |
| `gpui-pre-linux` | 0.3.12 | `54bab418d0b7c027af9495f278619b46887362b3bd6446e246c87c5b0fa167df` |
| `gpui-pre-macos` | 0.3.12 | `2de7d3f4ad51ac387e822495db363fcc1188d349793365731d054444c4cb06ee` |
| `gpui-pre-macros` | 0.3.12 | `1c7f67904eced61ed42ae3320a9b4b388aed893f8547fffe9161ec425ad162eb` |
| `gpui-pre-platform` | 0.3.12 | `d2b785bdcbd229f762096aebf4f9ae913bdaa9db49794907cabaf516eb55227b` |
| `gpui-pre-refineable` | 0.3.12 | `4d197569e3b1fd84170f574bb866e95ecbc2625e7ae9443c877150eaf7d53374` |
| `gpui-pre-reqwest` | 0.12.15 | `05be23908e707966824f8c51a609904b7f30739e8d915e120a53c1b995ba964c` |
| `gpui-pre-scheduler` | 0.3.12 | `b1d3fa97eeb4ea233635b756684affe96db9e29fc2af8cdc269268bd7ff649fc` |
| `gpui-pre-shared-string` | 0.3.12 | `63c2ed1e44195079d4461b83a99845aa9a52ce8fcf1516410816670a03d32d94` |
| `gpui-pre-sum-tree` | 0.3.12 | `6085e61458366decd21491396b0b90b3adf928f2ec27e170c4d3c9fba455d536` |
| `gpui-pre-util` | 0.3.12 | `52ead57ff96b5a34e71d64758080347066d667e0c4bd4ff71e85f3a50643fa3e` |
| `gpui-pre-web` | 0.3.12 | `a85e775c6874257c63f9cf51d99f0aba13b3bc615ed8d47e7d5338e3b5941f6a` |
| `gpui-pre-wgpu` | 0.3.12 | `dc76608d385f6d0dcbad44504b1bf669dac3b7d7ffa2885acdb7c1c358caf852` |
| `gpui-pre-windows` | 0.3.12 | `7201762420222d5b5ce5bdf0056996199212a3349bfb2457a3a0238c688681f3` |
| `gpui-pre-ztracing` | 0.3.12 | `1043bbe8d711baf03e774b03bae6521155f017450b866dcaa987a07d978d660a` |
| `gpui-pre-ztracing-macro` | 0.3.12 | `85701631a9bbecf47e6d19f145e85e57767e7c50f7192d5b1bf65b5ef43cb373` |

### crates.io owners

All 27 crates: `huacnlee` (single personal account, no team). **No change** since the P0-A review (2026-11-14). The single-owner residual risk is the one ADR-0013 accepts.
