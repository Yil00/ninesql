# Feature parity inventory

NineSQL's long-term goal is **parity with the features DBeaver users rely on**, on the 20 engines of [ADR-0003](adr/0003-supported-engines-v1.md). This file is the inventory: each feature has a stable id (`F-<area>-<nn>`) that ROADMAP tasks and PRs quote.

> **Last audit (P0-E)**: 2026-11-14, against DBeaver **26.2.4** Community Edition (released 2026-11-02), the official documentation at <https://dbeaver.com/docs/dbeaver/> (built for 26.2.3) and the Community release notes from 24.2.5 to 26.2.4 (<https://dbeaver.io/news/>). Editions come from each documentation page's "This feature is available in … editions only" notes and its "Available only in PRO" markers. The **Audit** column gives the result for each row and the documentation page it was checked against (a page slug under the documentation URL above).

- **Edition**: where the feature lives in DBeaver.
  - `CE` = Community Edition (also in the commercial editions unless noted).
  - `PRO` = commercial editions only (Lite, Enterprise and Ultimate). `PRO (EE/UE)` = Enterprise and Ultimate only, not Lite. `UE` = Ultimate only.
  - `CE / PRO (…)` = the base feature is in CE; the part in parentheses is commercial-only.
  - `—` = NineSQL-specific, no DBeaver equivalent.
  - A trailing `?` marks an edition the audit could not confirm from the documentation.
  - Engines: MongoDB, Redis, Valkey, Cassandra, Neo4j and InfluxDB are PRO-only in DBeaver, so their parity target is DBeaver PRO's behavior. Elasticsearch and OpenSearch have a basic CE connection and "enhanced" support in PRO.
- **Phase**: the ROADMAP phase that delivers it. MVP = phases 0 to 6. `—` = added by an audit and not yet scheduled in the ROADMAP.
- **Status**: `planned` · `in progress` · `done` · `partial` (with a note) · `won't do` (with a reason). Updated by `docs-keeper` after each merge.
- **Audit**: `verified` (feature and edition confirmed) · `corrected` (edition or description changed by the audit; the change is described) · `added` (new row) · `unverified` (not found in the documentation; the previous edition is kept, with `?`).

Parity means the user can do the same job, not that the UI is a copy: NineSQL may solve a need differently (e.g. a command palette instead of a menu path). Descriptions state NineSQL's target; the Audit column says what DBeaver does where it differs.

