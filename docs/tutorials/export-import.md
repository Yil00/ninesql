# Export and import data

This tutorial moves data in and out of NineSQL (phase 5: P5-C, P5-D, P5-A; features F-XFER-01, F-XFER-03, F-DATA-06, F-CON-15): export a result or a table to a file, the rules for the files NineSQL writes, the spreadsheet formula guard, import a CSV file with a preview and a mapping, the import confirmation on production, the copy formats of the grid, and the export of connections to DBeaver with its manual check.

> Written without a display: the labels and keys below come from the code, but the screens have not had their visual pass yet (deferred to the end of the MVP, P6-F). The dialogs are tested with the mock driver; the core's exports and imports are tested against PostgreSQL, MySQL and SQLite.

## Before you start
- NineSQL builds and runs ([Build and run](build-and-run.md)), the dev databases are up (`make dev-up`, [Start the dev databases](dev-databases.md)) and a `nine-pg` connection is open ([Create your first connection](first-connection.md)).
- You know how to run a query ([Run SQL and read results](run-sql.md)) and select a table in the navigator ([Browse a database](browse-database.md)).

Every action below is in the command palette (`Ctrl+Shift+P` or `F1`): the transfer actions under "Data", the copy actions under "Results", the DBeaver export under "Connections".

## 1. Export a result or a table
| Action | Default keys | Exports |
|---|---|---|
| "Export results…" | `Ctrl+Alt+E` | The statement of the active result, **run again**: every row, not only the loaded ones |
| "Export table…" | `Alt+G` then `X` | Every row of the table (or view) selected in the navigator |

1. Run a query in an SQL editor and run "Export results…", or select a table in the navigator and run "Export table…". The dialog ("Export results" or "Export table") says what will run: "The statement of the result runs again (reads only); rows are streamed to the file." or "Every row of <table> is streamed to the file."
2. Pick a format: "CSV", "TSV", "JSON" (one array of objects), "JSON Lines" (one object per line), "SQL INSERT", "Markdown" or "HTML". For CSV and TSV, "Header row: on" / "off" and "Formula guard: on" / "off" (section 3).
3. Type the file's full path, or press "Choose…" for the desktop's save dialog (it proposes `export.<extension>` in your home folder).
4. Press "Export". NineSQL checks the file ("Checking the file…"), then shows "Exporting… N rows, <size>". "Cancel" stops it: "Cancelled: the partial file was removed."
5. At the end: "N rows, <size> written to <file> in N s.", plus any of: "Incomplete: the driver stopped the result at its budget; the file holds the rows delivered before.", "Values cut at the value size limit in: …", "N further result set(s) were not exported.", and the warnings of section 2.

Only statements that **read** can be exported: anything else is refused before it runs ("only a query that reads data can be exported (this statement is classified …)"). "Export results…" needs the statement of the active result: a result whose statement had bind parameters cannot be exported ("This statement had bind parameters, which are not kept after the run: export it with the values written in the statement."). A SQL `INSERT` export fails on a value the driver cut at the value size limit rather than write it cut; other formats write the cut value and name the column.

