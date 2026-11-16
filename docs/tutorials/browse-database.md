# Browse a database

This tutorial browses a database in the navigator (phase 4): expand the object tree, load more objects, refresh, filter, find an object, read it in the object editor, generate SQL templates and DDL, and use object completion in the SQL editor; from phase 5, open a table's rows, export it or import a CSV file into it. It uses the PostgreSQL dev database and its "nine" dataset; the other engines work the same way, with the object kinds their driver declares.

> Written without a display: the labels and keys below come from the code, but the screens have not had their visual pass yet (deferred to the end of the MVP, P6-F). The navigator is tested with the mock driver; the drivers' metadata is tested against each dev engine.

## Before you start
- NineSQL builds and runs ([Build and run](build-and-run.md)).
- The dev databases are up (`make dev-up`, [Start the dev databases](dev-databases.md)) and a `nine-pg` connection is saved ([Create your first connection](first-connection.md)).

Every action below is in the command palette (`Ctrl+Shift+P` or `F1`, type "navigator", "object" or "generate") and in the context menu of a navigator row (right click), which lists the actions that apply to that row. The actions work on the object selected in the navigator; "Copy DDL" also works on the active object editor.

## 1. Expand the tree
1. Show the navigator (`Ctrl+B`) and move the focus there (`Ctrl+K` then `Ctrl+N`).
2. Select `nine-pg` and press `Right` (or click it). If the connection is not open, expanding it starts the usual connect flow (vault unlock, "ask every time" password, SSH host key); a closed connection's row says "Not connected ("Connect": Ctrl+K Ctrl+O)".
3. Under an open connection, the tree shows one group per object kind the driver declares, for example "Schemas", then under `public` "Tables", "Views", "Sequences", "Types", "Routines", and under a table "Columns", "Constraints", "Foreign keys", "Indexes", "Triggers", "Partitions". A kind the engine does not have is not shown (an engine with none says "No objects are listed for this engine"). A connection with a single top-level kind expands that group at once.
4. Each group loads when you expand it ("Loading…"), then shows its count. Collapsing a group that is still loading cancels the request on the server. A group that failed shows the error, "… (Enter to retry)"; an empty one shows "(empty)".

Keyboard in the tree: `Up` / `Down`, `Home` / `End`, `Page Up` / `Page Down` move the selection; `Right` expands (or moves to the first child), `Left` collapses (or moves to the parent); `Enter` expands or collapses a row, and on a table, view or materialized view opens the object editor (a double click does the same; a single click expands).

