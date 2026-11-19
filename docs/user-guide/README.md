# NineSQL user guide

This guide is for people who use NineSQL to work with their databases. It explains what each part of the application does, what it refuses to do and why, and where its limits are. For step-by-step walkthroughs it points to the [tutorials](../tutorials/README.md), which follow the interface screen by screen.

> **Status.** NineSQL is **pre-alpha**: no version has been released yet, and you build it from source ([Install](install.md)). This guide describes the code on the `dev` branch as of phase 6 (hardening for v0.1.0). The interface was written and tested without a display; its visual check on real Linux desktops, and the measurements of start time, memory and scrolling, are part of the final tests before v0.1.0 (ROADMAP P6-C and P6-F). Labels and key bindings quoted here come from the code.

## Pages
| Page | What it covers |
|---|---|
| [Install](install.md) | The `.deb` (build it yourself for now), the Flatpak status, verifying release signatures, building from source, system requirements, where NineSQL keeps its files |
| [First start and secrets](first-start-and-secrets.md) | The first window, the three places a password can go (keyring, vault, ask every time), the vault master password, locking |
| [Import from DBeaver](import-from-dbeaver.md) | Bringing your DBeaver connections and saved passwords over, what is and is not imported, importing again |
| [Connections](connections.md) | The connection dialog, TLS, SSH tunnels, connection types, read-only and production connections, per-engine conditions |
| [SQL editor](sql-editor.md) | Running statements and scripts, parameters, completion, confirmations, transactions, script files and session restore, history and preferences |
| [Results, editing and transfer](results-editing-transfer.md) | The results grid, copying, editing table rows, exporting to files, importing CSV, exporting connections to DBeaver |
| [Navigator](navigator.md) | The object tree, filters, finding objects, the object editor, generated SQL and DDL |
| [Safety model](safety-model.md) | How read-only and production connections are protected, what NineSQL never does, and the limits of its statement checks |
| [Keyboard shortcuts](keyboard-shortcuts.md) | Every action with its default keys, and how to change them |
| [Troubleshooting](troubleshooting.md) | Common messages and what to do about them |
| [Known limitations](known-limitations.md) | What does not work yet, grouped by area, and what is planned for v0.1.0 |

## Engines
The v0.1.0 MVP supports eight engines: PostgreSQL, MySQL, MariaDB, SQLite, TimescaleDB, YugabyteDB (YSQL), QuestDB and TiDB. Some of them have conditions (no TLS on QuestDB and TiDB, read-only only under server-side conditions on QuestDB and TiDB, no editing or transactions on QuestDB): see [Connections](connections.md#engine-conditions) and the [capability matrix](../08-engine-capabilities.md#feature-capabilities). Twelve more engines are planned for later versions ([README](../../README.md#supported-databases)).

## Conventions
- "Label" in quotes is the exact text of the interface.
- `Ctrl+K` then `Ctrl+O` is a two-key sequence: press `Ctrl+K`, release, then press `Ctrl+O`.
- Key bindings are the defaults. You can change them ([Keyboard shortcuts](keyboard-shortcuts.md#change-a-binding)). Every action is also in the command palette (`Ctrl+Shift+P` or `F1`).
- On macOS (a development platform only) the defaults also use `Ctrl`, not `Cmd`.
