# Fuzz targets for the DBeaver decoders

Coverage-guided fuzzing (cargo-fuzz, libFuzzer) of the two untrusted-input entry points of `ninesql-dbeaver` (ADR-0006: total parsing, never a panic):

| Target | Input | Entry points |
|---|---|---|
| `data_sources` | raw bytes as a `data-sources*.json` file | `parse_data_sources`, then `mapping::map_sources` |
| `credentials` | raw bytes as a `credentials-config.json` file | `decrypt_credentials`, then `mapping::map_sources` against a fixed data-sources file |

The same entry points are fuzzed on stable by `../tests/fuzz_decoders.rs` (proptest), which runs in CI.

## Running

This directory is its own Cargo workspace (libFuzzer stays out of the main workspace, its lockfile and its `cargo deny` scope). It needs a nightly toolchain:

```sh
rustup toolchain install nightly --profile minimal
cargo install cargo-fuzz --locked
cd crates/ninesql-dbeaver/fuzz
cargo +nightly fuzz run data_sources -- -max_total_time=300
cargo +nightly fuzz run credentials -- -max_total_time=300
```

To seed the corpora with the P0-E fixtures (synthetic content, see `../tests/fixtures/README.md`), copy `../tests/fixtures/*/workspace6/General/.dbeaver/data-sources.json` into `corpus/data_sources/` and the `credentials-config.json` files into `corpus/credentials/` before the first run.

`corpus/`, `artifacts/`, `coverage/`, `target/` and `Cargo.lock` are git-ignored: corpora are never committed. A crash found by a run is turned into a regular test in `../tests/` (with synthetic data only) before it is fixed.

## Results

| Date | Commit base | Target | Duration | Runs | Crashes | Final corpus | Coverage (edges / features) |
|---|---|---|---|---|---|---|---|
| 2026-11-14 | `dev` at `5174009` + the P2-A follow-ups (L-a, L-b, ADR-0016 §1.2) | `data_sources` | 300 s | 1 254 742 | none | 2 296 inputs, 12 MiB (seeded with the 5 fixture files and 1 synthetic jump-host file) | 5 090 / 17 890 |
| 2026-11-14 | same | `credentials` | 300 s | 1 655 497 | none | 266 inputs, 1.1 MiB (seeded with the 5 fixture files) | 1 799 / 3 070 |
| 2026-11-15 | `feat/p2-a-import-mapping` at `9e82c71` (`dev` at `4373a65` + the driver property mapping) | `data_sources` | 1 501 s | 8 567 103 | none | 4 389 inputs, 6.6 MiB (seeded with the 5 fixture files and 1 synthetic file exercising `options`, `sessionVariables`, the Connector/J time zone, sqlite-jdbc properties and every `bootstrap` key) | 5 798 / 23 774 |
| 2026-11-15 | same | `credentials` | 1 501 s | 10 765 455 | none | 433 inputs, 105 KiB (seeded with the 5 fixture files) | 1 780 / 3 270 |

Environment: rustc 1.102.0-nightly (2026-11-13), cargo-fuzz 0.13.2, one libFuzzer process per target (the two targets ran side by side on a 4-core machine), `cargo fuzz run -O … -- -max_total_time=… -rss_limit_mb=2048`, x86_64 Linux. Peak RSS: 589 MB (`data_sources`) and 456 MB (`credentials`) in the 2026-11-15 runs; slowest unit under 1 s.

Limits of these runs: the 2026-11-14 runs (five minutes each) were smoke runs; the 2026-11-15 runs (25 minutes each, the P2-A follow-up campaign) are the first longer campaign. Coverage was still growing slowly at the end of the `data_sources` run, so a longer or repeated campaign on the saved corpus is still worth running before the P2-D review. Random bytes rarely pass the AES-CBC padding check of `credentials-config.json`, so the `credentials` target mostly exercises the decryption and error paths (its coverage barely moved after the first minutes); the proptest suite covers validly encrypted plaintexts.
