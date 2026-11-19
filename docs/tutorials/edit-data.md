# Edit data

This tutorial opens a table's rows and changes them (phase 5: P5-A, P5-B; features F-DATA-02 to F-DATA-07, F-DATA-09, F-DATA-10): open table data, edit a cell, insert, duplicate and delete rows, preview the generated SQL, save, confirm on production, commit in manual-commit mode, filter and sort on the server, follow foreign keys, and read rows in the record and text views and the side panels. It uses the PostgreSQL dev database and its "nine" dataset; MySQL, MariaDB and SQLite work the same way.

> Written without a display: the labels and keys below come from the code, but the screens have not had their visual pass yet (deferred to the end of the MVP, P6-F). The editing UI is tested with the mock driver; the core's editing is tested against PostgreSQL, MySQL, MariaDB and SQLite, and the generated SQL against PostgreSQL, TimescaleDB, MySQL, MariaDB, TiDB and SQLite.

## Before you start
- NineSQL builds and runs ([Build and run](build-and-run.md)), the dev databases are up (`make dev-up`, [Start the dev databases](dev-databases.md)) and a `nine-pg` connection is saved and open ([Create your first connection](first-connection.md)). Use a **dev** connection to try this: the rows really change.
- You know how to run SQL and read the grid ([Run SQL and read results](run-sql.md)) and how to select a table in the navigator ([Browse a database](browse-database.md)).

Every action below is in the command palette (`Ctrl+Shift+P` or `F1`): the table actions under "Data" and "Data editing", the grid's under "Results". "Open table data" is also the first entry of the context menu of a table or view row in the navigator (right click); the editing actions are not in a context menu yet: use the palette or the keys. Their key bindings work from the SQL editor and the rest of the window; until 2026-11-18 they were not handled, and these actions were reachable from the palette only.

## 1. Open table data
1. Select a table in the navigator, for example `public.customers`.
2. Run "Open table data" (`Alt+G` then `O`, or right click the table in the navigator). A new tab, named after the table, runs NineSQL's own query of the table and shows its rows in the grid. The toolbar says "Table: <name>".
3. NineSQL first reads the table's key (the toolbar says "Reading the table's key…"), then runs the query: the key is read before the rows because a table larger than one page keeps its session busy until the result ends or you stop it. Then the toolbar says either "Editable (key: <columns>)", or why the rows are read-only, for example:
   - "Read-only: the connection is read-only."
   - "Read-only: the table has no primary key or unique key that identifies rows: rows cannot be edited."
   - "Read-only: the table's storage engine has no transactions, so a failed save could not be rolled back: rows cannot be edited." (MySQL MyISAM, MEMORY, ARCHIVE…)
   - "Read-only: this engine does not support editing rows." (QuestDB)

The key comes from the table's metadata: the primary key, else the smallest unique key whose index is valid, not deferrable, not partial and has no expression part (SQLite's `INTEGER PRIMARY KEY` counts). A view opens too, but usually has no key, so it stays read-only.

**Only results of "Open table data" are editable.** A `SELECT` you type in an SQL editor is never editable, even on one table: its toolbar says "Read-only: open the table from the navigator (Open table data) to edit its rows." The same holds for the server-side filter and sort (section 6) and foreign-key navigation (section 7).

## 2. Change rows
Select a cell (or rows) of the table's result, then:

| Action | Default keys | What it does |
|---|---|---|
| "Edit cell…" | `Alt+E` then `E` | Opens "Edit <column>" with the current value: "Apply" keeps your text, "Set NULL" sets NULL, "Cancel" changes nothing |
| "Set cell to NULL" | `Alt+E` then `N` | Sets the focused cell to NULL |
| "Insert row…" | `Alt+E` then `I` | Opens "Insert row": one field per column, each with a "NULL" box. "An empty field takes the column's default. Save to insert the row." Press "Add row" |
| "Duplicate row" | `Alt+E` then `C` | Adds a copy of the focused row to insert ("Row duplicated: save to insert it.") |
| "Delete rows" | `Alt+E` then `D` | Marks the selected rows for deletion |
| "Revert row changes" | `Alt+E` then `R` | Drops the pending changes of the selected rows. Refused while a save runs or waits for its confirmation, while a statement runs, or while an export or import holds the session ("A save waits for its confirmation.", "A save is running: wait for it to end."…), so the changes you confirm are the ones shown |
| "Discard changes" | `Alt+E` then `X` | Drops every pending change ("Changes discarded.") |