## Connections (`CON`)
| Id | Feature | Edition | Phase | Status | Audit |
|---|---|---|---|---|---|
| F-CON-01 | Connection manager: create, edit, duplicate, delete, test connection | CE | 1 | done (P1-C, wave 3): connection dialog to create and edit, duplicate and delete (with confirmation) from the navigator, Test connection through the core connect flow; saves validated against the driver (`save_connections_checked`); the maintainer's visual check is pending | verified (`Create-Connection`, `Manage-Connections`) |
| F-CON-02 | Folders and drag-and-drop organization | CE | 1 | partial: folders in the navigator list, set in the dialog as a `/`-separated path (P1-C, wave 3); no drag-and-drop | verified (`Manage-Connections`, `Database-Navigator`) |
| F-CON-03 | Connection types (Dev / Test / Prod, custom) with color, auto-commit default, smart commit, confirmation of SQL execution and of data changes, auto-close of idle transactions and connections | CE | 1 | partial: types and their policies in the core model, single-use confirmation tokens for production statements (wave 2); type picked in the dialog, colored frame, type and read-only badges driven by the open session (P1-C, wave 3); the statement confirmation dialog waits for P3 to run statements; no editor for custom types; smart commit and auto-close open | corrected: read-only is not a connection-type setting in DBeaver (it is per connection, F-CON-11); smart commit and auto-close added (`Connection-Types`) |
| F-CON-04 | Saved passwords in a secure store; "ask every time" mode | CE / PRO (OS secure storage) | 1 | partial: backends done in `ninesql-secrets` (P1-D, wave 2): encrypted vault (Argon2id + XChaCha20-Poly1305), "ask every time" in-memory cache, selection keyring → vault → ask, never plaintext; the Secret Service backend is written but off by default until the desktop round trip passes, so default builds save passwords in the vault; vault locked and cache cleared on sleep and screen lock (wave 3a), in default builds through the `desktop-lock` feature and started by `CoreServices` with the idle lock (wave 3); UI (P1-C, wave 3): "saved" / "not saved" per secret field, "ask every time" prompts, vault unlock and create prompts, "Lock secrets now"; the "ask every time" cache is not used by the connect flow yet (asked at each connect) | corrected: CE saves credentials in a locally encrypted file (`credentials-config.json`); keeping them in secure storage is PRO (`Security`, `Admin-Manage-Connections`) |
| F-CON-05 | SSH tunnel: password, private key, agent; jump hosts; shared tunnels, keep-alive | CE | 1 | partial: `ninesql-ssh` (P1-D) has tunnels with password, Ed25519 / ECDSA key files, agent, jump hosts, keep-alive and per-hop host-key policies, and is wired into the core connect flow with a per-hop policy in `connections.json` (wave 2); RSA key files are refused (ADR-0016), no proxy in front of the first hop, shared tunnels not done; SSH settings in the connection dialog with jump hosts, per-hop host-key policies (`AcceptNew` / `Disabled` warned), host-key confirmation at connect (P1-C, wave 3) | verified (`SSH-Configuration`) |
| F-CON-06 | TLS: modes, CA, client certificates | CE / PRO (certificate management UI) | 1 | partial: pgwire (PostgreSQL) supports `disable`, `require`, `verify-ca`, `verify-full`, a CA file and PEM client certificates (unencrypted key), with `server_name` set behind SSH tunnels (wave 2); mysqlwire (MySQL, MariaDB) supports the same modes and trust rules (wave 3a; client certificates not integration-tested); TLS does not apply to sqlite (a local file, `tls` ignored); TLS settings in the connection dialog, modes offered from the driver's declared capabilities, with warnings for weakened TLS and `verify-ca` without a CA file; a mode the driver does not declare is refused at save (P1-C and core, wave 3); other drivers open | corrected: TLS settings are CE; the certificate (truststore) manager is PRO (`SSL-Configuration`, `Managing-Truststore-Settings`) |
| F-CON-07 | SOCKS / HTTP proxy | CE / PRO (automatic proxy detection) | 8 | planned | corrected: DBeaver's per-connection proxy is SOCKS; its HTTP proxy is global (driver downloads); automatic detection is PRO (`Proxy-configuration`) |
| F-CON-08 | Driver / connection properties (engine-specific options) | CE | 1 | partial: per-driver property allowlists (`Capabilities::connect`, driver-API PR); property keys offered in the connection dialog from the declared capabilities and refused keys explained (P1-C, wave 3); pgwire, mysqlwire and sqlite only | verified (`Create-Connection`) |
| F-CON-09 | Bootstrap queries / init SQL, default schema / catalog, isolation level | CE | 3 | partial: the core runs init SQL at connect after read-only is applied and asks for confirmation when the connection type requires it (wave 2); init SQL marked as imported (`init_sql_needs_confirmation`, set by the P2-C import, not built yet) is confirmed once whatever the type (pre-P2 hardening); default schema / catalog, isolation level and the UI are open | verified; isolation level added to the description (`Configure-Connection-Initialization-Settings`) |
| F-CON-10 | Keep-alive, auto-reconnect, close idle connections | CE | 3 | planned | verified (`Configure-Connection-Initialization-Settings`, `Manage-Connections`) |
| F-CON-11 | Read-only connection | CE | 1 | partial: read-only taken from the connection type policy and checked by the core (a session that is not read-only is closed); enforced engine-side by pgwire for PostgreSQL and verified after every statement (wave 2), re-asserted inside transaction blocks (wave 3a fix); enforced engine-side by mysqlwire for MySQL and MariaDB, read back and re-asserted after every statement (wave 3a); enforced engine-side by sqlite for SQLite (`OpenMode`: read-only open, `query_only`, an authorizer that allows only reads, no attached databases; verified after every statement, wave 3b); read-only badge in the UI driven by the open session, plus a "Read-only: DBeaver file" badge (P1-C, wave 3); a file inside a DBeaver workspace is also opened read-only by the core, which refuses a session the driver does not report read-only (pre-P2 hardening); other drivers are open | verified; DBeaver also offers finer restrictions: data edit, structure edit, script execution, data import (`Managing-security-restrictions-for-database-connection`) |
| F-CON-12 | Connect by URL / connection string | CE | 1 | partial: URL input in the connection dialog (P1-C, wave 3), for `postgresql:`, `mysql:`, `mariadb:` and `sqlite:` URLs; a password or any credential-like parameter in the URL is refused | verified (`Create-Connection`, release notes 25.2.1) |
| F-CON-13 | Multiple projects | CE | 10 | planned | verified (`Projects`) |
| F-CON-14 | Import connections from DBeaver (connections, folders, types, SSH, TLS, passwords, scripts) | — | 2 | partial: the read-only parser and mapping landed (P2-A, [docs/06](06-dbeaver-interop.md)); the import flow (P2-B), persistence and secret move (P2-C) and scripts are open | verified: NineSQL-specific |
| F-CON-15 | Export connections to DBeaver format (no passwords) | — | 5 | planned | verified: NineSQL-specific |
| F-CON-16 | Open a local database file (SQLite / DuckDB) directly; local engine detection (sockets) | CE (file open only) | 10 | partial: the sqlite driver opens a local SQLite file by absolute, canonical path (regular files only, created only for read-write sessions when asked) and private in-memory databases (P1-B, wave 3b); the connection dialog takes the file path (P1-C, wave 3); no File-menu open, DuckDB and local engine detection are open | corrected: DBeaver opens SQLite / DuckDB files from the File menu (release notes 25.3.4); automatic detection of local engines has no DBeaver equivalent |
| F-CON-17 | Network profiles: reusable SSH / proxy settings shared by several connections, global or per project | CE | 10 | planned | added (`Network-profiles`, release notes 26.1.3) |
| F-CON-18 | Password retrieved from a shell command at connect time | CE | — | planned | added (`Password-from-Shell-Command`, release notes 26.1.4; 26.2.3 added a confirmation before the command runs) |
| F-CON-19 | Change the current user's database password, including an expired password at login | CE | — | planned | added (`Change-current-user-password`: PostgreSQL and MySQL in CE, some other engines PRO) |
| F-CON-20 | Import connections from other tools (DataGrip, pgAdmin 4, DbVisualizer, MySQL Workbench, Navicat, SQL Developer, SQuirreL, Toad, CSV) | CE | — | planned | added (`How-to-import-Connections-from-External-Tools`); DBeaver does not import their secrets |
| F-CON-21 | Additional authentication methods: `.pgpass`; Kerberos, cloud IAM (AWS RDS, Entra ID), MySQL two-factor | CE / PRO (Kerberos, cloud IAM, two-factor) | — | planned | added (`Database-driver-PostgreSQL`, `Database-driver-MySQL`, `Kerberos-Authentication`) |
| F-CON-22 | Master password protecting every saved credential | PRO | — | planned | added (`Managing-Master-Password`); NineSQL's encrypted vault (ADR-0005) covers a similar need |

