# Run the local end-to-end check and send the report

`make e2e-local` runs, on your own Linux desktop, what CI runs plus what only a machine with a display can run: the checks, the unit tests, the driver tests against the dev engines, a short GUI smoke test and the end-to-end UI scenarios of [`e2e/scenarios/`](../../e2e/README.md), which drive a real NineSQL window and take a screenshot of it at each checkpoint. It writes every log into one report directory with a `summary.md` and an archive. You send the summary (or the archive) back to a cloud agent session, which has no display and no GPU ([docs/09](../09-agent-workflow.md#local-end-to-end-runs-for-cloud-sessions)), and the agent fixes what failed.

The script is [`scripts/e2e-local.sh`](../../scripts/e2e-local.sh). It calls the root [`Makefile`](../../Makefile) targets and [`dev/ready.py`](../../dev/README.md), so it runs the same commands as [the checks before a PR](checks-before-a-pr.md).

## 1. Prerequisites
- Debian or Ubuntu with a Wayland or X11 session, and a GPU with Vulkan drivers ([build and run](build-and-run.md#1-install-the-toolchain)).
- The build requirements: Rust through rustup (the repository's `rust-toolchain.toml` picks the toolchain) and the GPUI system packages (`make deps-debian` prints them).
- Docker Engine with the Compose v2 plugin, usable without `sudo` ([dev databases](dev-databases.md#1-requirements)).
- Python **3.11 or newer**: `scripts/verify-patches.sh` reads `Cargo.toml` with `tomllib`. Debian 12 ships 3.11 and Ubuntu 22.04 ships 3.10. On 3.10 the `verify-patches` step fails, and the `env` stage says so.
- `cargo-deny` (`cargo install cargo-deny --locked`; CI uses 0.20.2).
- [gitleaks](https://github.com/gitleaks/gitleaks) for the archive: the script writes `report.tar.gz` only when gitleaks scanned the report clean. Without gitleaks you still get `summary.md` and the logs, and no archive unless you pass `--no-secret-scan` ([section 6](#6-privacy)).
- Recommended:
  - `dbus-daemon` (`sudo apt install dbus-daemon`), so the lock-watcher tests run instead of skipping;
  - `mesa-utils` (`glxinfo`) and `vulkan-tools` (`vulkaninfo`), so the report names your GPU;
  - for screenshots: `xdotool` or `wmctrl` and ImageMagick (`import`), which capture the window of the process the script started, alone, on X11 or XWayland. The `scenarios` stage uses them at every checkpoint by default; `gui-smoke` only with `--screenshot`. There is no whole-screen fallback: without them, or on a Wayland-native window, no screenshot is taken.
- An X server for the `scenarios` stage: an X11 session, or XWayland on a Wayland session (`DISPLAY` must be set). The script unsets `WAYLAND_DISPLAY` for each scenario run, so NineSQL uses its X11 backend and its window can be captured alone ([ADR-0029](../adr/0029-automated-ui-end-to-end-tests-and-application-logging.md) §2.7). Without `DISPLAY` the stage is skipped.
- One command for the optional tools on Ubuntu 24.04 or Debian 13 (on older releases, where `gitleaks` has no package, take the binary from the [gitleaks releases](https://github.com/gitleaks/gitleaks/releases)):
  ```bash
  sudo apt install gitleaks dbus-daemon mesa-utils vulkan-tools xdotool imagemagick
  ```
- Free disk space: the script warns below 15 GiB. The debug build, the release build, the test build of the `scenarios` stage (`target/e2e-build/`, a second release build) and the engine images all take room.
- Nothing listening on the dev engines' host ports (15432, 13306, 13307, 15433, 15434, 18812, 19000, 14000, 14080). Stop your own dev containers first (`make dev-down`), or reuse them with `--engines-running` ([section 3](#3-options)).

## 2. First run
From the repository root, on the commit you were asked to test:
```bash
git switch dev && git pull      # or the branch the agent named
make e2e-local                  # = scripts/e2e-local.sh
```
The `scenarios` stage takes its checkpoint screenshots by default. To also capture the `gui-smoke` window (X11 or XWayland, see the prerequisites), which is the run asked for the end-of-MVP visual review (P6-F):
```bash
make e2e-local E2E_ARGS="--screenshot"
```
At the end the script prints the paths of `summary.md` and `report.tar.gz`; send `summary.md` back to the session ([section 5](#5-what-to-send-back)).
The terminal shows one line per stage and step, and a `PASS`, `FAIL` or `SKIP` line when each stage ends. The command output goes to the log files, not to the terminal; add `--verbose` to see it too.

| Stage | What it runs |
|---|---|
| `env` | Prerequisites and versions: OS, kernel, CPU, memory, free disk, rustc / cargo, cargo-deny, Python (with the 3.11 check), Docker and Compose, dbus-daemon, gitleaks, the session type (Wayland / X11), the desktop, the display scale if it can be read (GNOME scaling factors, `Xft.dpi`, `GDK_SCALE`…), the GPU from `glxinfo -B` and `vulkaninfo --summary` (only an allowlist of lines such as the renderer, device and driver names and versions; no device or driver UUID). No host name, user name or IP address. It always runs unless you `--skip env`. |
| `static` | `make fmt-check`, `clippy`, `deny`, `verify-patches`, `forbidden-apis`, `md-links`, `dev-check`, each with its own log. A failing step does not stop the others. |
| `unit` | `make test` (`cargo test --workspace --locked`) with `--no-fail-fast`, so every test binary runs, and with `NINESQL_REQUIRE_DBUS=1` when `dbus-daemon` is installed. Failing tests and their output are copied into the summary. |
| `engines` | The `driver-integration` CI job. It pulls the images, starts the engines with `dev/ready.py up` (PostgreSQL, MySQL and MariaDB from the `core` profile; TimescaleDB, YugabyteDB, QuestDB and TiDB from `sql`), runs `make test-engines-run` with `REQUIRE_ENGINES=postgresql,mysql,mariadb,ssh,timescaledb,yugabytedb,questdb,tidb`, and saves `docker compose ps` and the last 200 log lines of each container. It then stops the containers with `docker compose down`. It never deletes volumes unless you pass `--clean-volumes`, and it never prunes images. It uses its own compose project, `ninesql-e2e-local`, and first checks that the fixed host ports are free. |
| `gui-smoke` | Runs `make release`, then `ninesql --version`, `--help` and an unknown argument (it must exit with status 2). It then starts `ninesql` with an empty temporary `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME` and `XDG_CACHE_HOME` (mode `0700`), so your own workspace is never touched and the first-run DBeaver detection never sees your DBeaver connections, and with `NINESQL_LOG=ninesql=debug,info` (never `trace`, which may carry query text; `RUST_LOG` is ignored), which also turns the file log on. It checks that the window is still running after 15 s, and takes a screenshot only with `--screenshot`. It then sends `SIGTERM` and expects the process to exit within 10 s, and looks for a panic on stderr. Last, a modest I1 check looks for secret-like text in the files the run wrote. The run makes no connection, so none should be there. Without a display the stage is skipped. |
| `scenarios` | The end-to-end UI scenarios ([ADR-0029](../adr/0029-automated-ui-end-to-end-tests-and-application-logging.md) §2, [section 4.1](#41-scenario-results)). It builds the test binary with `cargo build --release --locked -p ninesql-ui --bin ninesql --features e2e-scenarios` into its own target directory, `target/e2e-build/`, so `target/release/ninesql` is never replaced, and checks that its `--version` says `(e2e-scenarios test build, not for distribution)`. It starts the dev engines the scenarios name (PostgreSQL, MySQL and MariaDB, in the same compose project as `engines`; reused when they are still up from it, untouched with `--engines-running`) and stops them at the end unless `--keep-engines`. Then, for each scenario of `e2e/scenarios/` (or `--scenario`), it creates a fresh `0700` run directory `target/e2e-runs/<UTC time>/<scenario>/` with `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME` and `XDG_CACHE_HOME` inside it, runs `ninesql --e2e-run e2e/scenarios/<name>.toml --e2e-out <run directory>/out` with `WAYLAND_DISPLAY` unset and `NINESQL_LOG=ninesql=debug,info`, and reads its standard output: at each `ninesql-e2e checkpoint <n> <name>` line it raises the window of that process and captures it alone, during the 1 500 ms the runner holds the UI still. Each run has a time limit (`--scenario-timeout`, default 1200 s). It collects `results.json`, stdout, stderr, the file log and the screenshots, then deletes the run directory (kept with `--keep-temp`). Without an X display the stage is skipped. |
| `deb` | Opt-in (`--with-deb`): `make deb`, then `make deb-install-test` ([packaging](../../packaging/README.md#the-deb)). It needs cargo-deb 3.8.0 and cargo-about 0.9.3. |

A window opens during `gui-smoke`, and one per scenario during `scenarios`. Leave them alone, and leave the desktop unlocked: the scenarios type into their window through NineSQL's own key dispatch (never OS input injection), but a click or a key press of yours in that window, or a screen lock (which locks the run's vault), can make a step fail. The script raises each test window before a screenshot.

The stage saves stderr and stdout, and the file log (`$XDG_STATE_HOME/ninesql/logs/ninesql.log`) as `gui-smoke-ninesql.log`.

## 3. Options
Pass them to the script, or through `E2E_ARGS` with make:
```bash
make e2e-local E2E_ARGS="--only engines"
scripts/e2e-local.sh --only static,unit --fail-fast
```

| Option | Effect |
|---|---|
| `--only LIST`, `--skip LIST` | Choose stages (comma-separated): `env`, `static`, `unit`, `engines`, `gui-smoke`, `scenarios`, `deb`. For the scenarios alone: `--only scenarios`. `env` is added to `--only` unless skipped. |
| `--with-deb` | Add the `deb` stage. |
| `--fail-fast` | Stop after the first failing stage; the rest are listed as `NOT RUN`. |
| `--dry-run` | Print each stage's commands without running them (`env` still runs). |
| `--verbose` | Show the commands' output on the terminal too. |
| `--engines LIST` | The engines that must not be skipped, e.g. `--engines postgresql,mysql,mariadb,ssh`. Only those are started. |
| `--engines-running` | Use engines that are already up (for example after `make dev-up`): start and stop nothing, in `engines` and in `scenarios`. The container state and logs come from the compose project that publishes the dev ports (`ninesql-dev` after `make dev-up`); when none or several do, they are not collected and the summary says so. |
| `--compose-project NAME` | The compose project of the `engines` stage (default `ninesql-e2e-local`; also `E2E_COMPOSE_PROJECT`). With `--engines-running`, it names the running project instead of detecting it. |
| `--keep-engines` | Leave the engines running after the stage; `scenarios` then reuses them. |
| `--clean-volumes` | Stop with `docker compose down -v`, which also deletes the engines' data volumes. Without it, each run leaves the previous containers' anonymous volumes behind (`docker volume prune` removes them). |
| `--log-lines N` | Container log lines kept per container (default 200). |
| `--timeout SECS` | Time limit of each long cargo command (default 2700). A command that hits it shows exit status 124. |
| `--dev-timeout SECS` | Time limit of the image pulls and of engine readiness (default 900). |
| `--gui-wait SECS` | How long the window stays open (default 15). |
| `--screenshot` | Take a screenshot in `gui-smoke` (off by default). On X11, or XWayland, the script looks for the visible windows of the process it started, by its pid (`xdotool search --pid`, or the pid column of `wmctrl -lp`), never by window class, so a NineSQL window of your own is never picked. When exactly one window matches, it raises it (`xdotool windowactivate --sync`) and captures that window only with ImageMagick `import -window`. When none or several match (a Wayland-native window, a window that does not set `_NET_WM_PID`, the tools missing), no screenshot is taken and the summary says why; the script never captures the whole screen instead. |
| `--no-screenshot` | No screenshot at all: none in `gui-smoke`, and none at the checkpoints of `scenarios`, which still run. |
| `--scenario LIST` | The scenarios to run (comma-separated names of `e2e/scenarios/<name>.toml`), e.g. `--only scenarios --scenario sqlite-editor-grid,sqlite-safety`. Default: all of them. An unknown name is refused (exit status 2). |
| `--scenario-timeout SECS` | Time limit of each scenario run (default 1200). A run that hits it is stopped (`SIGTERM`, then `SIGKILL` after 10 s) and marked `FAIL (time limit …)`. The runner has its own whole-run limit, 15 minutes unless the scenario sets `timeout_secs`. |
| `--no-secret-scan` | Write `report.tar.gz` without the gitleaks scan, for example when gitleaks is not installed. Read the report before you send it. |
| `--keep-temp` | Keep the temporary `HOME` and XDG directories of `gui-smoke` and the run directories of `scenarios` (`target/e2e-runs/`), for a closer look. A kept run directory holds the run's encrypted vault and its workspace files. |
| `--check-report DIR` | Run no stage: scrub and secret-check an existing report directory, then archive it if the checks pass. The directory must be yours and hold a `summary.md` whose first line is `# NineSQL local end-to-end report`; any other directory is refused (exit status 2), as is one where `summary.md`, `credential-uris.txt`, `gitleaks.log` or `report.tar.gz` is a symbolic link or not a regular file. Only regular files are read and rewritten: symbolic links and other special files are skipped, and a rewrite replaces the file rather than writing through a hard link. `scripts/e2e-local-report-test.sh` uses this option to test the scrubber and the secret checks on fake reports. |

Exit status: 0 when no stage failed, 1 otherwise, 2 for a wrong option.

## 4. Read the report
The report directory is `target/e2e-reports/<UTC time>-<short commit>/`:

| File | Content |
|---|---|
| `summary.md` | The commit, the branch, whether the working tree is dirty, a table of the stages (result, exit status, duration, log), each failure with its first error lines and last 40 lines, the details of each stage (the `static` steps, the failing tests, the GUI results) and the environment |
| `NN-<stage>.log` | The full log of each stage, with every command it ran on a `+` line |
| `static-<step>.log` | One log per `static` step |
| `engines-compose-ps.txt`, `engines-containers.log` | Container state and the last lines of each container's log |
| `gui-smoke-stderr.log`, `gui-smoke-stdout.log` | What the window printed |
| `gui-smoke-window.png` | Only with `--screenshot`, and only when the window of the launched process could be captured alone |
| `scenarios/<name>/results.json` | The runner's results ([section 4.1](#41-scenario-results)) |
| `scenarios/<name>/checkpoint-<n>-<checkpoint>.png` | The test window at each checkpoint, when it could be captured alone |
| `scenarios/<name>/stdout.log`, `stderr.log`, `ninesql.log` | The checkpoint lines, what the window printed, and its file log |
| `gitleaks.log` | The secret scan's output (findings redacted) |
| `credential-uris.txt` | Only when the check found one: the `file:line` of each URI with a user and a password, without the values |
| `report.tar.gz` | The whole directory, written only when the secret checks passed ([section 6](#6-privacy)) |

`SKIP` means the stage could not run here (for example `gui-smoke` without a display); the reason is in the summary. `FAIL` with exit status 124 is a time limit, not a test failure.

### 4.1 Scenario results
The `scenarios` section of `summary.md` has one row per scenario: its result, the runner's exit status, the steps passed and failed, the checkpoints reached out of those planned, the screenshots taken (or why none) and the duration. Under the table, each step that failed or timed out is listed with the probe's message, and a run that wrote no `results.json` with the start of its stderr.

| Exit | Result | Meaning |
|---|---|---|
| 0 | `PASS` | Every step passed and the runner's I1 scan found nothing |
| 1 | `FAIL` | A step failed or timed out, or the run hit its whole-run limit |
| 2 | `FAIL (refused)` | The runner refused to start: the run directory, the scenario file or a core guard (a build with `secret-service` included) |
| 3 | `FAIL (SECRET FOUND)` | The runner found a password of `dev/engines.json` or the run's master password in a file it wrote. The script prints a warning, does not copy that run's file log, and writes **no archive** whatever the other checks say. `results.json` names the files (`i1_scan`), never the values. Report it: it is a bug |
| 4 | `FAIL (setup error)` | The setup failed after the start (the run's files, the core, the vault, a connection, the window) |
| 5 | `SKIP` | A dev engine the scenario needs did not answer; nothing ran |

A scenario that exits 0 but printed a panic on stderr is marked `FAIL (panic on stderr)`. The stage fails when any scenario fails, and is skipped when every scenario was skipped.

In `results.json`, `steps` gives each step's kind, outcome, duration and message; for an `action` step, `dispatched` says whether the action went to the focused element (`focus`) or took the command palette's path (`palette`). The file holds no typed text, statement, parameter value, cell value or password ([e2e/README.md](../../e2e/README.md#output)).

A screenshot is taken while the runner holds the UI still (1 500 ms). When the capture is slow it can show the state just after the checkpoint. Look at the screenshots in the order of their numbers; the checkpoint names match the scenario file.

## 5. What to send back
Send `summary.md` back to the session. That is usually enough. If the agent asks for more, a failure needs the full logs, or the session is to review the scenario screenshots (P6-F), send `report.tar.gz`: it holds the screenshots. The script's last lines give both paths.

When no archive was written, the last line reads "Do not send anything before fixing the lines listed above." instead of "Next: send summary.md…": the lines above it, and the "Secret checks" section at the end of `summary.md`, say why (a credential URI, a gitleaks finding, gitleaks missing). Fix them, then run `scripts/e2e-local.sh --check-report <report directory>` to check and archive the report again.

## 6. Privacy
- Every file the script writes is readable by you only (`umask 077`: directories `0700`, files `0600`).
- The `env` stage does not record your host name, user name or IP addresses. Before the summary and the archive are written, every text file in the report is scrubbed:
  - the repository path becomes `<repo>` and `$HOME` becomes `~`;
  - your user name becomes `<user>` and your host name becomes `<host>`, as whole words, except generic names such as `root`, `user` or `ubuntu`;
  - IPv4 addresses become `<ip>`, private and public alike; `127.x` and `0.0.0.0` stay readable. The only four-part numbers kept are the ones `vulkaninfo` prints right after `conformanceVersion = `, `apiVersion = ` or `driverVersion = `, and dotted numbers that cannot be addresses (a part above 255, such as `1.2.300.4`). Any other version number shaped like an address (`version: 10.4.5.6`) becomes `<ip>`: the scrubber errs toward hiding;
  - global, unique local (`fc00::/7`) and link-local (`fe80::/10`) IPv6 addresses become `<ip>`, with their zone (`%eth0`); `::1` stays readable;
  - MAC addresses become `<mac>`.
- The summary gives the number of uncommitted paths in your working tree, not their names.
- Then the secret checks, in this order:
  1. every password listed in [`dev/engines.json`](../../dev/README.md#engines-ports-and-dev-credentials) (the dev engines' fake, public credentials) is replaced with `<dev-password>`, and the summary says in how many files;
  2. any other URI with a user and a password (`scheme://user:password@…`) fails the run: no archive, and `credential-uris.txt` lists where, without the values. The scheme is matched in any case (`HTTP://`), the user may be empty (`redis://:password@host`), and the password runs to the next `@` on the same line, spaces included (it never spans another `://`). A line with a URL with a port and, later, an `@` (an e-mail address) can be flagged too; check the line it names;
  3. gitleaks scans the report. The archive is written only when it finds nothing. A finding, or a gitleaks error, fails the run with no archive (see `gitleaks.log`). When gitleaks is not installed, no archive is written either, unless you pass `--no-secret-scan`.
- The "Secret checks" section at the end of `summary.md` records what happened.
- **Screenshots.** `gui-smoke` takes one only with `--screenshot`. The `scenarios` stage takes one at each checkpoint by default (`--no-screenshot` turns them off), because its windows are test windows that show only dev data: the dev engines' "nine" dataset, a SQLite file created for the run and synthetic DBeaver fixtures. In both cases the script captures only the window of the process it started, found by its pid (X11 or XWayland with `xdotool` or `wmctrl` and ImageMagick). When it cannot find exactly one such window it takes no screenshot and records why; it never captures the whole screen.
- **Screenshots are part of the archive and are not scrubbed.** The scrubber and the secret checks read text files only. `import -window` copies the window's area of the screen, so a notification or popup over the window at that moment can show in it: look at the images before you send the archive. The script reminds you at the end.
- The GUI runs use empty temporary `HOME` and XDG directories, so no saved password or DBeaver connection of yours can appear in their windows, logs or screenshots. `gui-smoke` makes no connection. Each scenario run connects only to loopback addresses (the dev engines) and keeps its secrets in a vault inside its run directory, under a random master password that is never written anywhere; the runner refuses to start otherwise ([docs/05](../05-security.md#test-build-e2e-scenarios-adr-0029-2)).
- Nothing is uploaded: the script writes files, and you choose what to send.

## 7. How long it takes
No full run has been timed yet. What takes the time:
- `clippy`, the tests and the release build each compile the workspace in a different profile, and the first GPUI build is long ([build and run](build-and-run.md#2-get-the-code-and-build)). A second run on the same commit reuses the build cache.
- The engine stage pulls the images on the first run. In CI, the wire-compatible engines were seeded within about 45 s (YugabyteDB, the slowest) once pulled.
- `gui-smoke` adds the release build, then about 15 s for the window and up to 10 s for it to exit.
- `scenarios` adds a second release build (the test binary, in its own target directory, so it shares no build output with `make release`), the start of the core dev engines if they are not up, and the scenarios themselves, one window at a time. Their run time on a real display has not been measured yet; each run stops at `--scenario-timeout` (default 1200 s). For a quick look, run one: `--only scenarios --scenario sqlite-editor-grid` (it needs no container).

Each cargo command stops after `--timeout` seconds (default 2700), and pulls and readiness after `--dev-timeout` (default 900). For a quick look at one area, use `--only`, for example `--only engines`.