Nothing is sent yet: these are **pending changes**. The toolbar says "Editable (key: …) · N unsaved changes" with "Save" and "Discard" buttons. Cells with a pending value are marked in the grid. Inserted rows are not drawn in the grid: they exist only as pending changes until you save and the table is read again.

Not possible: editing a cell in place (always through the dialog), editing a key column value the driver cut at the value size limit ("The value of … was cut when it was read: it cannot be edited or used as a key"), or editing a row marked for deletion. While changes are pending, the editor does not let you choose another connection ("There are unsaved changes in the result: save or discard them before choosing another connection.") or format the script.

## 3. Preview the SQL
Run "Preview changes…" (`Alt+E` then `P`). The "Changes to <table>" dialog shows the counts ("N updated, N inserted, N deleted") and the exact statements a save would run, one per row, with values written as literals of the connection's dialect, for example:
```sql
UPDATE "public"."customers" SET "email" = 'ada@example.org' WHERE "customer_id" = 42 AND "email" = 'ada@old.example'
```
- `UPDATE` and `DELETE` always have a `WHERE` on the key, plus the original values of the columns you edited, so a row changed by someone else since you read it is not overwritten (the save then fails, below).
- Columns whose type has no reliable equality (PostgreSQL `json`, `xml`, geometric types; MySQL `JSON`, spatial types, `BIT`, `FLOAT`…) are left out of that check: "Concurrent changes to these columns are not detected: …".
- What is sent is the same statement with every value bound as a parameter, never pasted into the text. The preview's literals mean the same in every session mode of the engine, so you can copy and run them elsewhere.