## Navigator and metadata (`NAV`)
| Id | Feature | Edition | Phase | Status | Audit |
|---|---|---|---|---|---|
| F-NAV-01 | Object tree: catalogs, schemas, tables, views, materialized views, columns | CE | 4 | planned | verified (`Database-Navigator`) |
| F-NAV-02 | Indexes, constraints, foreign keys, triggers, sequences, routines, types, partitions | CE | 4 | planned | verified (`Creating-Indexes`, `Implementing-Constraints`, `Incorporating-Triggers`, `How-to-work-with-database-Partitions`) |
| F-NAV-03 | Lazy, async loading with cancel; refresh per node | CE | 4 | planned | verified (`Database-Navigator`, `Background-Tasks`) |
| F-NAV-04 | Navigator filters (include / exclude patterns per connection) | CE | 4 | planned | verified (`Configure-Filters`, `Filter-Database-Objects`) |
| F-NAV-05 | Quick search of objects (fuzzy) across one or more connections, optionally in comments and definitions | CE | 4 | planned | corrected: DBeaver's metadata search matches "starts with", "contains" or a `LIKE` pattern, not fuzzy; fuzzy matching is a NineSQL choice (`DB-Metadata-Search`) |
| F-NAV-06 | Object editor: properties, columns, keys, indexes, DDL tab | CE | 4 | planned | verified (`Database-Object-Editor`, `Properties-Editor`) |
| F-NAV-07 | Generate DDL / SQL for objects (CREATE, SELECT, INSERT templates) | CE | 4 | planned | verified (`SQL-Generation`) |
| F-NAV-08 | Create / alter / drop objects through forms, with a script preview before apply | CE | 10 | planned | verified (`New-Table-Creation`, `Creating-columns`, `Properties-Editor`); creating a table from a diagram is PRO (see F-ERD-04) |
| F-NAV-09 | Object dependencies (uses / used by) | CE | 10 | planned | verified for PostgreSQL only ("Dependencies" tab in `Database-driver-PostgreSQL`); not documented for other engines |
| F-NAV-10 | Users, roles and grants browser / editor | CE | 10 | planned | verified (release notes 26.1.3, 26.2.2: PostgreSQL permissions tabs in CE) |
| F-NAV-11 | Full-text data search across tables | CE | 10 | planned | verified (`DB-Full-Text-Search`) |
| F-NAV-12 | Schema compare: simple structure diff; full schema compare with a migration script | CE / PRO (EE/UE: schema compare with migration script) | 10 | planned | corrected: "Simple structure compare" is CE; "Schema compare" (Liquibase-based, generates scripts) is Enterprise / Ultimate (`Simple-Structure-Compare`, `Schema-compare`) |

