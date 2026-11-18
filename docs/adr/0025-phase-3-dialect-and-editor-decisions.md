# ADR-0025: Phase 3 dialect, editor and UI decisions

- **Status**: Accepted (maintainer, 2026-11-15). It adds to [ADR-0007](0007-production-safety-model.md) §3 and to its [classifier amendment](0007-production-safety-model.md#amendment-classifier-2026-11-15) (§1 below: conditional comments, triggers, rules and events; §1.4 records that `MERGE` and upsert forms stay `Write` as §3 says) and builds on [ADR-0004](0004-driver-api-runtime-and-results.md) (I6: a run is never awaited on the UI thread) and [ADR-0008](0008-workspace-and-config-storage.md) §1 (the session file in `$XDG_STATE_HOME/ninesql`, §4 below). It does not rewrite their decisions. A [dated note on §4.3](#note-on-43-script-files-and-session-restore-wired-2026-11-18) (2026-11-18) records the wiring of script files and session restore. The execution, history, preferences and results decisions of the phase are in [ADR-0024](0024-phase-3-execution-history-and-preferences-decisions.md).
- **Date**: 2026-11-15

## Context
Phase 3 landed on 2026-11-15: script splitting and classifier hardening in `ninesql-driver-api` and the three drivers (P3-D, [docs/08](../08-engine-capabilities.md#script-splitting-and-classification-p3-d-f-sql-18)), the SQL editor (P3-A, `crates/ninesql-editor/`) and its integration in `ninesql-ui` (`crates/ninesql-ui/src/editor_tab/`). The task reports and the security review rounds of P3-D and of the UI integration listed decisions that are security-relevant, touch ADR-0007 or add dependencies. This ADR records them as implemented; it changes no behaviour. The rustdoc is the reference.

Invariants involved: I1 (no secret on disk), I3 (production confirmations: what was confirmed is what runs, where the dialog said), I6 (the UI never waits on I/O), I7 (licenses).

## Decision
### 1. Classifier (adds to ADR-0007 §3)
1. **MySQL / MariaDB conditional executable comments** (`/*!NNNNN … */`, `/*M! … */`) may or may not run depending on the server. The classifier tries every combination of them run or skipped and keeps the **riskiest** class (`DELETE FROM t /*!99999 WHERE 0 */` is `Destructive`). Ambiguous texts are `Unknown`: more than eight conditional comments, a text over 64 KiB holding any, an unterminated one, one whose body holds `/*` or `*/` in a token, a `;` inside one, and a piece that starts inside one. Consequence, accepted: the view headers of `mysqldump` (`/*!50001 CREATE ALGORITHM=… */ /*!50013 DEFINER=… */ /*!50001 VIEW … */`) are `Unknown` and need confirmation on production.
2. **`CREATE TRIGGER` and `CREATE RULE`** take the class of the riskiest statement of their body (after `BEGIN`, `DO` or `FOR EACH ROW | STATEMENT`), at least `Ddl`: a trigger whose body deletes every row is `Destructive`. A MySQL / MariaDB **`CREATE EVENT`** is `Unknown` (its body may run at once). `BEGIN` is `Transaction` only with transaction modes; MariaDB `BEGIN NOT ATOMIC` and `BEGIN ATOMIC` are `Unknown`.
3. **Names are never keywords that lower a class**: a keyword right after `.` or `AS` is a name; `@name` (MySQL user variables) is one token; `$` is an identifier character in MySQL, MariaDB and SQLite; PostgreSQL line comments end at a carriage return; PostgreSQL identifiers take no backslash escapes with `standard_conforming_strings` off; a top-level `GRANT` / `REVOKE` in a `CREATE` (`CREATE SCHEMA … GRANT`) makes it at least `Destructive`; `[OR REPLACE] [ALGORITHM = x] DEFINER = user[@host]` is skipped before the object kind. Every keyword test whose presence lowers a class follows this rule, so a name can only over-classify.
4. **`MERGE`, `REPLACE INTO`, `INSERT … ON CONFLICT DO UPDATE` and `INSERT … ON DUPLICATE KEY UPDATE` stay `Write`**, as ADR-0007 §3 lists neither: they are confirmed on types that confirm every write (production by default), not as destructive statements. The maintainer may amend ADR-0007 to classify them `Destructive`; that needs a corpus change and a security review.
5. mysqlwire refuses statement text holding a NUL byte (`InvalidParams`), as sqlite does.

### 2. Script splitting
1. `Dialect::split_script` items are valid only for the lexical modes in force when the text was split. A caller running a script compares the session dialect's `lexical_rules()` after **every** statement with the rules its pending items were split with, and splits the rest again from the end of that statement when they differ (a `SET`, but also a `ROLLBACK` or `COMMIT` that undoes one, or a function calling `set_config`). Detecting "statements that may change modes" by class is not enough.
2. Splitting is lazy and linear: `split_script_ahead` returns the next items from a position, and `script_state_after` carries the `ScriptState` (the MySQL `DELIMITER` in force).
3. A `ScriptState` delimiter that the `DELIMITER` directive itself would refuse (empty, with whitespace, a backslash, a quote, `#`, `--` or `/*`) can never be in force: mysqlwire returns one `Refused(InvalidDelimiter)` item for the **rest** of the script; pgwire and sqlite have no directive and ignore a state delimiter. The core also validates a carried state before splitting ([ADR-0024](0024-phase-3-execution-history-and-preferences-decisions.md) §8).
4. Known limit, accepted (fails closed): a piece of a MySQL script that starts inside an executable comment, after a `;` in it, is lexed without that context when the split resumes there and may split further; the driver then refuses it as several statements.

### 3. Editor text hygiene (closes the P1-A IME follow-up)
1. Text entering the editor, typed, committed by an IME, pasted or loaded from a file, loses C0 and C1 control characters and DEL, and its line ends become `\n`.
2. Bidirectional control characters are **kept** (they are legitimate in right-to-left text) but flagged: a warning diagnostic per character, a banner, "Select bidirectional control characters" (`alt-f8`), and their count in every execution request, shown in the statement confirmation dialog.

### 4. The session file holds references only; unsaved tabs are not restored (I1)
1. The editor session model (F-WS-06) holds script paths relative to the project's `scripts/`, connection ids and cursor positions: never editor text.
2. An unsaved, untitled tab is **not restored** after a restart: its text could hold a password typed in a statement, and I1 forbids writing it anywhere the user did not choose. DBeaver autosaves scratch scripts; doing the same needs a new ADR (where, how protected, how purged).
3. Script paths are validated (relative, inside `scripts/`, no `..`, `.sql` only). Neither model is wired into the shell yet.

#### Note on §4.3 (script files and session restore wired, 2026-11-18)
Both models are now wired into the shell (P3-A follow-up, F-SQL-11, F-WS-06; [docs/02](../02-architecture.md#storage), [tutorial](../tutorials/run-sql.md#12-script-files-and-session-restore)); the decisions of §4 are unchanged.
- `session.json` (`$XDG_STATE_HOME/ninesql`) and the project's `scripts.json` (script path → linked connection) are ADR-0008-style documents with `schema_version` 1; a file written by a newer NineSQL is refused and left as it is.
- Unsaved tabs, and the unsaved edits of a saved script, are never written to `session.json` nor restored (I1); closing a tab, a window or NineSQL with unsaved text asks first.
- Restore never connects: a restored tab shows its connection "(not connected)" and connects through the normal connect flow. When a connection opens, only the active editor takes the new session; other editors linked to it stay not connected, so a production session never reaches a tab in the background.
- "Save script" replaces a file only while its size and modification time match the stamp recorded when the tab loaded or last saved it, otherwise it asks; "Save script as…" and "New script" never replace an existing file (`write_new_atomic`, `renameat2(RENAME_NOREPLACE)`).
- A script read is capped at 8 MiB (`MAX_SCRIPT_BYTES`). Restore is bounded: at most 8 windows, 64 tabs per window, 256 tabs tried in total and 64 MiB of script text; a script missing or changed since the quit is skipped with a notice.

### 5. Editor actions and key bindings
1. With a selection, **"Execute statement" runs the selection** (and "Execute script" runs the selection, not the whole buffer). "Execute selection" without a selection does nothing and says why.
2. The editor's bindings use the key context predicate **`SqlEditor > Input`**: the editor view sets `SqlEditor` around gpui-component's `Input`, which binds `ctrl-enter`, `ctrl-f` and `ctrl-h` itself; matching at the depth of the focused `Input` lets the editor's actions win there, and the bindings never apply outside an SQL editor.
3. The editor's actions live in the `ninesql_editor::` namespace and the grid's in `ninesql_grid::`; the shell merges both registries into its keymap, its command palette and the `[keybindings]` overrides of `settings.toml`, where they are named in full (`"ninesql_editor::ExecuteStatement" = "f5"`). Default bindings never collide across the shell, the editor and the grid (tested).
4. Bind parameters need a single statement: a run of several statements with placeholders is refused before anything is sent.

### 6. New direct dependencies of `ninesql-editor`: `lsp-types` and `anyhow`
`lsp-types` 0.97 (MIT, default features off) and `anyhow` 1 (MIT OR Apache-2.0) are direct dependencies of `ninesql-editor` because gpui-component's completion provider trait uses their types (`CompletionResponse`, `anyhow::Result`). Both were already in the dependency graph through gpui-component; no new package entered `Cargo.lock`. `anyhow` is used only at that boundary; the crate's own errors stay `thiserror` (AGENTS.md).

### 7. A run is bound to its session; confirmations are explicit
1. An editor run keeps the `SessionHandle` it started on. Resumes after a confirmation go to that session only; a closed or changed session ends the run with a message. While a run lasts, the connection picker is disabled and changing the editor's session is refused; a background connect never gives a running editor a new session. The confirmation dialog's color comes from the run's session. Reason (ADR-0007): what was confirmed runs where the dialog said.
2. A confirmation dialog that goes away without an answer (closed by other code, its window closed) counts as **not confirmed**: the run ends and its bind values are dropped. "Cancel", Escape, the close button and a click outside count as "Cancel".
3. "Commit transaction" and "Roll back transaction" are refused while a run lasts, so they never land between two statements of a script. Closing a tab with an open transaction closes it only after the commit or rollback succeeded.
4. A script request snapshots the text of its range; the run is refused if any byte of it changed between the request and the run.

## Consequences
- More statements need confirmation on production than strictly necessary (mysqldump view headers, conditional comments, `CREATE EVENT`); none that writes can skip it through the forms above. The corpus (`crates/ninesql-driver-api/tests/corpus/classification.txt`) and each driver's engine tests pin these rules; narrowing one needs an amendment and a security review.
- After a restart, unsaved editor text is gone; users save scripts they want to keep (once script files are wired).
- `SqlEditor > Input` depends on gpui-component's widget structure; a GPUI or gpui-component bump re-checks the editor's key binding tests.

## Rejected alternatives
- **Read conditional comments as the connected server would**: the classifier does not know the exact server version and flavour at every point, and one wrong guess hides a `WHERE` or a write.
- **Classify triggers and rules as plain `Ddl`**: a trigger body that deletes every row would skip the production confirmation.
- **Strip bidirectional controls like other controls**: breaks right-to-left text; flagging them keeps the text and warns.
- **Save unsaved tabs' text in the session file**: puts whatever was typed, passwords included, on disk without the user choosing a file (I1).
- **Bind the editor's keys in a plain `SqlEditor` context**: the `Input`'s own bindings for `ctrl-enter`, `ctrl-f` and `ctrl-h` would win.
- **Let the editor follow the window's active connection during a run**: a confirmed statement could run on another session than the one shown in the dialog.
