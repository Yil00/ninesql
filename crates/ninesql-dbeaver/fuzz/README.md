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

Environment: rustc 1.102.0-nightly (2026-11-13), cargo-fuzz 0.13.2, one libFuzzer process per target, `-rss_limit_mb=2048`, x86_64 Linux. Peak RSS stayed near 500 MB.

Limits of these runs: five minutes each is a smoke run, not a campaign. Random bytes rarely pass the AES-CBC padding check of `credentials-config.json`, so the `credentials` target mostly exercises the decryption and error paths; the proptest suite covers validly encrypted plaintexts. Run longer campaigns before the P2-D review.