## SQL editor and execution (`SQL`)
| Id | Feature | Edition | Phase | Status | Audit |
|---|---|---|---|---|---|
| F-SQL-01 | Syntax highlighting per dialect (tree-sitter) | CE | 3 | planned | verified (`SQL-Editor`) |
| F-SQL-02 | Autocompletion: keywords, objects, columns, aliases, joins on foreign keys | CE / PRO (AI completion) | 3 | planned | verified for keywords, objects, columns, aliases and CTEs (semantic engine); join-on-foreign-key completion is not documented (`SQL-Assist-and-Auto-Complete`, `AI-Smart-Assistance`) |
| F-SQL-03 | Execute statement under cursor / selection / script; statement splitting per dialect | CE | 3 | planned | verified (`SQL-Execution`) |
| F-SQL-04 | Multiple result tabs; results in separate tabs or panels; pin and detach tabs | CE | 3 | planned | verified (`SQL-Execution`) |
| F-SQL-05 | Cancel running query (server-side where supported) | CE | 3 | partial: core cancel path (execution-scoped, waits for the server to confirm the end) and pgwire cancel request for PostgreSQL (wave 2); mysqlwire `KILL QUERY` on a control connection for MySQL and MariaDB (wave 3a); sqlite `sqlite3_interrupt`, also sent when a stream or session is dropped (wave 3b); no UI yet | verified (`Background-Tasks`; release notes 25.2.5: ClickHouse cancel) |
| F-SQL-06 | Query parameters and variables (`:name`, `?`, `${var}`, `{{name}}`) with a prompt | CE | 3 | planned | verified; `{{…}}` parameters added in 26.1 (`SQL-Execution`, `Variables-panel`) |
| F-SQL-07 | Transaction modes: auto-commit, manual, smart commit; commit / rollback; pending transactions view; transaction log | CE | 3 | planned | verified (`Auto-and-Manual-Commit-Modes`, `Pending-Transactions`, `Transaction-Log`) |
| F-SQL-08 | Execution log and query history (searchable, per connection) | CE / PRO (history kept after restart, filters, search) | 3 | planned | corrected: CE has the session's Query Manager log; history kept across restarts, filtering by type, time and connection, and history search are PRO (`Query-Manager`, `Lite-Edition`) |
| F-SQL-09 | SQL formatter (configurable) | CE | 5 | planned | verified (`SQL-Formatting`) |
| F-SQL-10 | SQL templates / snippets | CE | 5 | planned | verified (`SQL-Templates`) |
| F-SQL-11 | Script files: save, open, folders, linked to a connection | CE | 3 | planned | verified (`Script-Management`) |
| F-SQL-12 | Error position highlighting from the server's error | CE / PRO (AI error explanation) | 3 | planned | verified (`SQL-Editor`: "Error indication") |
| F-SQL-13 | Explain plan: text and visual tree, with costs | CE / PRO (some engines) | 10 | planned | corrected: CE for PostgreSQL, MySQL, Firebird; ClickHouse plans are PRO. The documentation does not say whether the graph view is CE (`Query-Execution-Plan`) |
| F-SQL-14 | Outline of the script, go to definition of objects | CE | 10 | planned | verified (`SQL-Editor`: "Outline", "Hyperlinks") |
| F-SQL-15 | Find / replace, multi-cursor, comment toggle, code folding | CE | 3 | planned | verified except multi-cursor, which DBeaver does not document (`SQL-Code-Editor`, `Shortcuts`) |
| F-SQL-16 | Execute script with "continue on error" / "stop on error" and a summary | CE | 5 | planned | verified: stop + rollback, stop + commit, ignore; commit every N statements (`SQL-Execution`) |
| F-SQL-17 | Visual query builder | PRO | 11 | planned | corrected: was `PRO?`; confirmed Lite, Enterprise, Ultimate (`Visual-Query-Builder`) |
| F-SQL-18 | Destructive statement confirmation (DROP, `UPDATE` / `DELETE` without `WHERE`) and confirmation of execution on production connections | CE | 3 | planned | corrected: was "CE (partial)"; DBeaver CE confirms recognized DROP statements, `UPDATE` / `DELETE` without `WHERE`, and every execution on types with "Confirm SQL execution" (`SQL-Execution`, `Configure-Confirmations`, `Connection-Types`) |
| F-SQL-19 | Client-side script commands: `@set`, `@include`, `@export`; `@pause`, `@accept`, `@ai` | CE / PRO (`@pause`, `@accept`, `@ai`) | — | planned | added (`Client-Side-Scripting`) |
| F-SQL-20 | Spell checking in scripts | PRO | — | planned | added (`Spelling`) |
| F-SQL-21 | Procedure debugger (PostgreSQL, through the `pldbgapi` extension) | PRO (EE/UE) | — | planned | added (`PGDebugger`, `Enterprise-Edition`, `Ultimate-Edition`) |

