# End-to-end scenarios (ADR-0029 step 2)

Scenario files for the scenario runner of the **test build** of `ninesql` (cargo feature
`e2e-scenarios`, off by default, Linux only; [ADR-0029](../docs/adr/0029-automated-ui-end-to-end-tests-and-application-logging.md) §2).
A scenario drives a real NineSQL window through the same actions key bindings and the command
palette dispatch, reads view states through a read-only probe, and marks checkpoints where the
harness of `make e2e-local` takes a screenshot of the NineSQL window. Scenario files are code:
they are reviewed like source code.

The test build is never packaged: `make e2e-feature-guard`, a compile-time refusal when
`NINESQL_PACKAGE_BUILD` is set, a marker string the package checks refuse, and a `--version`
line that says `(e2e-scenarios test build, not for distribution)` keep it out.

## Running the scenarios

`make e2e-local` runs them all in its `scenarios` stage: it builds the test binary in
`target/e2e-build/`, creates the run directories, takes a screenshot of the test window at each
checkpoint and puts the results in its report (`make e2e-local E2E_ARGS="--only scenarios"`, or
`--scenario NAME[,NAME]` for some of them). See the
[local end-to-end tutorial](../docs/tutorials/local-e2e.md) for the options and how to read the
results.

## Running one scenario by hand

```sh
CARGO_TARGET_DIR=target/e2e-build cargo build --release -p ninesql-ui --bin ninesql --features e2e-scenarios
run="$(mktemp -d -t ninesql-e2e-XXXXXX)"            # mode 0700, under the temporary directory
mkdir -m 0700 "$run"/{home,config,data,state,cache,out}
env HOME="$run/home" XDG_CONFIG_HOME="$run/config" XDG_DATA_HOME="$run/data" \
    XDG_STATE_HOME="$run/state" XDG_CACHE_HOME="$run/cache" \
    NINESQL_LOG=info NINESQL_E2E_COMMIT="$(git rev-parse HEAD)" \
    target/e2e-build/release/ninesql --e2e-run e2e/scenarios/sqlite-editor-grid.toml --e2e-out "$run/out"
```

The dev engines must be up for the scenarios that name them (`python3 dev/ready.py up core`).
`NINESQL_LOG` turns the file log on (`$run/state/ninesql/logs/ninesql.log`); `NINESQL_E2E_COMMIT`
is only copied into `results.json`. No environment variable starts a run.

Before anything is read or written, the runner checks that `HOME` and the four XDG directories
are set, absolute, inside one directory that also holds `--e2e-out`, owned by you with mode
`0700`, under the system temporary directory or a `target/` directory, not your account's home
directory (from the password database) nor a default XDG directory under it, and that the
NineSQL directories in it are empty. It then installs the core's guards: connections to
loopback addresses only (after DNS resolution, SSH hops included), export and import files and
database files inside the run directory, the encrypted vault in the run directory (created with
a random master password that is never written anywhere), never the keyring. A dev engine of
`dev/engines.json` must be at a loopback IP address before its reachability probe opens a
socket: any other entry, a host name included, is a refusal (status 2), not a skip.

### Limits of the run-directory confinement

The guards cover what NineSQL itself opens: connections, export and import files, database
files and its own storage. They do not make the run a sandbox:

- **File I/O done by an engine through SQL.** A statement can read or write files the core never
  sees as paths: SQLite `ATTACH DATABASE` or `VACUUM INTO` on a SQLite connection, and the
  server-side file functions of the dev engines (which run in their containers).
- **Files read as they are.** Read-only inputs such as a CA file or an SSH key named by a
  connection are read where they are, outside the run directory.
- **Inherited environment.** `SSH_AUTH_SOCK` and the agent's keys stay reachable when it is set;
  the harness of `make e2e-local` runs with what the desktop session gives it.
- **Same-user races and hard links.** The checks of paths are done by the user who runs the
  scenario; another process of that user can swap a path between the check and the open, and a
  hard link inside the run directory can point at a file outside it.

This is acceptable because scenario files are code, reviewed like the rest of the repository:
the confinement guards against mistakes in a scenario and in the runner, not against a hostile
scenario or a hostile process of the same user.

## Exit status

| Status | Meaning |
|---|---|
| 0 | Every step passed and the I1 scan found nothing |
| 1 | A step failed or timed out, or the whole-run timeout was reached |
| 2 | Refused before anything was read or written: arguments, isolation, the scenario file, the core's guards (a build with `secret-service` included) |
| 3 | The I1 scan found a password of `dev/engines.json` or the run's master password in a file of the run directory |
| 4 | The setup failed after the start: the run's files, the core, the vault, a connection that could not be saved, the window |
| 5 | A dev engine the scenario needs did not answer; nothing ran (skipped) |

## Output

- Standard output: one line `ninesql-e2e checkpoint <n> <name>` per checkpoint, once the step's
  state has been presented in a frame; the UI is then held still for `checkpoint_hold_ms`
  (default 1 500 ms) while the harness captures the window. The runner takes no input during a
  run and opens no port, socket, D-Bus name or pipe.
- `results.json` in `--e2e-out` (schema 1): `schema_version`, `commit`, `scenario`, `build`,
  `outcome` (`pass`, `fail`, `refused`, `secret-found`, `setup-error`, `skipped`), `exit_code`,
  `message`, `window` (`width`, `height`, `scale_factor`), `checkpoints` (reached),
  `planned_checkpoints`, `steps` (per step `index`, `name`, `kind`, `outcome` = `pass` / `fail` /
  `timeout` / `skipped`, `duration_ms`, `checkpoint`, `message`, `dispatched` = `focus` /
  `palette` for actions) and `i1_scan` (`outcome`, `files_scanned`, `files_with_a_secret` as
  paths). It holds no typed text, statement, parameter value, cell value or password; failure
  messages are built from the probe, and text values only for probe keys that read labels.