The script box shows the whole script, never cut:
- a size line, "N lines · M bytes";
- one row per line with its line number; a line longer than the box wraps onto the next rows, each marked `↪` in place of a line number, so the `WHERE` at the end of a long `UPDATE` is always there to read. Rows wrap to the box's current width, so a narrower window wraps sooner; wide characters (CJK, emoji, flags, arrows) are counted generously, and if a glyph is still wider than expected the box scrolls sideways;
- invisible, line-breaking and bidirectional characters shown as `�`, unusual spaces as `␣`, tabs as `→` (the navigator's rule), so the text cannot hide or reorder a clause;
- under the box, when the script is longer than the box (16 rows) or its end may be out of sight, "Ends with:" and its last line.

Only the rows in sight are drawn, so a save of thousands of rows opens at once and scrolls.

"Save changes" in the dialog saves; "Close" closes it.

## 4. Save
Run "Save changes" (`Alt+E` then `S`), or press "Save" in the toolbar. The status says "Saving…", then "Saving: N of M…".
- Each statement must change **exactly one row**. If one changes none (the row was changed or deleted meanwhile) or more than one, or anything fails or you cancel, the whole save is undone and NineSQL says "Not saved: change N of the save: …; nothing was saved", followed by "The changes are still pending." Read the table again ("Refresh table data", `Alt+T` then `R`) and redo the change.
- A save runs on its own: while it runs, the session refuses other statements ("A save, export or import is using this session: wait for it to end."). A session with a statement still running, or a result still being paged, refuses the save until it ends.
- **A table larger than one page.** Its result stays open while more rows can be fetched: you can edit the loaded rows, but "Save changes" says "Not saved yet: the table's result is still open (more rows can be fetched, or the statement is running). Press "Stop fetching" in the toolbar (or run "Cancel execution") to end it: the rows already loaded and your pending changes are kept. Then save again." (the button says "Cancel" on drivers that cancel on the server: PostgreSQL, MySQL, MariaDB, SQLite). Press it, then "Save changes" again: the tab stays editable after "Cancel" / "Stop fetching" (the status line says "stopped"), with the loaded rows and your pending changes. While the next page is being fetched, edits wait for it.
- One save takes at most 10 000 changes ("too many changes for one save (at most 10000): save in smaller steps").
- When it ends: "Saved N changes." in auto-commit mode (the save committed its own transaction). Run "Refresh table data" to see the rows as saved, inserted rows included.

## 5. Production and manual commit
**Confirmation.** On a connection typed production, or whose type confirms every write, "Save changes" first opens "Confirm save", in the type's colour: "Save N changes to <table> on a production connection? They run in one transaction.", the counts, the unchecked columns and the exact script in the same script box as the preview (section 3). "Save changes" saves exactly that; "Cancel", Escape or closing the dialog saves nothing ("Not saved: the changes were not confirmed. They are still pending."). You confirm once for the whole save, not once per row. If you change anything after the dialog appeared, or wait more than 10 minutes, the confirmation no longer matches and NineSQL asks again.

**Manual commit** (production connections by default). The save runs inside the session's open transaction (it opens one if needed), under a savepoint, and leaves the transaction open: "Saved N changes in the open transaction: commit to keep them (Commit transaction), or roll back." The toolbar shows "Transaction open". Run "Commit transaction" (`Ctrl+K` then `Ctrl+C`) to keep the changes, or "Roll back transaction" (`Ctrl+K` then `Ctrl+R`) to drop them with everything else of the transaction ([Run SQL](run-sql.md#7-commit-and-roll-back)).
- A failed save is undone on its own (back to the savepoint); your earlier uncommitted statements stay. If that undo itself fails, NineSQL rolls back the whole transaction and says so: "Not saved, and the session's whole transaction was rolled back: earlier uncommitted changes in this session were lost too." If even that rollback fails, NineSQL closes the session, which makes the server roll its transaction back, and says "Not saved, and the save could not be undone, so NineSQL closed the session: …": connect again to go on.
- An engine without savepoints refuses saves in manual-commit mode ("this engine has no savepoints, so rows cannot be saved in manual-commit mode: switch the session to auto-commit to save").
- In auto-commit mode, a save refuses a session with an open transaction ("a transaction is open on this session: commit or roll back first").

## 6. Filter and sort on the server
These re-run the table's query with a `WHERE` or an `ORDER BY`, so they see every row of the table, not only the loaded ones. They work on a tab opened with "Open table data" (or a foreign-key tab), and the result stays editable.

| Action | Default keys |
|---|---|
| "Filter table rows…" | `Alt+T` then `F` |
| "Filter table by selected value" | `Alt+T` then `V` |
| "Sort table by column ascending" / "descending" | `Alt+T` then `A` / `Alt+T` then `D` |
| "Clear table filter and sort" | `Alt+T` then `C` |
| "Refresh table data" | `Alt+T` then `R` |

- **"Filter table rows"** opens a dialog: "WHERE expression, run on the server. It must stay a read: no ';', UNION, ORDER BY or writes." Type a condition such as `price > 10 AND status = 'open'` and press "Apply"; "Clear filters" removes it. NineSQL refuses an expression that does not stay a read inside its parentheses ("the WHERE expression was refused: …"): no `;`, no placeholder, no `UNION`, `ORDER`, `LIMIT`, `INTO`, `FOR`, `UPDATE`, `DELETE`… outside strings, at most 16 KiB, and the whole query must classify as a read.
- **"Filter table by selected value"** adds `column = value` for the focused cell (`IS NULL` for a NULL); the value is bound, never pasted into the SQL.
- **"Recent filters"** in the dialog lists the filters used on this connection, newest first (up to 50). The list is kept in memory only and is empty after a restart. On production connections and types that do not log query text it keeps only the shape (columns, comparisons, order), never values or the typed `WHERE` text; then NineSQL says "Some values of this filter were not kept in the history: they were left out."

Client-side sort and filter of the grid ("Sort loaded rows ascending" `Alt+S` then `A`, "Sort loaded rows descending" `Alt+S` then `D`, "Filter loaded rows" `Alt+S` then `F`, "Filter loaded rows by selected value" `Alt+S` then `V`, "Clear sort and filters" `Alt+S` then `C`) work on any result but only on the **loaded rows**; the grid says "Loaded rows only" while they apply. Text is compared by code point, so the order can differ from the engine's. The client-side sort stops at 200 000 loaded rows: above that it is refused, or dropped if more rows arrive while it applies, and the grid says "Not sorted: more than 200000 rows are loaded, over the client-side sort limit. Sort on the server ("Order by") instead." On a table tab, use "Sort table by column ascending" / "descending" (the palette finds them with "order by"); on a typed query, add an `ORDER BY`. Client-side filters have no such limit.

## 7. Follow foreign keys
On a row of a tab opened with "Open table data":
- "Open referenced rows" (`Alt+T` then `K`) opens the row(s) the focused row's foreign key points to;
- "Open referencing rows" (`Alt+T` then `B`) opens the rows of other tables that point to the focused row.

With one target, it opens at once in a new tab; with several, a dialog lists them and you pick one: a referenced target as "<foreign key> → <table> (<columns>)", a referencing one as "<table> (<columns>)". Each target opens as a table tab with a bound filter, editable like any other. Referencing rows are searched in the tables of the same schema only (at most 512 tables): "Only the foreign keys of the same schema were searched (or the list was cut)." A NULL foreign key value references no row. SQLite names its foreign keys `fk_<id>`.

## 8. Record and text views, side panels
For any result, not only table data:

| Action | Default keys |
|---|---|
| "Show results as grid" / "Show row as record" / "Show results as text" | `Alt+1` / `Alt+2` / `Alt+3` |
| "Next record" / "Previous record" (record view) | `Alt+Down` / `Alt+Up` |
| "Toggle value panel" / "Next value viewer" | `Alt+V` / `Alt+Shift+V` |
| "Toggle aggregate panel" | `Alt+A` |
| "Toggle metadata panel" | `Alt+M` |

- **Record view**: one line per column (name, type, value) for the selected row, "Record N of M (Next record / Previous record)".
- **Text view**: the loaded rows as fixed-width text, at most 10 000 rows and 4 MiB ("Text view: first N of M rows (bounded).").
- **Value panel** ("Value"): the selected cell as text, JSON (indented, read-only; "Not valid JSON (at byte N): shown as text."), XML (indented, nothing resolved), hex, or an image preview (PNG and JPEG only, at most 16 MiB, 8 192 pixels a side and 4 096 × 4 096 pixels in all). GIF and animated PNG values are not previewed: the value panel offers hex only and says "GIF preview not supported; showing hex." or "Animated PNG preview not supported; showing hex.", because the GPU image decoder would decode every frame with no limit. A PNG whose chunks do not frame cleanly, or a JPEG without exactly one start-of-frame before its first start-of-scan (a JPEG with no start-of-scan included), is refused for preview. Previews you no longer show (another cell, viewer or result, the panel closed) are released from the GPU caches at the end of the frame. A long value shows its first part ("Showing the first N KiB of the value."); a value the driver cut says so.
- **Aggregate panel** ("Aggregates"): count, sum, average, minimum and maximum of the selected cells per column, loaded rows only; sums of floats or decimals are approximate, text and dates get count, minimum and maximum only; it stops after 1 000 000 cells and says so.
- **Metadata panel** ("Columns"): name, type, nullability and source of each column, as the driver reports them.

## Not built yet
- Editing a cell in place in the grid, drawing inserted rows before a refresh.
- A filter history kept after a restart.
- The editing actions in a context menu.
- QuestDB declares no editing.
- Value statistics in the metadata panel, folding in the JSON and XML viewers, GIF and animated PNG previews (shown as hex).
- The visual pass of these screens (P6-F).

## Next
- [Export and import data](export-import.md): files, copy formats, CSV import, export to DBeaver.
- How editing works inside: [docs/02](../02-architecture.md#data-editing-p5-a); the generated SQL per engine: [docs/08](../08-engine-capabilities.md#dml-generation-p5-b); the safety rules: [docs/05](../05-security.md#confirmations-of-bulk-writes-and-saves-p5).