## Data viewer and editor (`DATA`)
| Id | Feature | Edition | Phase | Status | Audit |
|---|---|---|---|---|---|
| F-DATA-01 | Virtualized grid with streaming fetch, fetch next page / fetch all | CE | 3 | planned | verified (`Navigation`, `Data-Editor`) |
| F-DATA-02 | Record (form) view and text view | CE / PRO (JSON and XML document views) | 5 | planned | verified for record and text views; the grid's JSON and XML views are PRO (`Data-Editor`, `Data-View-and-Format`) |
| F-DATA-03 | Sort and filter: client-side and server-side (`WHERE` / `ORDER BY` rewriting), filter history | CE | 5 | planned | verified (`Data-Filters`); DBeaver also has find / replace and a quick filter in results (`Data-Search`) |
| F-DATA-04 | In-cell editing, insert / duplicate / delete rows, save with generated DML preview | CE | 5 | planned | verified (`Data-Viewing-and-Editing`) |
| F-DATA-05 | Value viewers: JSON (tree, folding), XML, text, binary (hex), image | CE / PRO (JSON / XML editors with folding) | 5 | planned | corrected: the CE value panel shows JSON / XML text with highlighting and formatting, text, binary, images and an HTML preview; the JSON / XML editors with folding and the JSON collapse settings are PRO (`Value-Panel`, `Working-with-XML-and-JSON`, `Data-Editor-preferences`) |
| F-DATA-06 | Advanced copy: CSV, TSV, Markdown, SQL `INSERT`, JSON | CE | 5 | planned | verified; DBeaver also copies as HTML, XML, TXT, DbUnit, source code (`Data-Viewing-and-Editing`) |
| F-DATA-07 | Navigate foreign keys (open referenced / referencing rows) | CE | 5 | planned | verified (`Navigation`, `References-Panel`) |
| F-DATA-08 | Foreign key value lookup / dictionary editing | CE | 10 | planned | verified (`Working-with-Dictionary-Data`) |
| F-DATA-09 | Aggregate panel (count, sum, avg, min, max of the selection) | CE | 5 | planned | verified (`Calc-Panel`) |
| F-DATA-10 | Column statistics / metadata panel | CE | 5 | planned | verified (`Metadata-Panel`) |
| F-DATA-11 | Grouping, pinning and hiding columns; color by value rules | CE | 10 | planned | verified (`Grouping-Panel`, `Data-Filters`, `Data-View-and-Format`) |
| F-DATA-12 | Spatial / GIS viewer (map for geometry columns) | CE / PRO (some engines) | 10 | planned | verified: CE for PostGIS, MySQL / MariaDB, SQLite GeoPackage, DuckDB, ClickHouse; Oracle, SQL Server and some cloud engines are PRO (`Working-with-Spatial-GIS-data`) |
| F-DATA-13 | Auto-refresh of results | CE | 10 | planned | verified (`Data-Refresh`) |
| F-DATA-14 | Compare two result sets / data compare | PRO (EE/UE) | 11 | planned | corrected: Enterprise and Ultimate only, not Lite (`Data-compare`) |
| F-DATA-15 | Charts from result sets | PRO | 11 | planned | added (`Managing-Charts`) |
| F-DATA-16 | Virtual model: virtual keys and foreign keys, calculated (virtual) columns, saved per connection | CE | 10 | planned | added (`Virtual-Keys`, `Virtual-column-expressions`) |

