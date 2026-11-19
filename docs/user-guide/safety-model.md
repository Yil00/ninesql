# Safety model

NineSQL connects to databases that matter, often production ones. This page explains, in plain words, what protects you from mistakes and what does not. The full security model is in [docs/05](../05-security.md); the rules behind it are the invariants I1 to I8 of [CONTEXT.md](../../CONTEXT.md#invariants-non-negotiable) and [ADR-0007](../adr/0007-production-safety-model.md).

## Read-only is enforced by the database
When a connection is read-only, NineSQL does not merely grey out buttons: it asks the **database engine itself** to refuse writes for the whole session, checks that it did, and closes the session if the check fails. NineSQL also refuses the statements it recognises as writes before sending them, but that is a second layer: a write it does not recognise (a function that writes, for example) is still refused by the engine.

| Engine | How read-only is enforced |
|---|---|
| PostgreSQL, TimescaleDB, YugabyteDB | A read-only session setting applied before anything runs; checked after **every** statement; a statement that lifts it closes the session |
| MySQL, MariaDB | The same, with the session's read-only mode read back after every statement and set again |
| SQLite | The file is opened read-only, and an authorizer denies every write, attached database and extension; checked after every statement |
| QuestDB | QuestDB has no session read-only mode: a read-only connection is accepted **only** when the server makes the account read-only (QuestDB's read-only user, or a read-only server), checked at connect with a write that must be denied; otherwise refused |
| TiDB | TiDB has no session read-only mode: a read-only connection is accepted **only** on a read-only TiDB instance and for an account that mode covers (not `root`, nor accounts holding `RESTRICTED_REPLICA_WRITER_ADMIN`, `SUPER` or `ALL PRIVILEGES ON *.*`); the instance's mode and the account's roles are read back after every statement |

NineSQL never pretends: where an engine cannot enforce read-only, the read-only connection is refused, not faked ([invariant I8](../../CONTEXT.md#invariants-non-negotiable)).

**Read-only is a guard against mistakes, not a privilege boundary.** An account that is allowed to write can, in principle, turn read-only off by means NineSQL does not see in time (on TiDB, a privileged user lifting the instance's mode; see [Known limitations](known-limitations.md#safety-and-read-only)). For real protection, connect with an account that has only the privileges it needs.

## Production connections
A connection typed **Production** (or any type with the same rules) adds:
- **Confirmations before anything is sent**: destructive statements (`DROP`, `TRUNCATE`, `DELETE` or `UPDATE` without `WHERE`, `ALTER`, `GRANT`, `CREATE OR REPLACE`…), every statement NineSQL cannot classify, init SQL, a save of edited rows and a CSV import. The dialog shows the exact text, its size and, when the end might be out of sight, its last line. One confirmation covers one statement, one save or one import, and is bound to exactly what was shown: if anything changes, or after 10 minutes for a save or an import, NineSQL asks again.
- **Manual commit** for new sessions: your changes stay in a transaction until you commit, which leaves you a chance to roll back a mistake the confirmations did not catch. Closing a tab or disconnecting with an open transaction always asks.
- **No statement text** in logs or in the query history.
- **Visual cues**: the type color in the navigator and the status bar, and a frame around the editor area while the production connection is active.

Types can also confirm **every** write, or turn off statement logging, without being production.

## Writes NineSQL makes for you
- **Row edits**: each generated statement must change exactly one row, identified by the table's key and the original values; otherwise the whole save is undone. Values are always bound, never pasted into the SQL.
- **CSV imports**: one transaction by default ("All or nothing"), not offered when the table cannot roll back. An import commits its own rows, even on a manual-commit session.
- **Exports run reads only**: anything else is refused, and the export runs inside an engine-side read-only transaction where the engine has one (not TiDB, not SQLite).
- **Generated SQL templates are never run**: they open in an editor, with placeholders that fail if you run them unedited.

## What NineSQL never does
1. **It never writes a password in clear to disk**: not in its configuration, logs, history, exports or the DBeaver export. Saved passwords live in the desktop keyring or the encrypted vault (I1).
2. **It never contacts anything but your own connections**: no telemetry, no analytics, no update check, no account (I2).
3. **It never enforces read-only only in the interface**: the engine enforces it, or the read-only connection is refused (I3).
4. **It never modifies, moves or deletes DBeaver files**: the import only reads them, the export only writes a new file you choose, and a database file inside a DBeaver workspace opens read-only (I4).
5. **It never skips TLS or SSH host-key checks by default**: turning a check off is an explicit, per-connection choice, flagged with a warning (I5).
6. **It never loads an unbounded result into memory** and never freezes the window waiting on the network: results stream under a memory budget (I6).
7. **It ships no GPL or proprietary code, no Java runtime and no web view** (I7).
8. **It never fakes a feature an engine does not support**: what a driver does not declare is hidden or disabled (I8).

## The limits of the statement checks
The confirmations and the client-side read-only refusals rely on a **lexical classifier**: NineSQL reads the statement's words, the way the engine's own lexer would, but does not know what your functions do.
- **Production confirms destructive statements, not every write.** An `UPDATE` or `DELETE` is confirmed when it has **no** `WHERE` at its top level. A `WHERE` that filters nothing (`DELETE FROM t WHERE 1=1`, `UPDATE t SET a = 0 WHERE true`) makes it an ordinary write, which is not confirmed; so is a `MERGE … WHEN MATCHED THEN DELETE`, an `INSERT … ON CONFLICT DO UPDATE` or a `REPLACE`. On production, manual commit is your safety net: check the update count and roll back if it is wrong. A connection type can be set to confirm **every** write.
- **A user-defined function or procedure called from a `SELECT` reads as a read.** On a production connection it is not confirmed. On a read-only connection the engine still refuses its writes. Functions with known side effects (sequences, advisory locks, `pg_sleep`, server file access, replication controls, TimescaleDB data functions, MySQL `SLEEP`, `GET_LOCK`, `LOAD_FILE`…) are treated as unknown and confirmed; a function missing from those lists is not ([docs/08](../08-engine-capabilities.md#capability-model)).
- **It errs on the side of asking**: some harmless statements need confirmation on production, for example an `EXPLAIN` with options, MySQL dump headers with conditional comments, or any statement starting with `ANALYZE`.
- **A statement over 64 KiB** is shown in the confirmation as its first and last 24 KiB; the part that makes it destructive can sit in the hidden middle. The reason for the confirmation is still shown.
- **The credential rule of the history and logs is lexical**: a password built at run time by concatenation is not recognised and could be recorded in the history (never for production connections).
- **MySQL**: a script resumed inside an executable comment `/*! … */` may be refused as several statements (it fails closed, nothing unexpected runs).

## What NineSQL does not protect against
- Malware running as your user, which can read an unlocked vault or keyring from memory.
- What the database account is allowed to do: use least-privilege accounts.
- Data you export or copy: once written, the file or the clipboard is yours to protect.
- Someone who replaces `vault.bin` with an older copy: NineSQL does not notice.
- **Someone on the network path of a TiDB connection.** TiDB connections have no TLS. The password is not sent in clear, but the login exchange (`mysql_native_password` or `caching_sha2_password` scramble) can be captured and a weak password guessed offline; queries and results cross in clear and can be read or **altered**, including the answers to the checks that keep a read-only TiDB session read-only. Reach a remote TiDB through an SSH tunnel.
- **Other users of the same machine while an SSH tunnel is open.** The tunnel's local end listens on `127.0.0.1` on a random port, and any local user can connect to it and reach the database through your tunnel. They do not get your credentials (the database still asks them to log in), but on a shared multi-user machine the tunnel gives them a network path they may not otherwise have ([docs/05](../05-security.md#accepted-risks)).

## Next
- [Known limitations](known-limitations.md).
- [docs/05 – Security](../05-security.md) for the threat model.
