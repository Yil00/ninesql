# ADR-0024: Phase 3 execution, history, preferences and results decisions

- **Status**: Accepted (maintainer, 2026-11-15). It amends [ADR-0004](0004-driver-api-runtime-and-results.md) (result model: the memory budget is per execution, "fetch all" never goes above it; §4 below) and [ADR-0008](0008-workspace-and-config-storage.md) (§4 history: nothing for production and for statements holding a credential; §1 `settings.toml` also holds execution preferences written by the core; §1 to §3 below), and adds notes to [ADR-0004](0004-driver-api-runtime-and-results.md) (per-connection timeout, §1) and [ADR-0007](0007-production-safety-model.md) (§5 manual commit and the close confirmation, §6 below). It does not rewrite their decisions.
- **Date**: 2026-11-15

## Context
Phase 3 landed on 2026-11-15: the execution service, sessions with transaction tracking, keep-alive and reconnect, the query history and the typed preferences in `ninesql-core` (P3-C, `crates/ninesql-core/src/execution.rs`, `session.rs`, `history.rs`, `preferences.rs`), the history file in `ninesql-storage` (`crates/ninesql-storage/src/history.rs`), the results grid (P3-B, `crates/ninesql-grid/`) and their integration in `ninesql-ui`. The P3-C and P3-B task reports and their security review rounds listed decisions that are security-relevant, touch accepted ADRs or add a dependency. This ADR records them as implemented; it changes no behaviour. The details are in [docs/02](../02-architecture.md#running-statements-p3-c) and the rustdoc, which is the reference.

Invariants involved: I1 (no secret in clear on disk, history included), I3 (production confirmations, read-only), I6 (the UI never waits, results under a memory budget).

## Decision
### 1. The per-connection timeout lives in `settings.toml` (notes ADR-0004 "Runtime", ADR-0008 §1)
1. ADR-0004 asks for a query timeout "configurable per connection". It is stored in `settings.toml`, under `[execution.connection_timeouts]`, keyed by connection id (`"<uuid>" = <seconds>`, 1 s to 7 days), and not in `connections.json`. A statement's default timeout is its connection's, else `[execution] query_timeout_secs` (`0`, the default, means none, as ADR-0004 says); a request may ask for another one or none. The timeout covers each statement, fetch included.
2. Reason: a schema 6 of `connections.json` (an older NineSQL would refuse the file, ADR-0022 / ADR-0023) for one optional number is not worth it; a timeout is a local preference rather than a property of the server to share with a project. A connection id is not a secret.
3. Consequence: a timeout does not travel with an exported or versioned `connections.json`, and an entry for a deleted connection stays in `settings.toml` until removed (it is ignored).

### 2. History records nothing for production, non-logging types or credential statements (amends ADR-0008 §4)
1. A statement is recorded in `history.sqlite` only when history is enabled, the connection's effective type allows query text in logs (`log_query_text`) **and** the connection is not production. ADR-0008 §4 names only types that disable query logging; production is added explicitly, whatever its type says, in line with ADR-0007 §6. When a statement is not recorded, nothing is: no redacted entry, no timing, no row count. There is no redaction mode.
2. A statement whose text holds a **credential word** or a **URI with credentials** is never recorded, and its text is never logged. The rule is `ninesql_driver_api::hygiene` (`CREDENTIAL_WORDS`: password, passwd, pwd, passphrase, identified, secret(s), credential(s), (un)encrypted, encrypt, decrypt, token, apikey, privatekey, crypt; whole words, ASCII case ignored, any non-alphanumeric character separating words, literals, quoted identifiers and comments included; `scheme://user:password@`). It is **shared** by the core's history and logs and by the drivers' statement logs, so both always skip the same statements. It fails closed: a false positive drops one history entry.
3. The rule is lexical. A credential built at run time (concatenation, dynamic SQL) or a secret typed as a literal in a statement without such a word is not detected; the parameters prompt binds such values instead, and bind parameter values are never recorded (the model has no place for them).
4. The file is created `0600`, opened without following symbolic links, refused when not owned by the user, with `secure_delete` on, `journal_size_limit = 0` and a `wal_checkpoint(TRUNCATE)` after every delete, clear or pruning that removed entries. Retention follows ADR-0008 §6 (10 000 entries per connection, 90 days by default, `[history]` in `settings.toml`).
5. `ninesql-storage` depends on `rusqlite` (MIT, the bundled SQLite of the workspace, public domain) for `history.sqlite`. The `unsafe` code stays in the upstream crate (AGENTS.md); ADR-0008 already chose `history.sqlite`.

### 3. The core writes execution preferences into `settings.toml` (notes ADR-0008 §1)
1. `[execution]`, `[history]` and `[confirmations]` of `settings.toml` belong to `ninesql_core::preferences`; the UI keeps `[appearance]`, `[keybindings]` and `[import]`. The core reads leniently (a missing or invalid key keeps its default and is reported, never echoing a string value) and writes strictly (`CoreServices::save_preferences` validates every change, edits only the changed keys with `toml_edit`, keeps comments, writes atomically, never overwrites a file with a newer `schema_version` or invalid TOML).
2. Both writers take one process-wide lock (`preferences::settings_write_lock`), so a UI write and a core write never interleave a read-modify-write. Across processes, the last write wins (each atomic), as already documented for `settings.toml`.
3. Nothing in these sections weakens ADR-0007: whether statements are confirmed is a property of the connection type; there is deliberately no key for it, and `confirmations.close_with_open_transaction = false` does not apply to production connections.

### 4. Results: one memory budget per execution, retain and never evict (amends ADR-0004 "Result model")
1. Each driver stream still stops at its own `FetchBudget::max_bytes` with `Truncated`. A script runs one stream per statement, so the grid keeps its own account **per execution** (one editor tab: every kept batch and notice of the run), with the same default (256 MiB, `[execution] result_memory_mib`). ADR-0004 says "256 MiB per result tab".
2. **Retain, never evict.** Streams are forward-only and running a statement again may not be repeatable, so the grid never drops a batch it accepted. At the budget, the batch that would cross it is not kept, the result shows "Truncated: memory budget reached", and the grid asks the source to release the stream (server cursor closed).
3. **"Fetch all" never goes above the budget**, so it asks no confirmation. ADR-0004 says "fetch all asks the user above the budget"; there is no way above the budget in the grid. Seeing more means narrowing the query, or an export (`ninesql-transfer`, streamed, P5-C).
4. The grid's actions live in their own namespace, `ninesql_grid::` (key context `ResultGrid`), next to the shell's `ninesql::` and the editor's `ninesql_editor::`; `settings.toml` overrides use the full name (short names are the shell's only).
5. A zoned timestamp is shown as its UTC instant, labelled `UTC` (Arrow stores a zoned timestamp as a UTC instant); the session or desktop time zone is not applied.

