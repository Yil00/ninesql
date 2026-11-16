# Run SQL and read results

This tutorial runs statements from the SQL editor and reads their results in the grid (phase 3: P3-A editor, P3-B grid, P3-C execution, P3-D dialects; features F-SQL-03 to F-SQL-08, F-SQL-12, F-SQL-15, F-SQL-18, F-DATA-01). It covers the editor tab and its connection, the three ways to run, parameters, confirmations on production, cancel, commit and roll back, the grid, and the history and preferences kept in `settings.toml`.

The screens were written without a display (cloud sessions); their visual pass is part of the MVP-end tests (P6-F). The labels and key bindings below are the exact texts and defaults of the code. Every action is also in the command palette (`Ctrl+Shift+P` or `F1`): the editor's under "SQL editor", the grid's under "Results".

## Before you start
- NineSQL builds and runs ([Build and run](build-and-run.md)).
- A connection you can open, for example the dev PostgreSQL with the "nine" dataset ([Start the dev databases](dev-databases.md), [Create your first connection](first-connection.md)).

## 1. Open an editor on a connection
1. Connect: select the connection in the navigator and run "Connect" (`Ctrl+K` then `Ctrl+O`).
2. Open an editor: "New SQL editor" (`Ctrl+T`). The tab is named `Script-1`, `Script-2`… A new editor takes the connection of the active editor, else the connection last opened in the window. If the active editor has no connection, it takes the next connection you open.
3. The toolbar above the editor shows "Connection: <name>" (or "No connection"), the connection's type and read-only badges, and the transaction mode ("Auto-commit", "Manual commit" or "Transaction open").
4. To change it, click the connection button or run "Choose editor connection" (`Ctrl+K` then `Ctrl+P`). The dialog lists one button per open session, marked "(read-only)" or "(production)" when they apply, and "No connection". With nothing open, it says "No connection is open: connect from the navigator first (Connect)." You cannot change the connection while a run is going on ("Choose editor connection (not during a run)").

The editor follows the dialect of its connection: how statements are split, which parameter forms are recognised and which keywords are completed. Highlighting uses one SQL grammar for every engine.

## 2. Write
| Action | Default keys |
|---|---|
| "Show completions" | `Ctrl+Space` |
| "Toggle line comment" (`--`) | `Ctrl+/` |
| "Find" / "Find and replace" | `Ctrl+F` / `Ctrl+H` |
| "Add next occurrence to selection" (multi-cursor) | `Ctrl+D` |
| Add a cursor above / below | `Ctrl+Alt+Up` / `Ctrl+Alt+Down` |

- **Completion** offers the dialect's keywords and the aliases and CTE names of the statement. Schemas, tables, views and columns are **not offered yet**: they need the metadata cache of phase 4. Joins on foreign keys are not completed.
- **Folding** works with the mouse in the gutter; there is no key binding to fold or unfold yet.
- **Pasted and loaded text is cleaned**: control characters are removed and line ends become `\n`. Invisible bidirectional control characters are kept but flagged: each one gets a warning, a banner says "N bidirectional control character(s) in this script (first: …). Alt+F8 selects them.", and "Select bidirectional control characters" (`Alt+F8`) selects them so you can review or delete them.

## 3. Run
| Action | Default keys | Runs |
|---|---|---|
| "Execute statement" | `Ctrl+Enter` | The statement under the cursor; with a selection, the selection |
| "Execute selection" | `Ctrl+Shift+Enter` | The selected statements ("select the statements to run first" without a selection) |
| "Execute script" | `Alt+X` | Every statement of the editor; with a selection, the selection |
| "Cancel execution" | `Ctrl+.` | Cancels the run (section 6) |
| "Go to error" | `F8` | Moves the cursor to the position of the last error |

