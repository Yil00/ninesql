# Navigator

The navigator lists your connections and, under an open connection, its objects. Step by step: [Browse a database](../tutorials/browse-database.md).

## The tree
- "Toggle database navigator" (`Ctrl+B`) shows or hides it; "Focus database navigator" (`Ctrl+K` then `Ctrl+N`) moves the keyboard there.
- Expanding a connection that is not open starts the usual connect flow. Under an open connection, the tree shows one group per object kind the driver declares (below); a kind the engine does not have is not shown.
- Groups load when you expand them, at most 10 pages of 1 000 objects at a time; a "Load more" row (or "Load more objects", `Ctrl+K` then `Ctrl+M`) loads the next ones. Collapsing a group that is still loading cancels the request on the server.
- "Refresh navigator node" (`F5`) reloads a group, the group of an object, or a whole connection. Loaded metadata is cached per connection and dropped when the session closes.
- Keyboard: arrows, `Home` / `End`, `Page Up` / `Page Down`; `Right` expands, `Left` collapses, `Enter` expands or, on a table or view, opens the object editor. A right click lists the actions that apply to the row.
- Navigator queries only read, and work on read-only sessions.

## What each engine lists
| Engine | Object kinds |
|---|---|
| PostgreSQL, TimescaleDB, YugabyteDB | Schemas; tables (partitioned and foreign included), views, materialized views, sequences, types, routines; under a table: columns, constraints, foreign keys, indexes, triggers, partitions. One database per connection. TimescaleDB: hypertables and continuous aggregates, chunks hidden |
| MySQL, MariaDB | Databases (as schemas); tables, views, routines, events (and sequences on MariaDB); under a table: columns, constraints, foreign keys, indexes, triggers, partitions |
| TiDB | Databases (as schemas); tables, views, sequences; under a table: columns, constraints, foreign keys, indexes, partitions (TiDB has no triggers, routines or events) |
| QuestDB | Tables, views, materialized views (no schemas); columns and time partitions |
| SQLite | Schemas (`main`, `temp`); tables (virtual included), views; under a table: columns, foreign keys (named `fk_<id>`), indexes, triggers (no constraints group) |

Details per engine: [docs/08](../08-engine-capabilities.md#feature-capabilities).

## Filters
"Configure navigator filters" (`Ctrl+K` then `Ctrl+F`) sets, per object kind, include and exclude patterns (`*` any run of characters, `?` one character, comma-separated, case-insensitive unless ticked). A group with hidden objects ends with "N hidden by filter". Filters are saved per project in `navigator.json`.

## Find an object
"Find object" (`Ctrl+Shift+O`) searches every open connection with a fuzzy match ("ordit" finds `order_items`), optionally in comments and loaded DDL. It looks only at **metadata already loaded**; "Load metadata" loads more in the background, within limits (256 nodes, 4 levels deep). `Enter` reveals the object in the tree, `Ctrl+Enter` opens it in the object editor.

## Object editor
"Open object editor" (`Ctrl+K` then `Enter`) opens a tab with "Properties", "Columns", "Keys / Constraints", "Indexes" and, when the engine can produce it, "DDL". A list the engine does not provide says "Not available for this engine."

## Generated SQL and DDL
- "Generate SQL: SELECT / INSERT / UPDATE / DELETE" (`Alt+G` then `S` / `I` / `U` / `D`) opens a template in a **new editor; nothing runs it**. Values and conditions are `<placeholders>` that no engine accepts, so a template run without editing fails instead of writing. `UPDATE` and `DELETE` always have a `WHERE` line. On a read-only session, on a view and on an engine that declares no row editing (QuestDB) only `SELECT` is offered.
- "Show DDL" (`Alt+G` then `L`) opens an object's DDL in a new editor; "Copy DDL" (`Ctrl+K` then `Ctrl+Shift+C`) copies it. DDL is never cut (over 1 MiB it is an error). Some engines leave parts out: PostgreSQL collations, privileges and identity options; a hypertable's `create_hypertable` call.
- "Open table data" (`Alt+G` then `O`), "Export table…" (`Alt+G` then `X`) and "Import CSV…" (`Alt+G` then `M`) work on the selected table ([Results, editing and transfer](results-editing-transfer.md)). "Open table data" is also the first entry of a table or view row's context menu.

## Names with unusual characters
Object names are shown as stored, with visible markers for what would otherwise be invisible or misleading: control, bidirectional and zero-width characters as `�`, unusual spaces as `␣`, a leading or trailing space as `␠`. Generated SQL always uses the real name, quoted.

## Not built yet
- Browsing other databases of a PostgreSQL server from one connection.
- Create, alter or drop forms, dependencies, users and roles, schema compare, ER diagrams (phase 10).
- The export and import actions in the context menu (use the keys or the palette).
- Settings for the load limits; removing the filters of a deleted connection from `navigator.json`.