### 5. Paging: only reads pause or are released; a paused result keeps the session busy
1. With paging, the first page of a result set is delivered unprompted, then the result pauses with its stream open until the grid fetches more or releases it. A release is a server-side cancel, so **only statements the dialect classifies as reads** pause or are released (on request, or after their first page in a script, every read but the last). Any other statement (a `DELETE … RETURNING`, a call) streams to its end under the budget: cancelling it could roll back or half-apply a change the user asked for.
2. A paused result keeps the session busy: another statement on that session waits or fails with `Busy`, and `close_with(Commit)` answers `Busy` at once instead of queuing. A `commit()` sent while a result is paused waits behind it (follow-up); the UI refuses commit and roll back while a run lasts.
3. After a release or a cancel, only `DriverError::Cancelled` or SQLSTATE `57014` counts as a cancelled statement; any other engine error stays a failure and is recorded as such.

### 6. Transactions (notes ADR-0007 §5)
1. Switching auto-commit **on** over an open transaction is refused (`CoreError::PendingTransaction`): depending on the engine it would commit or roll back silently. The user commits or rolls back first.
2. Closing a tab, deleting or disconnecting a connection with an open transaction asks first: always for production (ADR-0007 §5), for the other types unless `[confirmations] close_with_open_transaction = false`.