- The window title ends with `[E2E TEST BUILD]`.

## Scenario format

TOML, `schema_version = 1`. See the module documentation of
`crates/ninesql-ui/src/e2e/scenario.rs` for every key. In short:

- `name` (`[a-z0-9-]`, the file's name), `description`, `engines` (the path of
  `dev/engines.json`, relative to the file, inside the repository), `timeout_secs` (whole run,
  default 900), `checkpoint_hold_ms` (default 1 500).
- `[[connection]]`: saved before the window opens, with `name`, `engine` (`postgresql`, `mysql`,
  `mariadb` from `dev/engines.json`, or `sqlite` with a `file` name created in the run's HOME
  from `dev/seed/sqlite/nine.sql`), `type` (`dev`, `test`, `prod`) and `read_only`. Dev engines
  connect in plaintext over loopback (their TLS uses a throwaway CA).
- `[[dbeaver]] fixture = "dbeaver-25.3.5"`: a copy of a synthetic fixture of
  `crates/ninesql-dbeaver/tests/fixtures/` at the run's `~/.local/share/DBeaverData/workspace6`.
- `[[file]]`: a small text file (`name`, `content`) in the run's HOME.
- `[[step]]`, exactly one of:
  - `action = "ninesql::FocusNavigator"`: a registered action by its full name, dispatched to
    the focused element as its key binding would be (or the palette's way when the focused
    element does not handle it and no dialog is open);
  - `keys = "home right"`: keystrokes (GPUI syntax), key down and key up through GPUI's own
    dispatch, never OS input injection;
  - `text = "SELECT 1"`: typed into the focused element; `{home}` expands to the run's HOME;
  - `wait = "<probe key>"` with `arg` when the key takes one, matchers (`equals`, `at_least`,
    `at_most`, `contains`) and `timeout_secs` (default 10, at most 120);
  - `expect = "<probe key>"`: the same, read once, recorded pass or fail (the run goes on);
  - `checkpoint = "<name>"`.

Probe keys (`crates/ninesql-ui/src/e2e/probe.rs`): `core.ready`, `core.problems`,
`connections.count`, `connection.exists`, `session.open`, `session.read_only`,
`session.production`, `navigator.selected`, `navigator.rows`, `navigator.has_row`,
`navigator.loading`, `dialog.open`, `focus.any`, `focus.input`, `tabs.count`,
`tabs.active_title`, `editor.connection`, `editor.running`, `editor.awaiting_confirmation`,
`editor.message` (compared, never recorded), `editor.error`, `editor.cancel_label`,
`grid.result_tabs`, `grid.rows`, `grid.state`, `grid.focused_row`, `grid.focused_column`,
`grid.update_counts`, `table.open`, `table.editable`, `table.pending_changes`,
`table.save_confirmation`, `object_editor.open`, `object_editor.tab`,
`object_editor.ddl_loaded`, `import.stage`, `import.preview_connections`,
`import.first_run_offer`, `export.stage`, `csv_import.stage`.

## Scenarios

| File | Covers | Checkpoints |
|---|---|---|
| `sqlite-editor-grid` | Phases 1 and 3 on SQLite: the navigator, Connect, a `SELECT` in the editor, the grid | start, navigator-connection, connected, select-results |
| `sqlite-navigator-object` | Phase 4 on SQLite: the navigator from the keyboard, Find object, the object editor, Show DDL | navigator-tables, find-object, object-editor, show-ddl |
| `sqlite-table-edit` | Phase 5 on SQLite: Open table data, the cell editor, a row marked deleted, Preview changes, revert, Save changes | table-data, cell-editor, pending-edit, preview-changes, saved |
| `sqlite-transfer` | Phase 5 on SQLite: Export table to CSV, Import CSV, through their dialogs | export-dialog, export-done, import-preview, import-done |
| `sqlite-safety` | I3 on SQLite: the production `DELETE` confirmation declined, Choose editor connection, a write refused on a read-only session | production-frame, run-on-production, production-declined, choose-editor-connection, read-only-refused |
| `postgresql-editor-safety`, `mysql-editor-safety`, `mariadb-editor-safety` | Phases 1, 3 and I3 on each dev engine: Connect, a `SELECT`, a paged `big_events` result stopped with Cancel, the production `DELETE` declined, a read-only write refused | connections, connected, select-results, paged-result, paged-result-cancelled, production-frame, run-on-production, read-only-refused |
| `dbeaver-import` | Phase 2: the first-run offer for a synthetic DBeaver workspace and the import preview | first-run-offer, import-preview |

Every scenario runs alone. `cargo test -p ninesql-ui --features e2e-scenarios --lib e2e::`
checks every file without a display (actions registered, probe keys known) and runs all but
`dbeaver-import` headless in a `TestAppContext` against the dev engines (skipped when they are
unreachable, unless `NINESQL_REQUIRE_ENGINES` lists them). With the feature on, run only the
`e2e::` tests: the core's guards are process-wide, so the other UI tests of the crate refuse to
connect.

Not covered yet: the save and import confirmations on production, inserting rows through the
"Insert row" form, script files and session restore (a restart), dialogs at 200 % scale and
native Wayland screenshots (X11 and XWayland only, ADR-0029 open question 4).
