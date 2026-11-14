# ADR-0012: Secret Service client crate and minimum Rust version

- **Status**: Accepted (maintainer, 2026-11-14). Partially supersedes [ADR-0005](0005-secrets-storage.md): only the choice of the Rust crate that talks to the Secret Service; every other point of ADR-0005 stands.
- **Date**: 2026-11-14

## Context
[ADR-0005](0005-secrets-storage.md) chose the `keyring` crate's Secret Service backend as the client for the primary secret store. The P0-D spike found that this choice cannot pass the dependency policy of [ADR-0002](0002-licensing-and-dependency-policy.md):

- `keyring` 3 pulls `zbus` 4; the store crates of `keyring` 4 pull `zbus` 5 / `secret-service` 5. Both dependency trees trigger RUSTSEC-2026-0370, RUSTSEC-2026-0371 and RUSTSEC-2026-0372 (advisories of type "unmaintained"), so `cargo deny --locked check` fails.
- `deny.toml` keeps an empty advisory ignore list; an ignore needs a reason and a `security-reviewer` review, and ignoring unmaintained crates on the path that carries every saved password is not an acceptable reason.
- The `secret-service` crate version 6 is pure Rust: `zbus` 6, an encrypted Secret Service session implemented with Rust crypto crates, no `libdbus`, `libsecret` or OpenSSL. Only a release candidate (`6.0.0-rc.1`) is published at the time of writing.
- `zbus` 6 requires Rust 1.89. Independently, `arrow` ≥ 58, needed by the driver API result model ([ADR-0004](0004-driver-api-runtime-and-results.md)), requires Rust 1.88. The workspace declared `rust-version = "1.85"`.

## Decision
1. `ninesql-secrets` talks to the Secret Service through the **`secret-service` crate, version 6**, used directly (no `keyring` wrapper), behind the `secret-service` Cargo feature of `ninesql-secrets`.
2. The dependency is pinned to **`=6.0.0-rc.1`** until a stable 6.x release ships; the move to the stable release is its own PR. Until then the release-candidate pin is a tracked risk (see Consequences).
3. **`ctutils` is locked at 0.4.8** in `Cargo.lock` because of RUSTSEC-2026-0385. Unlocking it requires a version that is not affected and a green `cargo deny --locked check`.
4. **Item attributes** written to the Secret Service (these replace the attribute list of ADR-0005 point 1):

   | Attribute | Value |
   |---|---|
   | `service` | `ninesql` |
   | `secret_id` | Lookup key, format `<connection uuid>:<slot>` |
   | `connection_id` | Connection UUID |
   | `kind` | Slot kind |

   Slots: `auth` (database password or token), `ssh` (SSH password or key passphrase), `ssh-jump-N` (the same for jump host N), `tls-client-key` (TLS client key passphrase). The workspace secret reference of ADR-0005 point 3 carries the `secret_id`.
5. The workspace **`rust-version` becomes 1.89** (root `Cargo.toml`). The toolchain stays `stable` (`rust-toolchain.toml` unchanged): 1.89 is the floor that contributors and anyone building from source must have, not the version CI builds with.

## Consequences
- `cargo deny --locked check` passes with the Secret Service client enabled, without any advisory ignore.
- No C library and no `unsafe` code of ours on the secret path, consistent with ADR-0002 and the `forbid(unsafe_code)` rule.
- Release-candidate risk: the `6.0.0-rc.1` API may change before 6.0.0, and a release candidate receives less scrutiny than a stable release. The `security-reviewer` checks the pin at each phase-end review; NineSQL does not ship v0.1.0 on a release candidate without recording that choice explicitly.
- The `ctutils` lock must be revisited when RUSTSEC-2026-0385 is fixed upstream.
- Contributors need Rust ≥ 1.89 (installed with `rustup`). The `.deb` is built in CI with stable Rust and the Flatpak with the Flatpak Rust SDK extension, so the `rustc` shipped by Debian 12 or Ubuntu 24.04 does not affect the release targets; it only matters to someone building from source with the distribution's compiler.
- **Validation pending** (P0-D follow-up): a real round trip (store, look up, delete) against GNOME Keyring and KWallet on Debian 12 and Ubuntu 24.04. Until it is done, the Secret Service backend is not considered validated.

## Rejected alternatives
- **Keep `keyring` and ignore the three advisories in `deny.toml`**: contradicts the dependency policy of ADR-0002 for the component that guards invariant I1.
- **Call `libsecret` through FFI**: adds a C dependency and `unsafe` code, already rejected by ADR-0005.
- **Stay on Rust 1.85 and keep the Secret Service feature off**: rejected by the maintainer; it leaves the primary store of ADR-0005 unavailable and still blocks `arrow` ≥ 58.
