# SQL editor

The SQL editor runs statements on one connection and shows their results in the grid below it. Step by step, with every message: [Run SQL and read results](../tutorials/run-sql.md).

## Open an editor
1. Connect (`Ctrl+K` then `Ctrl+O` on a connection in the navigator).
2. "New SQL editor" (`Ctrl+T`). A new editor takes the connection of the active editor, else the connection last opened in the window.
3. The toolbar shows "Connection: <name>", the type and read-only badges, and the transaction mode ("Auto-commit", "Manual commit" or "Transaction open"). "Choose editor connection" (`Ctrl+K` then `Ctrl+P`) switches to another open session (not during a run).

Several editors can share a window: "Split editor right" (`Ctrl+\`), "Split editor down" (`Ctrl+K` then `Ctrl+\`). Each pane on a production connection gets a frame in the type's color.

## Run
| Action | Keys | Runs |
|---|---|---|
| "Execute statement" | `Ctrl+Enter` | The statement under the cursor, or the selection |
| "Execute selection" | `Ctrl+Shift+Enter` | The selected statements |
| "Execute script" | `Alt+X` | Every statement, or those of the selection, one at a time |
| "Cancel execution" | `Ctrl+.` | Cancels the run |
| "Go to error" | `F8` | Moves to the position the engine reported for the last error |

- **Splitting follows each engine's own client**: psql rules on PostgreSQL (a `COPY … FROM STDIN` block or a `\` meta-command is refused), the mysql client's `DELIMITER` on MySQL and MariaDB, `CREATE TRIGGER … BEGIN … END` kept whole on SQLite. A script holding a refused piece runs nothing.
- A script stops at the first error unless the `on_error = "continue"` preference is set.
- **Cancel** says "Cancel" when the driver stops the statement on the server (every MVP engine does once connected; QuestDB only when the server allows it), "Stop fetching" when it can only stop reading.
- **Timeouts**: none unless you set one (`query_timeout_secs`, or per connection).

## Parameters
A statement with placeholders opens a "Parameters (values are not saved)" panel, one field per parameter with a "NULL" toggle. PostgreSQL recognises `:name` and `$1`; MySQL and MariaDB `:name` and `?`; SQLite `:name`, `?`, `?1`, `@name` and `$name`. Values are always bound, never pasted into the SQL text, and never logged or kept in the history. Parameters need a single statement.

## Completion
"Show completions" (`Ctrl+Space`) offers the dialect's keywords, the statement's aliases and CTE names, and the schemas, tables, views, routines and columns **already loaded** for the connection: what you expanded in the navigator, what "Find object" loaded, and the default schema, which NineSQL loads in the background when the editor binds to the connection. Joins on foreign keys are not completed.

## Editing text
"Find" (`Ctrl+F`), "Find and replace" (`Ctrl+H`), "Toggle line comment" (`Ctrl+/`), "Add next occurrence to selection" (`Ctrl+D`), a cursor above or below with `Ctrl+Alt+Up` / `Ctrl+Alt+Down`, folding with the mouse in the gutter. Pasted and loaded text is cleaned: control characters removed, line ends normalised. Invisible bidirectional control characters are kept but flagged with a banner; "Select bidirectional control characters" (`Alt+F8`) selects them.

**"Format SQL"** (`Ctrl+Shift+F`) puts keywords in upper case and one clause per line, without changing what the statement means (strings, names, numbers and comments stay byte for byte). **"Insert snippet"** (`Ctrl+K` then `Ctrl+J`) inserts a snippet of the project's `snippets.json`, asking for its placeholders; nothing runs until you run it. There is no screen to create snippets yet: the file format is in [Run SQL, section 11](../tutorials/run-sql.md#11-format-sql-and-snippets).

## Confirmations
Before each statement runs, NineSQL classifies it. On a production connection (and on types that ask for it), a destructive statement (`DROP`, `TRUNCATE`, `DELETE` or `UPDATE` without `WHERE`, `ALTER`, `GRANT`, `CREATE OR REPLACE`…) and any statement it cannot classify open "Run on production?" with the exact statement, its size and, when the end could be out of sight, its last line ("Ends with:"). "Run" runs it and the script goes on (it may stop again at the next statement that needs a confirmation); "Cancel", Escape or closing the dialog runs nothing more. What the classifier can and cannot see: [Safety model](safety-model.md#the-limits-of-the-statement-checks).

## Transactions
In manual-commit mode (production by default, or a connection set to manual commit) your statements stay in an open transaction and the toolbar says "Transaction open". "Commit transaction" (`Ctrl+K` then `Ctrl+C`) and "Roll back transaction" (`Ctrl+K` then `Ctrl+R`) end it. Closing a tab, disconnecting or deleting a connection with an open transaction asks first ("Commit and close", "Roll back and close", "Cancel"). There is no auto-commit toggle in the editor yet: the mode comes from the connection type and the connection's setting.

## Script files and session restore
An editor tab is a scratch buffer until you save it. Scripts live in the project's `scripts/` folder and each one remembers its connection.

| Action | Keys |
|---|---|
| "New script" | `Ctrl+N` |
| "Open script…" | `Ctrl+O` |
| "Save script" | `Ctrl+S` |
| "Save script as…" | `Ctrl+Shift+S` |
| "Connect script connection" | `Ctrl+K` then `Ctrl+Shift+O` |

- A tab with unsaved text shows `●`; closing it, closing the window or quitting asks first.
- "Save" asks before overwriting a file changed on disk since you opened it; "Save as…" never replaces an existing file. Files are written atomically and readable by you only.
- Refused: names outside `scripts/`, hidden names, symbolic links, files over 8 MiB or not in UTF-8, and projects inside a DBeaver workspace.
- **Session restore**: when NineSQL quits it remembers the saved scripts open in each window, their connection, cursor, scroll and layout. At the next start they reopen **as saved on disk**: untitled tabs and unsaved edits are not kept (they could hold a password typed in a statement), and **nothing connects by itself**: a restored tab says "Connection: <name> (not connected)" with a "Connect" button. A script deleted or changed since is skipped with a "Previous session" notice.

## History
Each statement run from the editor is recorded in `history.sqlite` (readable by you only) with its text, connection, time, duration, row count and status, except: nothing for production connections and types that do not log statement text, nothing for a statement holding a credential word or a URL with a password, and never parameter values. **There is no history view yet**: the file is kept for it.

## Preferences
There is no preferences page yet. Preferences live in `~/.config/ninesql/settings.toml` under `[execution]` (timeout, fetch size, memory budget, value size limit, script error behavior, keep-alive, idle limit of a paused result), `[history]` (on / off, retention) and `[confirmations]` (asking before closing with an open transaction, always on for production). Each key with its default and range: [Run SQL, section 10](../tutorials/run-sql.md#10-preferences-in-settingstoml).

## Not built yet
- A history view, a preferences page, an auto-commit toggle, a reconnect when a session is lost.
- A keyboard action to fold, completion of foreign-key joins, `${var}` variables and a variables panel.
- Renaming, moving or deleting scripts from NineSQL, a script list in the navigator, restoring object editors, an autosave of untitled tabs.
- A screen to create snippets, formatter settings, the per-statement run summary in the interface.
- An "Explain" action and visual explain plans (phase 10): today you type and run an `EXPLAIN` statement yourself, and the text plan shows in the grid.