Memory stays bounded whatever the size: rows are written as they arrive. In the phase 5 tests (debug build, dev PostgreSQL in a container, one machine, commit ccc7215), 1 000 000 rows went to CSV in about 4.5 s with about 216 KB of batches buffered at the peak ([ROADMAP](../ROADMAP.md#phase-5--data-editing-and-transfer)).

## 2. While an export runs: session and transaction
- **Which session.** "Export results…" runs on the editor's session. "Export table…" and "Import CSV…" (section 5) take an open session of the table's connection: the active editor's session when it is on that connection and free; otherwise the newest free one other than the connection's first (oldest) session, which the navigator loads its objects through, so a long transfer does not stop the navigator ("busy"); otherwise that first session. A session is busy while an editor tab of the window runs a statement on it or pages a result there. To keep the navigator free during a long import, open the connection a second time first. NineSQL does not open a session of its own for a transfer.
- An export uses its session alone: other statements on it are refused until it ends ("Another export or import is using this session: wait for it to end."). A session still running a statement, or paging a result, refuses the export: let it finish or cancel it first.
- When the session is not read-only and has no open transaction, the export runs inside an engine-side **read-only transaction** (PostgreSQL, TimescaleDB, YugabyteDB: `BEGIN READ ONLY`; MySQL, MariaDB, TiDB: `START TRANSACTION READ ONLY`; none on SQLite), ended before the summary. So even a function with side effects that looks like a read cannot write.
- On a **production** connection, or one whose type confirms every write, an export is refused when a transaction is already open on the session ("a transaction is open on this session: commit or roll back first"), and fails if the read-only transaction cannot be opened. On other types it goes on, with "A transaction was open on the session: the export read inside it (uncommitted changes included)." or "The export could not open a read-only transaction: it ran without one."

## 3. The files NineSQL writes
- **Always a new file.** If the file exists, the dialog stops: "<file> already exists. Replace it? Its current content is lost." with "Back" and "Replace". Nothing is replaced without that step, even if the file appears while the export runs.
- **Private.** The file is created readable by you only (`0600`) as a hidden temporary file next to the target, then moved into place at the end; a cancelled or failed export leaves nothing behind. A symbolic link is never followed.
- **Refused folders.** A folder inside a DBeaver workspace ("the folder is inside a DBeaver workspace, which NineSQL never modifies"), NineSQL's own folders ("the folder is part of NineSQL's own storage (workspace, secret vault, history)") and `~/.ssh` ("the folder holds SSH keys and settings"), including through a symbolic link or a bind mount. The path must be absolute and its folder must exist.
- **Formula guard** (CSV and TSV, on by default): a text cell or header that a spreadsheet would run as a formula (one starting with `=`, `+`, `-`, `@`, `|`, their full-width forms, a tab or a carriage return, after any leading blanks) is written with a `'` in front, so the spreadsheet shows it as text. Numbers are never changed, so `-5` stays a number. Turn it off ("Formula guard: off") only for a file no spreadsheet will open.
- Other formats escape their own syntax: JSON strings, Markdown punctuation, HTML entities; the HTML file forbids scripts. A file holds result values only, never a password; it is yours to protect once written.

## 4. Copy formats
For a selection in the grid:

| Action | Default keys |
|---|---|
| "Copy selected cells" (tab-separated) | `Ctrl+C` |
| "Copy selected cells with column names" | `Ctrl+Shift+C` |
| "Copy selected cells as CSV" | `Ctrl+Alt+C` |
| "Copy selected cells as Markdown" | `Ctrl+Alt+M` |
| "Copy selected cells as JSON" | `Ctrl+Alt+J` |
| "Copy selected rows as SQL INSERT" | `Ctrl+Alt+I` |

- CSV, Markdown and JSON use the export formats. CSV and both tab-separated copies (`Ctrl+C`, `Ctrl+Shift+C`) have the formula guard of section 3, always on: a text cell or column name a spreadsheet would run as a formula gets a `'` in front, a single copied cell included; values of number columns are never changed.
- "Copy selected rows as SQL INSERT" writes one statement per row with the connection's dialect and asks for the table when it is not known: "Table name: table or schema.table", "The rows are copied as INSERT statements into this table. They are not run." It needs a result run on a connection, and refuses a column the driver cut.
- Copies follow the grid's client-side sort and filter and stop at 64 MiB; the status line says "Copied N rows × M columns as …".

## 5. Import a CSV file
1. Select the target table in the navigator and run "Import CSV…" (`Alt+G` then `M`). The table must exist, and its columns must all be listed ("The table's columns are not all listed (a navigator filter hides some, or the list is incomplete): the import needs every column.").
2. The "Import CSV" dialog says "Rows of a CSV file are inserted into <table>." Type the file's path or press "Choose…", set "Header row: yes" / "no", then press "Preview".
3. The preview shows the first rows ("Preview: N field(s)", ", whole file read" for a short file) and the issues found in them (line, column, reason, never a value).
4. Under "Column mapping", each table column ("name (type)") shows the file column it takes ("← <field>"), matched by name; click to cycle through the file's columns, down to "← (not imported: default)".
5. Choose how rows are committed: "All or nothing (one transaction)" (default: any error or a cancel rolls everything back) or "Commit every 1000 rows" (rows committed before an error stay). An engine without transactions offers only the second ("this connection has no transactions: choose "commit every N rows"").
6. Choose what an invalid row does: "Invalid rows: stop" (default) or "Invalid rows: skip" (skipped rows are counted and listed).
7. Press "Import". Progress: "Importing… N rows inserted, <read> of <size> read". At the end: "Import completed: N rows read, N inserted, N committed, N skipped, in N s.", or "Import failed" / "Import cancelled" with the same counts and, after the error:
   - when rows were committed before the end (always "Commit every 1000 rows"): "N rows were committed before the end and stay in the table.", plus "The M rows inserted after the last commit were rolled back." when some were;
   - when nothing was committed: "The transaction was rolled back: nothing was written." ("All or nothing") or "No rows were committed before the end: nothing was written." ("Commit every 1000 rows").

The import refuses a read-only session ("The session is read-only: an import writes data."), a session with an open transaction ("A transaction is open on this session: commit or roll back first."), a driver without bind parameters, a file that is not a regular file or is over 8 GiB, and a file inside NineSQL's own folders or `~/.ssh`. Rows are sent as bound multi-row `INSERT`s (up to 1 000 rows each): values are converted to the column types and never pasted into the SQL text. The file must be UTF-8 (a byte order mark is skipped), comma-separated with `"` quotes; records longer than 1 MiB or with more than 4 096 fields are refused. In the phase 5 tests, 100 000 lines with quoted fields, line breaks and NULLs went into PostgreSQL, MySQL and SQLite in about 1 to 2 s each (same conditions as section 1); MariaDB is not engine-tested yet.

## 6. Import confirmation on production
On a connection typed production, or whose type confirms every write, "Import" first shows "Confirm the import: these rows are written to the database." with exactly what will happen:
- "Connection": the name, with "(production)" when it applies;
- "Table" and "Columns" as they will be sent;
- "File": the resolved path (invisible characters shown as markers) and its size;
- "Statement": the `INSERT` for one row (the import sends the same with more rows), whole, in the same script box as the save confirmation: "N lines · M bytes", line numbers, long lines wrapped to the box's width onto rows marked `↪` (never cut), invisible and bidirectional characters shown as markers, and "Ends with:" and its last line when the end may be out of sight ([Edit data](edit-data.md#3-preview-the-sql)).

"Import" runs it; "Cancel" sends nothing. You confirm **once** for the whole file, not once per `INSERT`. The confirmation is bound to that file as it is now (if it is changed or replaced, NineSQL asks again), to the table, columns, mapping, commit mode and invalid-row choice, and expires after 10 minutes.

## 7. Export connections to DBeaver
1. Run "Export connections to DBeaver…" (`Ctrl+K` then `Ctrl+Shift+D`). The dialog says "Writes a new DBeaver file without any password. Close DBeaver before copying it into a workspace."
2. Tick the connections ("Select all" ticks every one).
3. Choose "New file" (a `data-sources.json`, proposed as `data-sources-ninesql.json`) or "New project folder" (a folder DBeaver can open as a project, proposed as `NineSQL-export`), type the path or press "Choose…".
4. Press "Export". The report says "Exported N of M connections to <file>. DBeaver asks for the passwords: none was exported.", then each connection, "(skipped)" when it was not written, with its notes: "dropped", "approximated", "secret not exported", "looks like a secret, not written", "skipped", "not verified".

What to expect:
- **No password**, passphrase or token is written, nor DBeaver's `credentials-config.json`: DBeaver asks for them on connect.
- The target must be new, and never inside a DBeaver workspace or install folder. If the target changes while it is written, nothing is exported: "The target changed while the export was written: nothing was exported. Choose the target again."
- **Read-only is weaker in DBeaver**: DBeaver applies read-only in its JDBC driver and its interface, not with an engine-side read-only transaction checked after every statement as NineSQL does. Each read-only connection gets this warning (and, for PostgreSQL-family connections, `readOnlyMode=always`); its init SQL keeps only statements that read.
- Skipped, with a reason: hosts, databases or file paths holding URL characters (`?`, `&`, `;`, `#`, `%`…), IPv6 address hosts, Unix sockets, connections NineSQL disabled because their route could not be reproduced, engines DBeaver has no driver for. QuestDB is written as PostgreSQL. Custom connection types are written as `ninesql-<id>`. SSH tunnels warn that DBeaver uses its own host-key checks; more than one jump host warns that DBeaver before 24.3 uses only the first.
- Imported back into NineSQL, the connections match (tested automatically, never with a weaker read-only or TLS setting).

**Check it in DBeaver** (maintainer, not done yet): copy the file or project into a **throwaway** DBeaver workspace, never your own, with DBeaver closed, and follow the [manual check](../06-dbeaver-interop.md#manual-check-in-dbeaver-maintainer) in docs/06: names, folders, endpoints, user names, types, read-only, properties, SSH and SSL tabs, then import it back into NineSQL.

## Not built yet
- XLSX, XML and Parquet export (phase 10), table-to-table transfer, saved transfer tasks.
- CSV import in other encodings, with another delimiter or quote, or into a new table; a MariaDB engine test.
- Opening the DBeaver export in DBeaver as part of the automatic tests (the manual check above stands in).
- The visual pass of these dialogs (P6-F).

## Next
- [Edit data](edit-data.md): change rows with a DML preview.
- How transfers work inside: [docs/02](../02-architecture.md#data-transfer-p5-c); the file and confirmation rules: [docs/05](../05-security.md#export-and-import-files-p5-c); what the DBeaver export writes: [docs/06](../06-dbeaver-interop.md#export-to-dbeaver-p5-d).