Statements are split the way each engine's own client does: psql rules on PostgreSQL (a `COPY … FROM STDIN` block or a `\` meta-command is refused, never sent), the mysql client's `DELIMITER` on MySQL and MariaDB (applied, never sent), `CREATE TRIGGER … BEGIN … END` kept whole on SQLite. A script holding a refused piece runs nothing: the editor says "Line N: …. Nothing was run." A script runs one statement at a time; after a statement that changes how the following ones are read (for example `SET sql_mode` or `SET standard_conforming_strings`), the rest is split again. Details: [docs/08](../08-engine-capabilities.md#script-splitting-and-classification-p3-d-f-sql-18).

A script stops at the first statement that fails, unless `on_error = "continue"` (section 10). When the engine reports where the error is, the editor marks it, the error line under the editor says "(line L, column C)", and "Go to error" (`F8`) moves the cursor there; no position is shown when you edited the statement since.

If you edit the script between asking for a run and its start, the run is refused: "The script changed since the run was asked: run it again."

## 4. Parameters
A statement with placeholders opens a "Parameters (values are not saved)" panel above the editor, one field per parameter, each with a "NULL" toggle. Press "Run" to run with these values, "Cancel" to run nothing.

| Engine | Placeholders recognised |
|---|---|
| PostgreSQL | `:name`, `$1` (`?` is a JSON operator there, never a parameter) |
| MySQL, MariaDB | `:name`, `?` (`@name` is a user variable, not a parameter) |
| SQLite | `:name`, `?`, `?1`, `@name`, `$name` |

Placeholders inside strings and comments are ignored. Values are always bound, never pasted into the SQL text, and sent as text. They are not logged, not written to the history and not kept after the run. Parameters need a single statement: a script with placeholders is refused ("Bind parameters need a single statement: run the statements one at a time.").

Limits: `${var}` and `{{name}}` variables and a variables panel do not exist. Values typed in the panel may stay in memory after the run (the text input does not wipe its buffer, as for password fields; [ROADMAP](../ROADMAP.md#phase-3-follow-ups)).

## 5. Confirmations on production
Before each statement runs, NineSQL classifies it with the session's dialect. On a connection whose type asks for it (always on **production**), a destructive statement (`DROP`, `TRUNCATE`, `DELETE` or `UPDATE` without `WHERE`, `ALTER`, `GRANT`, `CREATE OR REPLACE`…), any statement it cannot classify, and on types that confirm every write any write, opens a dialog before anything is sent:
- Title: "Run on production?" (or "Run this statement?" for other types that confirm).
- The connection name in the type's colour, "This statement needs confirmation (…)" with the reason, the exact statement, "N bind parameters (values not shown)" when there are some, and a warning when the script holds bidirectional control characters.
- "Run" runs this statement, then the script goes on (and may stop again at the next one); "Cancel", Escape, the close button or a click outside run nothing more: "Not run: the statement was not confirmed." A dialog that disappears without an answer also runs nothing.

The confirmed statement runs on the session the run started on, never another one, even if you switch the window's active connection meanwhile. With several panes, each editor on a production connection has a frame in the type colour.

What is classified how, and why some harmless statements also ask (for example `mysqldump` view headers or an `EXPLAIN` with options), is in [docs/08](../08-engine-capabilities.md#script-splitting-and-classification-p3-d-f-sql-18). Read-only connections refuse writes in the engine itself ([invariant I3](../../CONTEXT.md#invariants-non-negotiable)), whatever the editor does.

## 6. Cancel
While a run is going on, the toolbar shows a button: "Cancel" for drivers that cancel on the server (PostgreSQL, MySQL, MariaDB, SQLite), "Stop fetching" for a driver that can only stop reading. "Cancel execution" (`Ctrl+.`) does the same. The editor says "Cancelled."; the grid keeps the rows already loaded and says "Stopped: the statement failed or was cancelled. N rows loaded." If the server takes a moment to stop, the next run waits ("Waiting for the previous statement to stop…").

A statement that runs past its timeout is cancelled the same way: "The statement exceeded its timeout (N s) and was cancelled." There is no timeout unless you set one (section 10).

## 7. Commit and roll back
In manual-commit mode (production connections by default, or a connection set to manual commit), your statements stay in an open transaction and the toolbar shows "Transaction open".

| Action | Default keys |
|---|---|
| "Commit transaction" (toolbar: "Commit") | `Ctrl+K` then `Ctrl+C` |
| "Roll back transaction" (toolbar: "Roll back") | `Ctrl+K` then `Ctrl+R` |

The buttons are enabled only with an open transaction and no run going on; during a run the editor says "A run is going on in this editor: cancel it or let it finish, then commit or roll back." Afterwards it says "Transaction committed." or "Transaction rolled back."

Closing an editor tab, disconnecting or deleting a connection whose session has an open transaction asks first ("Uncommitted transaction": "Commit and close", "Roll back and close", "Cancel"). It always asks for production; for other types you can turn it off (section 10). There is no toggle between auto-commit and manual commit in the editor yet: the mode comes from the connection type and the connection's auto-commit setting.

## 8. Read the results
The grid sits under the editor; drag the divider to resize. One execution fills one set of tabs:
- one tab per result set: "Result", "Result (2)"… and, for a script, "2 · Result" (statement 2);
- "Update counts (N)": "Statement 2: 5 rows affected"…;
- "Notices (N)": server notices and warnings, with their statement.

Values are shown by type: NULL distinctly, numbers right-aligned, dates and times in ISO form, timestamps with a time zone as their UTC instant labelled `UTC`, binary as a hex preview, JSON on one line, long text cut with `…`, control and bidirectional characters made visible. A column whose values were cut at the value size limit is marked "cut" in its header and is read-only.

**Paging.** The first page arrives on its own (`fetch_size` rows, 200 by default). The status line under the grid says "N rows · used of budget · state" ("more rows available", "all rows", "truncated"…).

| Action | Default keys / button |
|---|---|
| "Fetch next page of rows" | `Alt+PageDown`, "Fetch next page", or scroll near the end |
| "Fetch all rows (up to the memory budget)" | `Alt+End`, "Fetch all" |
| "Next result tab" / "Previous result tab" | `Alt+Right` / `Alt+Left` |
| "Fit column width to content" / "Fit all column widths to content" | `Alt+R` / `Alt+Shift+R` |

Only reads wait for you between pages; a statement that changes data (a `DELETE … RETURNING`, for example) is fetched to its end. While a result waits for more fetching, its session is busy: another statement on it reports "The session is busy with another statement or a paused result: cancel it, or fetch the result to its end, then try again." In a script, every result but the last is closed after its first page; the grid then says "The result was closed before its end: N rows loaded. Run the statement again to see more."

**A result left waiting is closed.** If you do not fetch more for `paused_result_idle_secs` (240 s by default, section 10), NineSQL closes the result itself, before the servers' own 300 s limit would drop the whole session. The rows already loaded stay in the grid and the session stays usable. To read further, run the statement again. A transaction that was open stays open: commit or roll back it (section 7).

**Memory budget.** One execution (one editor tab) never keeps more than `result_memory_mib` (256 MiB by default) of results, all its result sets together. At the budget the grid stops loading, closes the result on the server and says "Truncated: memory budget reached. N rows kept (… of …); narrow the query to see the rest." Rows already loaded are never dropped, and "Fetch all" never goes past the budget, so it asks no question. Every result set and every update count takes a small fixed share of the budget, even when empty, and one execution shows at most 1 000 result tabs by default; past that NineSQL stops reading results and says so in a banner.

**Selection and copy.**

| Action | Default keys |
|---|---|
| "Select all loaded rows" | `Ctrl+A` |
| "Extend selection up / down / left / right" | `Shift+Up` / `Shift+Down` / `Shift+Left` / `Shift+Right` |
| "Copy selected cells" (tab-separated) | `Ctrl+C` |
| "Copy selected cells with column names" | `Ctrl+Shift+C` |
| "Copy selected cells as CSV" (with column names) | `Ctrl+Alt+C` |

Arrow keys, Page Up / Down, Home / End move between cells; drag a header to reorder a column, drag its edge to resize, double-click the edge to fit. After a copy, the status line says "Copied N rows × M columns", with "(stopped at the clipboard size limit)" past 64 MiB.

Not built yet: editing cells, sorting and filtering, record view and value viewers (phase 5), pinned or detached result tabs, export (P5-C).

## 9. History
Each statement run from the editor is recorded in `~/.local/state/ninesql/history.sqlite` (the file is readable by you only), with its text, connection, time, duration, row count and status, **except**:
- nothing at all for **production** connections and for connection types that do not allow query text in logs;
- nothing for a statement that holds a credential word (password, secret, token, identified…) or a URL with a password, anywhere in its text, comments included;
- parameter values are never stored, only their number.

There is no history view in the interface yet; the file is kept for the history view to come. Retention is set in `[history]` (section 10).

## 10. Preferences in settings.toml
There is no preferences page yet: set these in `~/.config/ninesql/settings.toml` ([Build and run](build-and-run.md#settings-and-key-bindings)). Values shown are the defaults; an invalid value keeps its default and is listed by "Show settings problems" (`Ctrl+K` then `Ctrl+E`). NineSQL rewrites only the keys it changes and keeps your comments.
```toml
[execution]
query_timeout_secs = 0          # 0 = no timeout; else 1 to 604800 (7 days)
fetch_size = 200                # rows per page, 1 to 10000
result_memory_mib = 256         # memory budget of one execution, 1 to 16384
max_value_kib = 1024            # longer values are cut and marked "cut", 1 to 1048576
on_error = "stop"               # scripts: "stop" or "continue"
keep_alive_secs = 0             # ping idle sessions; 0 = off, else 10 to 86400
auto_reconnect = true           # read, not used yet: the interface does not reconnect
reconnect_attempts = 3          # 1 to 20, for the reconnect to come
paused_result_idle_secs = 240   # close a result left waiting for a fetch, 30 to 280

[execution.connection_timeouts] # per-connection timeout in seconds, by connection id
# "6f1c2b0e-…" = 30

[history]
enabled = true
max_entries_per_connection = 10000   # 0 to 1000000
max_age_days = 90                    # 0 = no age limit, else 1 to 36500

[confirmations]
close_with_open_transaction = true   # always asked for production connections
```
- The connection id of `[execution.connection_timeouts]` is the `id` of the connection in `connections.json`.
- Statement confirmations are not a preference: they come from the connection type ([Create your first connection](first-connection.md#5-connection-type-and-read-only)).
- Editor and grid key bindings are overridden under `[keybindings]` with their full names, for example `"ninesql_editor::ExecuteStatement" = "f5"` or `"ninesql_grid::FetchAllRows" = ["alt-end", "ctrl-shift-end"]`.

## Not built yet
- Saving and opening script files, and reopening tabs after a restart. Unsaved tabs will not be restored even then: their text could hold a password.
- Completion of tables and columns, and of joins on foreign keys (phase 4).
- A keyboard fold action, a history view, a preferences page, an auto-commit toggle, a reconnect when a session is lost.
- Measurements on a real GPU: the time to first rows was measured only at the core level, and scrolling has not been measured ([ROADMAP](../ROADMAP.md#phase-3--sql-editor-and-results-grid)).

## Next
- [Secrets](secrets.md): what a lock does to running sessions.
- [docs/08](../08-engine-capabilities.md): per-engine splitting, classification and cancel.
