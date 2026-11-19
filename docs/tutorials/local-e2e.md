# Run the local end-to-end check and send the report

`make e2e-local` runs, on your own Linux desktop, what CI runs plus what only a machine with a display can run: the checks, the unit tests, the driver tests against the dev engines and a short GUI smoke test. It writes every log into one report directory with a `summary.md` and an archive. You send the summary (or the archive) back to a cloud agent session, which has no display and no GPU ([docs/09](../09-agent-workflow.md#local-end-to-end-runs-for-cloud-sessions)), and the agent fixes what failed.

The script is [`scripts/e2e-local.sh`](../../scripts/e2e-local.sh). It calls the root [`Makefile`](../../Makefile) targets and [`dev/ready.py`](../../dev/README.md), so it runs the same commands as [the checks before a PR](checks-before-a-pr.md).

## 1. Prerequisites
- Debian or Ubuntu with a Wayland or X11 session, and a GPU with Vulkan drivers ([build and run](build-and-run.md#1-install-the-toolchain)).
- The build requirements: Rust through rustup (the repository's `rust-toolchain.toml` picks the toolchain) and the GPUI system packages (`make deps-debian` prints them).
- Docker Engine with the Compose v2 plugin, usable without `sudo` ([dev databases](dev-databases.md#1-requirements)).
- Python **3.11 or newer**: `scripts/verify-patches.sh` reads `Cargo.toml` with `tomllib`. Debian 12 ships 3.11 and Ubuntu 22.04 ships 3.10. On 3.10 the `verify-patches` step fails, and the `env` stage says so.
- `cargo-deny` (`cargo install cargo-deny --locked`; CI uses 0.20.2).
- Recommended:
  - `dbus-daemon` (`sudo apt install dbus-daemon`), so the lock-watcher tests run instead of skipping;
  - [gitleaks](https://github.com/gitleaks/gitleaks), so the report is scanned for secrets before it is archived;
  - `mesa-utils` (`glxinfo`) and `vulkan-tools` (`vulkaninfo`), so the report names your GPU;
  - a screenshot tool: `grim` (wlroots Wayland), `spectacle` (KDE), `gnome-screenshot`, or ImageMagick `import` / `scrot` (X11).
- Free disk space: the script warns below 15 GiB. The debug build, the release build and the engine images all take room.
- Nothing listening on the dev engines' host ports (15432, 13306, 13307, 15433, 15434, 18812, 19000, 14000, 14080). Stop your own dev containers first (`make dev-down`), or reuse them with `--engines-running` ([section 3](#3-options)).

## 2. First run
From the repository root, on the commit you were asked to test:
```bash
git switch dev && git pull      # or the branch the agent named
make e2e-local                  # = scripts/e2e-local.sh
```
The terminal shows one line per stage and step, and a `PASS`, `FAIL` or `SKIP` line when each stage ends. The command output goes to the log files, not to the terminal; add `--verbose` to see it too.

| Stage | What it runs |
|---|---|
| `env` | Prerequisites and versions: OS, kernel, CPU, memory, free disk, rustc / cargo, cargo-deny, Python (with the 3.11 check), Docker and Compose, dbus-daemon, gitleaks, the session type (Wayland / X11), the desktop, the display scale if it can be read (GNOME scaling factors, `Xft.dpi`, `GDK_SCALE`…), the GPU from `glxinfo -B` and `vulkaninfo --summary`. No host name, user name or IP address. It always runs unless you `--skip env`. |
| `static` | `make fmt-check`, `clippy`, `deny`, `verify-patches`, `forbidden-apis`, `md-links`, `dev-check`, each with its own log. A failing step does not stop the others. |
| `unit` | `make test` (`cargo test --workspace --locked`) with `--no-fail-fast`, so every test binary runs, and with `NINESQL_REQUIRE_DBUS=1` when `dbus-daemon` is installed. Failing tests and their output are copied into the summary. |
| `engines` | The `driver-integration` CI job. It pulls the images, starts the engines with `dev/ready.py up` (PostgreSQL, MySQL and MariaDB from the `core` profile; TimescaleDB, YugabyteDB, QuestDB and TiDB from `sql`), runs `make test-engines-run` with `REQUIRE_ENGINES=postgresql,mysql,mariadb,ssh,timescaledb,yugabytedb,questdb,tidb`, and saves `docker compose ps` and the last 200 log lines of each container. It then stops the containers with `docker compose down`. It never deletes volumes unless you pass `--clean-volumes`, and it never prunes images. It uses its own compose project, `ninesql-e2e-local`, and first checks that the fixed host ports are free. |
| `gui-smoke` | Runs `make release`, then `ninesql --version`, `--help` and an unknown argument (it must exit with status 2). It then starts `ninesql` with empty temporary `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME` and `XDG_CACHE_HOME`, so your own workspace is never touched, and with `RUST_LOG=ninesql=debug,info` (never `trace`, which may carry query text). It checks that the window is still running after 15 s and takes a screenshot if a tool works. It then sends `SIGTERM` and expects the process to exit within 10 s, and looks for a panic on stderr. Last, a modest I1 check looks for secret-like text in the files the run wrote. The run makes no connection, so none should be there. Without a display the stage is skipped. |
| `deb` | Opt-in (`--with-deb`): `make deb`, then `make deb-install-test` ([packaging](../../packaging/README.md#the-deb)). It needs cargo-deb 3.8.0 and cargo-about 0.9.3. |

A window opens during `gui-smoke`. Leave it alone: the script closes it.

The binary installs no log subscriber yet, so `RUST_LOG` has no effect today. The stage still saves stderr and stdout, and `ninesql.log` if one is written.

## 3. Options
Pass them to the script, or through `E2E_ARGS` with make:
```bash
make e2e-local E2E_ARGS="--only engines"
scripts/e2e-local.sh --only static,unit --fail-fast
```

| Option | Effect |
|---|---|
| `--only LIST`, `--skip LIST` | Choose stages (comma-separated): `env`, `static`, `unit`, `engines`, `gui-smoke`, `deb`. `env` is added to `--only` unless skipped. |
| `--with-deb` | Add the `deb` stage. |
| `--fail-fast` | Stop after the first failing stage; the rest are listed as `NOT RUN`. |
| `--dry-run` | Print each stage's commands without running them (`env` still runs). |
| `--verbose` | Show the commands' output on the terminal too. |
| `--engines LIST` | The engines that must not be skipped, e.g. `--engines postgresql,mysql,mariadb,ssh`. Only those are started. |
| `--engines-running` | Use engines that are already up (for example after `make dev-up`): start and stop nothing. |
| `--keep-engines` | Leave the engines running after the stage. |
| `--clean-volumes` | Stop with `docker compose down -v`, which also deletes the engines' data volumes. Without it, each run leaves the previous containers' anonymous volumes behind (`docker volume prune` removes them). |
| `--log-lines N` | Container log lines kept per container (default 200). |
| `--timeout SECS` | Time limit of each long cargo command (default 2700). A command that hits it shows exit status 124. |
| `--dev-timeout SECS` | Time limit of the image pulls and of engine readiness (default 900). |
| `--gui-wait SECS` | How long the window stays open (default 15). |
| `--no-screenshot` | Take no screenshot. |
| `--keep-temp` | Keep the temporary XDG directories of `gui-smoke`, for a closer look. |

Exit status: 0 when no stage failed, 1 otherwise, 2 for a wrong option.

## 4. Read the report
The report directory is `target/e2e-reports/<UTC time>-<short commit>/`:

| File | Content |
|---|---|
| `summary.md` | The commit, the branch, whether the working tree is dirty, a table of the stages (result, exit status, duration, log), each failure with its first error lines and last 40 lines, the details of each stage (the `static` steps, the failing tests, the GUI results) and the environment |
| `NN-<stage>.log` | The full log of each stage, with every command it ran on a `+` line |
| `static-<step>.log` | One log per `static` step |
| `engines-compose-ps.txt`, `engines-containers.log` | Container state and the last lines of each container's log |
| `gui-smoke-stderr.log`, `gui-smoke-stdout.log`, `gui-smoke-screenshot.png` | What the window printed, and the screenshot |
| `gitleaks.log` | The secret scan's output (findings redacted) |
| `report.tar.gz` | The whole directory |

`SKIP` means the stage could not run here (for example `gui-smoke` without a display); the reason is in the summary. `FAIL` with exit status 124 is a time limit, not a test failure.

## 5. What to send back
Send `summary.md` back to the session. That is usually enough. If the agent asks for more, or a failure needs the full logs, send `report.tar.gz`. The script's last line gives both paths.

## 6. Privacy
- The `env` stage does not record your host name, user name or IP addresses. Before the summary and the archive are written, every text file in the report is scrubbed:
  - the repository path becomes `<repo>` and `$HOME` becomes `~`;
  - your user name becomes `<user>` and your host name becomes `<host>`, as whole words, except generic names such as `root`, `user` or `ubuntu`;
  - private IPv4 addresses (10.x, 172.16-31.x, 192.168.x, 169.254.x) become `<ip>`.
- When gitleaks is installed, it scans the scrubbed report. If it finds something, the script writes **no archive** and fails, pointing to `gitleaks.log`. Without gitleaks, the script warns that the report was not scanned. Read it before you send it.
- **The screenshot shows your whole screen**, not only the NineSQL window. Look at it before you send the archive, or run with `--no-screenshot`. The script reminds you at the end when the archive holds one.
- The container logs contain the dev engines' fake, public credentials ([dev/README.md](../../dev/README.md#engines-ports-and-dev-credentials)), never yours. The GUI run uses empty temporary directories and makes no connection, so no saved password of yours can be in the report.
- Nothing is uploaded: the script writes files, and you choose what to send.

## 7. How long it takes
No full run has been timed yet. What takes the time:
- `clippy`, the tests and the release build each compile the workspace in a different profile, and the first GPUI build is long ([build and run](build-and-run.md#2-get-the-code-and-build)). A second run on the same commit reuses the build cache.
- The engine stage pulls the images on the first run. In CI, the wire-compatible engines were seeded within about 45 s (YugabyteDB, the slowest) once pulled.
- `gui-smoke` adds the release build, then about 15 s for the window and up to 10 s for it to exit.

Each cargo command stops after `--timeout` seconds (default 2700), and pulls and readiness after `--dev-timeout` (default 900). For a quick look at one area, use `--only`, for example `--only engines`.
