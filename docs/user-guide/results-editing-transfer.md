# Results, editing and transfer

Step by step: [Run SQL and read results](../tutorials/run-sql.md#8-read-the-results) for the grid, [Edit data](../tutorials/edit-data.md) for editing, [Export and import data](../tutorials/export-import.md) for files and the DBeaver export.

## The results grid
One run fills one set of tabs under the editor: one tab per result set, "Update counts (N)" and "Notices (N)". Values are shown by type: NULL distinctly, numbers right-aligned, dates in ISO form, timestamps with a time zone as their UTC instant, binary as hex, control and bidirectional characters made visible. A value longer than the value size limit (1 MiB by default) is cut, and its column is marked "cut" and cannot be edited.

**Paging.** The first page arrives on its own (200 rows by default). "Fetch next page of rows" (`Alt+PageDown`, or scroll near the end) and "Fetch all rows (up to the memory budget)" (`Alt+End`) read more. While a result waits for more fetching its session is busy; a result left waiting for 240 s (a preference) is closed by NineSQL, keeping the rows already loaded.

**Memory budget.** One editor's results never take more than 256 MiB (a preference). At the budget the grid stops, closes the result on the server and says "Truncated: memory budget reached …". Rows already loaded are never dropped; one run keeps at most 1 000 result tabs. Nothing loads an unbounded result into memory ([invariant I6](../../CONTEXT.md#invariants-non-negotiable)).

**Views and panels** (any result): "Show results as grid" / "Show row as record" / "Show results as text" (`Alt+1` / `Alt+2` / `Alt+3`); the value panel (`Alt+V`, viewers for text, JSON, XML, hex and PNG / JPEG images; GIF and animated PNG shown as hex only); the aggregate panel (`Alt+A`: count, sum, average, minimum, maximum of the selection, loaded rows only); the metadata panel (`Alt+M`).

**Copy.** "Copy selected cells" (`Ctrl+C`, tab-separated), with column names (`Ctrl+Shift+C`), as CSV (`Ctrl+Alt+C`), Markdown (`Ctrl+Alt+M`), JSON (`Ctrl+Alt+J`) or SQL `INSERT` statements (`Ctrl+Alt+I`), at most 64 MiB. The tab-separated and CSV copies have the spreadsheet formula guard (below), always on.

**Sort and filter.** On any result, "Sort loaded rows ascending / descending" (`Alt+S` then `A` / `D`) and "Filter loaded rows" (`Alt+S` then `F`) work on the **loaded rows only** and say so; the client-side sort stops at 200 000 loaded rows. On a table opened with "Open table data", the server-side actions below see every row.

## Editing table rows
1. Select a table in the navigator and run "Open table data" (`Alt+G` then `O`). The toolbar says "Editable (key: <columns>)", or why the rows are read-only: a read-only connection, no primary or usable unique key, a storage engine without transactions (MySQL MyISAM, MEMORY…), or an engine without editing (QuestDB).
2. Change rows with "Edit cell…" (`Alt+E` then `E`), "Set cell to NULL" (`Alt+E` then `N`), "Insert row…" (`Alt+E` then `I`), "Duplicate row" (`Alt+E` then `C`), "Delete rows" (`Alt+E` then `D`). These are **pending changes**: nothing is sent yet. "Revert row changes" (`Alt+E` then `R`) and "Discard changes" (`Alt+E` then `X`) drop them.
3. "Preview changes…" (`Alt+E` then `P`) shows the exact statements a save would run, one per row, whole and never cut.
4. "Save changes" (`Alt+E` then `S`) runs them.

What a save guarantees:
- Each statement must change **exactly one row**, identified by the key plus the original values of the edited columns; if one changes none or several (the row changed meanwhile), or anything fails, the whole save is undone and the changes stay pending.
- Values are bound, never pasted into the SQL. At most 10 000 changes per save.
- On a **production** connection (or a type that confirms every write), "Confirm save" shows the whole script once for the whole save.
- In **manual-commit** mode the save runs inside your open transaction, under a savepoint, and waits for your own "Commit transaction".
- Only results of "Open table data" (and its filters and foreign-key tabs) are editable: a `SELECT` you type is never editable.

Server-side filter and sort on a table tab: "Filter table rows…" (`Alt+T` then `F`, a `WHERE` expression that must stay a read), "Filter table by selected value" (`Alt+T` then `V`), "Sort table by column ascending / descending" (`Alt+T` then `A` / `D`), "Clear table filter and sort" (`Alt+T` then `C`), "Refresh table data" (`Alt+T` then `R`). Foreign keys: "Open referenced rows" (`Alt+T` then `K`) and "Open referencing rows" (`Alt+T` then `B`).

Editing is engine-tested in the core against PostgreSQL, MySQL, MariaDB, SQLite and YugabyteDB, and the generated SQL against those and TimescaleDB and TiDB.

## Export to a file
"Export results…" (`Ctrl+Alt+E`) runs the statement of the active result **again** and streams every row to a file; "Export table…" (`Alt+G` then `X`) exports the table selected in the navigator. Formats: CSV, TSV, JSON, JSON Lines, SQL `INSERT`, Markdown, HTML.
- Only statements that read can be exported. Where the engine has one, the export runs in an engine-side **read-only transaction** (PostgreSQL, TimescaleDB, YugabyteDB, MySQL, MariaDB). TiDB has none, so a TiDB export from a connection that is not read-only is refused on production and confirm-every-write types and warned on others; SQLite exports without one.
- The file is new, readable by you only, written to a hidden temporary file and moved into place at the end. An existing file is replaced only after "Replace". Folders inside a DBeaver workspace, NineSQL's own folders and `~/.ssh` are refused.
- **Formula guard** (CSV and TSV, on by default): a text cell that a spreadsheet would run as a formula (starting with `=`, `+`, `-`, `@`…) gets a `'` in front. Turn it off only for files no spreadsheet will open.
- Memory stays bounded whatever the size. Measured in tests (debug build, dev PostgreSQL in a container, one machine, commit ccc7215): 1 000 000 rows to CSV in about 4.5 s with about 216 KB of batches buffered at the peak ([ROADMAP](../ROADMAP.md#phase-5--data-editing-and-transfer) for the method).

## Import a CSV file
"Import CSV…" (`Alt+G` then `M`) loads a CSV file into the table selected in the navigator: a preview of the first rows and their issues, a column mapping, then "Import".
- "All or nothing (one transaction)" (default) rolls everything back on any error or cancel; "Commit every 1000 rows" keeps the rows committed before an error. "All or nothing" is not offered for a table whose storage cannot roll back (MyISAM and the like) or, on the MySQL family, whose storage NineSQL could not read.
- **The import commits its own rows**, even on a manual-commit session: "Roll back transaction" afterwards cannot undo it.
- On production and confirm-every-write types, one confirmation shows the connection, table, columns, file, commit mode and the `INSERT` statement; it is bound to that file and those options.
- The file must be UTF-8, comma-separated with `"` quotes, at most 8 GiB. A read-only session, or one with an open transaction, refuses the import.
- Measured in tests (debug builds, dev engines, one machine): 100 000 lines in about 1 to 2 s into PostgreSQL, MySQL or SQLite (commit ccc7215), about 1.6 to 1.7 s per mode into MariaDB and 4.3 to 4.6 s into YugabyteDB (commit 00a1d29).

## Export connections to DBeaver
"Export connections to DBeaver…" (`Ctrl+K` then `Ctrl+Shift+D`) writes the connections you tick to a **new** DBeaver `data-sources.json` or project folder. No password is written: DBeaver asks for them. The target must be new and outside any DBeaver workspace ([invariant I4](../../CONTEXT.md#invariants-non-negotiable)).
- **Read-only is weaker in DBeaver**: DBeaver applies read-only in its JDBC driver and interface, not with an engine-side check after every statement. Each read-only connection gets a warning.
- Connections DBeaver could not reproduce safely are skipped with a reason (URL-unsafe hosts or paths, IPv6 hosts, Unix sockets, routes NineSQL could not reproduce, engines DBeaver has no driver for). QuestDB is written as PostgreSQL.
- Copy the result into a **throwaway** DBeaver workspace with DBeaver closed. Opening the export in DBeaver itself has not been checked by the project yet; it is part of the final tests before v0.1.0.

## Not built yet
- In-cell editing (cells are edited through a dialog) and inserted rows drawn in the grid before a refresh.
- A filter history kept after a restart, GIF and animated PNG previews, value statistics in the metadata panel.
- XLSX, XML and Parquet export, table-to-table transfer, saved transfer tasks (phase 10); CSV import in other encodings or with another delimiter, or into a new table.
- Pinned or detached result tabs.
