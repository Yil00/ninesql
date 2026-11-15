# ADR-0023: Phase 2 import decisions

- **Status**: Proposed. It adds notes to [ADR-0006](0006-dbeaver-interoperability.md) (§3 connection types, §8 re-import) and [ADR-0007](0007-production-safety-model.md) (§1 connection types, §5 manual commit for production) without rewriting their decisions, and builds on [ADR-0021](0021-phase-1-core-secrets-and-storage-decisions.md) and [ADR-0022](0022-pre-phase-2-hardening-decisions.md) (`connections.json` schemas 3 and 4).
- **Date**: 2026-11-15

## Context
Phase 2 landed the DBeaver import on 2026-11-15: the P2-A follow-ups in `ninesql-dbeaver`, the core import service (P2-C, `crates/ninesql-core/src/import.rs`), the UI flow (P2-B, `crates/ninesql-ui/src/import/`), then the follow-ups of the P2-D end-of-phase review (verdict: "phase 2 can close with follow-ups"). The P2-C task report and its security review rounds listed several decisions that are security-relevant or touch accepted ADRs, and the P2-B report listed one deviation from the layering described in [docs/02](../02-architecture.md#dbeaver-import-p2-c). This ADR records them as implemented; it changes no behaviour. The details are in [docs/02](../02-architecture.md#dbeaver-import-p2-c), [docs/05](../05-security.md#dbeaver-import-p2-c) and [docs/06](../06-dbeaver-interop.md#persistence-in-the-core-p2-c); the rustdoc is the reference.

Invariants involved: I1 (no secret in clear), I2 (no traffic outside the declared route), I3 (read-only and production confirmations), I4 (DBeaver files never modified), I5 (TLS verified by default). The threat model: the DBeaver workspace is untrusted input. A crafted or edited workspace can claim a "dev" type for a production server, weaken TLS, point a known connection id at another host to collect a saved password, or carry a route NineSQL cannot reproduce.

## Decision
### 1. `connections.json` schema 5
1. `connections.json` moves to `schema_version` **5**, which adds, per connection, the optional `default_schema`, `default_catalog` (DBeaver's `bootstrap.defaultSchema` / `defaultCatalog`), `auto_commit` (a per-connection override of the type's default, §5), `disabled_reason` (§3) and, in a DBeaver origin, `source_workspace` (the canonical path of the workspace it was imported from, §4).
2. The 4 → 5 migration rewrites nothing and **removes** these keys if a version 4 file has them (hand edits made before schema 5 existed); unlike the marker of ADR-0022 §1.5, they are not kept. The version 4 file is kept as `connections.json.bak` (scrubbed, ADR-0017 §3). Fixture: `crates/ninesql-core/tests/fixtures/connections/v5.json`.
3. An older NineSQL **refuses** a schema 5 file. Downgrading means restoring `connections.json.bak` and losing the connections created, edited or imported since.
4. `default_schema` and `default_catalog` are stored but **not applied** by the drivers: the driver API has no field for them. When one is added (reviewed driver-API PR), each driver binds the value or quotes it as an identifier with its dialect; it is never interpolated, since it comes from an imported file.

### 2. Connection types are only ever tightened, built-ins included (notes ADR-0006 §3, ADR-0007 §1)
1. The connection types of every imported project (and every detected workspace) are folded **tighten-only**: a type id defined several times keeps the stricter value of each rule (auto-commit, confirmations, read-only, query-text logging).
2. Against the project's types, an import creates a missing custom type, keeps an existing type that is at least as strict, or makes it **stricter**, never weaker; its name and color are kept. This applies to the built-in Development, Test and Production types too.
3. Tightening a built-in type is **reported**: the preview highlights the line and says how many saved connections of that type it changes, and the import report has an entry. Reason: DBeaver's `test` confirms data changes; keeping NineSQL's looser built-in would let imported connections lose a confirmation their user relied on.
4. This reverses the earlier wording that imported types never change a built-in type (P1-E follow-up rows and an earlier version of docs/06). Relaxing a tightened built-in has no UI yet (follow-up).

### 3. Disabled reasons: `DisabledReason`, `try_enable`, `route_fixed`
1. A connection the import cannot run safely is saved **disabled** with a persisted `disabled_reason` of kind `no-driver` (no driver for its engine in this build), `invalid` (its driver refuses the settings) or `route` (a network profile, an unknown handler such as a proxy, or an SSH forwarding target that NineSQL cannot reproduce), with a message.
2. While a reason is set, `enabled` must be `false` (`Connection::validate`). `Workspace::save_connections_checked` and `update_connection` refuse any change of the reason the file holds: dropped, added, another kind or another message. `Duplicate` copies `enabled` and the reason.
3. Only `Workspace::enable_connection(…, route_fixed, registry)` clears a reason, through `Connection::try_enable`: the driver must accept the connection (its properties are checked against the driver's allowlist at that point), and a `route` reason also needs `route_fixed = true`. The UI passes `route_fixed = true` only for a `route` reason and only after the user ticked "I set up the route again in NineSQL (SSH tunnel or network) and checked it". Reason (I2): enabling such a connection without its route would reach the server another way than the user configured.

### 4. Re-import: match and merge rules (notes ADR-0006 §8)
1. A connection is updated in place only when its origin matches the DBeaver id, the DBeaver project **and** the canonical source workspace. A match on the id alone (another workspace or project, or a connection imported before schema 5) is reported and imported as a **new** connection. ADR-0006 §8 says `origin.dbeaver_id`; the id alone is not enough, because a crafted workspace can reuse a known id.
2. Kept from the saved copy: id, name, description, folder, password-exchange policy, a "disabled by the user" state, and a stricter read-only or manual-commit override.
3. The saved **connection type is kept unless the imported type is at least as strict on every rule**.
4. **TLS kept unless accepted** (I5): a weaker TLS mode or another CA file from DBeaver is not applied; the saved setting stays unless the user accepts the change for that connection (`ImportChoices::accept_tls_changes`), and the imported settings then apply only if the driver accepts them. Accepting drops the saved database password (rule 5). A test-before-save uses the kept setting.
5. **Secret binding** (I1, I5): a secret saved in NineSQL survives a re-import only while everything it authenticates against is unchanged: for the database secret, engine, host, port, user, TLS mode and CA file, and SSH route; for an SSH secret, the hop's host, port, user and key file and the hops in front of it; for a client key, the certificate and key files and the host. Otherwise it is dropped, reported, and deleted after the write; the field is asked at connect. A non-strict host-key policy is never carried across a route change (the hop is strict again, reported).
6. Imported secrets are saved under **fresh** references `<connection id>:<slot>:<uuid>`; the references the connections no longer use are deleted only after the bulk write of `connections.json`, so a failed apply loses no saved secret. `ImportPreview::connection_for_test` puts every credential on "ask": a test never sends a saved or imported secret to settings the user has not accepted.
7. The imported init SQL marker of ADR-0022 §1 is set on every imported connection with init SQL, again on every re-import with init SQL, never taken from the DBeaver file, and set in the request of a test-before-save.

### 5. Production auto-commit is tighten-only (notes ADR-0007 §5)
1. The per-connection `auto_commit` override can always ask for manual commit (`Some(false)`); `Some(true)` never turns auto-commit on for a production type (`Connection::policy`), the same rule as the `read_only` override.
2. DBeaver's `bootstrap.autocommit` is imported into that override with a report entry; on a production type it therefore never relaxes the manual commit of ADR-0007 §5. A re-import keeps a saved `auto_commit = false`.

### 6. Write paths keep the on-disk marker
1. `Workspace::save_connections_checked` and `update_connection` read `connections.json` under the process-wide write guard (ADR-0022 §2) and OR the on-disk `init_sql_needs_confirmation` into the written connection whenever its init SQL is unchanged, even when the caller's copy (the UI hub's) lost the marker: a stale UI copy can add a confirmation, never remove one.
2. Imported connections are written by `Workspace::save_imported`, one checked bulk write that validates every connection against its driver and refuses (`SourceChanged`) when a connection to update is no longer on disk as planned.

### 7. Apply re-runs the preview instead of comparing checksums
1. `apply_import` reads the DBeaver source again and computes the preview again; it refuses with `ImportError::SourceChanged` if the result differs from the preview the user approved (DBeaver files or saved NineSQL connections changed). The UI then previews again and says so.
2. Reason: a checksum of the DBeaver files would miss a change of the saved NineSQL connections, of the connection types, or of the registered drivers, all of which change what the import writes; comparing the computed plan covers them and approves exactly what was shown. Cost: the source is parsed and decrypted twice, which stays well inside the phase 2 bound (100 connections in under 2 s).
3. The import report is written last and is best effort (P2-D L1): a report that cannot be saved does not fail an apply whose connections are already written; the outcome carries `report_error` and the UI shows it.

### 8. First-run detection from the UI (P2-B deviation, accepted at the P2-D review)
1. [docs/02](../02-architecture.md#dbeaver-import-p2-c) describes the UI reaching DBeaver workspaces only through the core import service. For the first-run offer, the UI's connection hub calls `ninesql_core::dbeaver::detect_workspaces` (the `ninesql-dbeaver` crate re-exported by `ninesql-core`) directly, on the blocking pool.
2. That function only lists folders in the known locations and reads DBeaver's product properties file for the version; it never opens `data-sources*.json` or `credentials-config.json` and decrypts nothing. Reading a workspace still happens only in `CoreServices::preview_import`.
3. The P2-D reviewer accepted it. Follow-up: a `CoreServices::detect_dbeaver()` wrapper, so the UI stops calling the parser crate.

## Consequences
- A crafted workspace cannot weaken a type, TLS, a production auto-commit or a confirmation, cannot move a saved password to another host by reusing an id, and cannot enable a connection whose route was dropped. These rules are covered by the integration tests in `crates/ninesql-core/tests/import.rs` and the unit tests of `crates/ninesql-core/src/import.rs` and `connection.rs`.
- Users may see built-in types become stricter after an import, for every connection of that type, including connections created in NineSQL. The preview and the report say so; relaxing needs a UI that does not exist yet.
- Users of a re-import may have to type passwords again when a host, user, TLS setting or SSH route changed in DBeaver: that is the binding of §4.5.
- Downgrading NineSQL after an import means restoring `connections.json.bak` and losing the import.
- `default_schema` / `default_catalog` are kept but inert until a driver-API field exists; the report says "not applied by the drivers yet".
- The UI depends on `ninesql-dbeaver` through the core re-export until the wrapper of §8.3 exists.

## Rejected alternatives
- **Never touch built-in types** (the earlier wording): imported connections typed `test` or `prod` would lose DBeaver's confirmations or manual commit, silently weakening them.
- **Copy DBeaver's types as new types beside the built-ins**: the DBeaver ids `dev`, `test`, `prod` map to the built-ins (docs/06); duplicating them would split connections of one type across two policies.
- **Match a re-import on `origin.dbeaver_id` alone** (ADR-0006 §8 as written): another workspace or project reusing an id could take over a saved connection and its password.
- **Keep saved secrets on every re-import**: an edited workspace could redirect a saved password to a host it controls, or downgrade TLS under it.
- **Let the user re-enable any disabled connection with a toggle**: a connection disabled for its route would then connect without it (I2).
- **A checksum of the DBeaver files at apply**: misses changes on the NineSQL side (§7.2).
- **Keep `default_schema` out of the model until drivers apply it**: the imported value would be lost, and the report could not say what was kept.