What each engine lists (PostgreSQL has no database level: a session is bound to one database; MySQL, MariaDB and TiDB list databases as schemas; QuestDB has no schemas; SQLite has no constraints group and names foreign keys `fk_<id>`) is in [docs/08](../08-engine-capabilities.md#feature-capabilities).

## 2. Load more and refresh
- A group loads at most 10 pages of 1 000 objects at a time. When the server has more, the group ends with a "Load more" row: press `Enter` on it, click it, or run "Load more objects" (`Ctrl+K` then `Ctrl+M`) with the group or one of its objects selected. With nothing more to load, NineSQL says "The selected group has no more objects to load."
- "Refresh navigator node" (`F5`) reloads the selected group, the group of the selected object, or a whole connection. Loaded metadata is cached per connection (at most 4 096 entries and 64 MiB) and dropped when the session closes, so reconnecting starts from a fresh tree.

## 3. Filter what the navigator shows
1. Select the connection (or anything under it) and run "Configure navigator filters" (`Ctrl+K` then `Ctrl+F`).
2. The "Navigator filters" dialog has one row per object kind the driver declares ("Tables", "Views"…), under: "Objects of "<connection>" shown in the navigator. Patterns use * and ?, separated by commas; an object is shown when it matches an include pattern (or there is none) and no exclude pattern."
3. Fill the include field ("Include: e.g. app_*, audit") and the exclude field ("Exclude: e.g. tmp_*, *_old"). A pattern matches the whole name: `*` is any run of characters, `?` one character. Matching ignores case unless you tick "Case-sensitive" for that kind.
4. Press "Save". Invalid patterns are shown on their kind and nothing is saved ("Some patterns are not valid: see the messages above."): at most 64 patterns per list, up to 256 bytes each, no control characters.

The tree reloads; a group with hidden objects ends with "N hidden by filter". Filters are saved per project in `navigator.json`, next to `connections.json` ([where files are](build-and-run.md#where-ninesql-keeps-its-files)); they hold patterns only, never a secret. Deleting a connection does not remove its filters from that file yet.

## 4. Find an object
1. Run "Find object" (`Ctrl+Shift+O`). The "Find object" dialog searches every open connection; untick a connection to leave it out.
2. Type part of a name: the match is fuzzy ("Object name (fuzzy), e.g. ordit for order_items"). Tick "Search comments" or "Search definitions (loaded DDL)" to match those too.
3. `Up` / `Down` move in the results; `Enter` (or a click) reveals the object in the navigator, expanding the path to it; `Ctrl+Enter`, a double click or "Open" opens it in the object editor.

Search looks only at the metadata loaded so far ("Search looks at the metadata loaded so far (expanded nodes, or "Load metadata")."); it never crawls the server on its own. "Load metadata" loads more in the background, within limits (256 nodes, 4 levels deep), and stops when you close the dialog. When an object cannot be revealed, NineSQL says it may be hidden by a filter, not loaded yet ("Load more") or gone.

## 5. Read an object in the object editor
Select a table and run "Open object editor" (`Ctrl+K` then `Enter`), or press `Enter` on it. The editor opens as a tab with:
- **"Properties"**: name, kind, connection, comment, and what the engine reports (estimated rows, flavour such as `hypertable` on TimescaleDB, column type and nullability).
- **"Columns"**: name, type, null, default, primary key ("PK"), generated ("Auto"), comment.
- **"Keys / Constraints"**: primary key, unique, check and exclusion constraints and foreign keys, with their columns and targets in "Details".
- **"Indexes"**: key parts, uniqueness, method and partial-index predicate where the engine reports them, and the flags that matter for editing: "partial" (also for a SQLite partial index, whose predicate is not read), "invalid", "deferrable" and "NULLS NOT DISTINCT", for example "unique · (email) · partial". A partial, invalid or deferrable unique index, or one with an expression part, is never used as the key to edit rows ([Edit data](edit-data.md#1-open-table-data)).
- **"DDL"**: only when the driver can produce DDL for this kind of object; read-only, highlighted, with "Copy DDL".

A list the engine does not provide says "Not available for this engine." instead of an empty list. The toolbar has one button per SQL template that applies (section 6) and "Refresh". The editor reloads when the metadata under it is refreshed or filtered; when the session closes it keeps what it shows and says "The session is closed: reconnect, then Refresh."

## 6. Generate SQL
Select a table or view and run one of:

| Action | Keys | Offered for |
|---|---|---|
| "Generate SQL: SELECT" | `Alt+G` then `S` | tables, views, materialized views |
| "Generate SQL: INSERT" | `Alt+G` then `I` | tables and views, sessions that are not read-only |
| "Generate SQL: UPDATE" | `Alt+G` then `U` | tables and views, sessions that are not read-only |
| "Generate SQL: DELETE" | `Alt+G` then `D` | tables and views, sessions that are not read-only |

The template opens in a **new SQL editor** on the same connection; **nothing runs it**. Names are quoted for the session's engine; values and conditions are `<placeholders>` that no engine accepts, so a template run without editing fails instead of writing. For `orders` in the dev PostgreSQL, "Generate SQL: UPDATE" gives something like:
```sql
UPDATE "public"."orders"
SET
  "customer_id" = <customer_id>,
  "product_id" = <product_id>,
  …
WHERE "id" = <id>;
```
- `UPDATE` and `DELETE` always have a `WHERE` line. The key condition comes from the primary key only when the metadata is complete; otherwise it reads `WHERE <condition>` with a comment: "-- The primary key could not be read in full (filtered or incomplete metadata): write the condition." A column list built from a filtered or incomplete list gets "-- Some columns are not listed (a navigator filter hides them, or the list is incomplete)."
- On a read-only session only `SELECT` is offered ("This session is read-only: only the SELECT template is offered.").
- A table or column name with control or line-separator characters gets no template, because the editor would change the name and the template could target another object.
- Running an edited template goes through the usual flow, including the "Run on production?" confirmation ([Run SQL](run-sql.md)).

## 7. Show and copy DDL
- "Show DDL" (`Alt+G` then `L`) opens the selected object's DDL in a new SQL editor titled "DDL <name>". DDL text with characters the editor would change opens in the object editor's read-only "DDL" tab instead.
- "Copy DDL" (`Ctrl+K` then `Ctrl+Shift+C`) copies the DDL of the active object editor, or of the selected object, and says "DDL copied."
- DDL is offered only where the engine provides it: for example no DDL for columns, PostgreSQL base types and foreign tables, or SQLite indexes created by a constraint ("DDL is not available for this object on this engine (or the connection is not open)."). DDL is never cut: one larger than 1 MiB is an error. What each engine's DDL leaves out (PostgreSQL collations, privileges and identity options; a TimescaleDB hypertable's `create_hypertable` call) is in [docs/08](../08-engine-capabilities.md#feature-capabilities).

## 8. Object completion in the SQL editor
Open an SQL editor on the connection (`Ctrl+T`, [Run SQL](run-sql.md)) and press `Ctrl+Space` ("Show completions"). Besides keywords, aliases and CTE names, completion now offers schemas, tables, views, routines and columns:
- It offers only metadata already loaded: what you expanded in the navigator, what "Find object" loaded, and the default schema, which NineSQL loads in the background (its objects, then their columns, within limits) when the editor is bound to the connection.
- Unqualified names resolve in that default schema: the connection's default schema when it has one (set by a DBeaver import) and the server lists it, otherwise the first schema the driver lists.
- Names the editor would change are not offered.

## 9. Open a table's rows, export, import
With a table (or view) selected in the navigator:
- "Open table data" (`Alt+G` then `O`) opens its rows in a new tab, editable when the table has a usable key ([Edit data](edit-data.md));
- "Export table…" (`Alt+G` then `X`) writes every row to a file, "Import CSV…" (`Alt+G` then `M`) loads a CSV file into a table ([Export and import data](export-import.md)).

These actions are in the command palette and on their keys; they are not in the navigator's context menu yet.

## Labels with unusual characters
Object names are shown as stored, with markers for what would otherwise be invisible or misleading: control, bidirectional and zero-width characters appear as `�` (U+FFFD), space characters other than the ordinary space as `␣` (U+2423), and a leading or trailing space as `␠` (U+2420). Long names are cut at 200 characters with `…`. These markers are labels only; generated SQL always uses the real name, quoted.

## Not built yet
- Completion of joins on foreign keys.
- Browsing other databases of a PostgreSQL server from one connection (no database level).
- Creating, altering or dropping objects from forms (F-NAV-08, phase 10), object dependencies, users and roles.
- Settings for the load limits and the background loading.
- The visual pass of the navigator and the object editor (P6-F).

## Next
- [Run SQL and read results](run-sql.md).
- [Edit data](edit-data.md) and [Export and import data](export-import.md).
- Per-engine object kinds, DDL and limits: [docs/08](../08-engine-capabilities.md).
- How the metadata service works: [docs/02](../02-architecture.md#metadata-service-p4-b).