## Data transfer (`XFER`)
| Id | Feature | Edition | Phase | Status | Audit |
|---|---|---|---|---|---|
| F-XFER-01 | Export results / tables to CSV, TSV, JSON, SQL `INSERT`, Markdown, HTML (streamed) | CE | 5 | planned | verified; DBeaver CE also exports to TXT, XML, DbUnit, source code (`Data-transfer`, `Data-export`) |
| F-XFER-02 | Export to XLSX, XML, Parquet | CE / PRO (XLSX, Parquet) | 10 | planned | corrected: XML export is CE; XLSX and Parquet export are marked PRO in the format table (`Data-transfer`). The CE release notes of 26.2.4 mention XLSX export settings; the documentation is taken as the reference |
| F-XFER-03 | Import CSV into a table with column mapping and preview | CE | 5 | planned | verified (`Data-import`, `Data-Import-and-Replace`) |
| F-XFER-04 | Table-to-table transfer across connections (data migration) with type mapping | CE / PRO (AI mapping strategy) | 10 | planned | verified (`Data-migration`); not in Lite |
| F-XFER-05 | Saved tasks (re-runnable exports / transfers) | CE | 10 | planned | verified (`Task-Management`, `SQL-Script-Task`); not in Lite |
| F-XFER-06 | Task scheduling | PRO (EE/UE) | 11 | planned | corrected: `Task-Scheduler` says Enterprise and Ultimate, while `Task-Management` says Lite, Enterprise and Ultimate. Composite tasks are also EE/UE (`Composite-Tasks`) |
| F-XFER-07 | Mock data generation | PRO (EE/UE) | 11 | planned | corrected: Enterprise and Ultimate only, not Lite (`Mock-Data-Generation`) |
| F-XFER-08 | Import from XLSX and XML files | PRO | 11 | planned | added (`Data-transfer`) |