### 7. Reconnect: transient failures only, bounded, prompted secrets dropped on lock
1. `SessionManager::reconnect` is a fresh `open` under every rule of the connect flow. It retries **transient** failures only (server or hop unreachable, connect timeout); a secret to ask, a locked vault, a host key to confirm, an authentication failure or a refused certificate ends it at once.
2. It is bounded: 1 to 20 attempts (`[execution] reconnect_attempts`, default 3), delays doubling from 1 s up to 60 s at most, 5 minutes in total.
3. It never re-runs a statement; an open transaction is lost (its last state stays readable on the old handle, so the UI can say so); init SQL is confirmed again under its rules, and a confirmation token is spent by the attempt that reaches the driver.
4. Prompted secrets stay in memory (zeroized when dropped) only while the reconnect lasts. Every secrets lock (lock now, sleep, screen lock, idle lock) ends a reconnect at once with `NeedsSecrets` and drops them (ADR-0005: a lock forgets what was typed).
5. The UI does not start a reconnect yet; `auto_reconnect` is read but not acted on.

### 8. Scripts: resume by offset with the `ScriptState`; a refused piece refuses the whole script
1. A statement that needs confirmation is not sent: the run ends, and the UI runs the same text again from the statement's **byte offset** with the carried `ScriptState` (the MySQL `DELIMITER` in force) and the token. Statements before the offset are not run again; the token goes only to the statement that starts exactly at the offset. Indexes are never used to resume, because they depend on the lexical modes in force when the text was split. A carried `ScriptState` is validated before anything is split (an empty delimiter, whitespace, a control character, a quote, a backslash, `#`, `--` or `/*` refuses the run). Without it, the state is rebuilt from the text before the offset.
2. A refused piece (`COPY … FROM STDIN`, a psql meta-command, a mysql client command, an invalid `DELIMITER`) refuses the **whole run before anything is sent**, found by one lazy pass up front, with its index and position. A refused piece that only a re-split reveals stops the run before its next statement.
3. Every run ends with exactly one `Done` event, from a drop guard if the run task panics.

## Consequences
- History can be relied on not to hold production statements, parameter values or statements with a credential word; it can miss entries the user expected (false positives of the credential rule, production connections). The I1 test checks the raw bytes of `history.sqlite` and its write-ahead log.
- A large result never grows past one budget per tab, and nothing is fetched twice; the user sees a banner instead. ADR-0004's "fetch all asks above the budget" no longer describes the product.
- `settings.toml` now holds machine-written sections; a hand edit and a write from NineSQL can still race across two processes (last write wins).
- To test or document: the reconnect UI when it lands (the rules above apply); the paused-result `commit()` follow-up; the per-connection timeout has no UI yet.

## Rejected alternatives
- **`connections.json` schema 6 for the per-connection timeout**: an older NineSQL would refuse the whole file for one optional local setting.
- **Redacted history entries** (timing and status without text) for production: still a record of activity on production that ADR-0007 §6 means to avoid, and redaction rules for arbitrary SQL are unreliable.
- **A credential rule that reads only SQL syntax** (outside literals and comments): dynamic SQL hides `ALTER ROLE … PASSWORD` inside a literal, and a password can sit in a comment.
- **Evicting old batches at the budget** (a sliding window): scrolling back would need to run the statement again, which may not be repeatable and must never happen silently.
- **Releasing any paused statement**: a release is a server-side cancel, which can roll back or half-apply a write.
- **Resuming a script by statement index**: indexes change when an earlier statement changed the lexical modes; a token could then confirm another statement.
- **Running the statements before a refused piece**: the user would get a half-run script and a refusal in the middle.
