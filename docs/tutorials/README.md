# Tutorials

Step-by-step guides for people who build, try or contribute to NineSQL. They describe what the code on the `dev` branch does today, and they are updated when a user-visible behaviour or a command changes ([AGENTS.md](../../AGENTS.md#definition-of-done)).

NineSQL is **pre-alpha**: nothing is released, and you run it from source. What exists today is the application shell, the connection dialog and the PostgreSQL, MySQL, MariaDB and SQLite drivers. There is **no SQL editor that runs statements and no results grid until phase 3**, and **no DBeaver import screen until phase 2** ([ROADMAP](../ROADMAP.md)).

| Tutorial | For | You will |
|---|---|---|
| [Build and run NineSQL from source](build-and-run.md) | Everyone | Install the toolchain, build, start the `ninesql` window on Linux (and on macOS for UI work) |
| [Start the dev databases](dev-databases.md) | Contributors, testers | Run PostgreSQL, MySQL, MariaDB and the other engines in containers with the "nine" dataset |
| [Create your first connection](first-connection.md) | Everyone | Create, test and open a PostgreSQL, MySQL, MariaDB or SQLite connection: TLS, SSH, connection types |
| [Secrets: keyring, vault and locking](secrets.md) | Everyone | Understand where passwords go, create and unlock the vault, lock it, and what never reaches the disk |
| [Run the checks before a PR](checks-before-a-pr.md) | Contributors | Run what CI runs, with and without database engines |

Conventions used in these pages:
- Key bindings are the defaults; you can change them in `settings.toml` ([build and run](build-and-run.md#settings-and-key-bindings)). Every action is also in the command palette (`Ctrl+Shift+P` or `F1`).
- `make <target>` commands come from the root [`Makefile`](../../Makefile); `make help` lists them. Each target prints the plain command it runs, so you can also run that command directly.
- Labels in quotes ("Test connection") are the exact texts of the interface.

For the design behind these steps, see [docs/02](../02-architecture.md) (architecture), [docs/05](../05-security.md) (security) and [docs/08](../08-engine-capabilities.md) (per-engine capabilities).