## Database tools (`TOOL`)
| Id | Feature | Edition | Phase | Status | Audit |
|---|---|---|---|---|---|
| F-TOOL-01 | Backup / restore through native tools (`pg_dump` / `pg_restore`, `mysqldump` / `mariadb-dump`…) when installed | CE | 10 | planned | verified (`Backup-Restore`, `Local-Client-Configuration`); not in Lite |
| F-TOOL-02 | Session manager: list sessions, cancel / kill | CE | 10 | planned | verified: CE for PostgreSQL, MySQL, MariaDB; MongoDB is PRO (`Session-Manager-Guide`) |
| F-TOOL-03 | Lock manager | CE | 10 | planned | verified; among the 20 engines DBeaver documents it for PostgreSQL only (`Lock-Manager`) |
| F-TOOL-04 | Maintenance: vacuum / analyze / optimize / reindex | CE? | 10 | planned | unverified: no documentation page; only listed as "Analyze and vacuum" in `Enterprise-Edition` |
| F-TOOL-05 | Server dashboards (connections, throughput, locks) | CE | 10 | planned | verified: predefined for PostgreSQL and MySQL (`Dashboards`); not in Lite |
| F-TOOL-06 | Execute SQL scripts from files against several connections | CE | 10 | planned | verified (`SQL-Script-Task`; release notes 26.2.3: run script files without opening them) |
| F-TOOL-07 | Execute a script through the engine's native client (`psql`, `mysql`) | CE | 10 | planned | added (`Execute-Script-with-Local-Client`) |

## ER diagrams (`ERD`)
| Id | Feature | Edition | Phase | Status | Audit |
|---|---|---|---|---|---|
| F-ERD-01 | Auto-generated diagram for a schema / selected tables | CE | 10 | planned | verified (`ER-Diagrams`) |
| F-ERD-02 | Custom diagrams: add tables, notes, layout, save in the project | CE | 10 | planned | verified (`Custom-Diagrams`, `How-to-use-Diagrams`) |
| F-ERD-03 | Export diagram as PNG / SVG | CE / PRO (SVG) | 10 | planned | corrected: PNG, GIF, BMP, GraphML export and printing are CE; SVG export is PRO (`ER-Diagrams`) |
| F-ERD-04 | Edit schema from the diagram | PRO (EE/UE) | 11 | planned | corrected: was `PRO?`; confirmed Enterprise and Ultimate only, not available on custom diagrams (`Edit-mode`) |

## NoSQL engines (`NOSQL`)
| Id | Feature | Edition | Phase | Status | Audit |
|---|---|---|---|---|---|
| F-NOSQL-01 | MongoDB: databases / collections / indexes tree, document grid and JSON view, find / aggregate editor, edit documents | PRO | 8 | planned | verified (`MongoDB`); DBeaver also runs JavaScript and SQL against MongoDB |
| F-NOSQL-02 | Redis / Valkey: key browser by pattern (SCAN), typed value editors (string, hash, list, set, zset, stream), TTL, command console | PRO | 8 | planned | partly verified: the hierarchical key browser with include / exclude patterns and commands / Lua in the SQL editor are documented; typed editors, TTL and streams are not (`Redis`) |
| F-NOSQL-03 | Elasticsearch / OpenSearch: indices tree, SQL endpoint and query DSL editor, document view | CE / PRO (enhanced) | 9 | planned | corrected: was `PRO`; both engines are available in CE, with "enhanced features" in PRO (`Ultimate-Edition` database table). Query DSL editing is not documented |
| F-NOSQL-04 | Cassandra: keyspaces / tables tree, CQL editor, paging, query trace | PRO | 9 | planned | verified (`Cassandra`; query trace panel in `Panels`) |
| F-NOSQL-05 | Neo4j: labels / relationship types, Cypher editor, table and graph result views | PRO | 9 | planned | partly verified: objects, Cypher and the JSON view are documented; a graph result view is not (`Database-driver-Neo4j`) |
| F-NOSQL-06 | InfluxDB: databases / measurements tree, query editor | PRO | 7 | planned | added: InfluxDB (1, 2, 3) is PRO-only in DBeaver, with InfluxQL and Flux editors (`InfluxDB`, `Ultimate-Edition`); NineSQL targets SQL on InfluxDB 3 Core (ADR-0003) |

