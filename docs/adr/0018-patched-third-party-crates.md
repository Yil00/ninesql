# ADR-0018: Patched third-party crates

- **Status**: Accepted (maintainer, 2026-11-14). Amends [ADR-0013](0013-gpui-source-and-supply-chain.md) (adds a supply-chain control for patched crates) and [ADR-0002](0002-licensing-and-dependency-policy.md) (dependency policy: path patches of third-party crates). Every other point of both ADRs stands.
- **Date**: 2026-11-14

## Context
The P0-A spike found that GPUI's X11 input-method client crashes on the first preedit that contains a non-ASCII character ([docs/spikes/p0-a-gpui.md](../spikes/p0-a-gpui.md), IME row). The fault is in two MIT crates pulled in by `gpui-pre-linux`, outside the `gpui-pre` / `gpui-kit` family that ADR-0013 controls:

- `xim-ctext` 0.3.0 decodes unescaped COMPOUND_TEXT as UTF-8 instead of ISO 8859-1 and returns errors on some escape sequences;
- `zed-xim` 0.4.0-zed calls `.expect()` on that result, so the error becomes a panic.

The P1-A follow-up (commit `c2385c4` on branch `feat/p1-a-settings-ime`, not merged into `dev` when this ADR was written) fixes both crates with `[patch.crates-io]` entries in the root `Cargo.toml` that point at copies under `crates/patches/<crate>/`:

- the `crates/patches` directory is excluded from the workspace;
- each copy started as the crates.io archive at the locked version, whose Cargo.lock checksum the `security-reviewer` verified;
- each modified file starts with a `Modified for NineSQL: <reason>` comment;
- the upstream `LICENSE` of `xim-ctext` was copied verbatim from the upstream repository, because the archive ships none;
- an entry was added to [docs/audits/gpui-snapshot.md](../audits/gpui-snapshot.md);
- `ninesql-ui` has a dev-dependency `xim-ctext = "=0.3.0"` and a test that fails if the patch drops out of the graph.

The `security-reviewer` approved the patch, but found that the existing rules do not cover it:

- ADR-0013 controls the GPUI family through exact pins, a snapshot diff and an audit log. It says nothing about patched crates.
- ADR-0002 §6 and `deny.toml` `[sources]` restrict registries and git sources. cargo-deny does not check path sources, so a `[patch]` entry pointing at a local directory passes `cargo deny check` whatever that directory contains.
- `zed-xim` is not anchored. A future 0.4.x release could resolve from the registry instead of the patch, and Cargo would only print a "patch was not used" warning.
- No CI check covers `[patch.*]` entries.

A patched crate is code we ship that no upstream reviewer has seen and that no automated check compares with its source. It needs rules of its own.

## Decision
A third-party crate may be replaced with `[patch.crates-io]` only under the following rules.

a. **Location and starting point.**
   - The copy lives under `crates/patches/<crate>/` and is excluded from the workspace.
   - It keeps the upstream name and version.
   - It starts as the unmodified crates.io archive whose checksum was in Cargo.lock before the patch.

b. **What may differ.**
   - Every modified or added file carries a `Modified for NineSQL: <reason>` header.
   - Nothing else differs from the archive. Changes to dependencies, features or `build.rs` need a new ADR.
   - A license file may be added only if it is copied verbatim from the upstream repository named in the crate's `repository` field.

c. **Audit entry.** The PR adds an entry to [docs/audits/gpui-snapshot.md](../audits/gpui-snapshot.md), or to a general patches audit log if one is created. The entry records:
   - the archive checksum;
   - the changed files, with line counts;
   - the reason for the patch;
   - the license check;
   - any new transitive crate (normally none);
   - the exit condition: the patch is dropped once upstream ships the fix.

d. **CI checks.** CI verifies that:
   - every `[patch.*]` entry matches an allowlist of `crates/patches/` paths;
   - Cargo.lock records every patched crate as a path source: none resolved from the registry, and no "patch was not used" warning;
   - after re-downloading each upstream archive, a diff shows changes only in files carrying the marker.

   A dev-dependency or a test pins every patched version, so that the patch cannot drop out of the graph unnoticed.

e. **Review.**
   - Adding, changing or dropping a patch needs a `security-reviewer` review.
   - A `chore(ui): bump gpui` PR that moves a patched crate re-applies or drops the patch in the same PR.

f. **cargo-deny.** cargo-deny does not cover path sources. Rules a to e are the control for patched crates. `cargo deny check` stays required for everything else (ADR-0002 §6, ADR-0013 control 5).

## Consequences
- Upstream fixes that NineSQL cannot wait for can be shipped. Each one is reviewed, recorded and checked against its archive, and has a planned removal.
- Patches cost more than a plain dependency: an audit entry, a review, a CI check, and re-applying the patch on each bump that moves the crate. Patches should be rare and small, and should be reported upstream.
- The P1-A patch satisfies rules a, b, c and e. For rule d, it had the `xim-ctext` pin only when this ADR was proposed.
- `.github/CODEOWNERS` gets rules for `/crates/patches/` and for the root `Cargo.toml` in the PR that adds this ADR. CODEOWNERS works per file, so the rule covers the whole `Cargo.toml`, not only its `[patch]` section.
- Follow-up tasks:
  - write `scripts/verify-patches.sh` and run it as a step in the required CI (rule d: allowlist, path sources in Cargo.lock, archive diff limited to marked files);
  - anchor `zed-xim`, either with an exact-pin dev-dependency (`zed-xim = "=0.4.0-zed"`) or with the CI check above;
  - if a `log` to `tracing` bridge is ever added, cap the `zed_xim` log target at `warn`: upstream `log::trace!` calls on XIM requests include the text the user types through the IME.
- State at acceptance (2026-11-14, `dev` at `1e46a98`): the P1-A patch landed with phase 1 wave 2. The first two follow-up tasks are done: `scripts/verify-patches.sh` runs as the "Patched crates" step of the `deny` job in `.github/workflows/ci.yml`, and `ninesql-ui` anchors `zed-xim` with an exact-pin dev-dependency (`=0.4.0-zed`) next to the `xim-ctext` pin. The checks the script performs are listed in the [audit log](../audits/gpui-snapshot.md). The third task (log cap) stays conditional.

## Rejected alternatives
- **Fork the crates under a new name and depend on the fork from crates.io**: GPUI depends on the upstream names, so the fork would still need a `[patch]` entry. A new published crate would also add a publisher to trust and maintain.
- **Git patches (`[patch.crates-io]` pointing at a git fork)**: needs a new `allow-git` source, which ADR-0013 control 5 restricts. The patched code would also live outside the repository, out of reach of review and of the archive diff.
- **Wait for upstream**: the crash affects every X11 user who types non-ASCII text through an input method, and no upstream release fixes it at the versions GPUI locks.
- **Work around the crash in NineSQL code**: the decoding and the panic happen inside GPUI's X11 platform code, below any API NineSQL calls.
- **Rely on cargo-deny**: it does not inspect path sources, so it cannot detect what a patch changes.
