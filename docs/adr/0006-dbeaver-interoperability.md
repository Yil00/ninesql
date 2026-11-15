# ADR-0006: DBeaver interoperability

- **Status**: Accepted. Note **proposed** (2026-11-15) in [ADR-0020](0020-phase-1-driver-decisions.md) §8: a database file inside a DBeaver workspace is opened read-only, and by sqlite immutable, at the cost of pages held only in a `-wal` file or a hot journal (point 1, I4); the decision above is unchanged.
- **Date**: 2026-11-14

## Context
Adoption depends on switching cost. DBeaver users have dozens of connections, folders, SSH tunnels, connection types and saved passwords. DBeaver stores them per project in `<workspace>/<project>/.dbeaver/` (`data-sources*.json`, `credentials-config.json`, `project-settings.json`), with SQL scripts under `<project>/Scripts/`. On Linux the default workspace is `~/.local/share/DBeaverData/workspace6/`; Flatpak and Snap installs use their own sandboxed paths. DBeaver Community is Apache 2.0, so its formats can be read and documented.

## Decision
1. **Import is one-way and read-only** (I4): NineSQL opens DBeaver files read-only, never writes, moves or deletes them, and never runs while holding a lock on them.
2. **Detection**: look in the known locations (native, Flatpak `~/.var/app/io.dbeaver.DBeaverCommunity/…`, Snap `~/snap/dbeaver-ce/…`), and let the user pick any workspace or project folder.
3. **Scope of the import (MVP)**: connections (engine, host, port, database, user, URL, driver properties that map to NineSQL options), folders, connection types (name, color, auto-commit, confirm execution, read-only), SSH tunnels (host, port, user, auth method, key path, jump hosts where present), TLS settings, bootstrap queries / init SQL, saved passwords, and SQL scripts (copied into the NineSQL workspace, optional).
4. **Credentials**: `credentials-config.json` is decrypted in memory with DBeaver's documented scheme (AES, key from the DBeaver source), and each secret goes directly to `ninesql-secrets` (ADR-0005). If the file is protected by a DBeaver PRO master password or the format is not recognized, NineSQL imports the connections without passwords and says so. The import report recommends deleting DBeaver's saved passwords once the user has switched; NineSQL never does it itself.
5. **Engines outside the v1 set** (ADR-0003) are imported as disabled connections, kept for a later version, and listed in the report.
6. **Import report**: for every connection, the fields mapped, the fields not supported (with the DBeaver key), and the secrets moved. Nothing is dropped silently.
7. **Export to DBeaver** (post-MVP, P5): writes a **new** `data-sources.json` (never into an existing DBeaver workspace) for the selected connections, **without passwords**, so colleagues on DBeaver can import it.
8. **Re-import** is idempotent: connections keep their DBeaver id in `origin.dbeaver_id`, so a second import updates or skips instead of duplicating, after a preview.
9. Format details are documented in [docs/06-dbeaver-interop.md](../06-dbeaver-interop.md) and verified against fixtures produced by real DBeaver versions (anonymized). Anything not verified is marked as such there.

## Consequences
- The importer is untrusted-input parsing: total parsing, fuzz targets, no panic (security review required).
- DBeaver format changes between versions are expected: fixtures are tagged with the DBeaver version that produced them, and CI runs them all.
- The import flow is the first thing a new user sees: its UX is part of the MVP exit criterion.

## Rejected alternatives
- **Two-way sync with a live DBeaver workspace**: risks corrupting the user's DBeaver setup and contradicts I4.
- **Reading DBeaver's Eclipse preference store for everything**: fragile; only the files listed above are in scope.
- **Leaving passwords behind**: safer for us, but most users would then re-type dozens of passwords, and many would give up.
