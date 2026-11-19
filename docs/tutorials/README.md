# Tutorials

Step-by-step guides for people who build, try or contribute to NineSQL. They describe what the code on the `dev` branch does today, and they are updated when a user-visible behaviour or a command changes ([AGENTS.md](../../AGENTS.md#definition-of-done)).

NineSQL is **pre-alpha**: nothing is released, and you run it from source. What exists today is the application shell, the connection dialog, the PostgreSQL, MySQL, MariaDB, SQLite, TimescaleDB, YugabyteDB, QuestDB and TiDB drivers, the DBeaver import (phase 2), the SQL editor with its results grid (phase 3) and the navigator with its object tree, object editor and object completion (phase 4, closed 2026-11-16), and data editing, export and import, the export of connections to DBeaver, "Format SQL" and snippets (phase 5, closed 2026-11-18), and script files with session restore (a phase 3 follow-up). Not built yet: in-cell editing ([ROADMAP](../ROADMAP.md)). The screens were written without a display; their visual pass is deferred to the end of the MVP (P6-F).

| Tutorial | For | You will |
|---|---|---|
| [Build and run NineSQL from source](build-and-run.md) | Everyone | Install the toolchain, build, start the `ninesql` window on Linux (and on macOS for UI work), build and install the `.deb` |
| [Start the dev databases](dev-databases.md) | Contributors, testers | Run PostgreSQL, MySQL, MariaDB, TimescaleDB, YugabyteDB, QuestDB, TiDB and the other engines in containers with the "nine" dataset |
| [Create your first connection](first-connection.md) | Everyone | Create, test and open a PostgreSQL, MySQL, MariaDB or SQLite connection: TLS, SSH, connection types; the conditions of TimescaleDB, YugabyteDB, QuestDB and TiDB |
| [Run SQL and read results](run-sql.md) | Everyone | Open an SQL editor on a connection, run statements, selections and scripts with parameters, confirm on production, cancel, commit or roll back, page through and copy results, set timeouts and history retention, format SQL, insert snippets, save and open script files, reopen them after a restart |
| [Browse a database](browse-database.md) | Everyone | Expand the object tree, load more, refresh, filter, find an object, read it in the object editor, generate SQL templates and DDL, complete object names in the editor |
| [Edit data](edit-data.md) | Everyone | Open a table's rows, edit cells, insert, duplicate and delete rows, preview the SQL, save (with confirmation on production, commit in manual-commit mode), filter and sort on the server, follow foreign keys, use the record and text views and the side panels |
| [Export and import data](export-import.md) | Everyone | Export results and tables to CSV, TSV, JSON, SQL, Markdown or HTML, understand the file rules and the formula guard, copy in several formats, import a CSV file with a preview, mapping and confirmation, export connections to DBeaver and check the result |
| [Import from DBeaver](import-from-dbeaver.md) | DBeaver users | Preview and import your DBeaver connections, move their saved passwords to the vault or keyring, enable connections imported disabled |
| [Secrets: keyring, vault and locking](secrets.md) | Everyone | Understand where passwords go, create and unlock the vault, lock it, and what never reaches the disk |
| [Run the checks before a PR](checks-before-a-pr.md) | Contributors | Run what CI runs, with and without database engines |

Conventions used in these pages:
- Key bindings are the defaults; you can change them in `settings.toml` ([build and run](build-and-run.md#settings-and-key-bindings)). Every action is also in the command palette (`Ctrl+Shift+P` or `F1`).
- `make <target>` commands come from the root [`Makefile`](../../Makefile); `make help` lists them. Each target prints the plain command it runs, so you can also run that command directly.
- Labels in quotes ("Test connection") are the exact texts of the interface.

For an overview of each feature written for end users, with its limits, see the [user guide](../user-guide/README.md); its [known limitations](../user-guide/known-limitations.md) page lists what does not work yet.

For the design behind these steps, see [docs/02](../02-architecture.md) (architecture), [docs/05](../05-security.md) (security) and [docs/08](../08-engine-capabilities.md) (per-engine capabilities).
