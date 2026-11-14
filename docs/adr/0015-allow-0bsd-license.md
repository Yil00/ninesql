# ADR-0015: Allow the 0BSD license

- **Status**: Accepted (maintainer, 2026-11-14). Amends [ADR-0002](0002-licensing-and-dependency-policy.md) Decision §2 (license allow-list): `0BSD` is added. Every other point of ADR-0002 stands.
- **Date**: 2026-11-14

## Context
[ADR-0002](0002-licensing-and-dependency-policy.md) §2 lists the licenses allowed for crates linked into the shipped binary and states that anything else needs a new ADR.

P1-A makes `gpui-kit` a normal (non-dev) dependency of `ninesql-ui` ([ADR-0013](0013-gpui-source-and-supply-chain.md)). Through `gpui-component` 0.9.1 this brings two crates licensed `0BSD`:

- `enum-iterator` 2.3.0
- `enum-iterator-derive` 1.5.0

0BSD (BSD Zero Clause License) grants permission to use, copy, modify and distribute the software, with or without fee. It has no attribution, notice or source-disclosure condition. It is OSI and FSF approved, and strictly more permissive than MIT and ISC, which ADR-0002 already allows.

Distributing NineSQL under Apache-2.0 with 0BSD code linked in is compatible; invariant I7 holds. The `security-reviewer` reviewed the license files of both crates and approved.

## Decision
1. `0BSD` is added to the license allow-list of ADR-0002 §2.
2. Implementation: `"0BSD"` is added to `[licenses] allow` in `deny.toml`, with a comment referencing ADR-0015 (done on the P1-A branch).
3. Any other license still needs a new ADR, as ADR-0002 §2 says.

## Consequences
- `cargo deny check licenses` passes with `gpui-kit` as a normal dependency.
- Any future crate under 0BSD is accepted without further review of its license. Given that 0BSD imposes no condition, this adds no compliance work; 0BSD crates do not need an entry in the third-party notices, though listing them is harmless.
- Follow-up: the license check currently covers only non-dev dependencies. Consider `include-dev = true` in the `[licenses]` section of `deny.toml` so that test-only crates are license-checked too (tracked in the [ROADMAP](../ROADMAP.md#phase-1-follow-ups)).

## Rejected alternatives
- **Per-crate `[[licenses.exceptions]]` entries**: 0BSD is strictly more permissive than licenses already allowed, so a per-crate exception protects nothing and adds churn on every new 0BSD crate or version.
- **Drop `gpui-component`**: it is the widget library chosen in ADR-0001 and ADR-0013 and validated by the P0-A spike; replacing it over a permissive license is out of proportion.
