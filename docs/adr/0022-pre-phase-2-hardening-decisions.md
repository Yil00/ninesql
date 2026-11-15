# ADR-0022: Pre-phase-2 hardening decisions

- **Status**: Proposed (the maintainer accepts it). Until then, the accepted ADRs it notes ([ADR-0005](0005-secrets-storage.md), [ADR-0017](0017-workspace-lock-rewrite-on-load-and-project-identity.md), [ADR-0021](0021-phase-1-core-secrets-and-storage-decisions.md)) are unchanged.
- **Date**: 2026-11-15

## Context
The P1-E end-of-phase review listed items to land before the DBeaver import flow (P2-B) and its persistence (P2-C) are merged ([ROADMAP](../ROADMAP.md#phase-1-follow-ups)). They were merged on 2026-11-15 from the branches `fix/p2-pre-classifier` and `fix/p2-pre-core`, each reviewed by `security-reviewer`. Three of the choices made there touch accepted ADRs without contradicting them. This ADR records them as implemented; it changes no behaviour. The classifier decisions of the same hardening are in the [ADR-0007 amendment](0007-production-safety-model.md#amendment-classifier-2026-11-15).

Invariants involved: I1 (no secret in clear), I3 (confirmations on production), I4 (DBeaver files never modified). The threat behind §1: a crafted DBeaver workspace can type a connection "dev" so that its init SQL skips the confirmation the type would otherwise ask for.

## Decision
### 1. `connections.json` schema 4: the imported init SQL marker (notes ADR-0021 §3)
1. `connections.json` moves to `schema_version` **4**, which adds the optional `init_sql_needs_confirmation` of a connection (`Connection::init_sql_needs_confirmation`): `false` when absent, written only when `true`.
2. While it is set, every init statement is flagged at connect (`ConfirmReason::Imported`, unless the type's own rule flags it first) and `InitSqlConfirmation::imported` is `true`, whatever the connection type. The `InitSqlToken` binds whether it was issued with the imported notice (`InitSqlKey`).
3. The core does not trust the request alone: when it has a workspace, a request with init SQL but without the marker is checked against `connections.json`; a saved connection with the marker, with other init SQL than the request, or a failed read counts as marked (fail-closed).
4. The marker is cleared on disk when the confirmation token is redeemed, before the driver connects, and only if the init SQL on disk is still the confirmed one (`Workspace::clear_init_sql_marker`, outcome `MarkerClear`). The core reports it with `CoreEvent::ImportedInitSqlConfirmed { connection, statements, cleared }`.
5. Migration 3 → 4 rewrites nothing, but **keeps** an `init_sql_needs_confirmation` key already present (a hand edit), and reads any non-boolean value as `true`: the marker can only add a confirmation, never weaken a connection. Earlier migrations dropped keys they did not expect; this one does not, for that reason.
6. The version 3 file is kept as `connections.json.bak` (scrubbed, ADR-0017 §3). An older NineSQL **refuses** a schema 4 file instead of dropping the marker; downgrading means restoring `connections.json.bak` and losing the changes made since.
7. Only the P2-C import sets the marker; the UI never sets it, and keeps it on an edit only while its own copy still has it. Details: [docs/02](../02-architecture.md#confirmations-adr-0007-adr-0011).

### 2. The core writes `connections.json` from the session manager (notes ADR-0017 §1)
1. Until now only the UI save flows wrote `connections.json`. Clearing the marker makes the session manager a second writer, from the driver runtime.
2. Every read-modify-write and save of one `connections.json` runs under a **process-wide guard** keyed by the file's canonical path and shared by every `Workspace` of the project in the process (`connections_guard_for` in `crates/ninesql-core/src/workspace.rs`). A save from the UI and a marker clear from the core cannot interleave.
3. Across processes, the single-writer workspace lock of ADR-0017 §1 still applies; this guard adds the in-process ordering that the lock does not give.
4. Load issues found by the clear's read-modify-write are fixed on disk and logged, as on every load (ADR-0017 §2), but not returned to the UI.

### 3. Master-password minimum (notes ADR-0005 §2)
1. A **new** master password, at vault creation or at a master-password change, must be at least **12 characters** (Unicode scalar values, `MIN_MASTER_PASSWORD_CHARS`) and not made only of whitespace. Otherwise `Vault::create` and `change_master_password` fail with `VaultError::PasswordTooShort` or `VaultError::EmptyPassword`, before any key derivation.
2. Unlocking an existing vault is not affected: a vault created earlier with a shorter password still opens with it.
3. The vault prompt shows the rule when creating a vault and checks it before calling the core. Reason: once `vault.bin` is copied off the machine, the master password and Argon2id are its only protection ([docs/05](../05-security.md)).

## Consequences
- The P2-C import must set the marker on every imported connection with non-empty init SQL, set it again on a re-import that changes or re-applies init SQL, write through a path that keeps it, never take its value from the DBeaver file, set it in the request of a test-before-save, and add an end-to-end test (ROADMAP, P2-C rows).
- A UI copy of a connection can fall behind the file when a clear returns `cleared = Some(false)`; the hub then keeps the marker and asks again at the next open (follow-up: reload).
- Users who pick a short master password are refused at creation; no existing vault is locked out.
- Each new `connections.json` schema still needs a migration with a fixture (`crates/ninesql-core/tests/fixtures/connections/v4.json`).

## Rejected alternatives
- **Use the connection type to decide whether imported init SQL is confirmed**: the type comes from the file being imported, which is what an attacker controls.
- **Clear the marker only after a successful connect**: a connect that then fails (wrong password, server down) would ask again for statements the user already approved, without adding a check.
- **Rely on the cross-process workspace lock alone**: it does not order two writers inside one process.
- **A longer minimum or a strength estimator**: a fixed length is predictable and testable; a strength estimator adds a dependency and rules users cannot see.
