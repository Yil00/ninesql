# ADR-0017: Workspace lock, rewrite-on-load and project identity

- **Status**: Proposed. Amends [ADR-0008](0008-workspace-and-config-storage.md) Decision §1 (a lock file in `$XDG_DATA_HOME/ninesql`, a stable id in `project.json`), §2 (contents of migration backups) and §3 (single writer); amends [ADR-0012](0012-secret-service-client-and-msrv.md) Decision §4 (two more item attributes). Every other point of both ADRs stands.
- **Date**: 2026-11-14

## Context
The P0-D security review deferred several storage items to "before P1-D" ([ROADMAP](../ROADMAP.md#phase-0-follow-ups)). They landed in phase 1, wave 1, in `ninesql-storage`, `ninesql-core` and `ninesql-secrets`. Some of them change behavior that ADR-0008 and ADR-0012 describe:

- **Two writers (L8).** ADR-0008 §3 makes each write atomic, but nothing stopped two NineSQL processes from loading the same workspace and overwriting each other's changes.
- **Plaintext found on load (N7.2).** When a workspace file holds a secret-looking value (a hand edit, a bug, an old build), the loader removed it from memory, but the file kept it on disk until the next save, which may never come. The same applied to a file that fails validation: it is kept as is for the user to fix, secret included.
- **Migration backups (`.bak` scrub).** ADR-0008 §2 keeps a `.bak` of the previous file on migration. A byte copy keeps any secret the old file held. JSON parsers keep only the last of duplicate keys, so a value hidden under a duplicate key can survive a scrub that only looks at the parsed value.
- **Keyring scope (N6).** Secret Service items were scoped by the project name, which the user can change; renaming a project lost its secrets, and two projects with the same name could collide.

## Decision
### 1. Single-writer lock
1. A NineSQL process that opens the workspace takes an exclusive, non-blocking `flock(2)` on **`$XDG_DATA_HOME/ninesql/ninesql.lock`** (file `0600`, owned by the user, opened with `O_NOFOLLOW`: a symbolic link or a non-regular file is refused).
2. The kernel releases the lock when the process exits or crashes, so a stale lock cannot outlive its owner. The lock file is **never deleted**: deleting it would let two processes lock two different inodes.
3. A second process is **refused** (`StorageError::Locked`), even if it would only read: it is told that another NineSQL uses the workspace. Inside one process, every open of the workspace shares the same lock.
4. On a file system without locks (some NFS or FUSE mounts: `ENOLCK`, `EOPNOTSUPP`), opening fails with `StorageError::LockUnsupported` and a message asking to move the XDG data directory to a local file system. NineSQL does not run unlocked.
5. The lock is advisory: it protects against a second NineSQL, not against other programs.

### 2. Rewrite on load
1. When loading finds a secret-looking key or value in a workspace file, the value is removed (and zeroized in memory) and the file is **rewritten at once**, atomically (ADR-0008 §3), instead of waiting for the next save. The loader reports what it removed as a load issue, never the value.
2. When a file fails validation, it is kept for the user to fix, but secret-looking keys and values are first removed from it by the same atomic rewrite of the raw file.

### 3. Migration backups are scrubbed
1. The `.bak` written before a migration (ADR-0008 §2) is built from a **scrubbed** copy of the original: secret-looking keys stripped, secret-looking values removed.
2. A JSON backup is always **re-encoded from the scrubbed value**, so a duplicate key cannot carry a secret into it.
3. A TOML backup is a byte copy of the original **only when scrubbing changed nothing** (TOML refuses duplicate keys); otherwise it is re-encoded from the scrubbed value.

### 4. Project identity
1. `project.json` moves to `schema_version` **2** and carries a stable **`id`** (UUID v4) generated when the project is created or migrated from version 1.
2. This id, not the project name, is the value of the **`project` attribute** of the project's Secret Service items. Items are written with these attributes, which extend the table of ADR-0012 §4:

   | Attribute | Value |
   |---|---|
   | `xdg:schema` | `org.ninesql.Secret` |
   | `service` | `ninesql` |
   | `project` | Project UUID from `project.json` |
   | `secret_id` | `<connection uuid>:<slot>` |
   | `connection_id` | Connection UUID |
   | `kind` | Slot kind |

   Lookups use `xdg:schema`, `service`, `project` and `secret_id`. The backend refuses a `project` value that is not a UUID.
3. Renaming a project keeps its secrets. Two projects with different ids never read or overwrite each other's items, even with the same connection ids.

## Consequences
- A user who starts NineSQL twice gets a clear refusal instead of silently lost changes. A user whose `$XDG_DATA_HOME` is on a file system without locks cannot start NineSQL until it is moved; this is documented in the error and must be in the user guide (P6-D).
- A workspace file never keeps a plaintext secret after NineSQL has loaded it, and a migration never copies one into a `.bak` (I1).
- **Copying a project directory copies its id**: the copy shares the original's keyring scope (same project id, same connection ids), so editing a password in one changes it in the other, and deleting a connection in one can delete the other's secret. Duplicating a project from NineSQL must give the copy a new id; a copy made outside NineSQL keeps the shared scope. This is a documented limitation.
- Items written by development builds under the project name are not migrated: the Secret Service backend never shipped.
- Tests to keep: lock refusal across processes and `LockUnsupported` mapping; rewrite of a file holding a plaintext secret on load and after a failed validation; `.bak` content after a migration, duplicate JSON keys included; `project.json` v1 → v2 fixture; keyring isolation between two project ids.

## Rejected alternatives
- **A PID file checked at startup**: a crash leaves it behind, and a PID can be reused; the kernel-released `flock` has neither problem.
- **Deleting the lock file on exit**: lets a third process create a new inode while the second still holds the old one.
- **Read-only second instance**: it would show stale data and could still trigger rewrite-on-load or migrations; refusing is simpler and safe.
- **Running unlocked on NFS with a warning**: silently reopens the two-writer problem the lock exists to prevent.
- **Waiting for the next save to drop a plaintext secret**: leaves it on disk for an unbounded time.
- **Keeping the project name as keyring scope and migrating items on rename**: a failed or interrupted migration loses secrets; a stable id avoids the migration.