## Workspace and UX (`WS`)
| Id | Feature | Edition | Phase | Status | Audit |
|---|---|---|---|---|---|
| F-WS-01 | Tabs, split panes, dockable panels, multiple windows | CE | 1 | partial: tabs, splits, status bar, multiple windows and a resizable, hideable navigator dock landed (P1-A); panels cannot be moved to another dock position yet | verified (`Application-Window-Overview`) |
| F-WS-02 | Command palette and configurable key bindings | CE | 1 | done (P1-A): 19 actions, each with a default binding and a palette entry (28 since P1-C, wave 3); overrides in `settings.toml`; bindings changed from the UI (`keymap::set_override`) apply at once and are written back (wave 2) | corrected: was `—`; DBeaver CE has "Find actions" (Ctrl+3) and configurable key bindings (`Shortcuts`) |
| F-WS-03 | Light / dark themes, font settings, HiDPI and fractional scaling on Wayland | CE | 1 | partial: light / dark / system themes, fonts and zoom landed (P1-A); changes from the UI are saved to `settings.toml` (wave 2: only changed keys, comments kept, atomic `0600` writes); fractional scaling relies on GPUI's scale factor and is not checked on a real display yet | verified (`User-Interface-Themes`; release notes 26.1.2, 26.2.2) |
| F-WS-04 | Preferences UI (global and per connection) | CE | 3 | planned | verified (`Admin-Manage-Preferences`) |
| F-WS-05 | Bookmarks on objects and scripts | CE | 10 | planned | verified (`Bookmarks`) |
| F-WS-06 | Session restore (open tabs and scripts after restart) | CE? | 3 | planned | unverified: not documented |
| F-WS-07 | Git-friendly projects (scripts and connections without secrets) | CE / PRO (EE/UE: integrated Git) | 10 | planned | corrected: integrated Git is Enterprise / Ultimate (`Project-team-work`); CE can add a Git extension (release notes 26.2.4) |
| F-WS-08 | AI assistant (opt-in, user-provided endpoint, never sends data without confirmation) | CE / PRO (query generation, error fixing, other providers) | 11 | planned | corrected: CE chat works with OpenAI (including a custom base URL) and GitHub Copilot; query generation, error fixing, object exploration, AI tools / MCP and the other providers (Ollama, Anthropic, Gemini, Azure, Bedrock, Grok) are PRO (`AI-Smart-Assistance`) |
| F-WS-09 | Localization (English first, then community translations) | CE | 11 | planned | verified (`UI-Language`) |

## Not inventoried
The audit found these DBeaver features outside the 20-engine desktop scope. They have no row on purpose; giving one a row is a scope decision for the ROADMAP:
- Cloud Explorer and Cloud Storage for AWS, GCP and Azure (`UE`), Kafka (`UE`), Tableau integration (`PRO (EE/UE)`).
- Secret providers (HashiCorp Vault, AWS Secrets Manager, Azure Key Vault…), Kubernetes and AWS SSM network handlers (`PRO`).
- Liquibase changelogs (`PRO (EE/UE)`), multi-source cross-database queries (`UE`), the ODBC driver and CData drivers (`PRO`).
- Data transfer to email, Slack or cloud storage (`PRO`), the MCP server (`PRO`), Team Edition collaboration.
- Engines outside ADR-0003.

## Deliberately different from DBeaver
- **Passwords** are never stored with a shared obfuscation key (ADR-0005).
- **Read-only** is enforced engine-side (ADR-0007).
- **No plugin marketplace** in v1: extensions may come later through a new ADR.
