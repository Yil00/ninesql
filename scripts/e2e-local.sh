#!/usr/bin/env bash
# Local end-to-end run of NineSQL with a shareable report (docs/tutorials/local-e2e.md).
#
# For the maintainer's Linux desktop (Debian / Ubuntu, a display and a GPU, Docker): runs what
# CI runs plus what only a desktop can run (the GUI smoke test), and writes everything to
# target/e2e-reports/<UTC time>-<short sha>/ with a summary.md and a report.tar.gz to send back
# to a cloud agent session.
#
# It reuses the root Makefile and dev/ready.py; it adds no check of its own besides the GUI
# smoke test and the report. Every stage has its own log, exit status and duration; a failing
# stage does not stop the next ones unless --fail-fast is given.
#
# Stages (default: env static unit engines gui-smoke scenarios; deb is opt-in with --with-deb):
#   env        prerequisites and versions (no hostname, user name or IP address)
#   static     make fmt-check clippy deny verify-patches forbidden-apis md-links dev-check
#   unit       make test (cargo test --workspace), NINESQL_REQUIRE_DBUS=1 when dbus-daemon exists
#   engines    the driver-integration CI job: dev engines up, make test-engines-run, logs, down
#   gui-smoke  release binary, --version / --help, a window kept open ~15 s, SIGTERM, I1 check
#              (temporary HOME and XDG directories; screenshot only with --screenshot)
#   scenarios  the e2e-scenarios test build (its own target directory), then each scenario of
#              e2e/scenarios/ in a real window with a fresh 0700 run directory, a window-only
#              screenshot at each checkpoint (X11 / XWayland), results.json and logs (ADR-0029 §2)
#   deb        make deb, then make deb-install-test
#
# Usage: scripts/e2e-local.sh [options]     (or: make e2e-local E2E_ARGS="...")
# Run scripts/e2e-local.sh --help for the options.
set -euo pipefail
# Everything the run writes (report, logs, temporary directories, archive) is for this user only.
umask 077

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ALL_STAGES=(env static unit engines gui-smoke scenarios deb)
DEFAULT_STAGES=(env static unit engines gui-smoke scenarios)
# Same list as the driver-integration CI job and `make test-engines`.
DEFAULT_ENGINES="postgresql,mysql,mariadb,ssh,timescaledb,yugabytedb,questdb,tidb"
STATIC_STEPS=(fmt-check clippy deny verify-patches forbidden-apis md-links dev-check)

ONLY=""
SKIP=""
WITH_DEB=0
FAIL_FAST=0
DRY_RUN=0
VERBOSE=0
ENGINES="$DEFAULT_ENGINES"
ENGINES_RUNNING=0
KEEP_ENGINES=0
CLEAN_VOLUMES=0
LOG_LINES=200
CARGO_TIMEOUT=2700
DEV_TIMEOUT=900
GUI_WAIT=15
SCREENSHOT=0
# Checkpoint screenshots of the scenarios stage: on by default (isolated test windows that show
# only dev data), off with --no-screenshot.
SCENARIO_SHOTS=1
SCENARIOS=""
SCENARIO_TIMEOUT=1200
KEEP_TEMP=0
NO_SECRET_SCAN=0
CHECK_REPORT=""
COMPOSE_PROJECT="${E2E_COMPOSE_PROJECT:-ninesql-e2e-local}"
COMPOSE_PROJECT_SET=0
[ -n "${E2E_COMPOSE_PROJECT:-}" ] && COMPOSE_PROJECT_SET=1
ORIG_ARGS="$*"

usage() {
    cat <<'EOF'
Usage: scripts/e2e-local.sh [options]
       make e2e-local E2E_ARGS="[options]"

Runs the NineSQL checks on this machine and writes a report to
target/e2e-reports/<UTC time>-<short sha>/ (summary.md, one log per stage, report.tar.gz).

Stages: env static unit engines gui-smoke scenarios deb
Default: env static unit engines gui-smoke scenarios. env always runs unless --skip env.

Options:
  --only LIST          Run only these stages (comma-separated), e.g. --only unit,engines
  --skip LIST          Skip these stages
  --with-deb           Add the deb stage (make deb, then make deb-install-test)
  --fail-fast          Stop after the first failing stage
  --dry-run            Print the commands of each stage without running them
  --verbose            Also print the commands' output on the terminal
  --engines LIST       Engines that must not be skipped in the engines stage
                       (default: postgresql,mysql,mariadb,ssh,timescaledb,yugabytedb,questdb,tidb)
  --engines-running    Use engines that are already up: start and stop nothing; their state
                       and logs are read from the compose project that publishes the dev ports
  --compose-project N  Compose project of the engines stage (default ninesql-e2e-local; with
                       --engines-running, the project of the running engines, detected if unset)
  --keep-engines       Leave the engines running after the engines stage
  --clean-volumes      Delete the engines' volumes at the end (down -v); default: down only
  --log-lines N        Last N log lines kept per container (default 200)
  --timeout SECS       Time limit of each long cargo command (default 2700)
  --dev-timeout SECS   Time limit of image pulls and engine readiness (default 900)
  --gui-wait SECS      How long the window stays open in gui-smoke (default 15)
  --screenshot         Take a screenshot in gui-smoke (off by default): the window of the
                       launched process only, found by its pid on X11 / XWayland (xdotool or
                       wmctrl, and ImageMagick import); when no single window matches, none is
                       taken (never the whole screen) and the summary says why
  --no-screenshot      No screenshot at all: none in gui-smoke, and none at the checkpoints of
                       the scenarios stage (taken by default there, the test window only)
  --scenario LIST      Scenarios to run in the scenarios stage (comma-separated names of
                       e2e/scenarios/<name>.toml; default: all of them)
  --scenario-timeout SECS
                       Time limit of each scenario run (default 1200); the run is then stopped
  --no-secret-scan     Write the archive without a gitleaks scan (by default the archive is
                       written only when gitleaks scanned the report clean); a report in
                       which a scenario run found a secret (scenarios/SECRET-FOUND) is never
                       archived, with this option or with --check-report
  --keep-temp          Keep gui-smoke's temporary HOME and XDG directories, and the run
                       directories of the scenarios stage (target/e2e-runs/)
  --check-report DIR   Run no stage: scrub and secret-check an existing report directory (one
                       you own, with the summary.md this script writes), then archive it;
                       symbolic links and other non-regular files are skipped
  -h, --help           Show this help

Environment: CARGO_TARGET_DIR is respected (the scenarios stage builds its test binary in
target/e2e-build/ whatever it says); E2E_COMPOSE_PROJECT names the compose project of the
engines and scenarios stages, like --compose-project.
EOF
}

die() { echo "e2e-local: $*" >&2; exit 2; }

while [ $# -gt 0 ]; do
    case "$1" in
        --only) [ $# -ge 2 ] || die "--only needs a value"; ONLY="$2"; shift 2 ;;
        --only=*) ONLY="${1#*=}"; shift ;;
        --skip) [ $# -ge 2 ] || die "--skip needs a value"; SKIP="$2"; shift 2 ;;
        --skip=*) SKIP="${1#*=}"; shift ;;
        --with-deb) WITH_DEB=1; shift ;;
        --fail-fast) FAIL_FAST=1; shift ;;
        --dry-run) DRY_RUN=1; shift ;;
        --verbose) VERBOSE=1; shift ;;
        --engines) [ $# -ge 2 ] || die "--engines needs a value"; ENGINES="$2"; shift 2 ;;
        --engines=*) ENGINES="${1#*=}"; shift ;;
        --engines-running) ENGINES_RUNNING=1; shift ;;
        --compose-project) [ $# -ge 2 ] || die "--compose-project needs a value"; COMPOSE_PROJECT="$2"; COMPOSE_PROJECT_SET=1; shift 2 ;;
        --compose-project=*) COMPOSE_PROJECT="${1#*=}"; COMPOSE_PROJECT_SET=1; shift ;;
        --keep-engines) KEEP_ENGINES=1; shift ;;
        --clean-volumes) CLEAN_VOLUMES=1; shift ;;
        --log-lines) [ $# -ge 2 ] || die "--log-lines needs a value"; LOG_LINES="$2"; shift 2 ;;
        --timeout) [ $# -ge 2 ] || die "--timeout needs a value"; CARGO_TIMEOUT="$2"; shift 2 ;;
        --dev-timeout) [ $# -ge 2 ] || die "--dev-timeout needs a value"; DEV_TIMEOUT="$2"; shift 2 ;;
        --gui-wait) [ $# -ge 2 ] || die "--gui-wait needs a value"; GUI_WAIT="$2"; shift 2 ;;
        --screenshot) SCREENSHOT=1; shift ;;
        --no-screenshot) SCREENSHOT=0; SCENARIO_SHOTS=0; shift ;;
        --scenario) [ $# -ge 2 ] || die "--scenario needs a value"; SCENARIOS="$2"; shift 2 ;;
        --scenario=*) SCENARIOS="${1#*=}"; shift ;;
        --scenario-timeout) [ $# -ge 2 ] || die "--scenario-timeout needs a value"; SCENARIO_TIMEOUT="$2"; shift 2 ;;
        --no-secret-scan) NO_SECRET_SCAN=1; shift ;;
        --keep-temp) KEEP_TEMP=1; shift ;;
        --check-report) [ $# -ge 2 ] || die "--check-report needs a directory"; CHECK_REPORT="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option: $1 (see --help)" ;;
    esac
done

for n in "$LOG_LINES" "$CARGO_TIMEOUT" "$DEV_TIMEOUT" "$GUI_WAIT" "$SCENARIO_TIMEOUT"; do
    case "$n" in ''|*[!0-9]*) die "not a whole number: $n" ;; esac
done
case "$ENGINES" in ''|*[!a-z0-9,]*) die "--engines takes ids like postgresql,mysql" ;; esac
case "$COMPOSE_PROJECT" in ''|*[!a-z0-9_-]*|[!a-z0-9]*) die "--compose-project takes a compose project name (lower-case letters, digits, - and _)" ;; esac

in_list() { # in_list <word> <comma-separated list>
    case ",$2," in *",$1,"*) return 0 ;; *) return 1 ;; esac
}

for s in ${ONLY//,/ } ${SKIP//,/ }; do
    in_list "$s" "$(IFS=,; echo "${ALL_STAGES[*]}")" || die "unknown stage: $s (stages: ${ALL_STAGES[*]})"
done

# The stages to run, in their fixed order.
STAGES=()
for s in "${ALL_STAGES[@]}"; do
    if [ -n "$ONLY" ]; then
        in_list "$s" "$ONLY" || [ "$s" = env ] || continue
    else
        in_list "$s" "$(IFS=,; echo "${DEFAULT_STAGES[*]}")" || { [ "$s" = deb ] && [ "$WITH_DEB" = 1 ]; } || continue
    fi
    in_list "$s" "$SKIP" && continue
    STAGES+=("$s")
done
[ -n "$CHECK_REPORT" ] && STAGES=()
[ -n "$CHECK_REPORT" ] || [ "${#STAGES[@]}" -gt 0 ] || die "no stage left to run"

# Test hooks of scripts/e2e-local-report-test.sh only (never needed for a real run):
# E2E_TEST_SCENARIO_DIR replaces e2e/scenarios, E2E_TEST_SCENARIO_BIN replaces the test build
# (no cargo build; its --version must still say it is a test build), E2E_TEST_OUTPUT_DIR
# replaces target/ as the parent of e2e-reports/ and e2e-runs/.
SCENARIO_DIR="${E2E_TEST_SCENARIO_DIR:-$ROOT/e2e/scenarios}"
OUT_BASE="${E2E_TEST_OUTPUT_DIR:-$ROOT/target}"
# The scenarios to run: --scenario, else every e2e/scenarios/*.toml, in name order.
SCENARIO_LIST=()
if [ -n "$SCENARIOS" ]; then
    IFS=, read -ra SCENARIO_ARGS <<<"$SCENARIOS"
    for sc in "${SCENARIO_ARGS[@]}"; do
        case "$sc" in
            ''|*[!a-z0-9-]*) die "--scenario takes scenario names (lower-case letters, digits and -) like sqlite-editor-grid,postgresql-editor-safety: ${sc:-<empty>}" ;;
        esac
        [ -f "$SCENARIO_DIR/$sc.toml" ] || die "unknown scenario: $sc (no e2e/scenarios/$sc.toml)"
        in_list "$sc" "$(IFS=,; echo "${SCENARIO_LIST[*]}")" || SCENARIO_LIST+=("$sc")
    done
elif [ -d "$SCENARIO_DIR" ]; then
    while IFS= read -r f; do
        sc="$(basename "$f" .toml)"
        case "$sc" in ''|*[!a-z0-9-]*) continue ;; esac
        SCENARIO_LIST+=("$sc")
    done < <(find "$SCENARIO_DIR" -maxdepth 1 -type f -name '*.toml' | LC_ALL=C sort)
fi

# ---------------------------------------------------------------------------------------------
# Report directory and helpers
# ---------------------------------------------------------------------------------------------

STARTED_EPOCH="$(date -u +%s)"
STARTED_UTC="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
GIT_SHA="$(git rev-parse --short=12 HEAD 2>/dev/null || echo unknown)"
REPORT_TITLE="# NineSQL local end-to-end report"
if [ -n "$CHECK_REPORT" ]; then
    CHECK_REPORT="${CHECK_REPORT%/}"
    [ -d "$CHECK_REPORT" ] && [ -O "$CHECK_REPORT" ] && [ ! -L "$CHECK_REPORT" ] ||
        die "--check-report: not a directory you own: $CHECK_REPORT"
    # Only a report this script wrote: a summary.md (a regular file, not a symbolic link) whose
    # first line is the report title. Any other directory you own (as root, /etc) is refused.
    [ -f "$CHECK_REPORT/summary.md" ] && [ ! -L "$CHECK_REPORT/summary.md" ] &&
        [ "$(head -n 1 "$CHECK_REPORT/summary.md" 2>/dev/null)" = "$REPORT_TITLE" ] ||
        die "--check-report: not a report directory (no summary.md starting with \"$REPORT_TITLE\"): $CHECK_REPORT"
    # The files this script writes into the report must be regular files or absent, never a
    # symbolic link or anything else (a planted link would redirect the write).
    for f in credential-uris.txt gitleaks.log report.tar.gz; do
        if [ -L "$CHECK_REPORT/$f" ] || { [ -e "$CHECK_REPORT/$f" ] && [ ! -f "$CHECK_REPORT/$f" ]; }; then
            die "--check-report: $f is not a regular file: $CHECK_REPORT/$f"
        fi
    done
    if [ -L "$CHECK_REPORT/.notes" ] || { [ -e "$CHECK_REPORT/.notes" ] && [ ! -d "$CHECK_REPORT/.notes" ]; }; then
        die "--check-report: .notes is not a directory: $CHECK_REPORT/.notes"
    fi
    REPORT="$(cd "$CHECK_REPORT" && pwd -P)"
else
    mkdir -p "$OUT_BASE/e2e-reports"
    REPORT="$OUT_BASE/e2e-reports/$STAMP-$GIT_SHA"
    suffix=1
    # A new directory, for this user only (mkdir fails when the name exists).
    until mkdir -m 0700 "$REPORT" 2>/dev/null; do
        suffix=$((suffix + 1))
        [ "$suffix" -le 100 ] || die "cannot create a report directory under $OUT_BASE/e2e-reports"
        REPORT="$OUT_BASE/e2e-reports/$STAMP-$GIT_SHA-$suffix"
    done
fi
NOTES="$REPORT/.notes"
mkdir -p "$NOTES"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
SUMMARY="$REPORT/summary.md"
ARCHIVE="$REPORT/report.tar.gz"
# Written into the report when a scenario run found a secret (runner exit 3). Its presence,
# or a "FAIL (SECRET FOUND)" row in summary.md, stops any archive of this report, later
# --check-report runs and --no-secret-scan included.
SECRET_MARKER="$REPORT/scenarios/SECRET-FOUND"
# What stderr.log of such a run holds instead of the runner's standard error.
STDERR_WITHHELD="stderr withheld: the runner found a secret in a file it wrote (exit 3); its standard error is not copied into the report."

say() { printf '%s\n' "$*"; }

fmt_dur() {
    local s=$1
    if [ "$s" -ge 3600 ]; then printf '%dh%02dm%02ds' $((s / 3600)) $((s % 3600 / 60)) $((s % 60))
    elif [ "$s" -ge 60 ]; then printf '%dm%02ds' $((s / 60)) $((s % 60))
    else printf '%ds' "$s"; fi
}

# run <log> <command...>: appends the command and its output to <log> and returns its status.
# In --dry-run, prints the command and returns 0.
run() {
    local log=$1
    shift
    local line
    line="$(printf '%q ' "$@")"
    printf '+ %s\n' "${line% }" >>"$log"
    if [ "$DRY_RUN" = 1 ]; then
        printf '    [dry-run] %s\n' "${line% }"
        return 0
    fi
    if [ "$VERBOSE" = 1 ]; then
        "$@" </dev/null 2>&1 | tee -a "$log"
        return "${PIPESTATUS[0]}"
    fi
    "$@" </dev/null >>"$log" 2>&1
}

# note <stage> <markdown line>: extra lines for the stage's section of summary.md.
note() { printf '%s\n' "$2" >>"$NOTES/$1.md"; }
# reason <stage> <text>: one-line reason of a FAIL or SKIP.
reason() { printf '%s\n' "$2" >"$NOTES/$1.reason"; }

have() { command -v "$1" >/dev/null 2>&1; }

have_display() { [ -n "${WAYLAND_DISPLAY:-}" ] || [ -n "${DISPLAY:-}" ]; }

python_ok_311() { python3 -c 'import sys; sys.exit(0 if sys.version_info >= (3, 11) else 1)' 2>/dev/null; }

# Extracts failing tests and their output from a cargo test log into the stage's notes.
note_test_failures() { # <stage> <log>
    python3 -I - "$2" >>"$NOTES/$1.md" <<'EOF' || true
import re, sys
lines = open(sys.argv[1], encoding="utf-8", errors="replace").read().splitlines()
failed, blocks, compile_errors, rerun = [], [], [], []
binary = "?"
i = 0
while i < len(lines):
    line = lines[i]
    m = re.match(r"\s*Running (?:unittests )?(\S+) \((?:.*/)?deps/([A-Za-z0-9_]+?)(?:-[0-9a-f]+)?\)", line)
    if m:
        binary = f"{m.group(2)} ({m.group(1)})"
    m = re.match(r"test (\S+) \.\.\. FAILED$", line)
    if m:
        failed.append(f"{m.group(1)}  [{binary}]")
    m = re.match(r"---- (\S+) stdout ----$", line)
    if m:
        block = [line]
        i += 1
        while i < len(lines) and not re.match(r"(---- \S+ stdout ----|failures:)$", lines[i]):
            block.append(lines[i])
            i += 1
        if len(block) > 60:
            block = block[:30] + [f"... ({len(block) - 50} lines cut) ..."] + block[-20:]
        blocks.append(block)
        continue
    if re.match(r"error(\[E\d+\])?: ", line) and not line.startswith("error: test failed"):
        compile_errors.append(line)
    if line.startswith("error: test failed") or "to rerun pass" in line:
        rerun.append(line.strip())
    i += 1
if failed:
    print(f"\nFailed tests ({len(failed)}):\n")
    for name in failed[:200]:
        print(f"- `{name}`")
if rerun:
    print("\nTest binaries that failed:\n")
    for r in sorted(set(rerun))[:50]:
        print(f"- {r}")
if compile_errors:
    print(f"\nCompiler errors ({len(compile_errors)}, first 20):\n\n```text")
    print("\n".join(compile_errors[:20]))
    print("```")
if blocks:
    print("\nFailure output (first 10 tests):\n\n```text")
    for b in blocks[:10]:
        print("\n".join(b))
    print("```")
EOF
}

# ---------------------------------------------------------------------------------------------
# Stage: env
# ---------------------------------------------------------------------------------------------

stage_env() {
    local log=$1 md="$NOTES/env.md" fatal=0 v
    : >"$md"
    kv() { printf -- '- %s: %s\n' "$1" "$2" >>"$md"; }
    warn() { printf -- '- **Warning**: %s\n' "$1" >>"$md"; printf 'WARNING: %s\n' "$1" >>"$log"; }
    first_line() { "$@" 2>/dev/null | head -n 1 || true; }

    if [ -r /etc/os-release ]; then
        # shellcheck disable=SC1091
        v="$(. /etc/os-release && printf '%s' "${PRETTY_NAME:-$NAME}")"
        kv "OS" "$v"
    fi
    kv "Kernel" "$(uname -sr) ($(uname -m))"
    kv "CPU" "$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ *//') ($(nproc 2>/dev/null || echo ?) threads)"
    kv "Memory" "$(awk '/MemTotal/ { printf "%.1f GiB", $2 / 1048576 }' /proc/meminfo 2>/dev/null)"
    v="$(df -Pk "$ROOT" 2>/dev/null | awk 'NR == 2 { printf "%.1f", $4 / 1048576 }')"
    kv "Free disk (repository file system)" "${v:-?} GiB"
    if [ -n "$v" ] && awk -v f="$v" 'BEGIN { exit !(f < 15) }'; then
        warn "less than 15 GiB free: the debug and release builds and the engine images need more"
    fi

    # Toolchain: rustup reads rust-toolchain.toml from the repository root.
    if have rustc && have cargo; then
        kv "rustc" "$(first_line rustc --version)"
        kv "cargo" "$(first_line cargo --version)"
        have rustup && kv "rustup toolchain" "$(first_line rustup show active-toolchain)"
        v="$(rustc --version 2>/dev/null | awk '{ print $2 }')"
        if [ -n "$v" ] && ! printf '1.89.0\n%s\n' "$v" | sort -V -C; then
            warn "rustc $v is older than 1.89 (docs/tutorials/build-and-run.md)"
        fi
    else
        warn "rustc / cargo not found: install Rust with rustup (docs/tutorials/build-and-run.md)"
        fatal=1
    fi
    if have cargo && cargo deny --version >/dev/null 2>&1; then
        kv "cargo-deny" "$(first_line cargo deny --version) (CI uses 0.20.2)"
    else
        warn "cargo-deny not found: the static stage's deny step will fail (cargo install cargo-deny --locked)"
    fi
    if have python3; then
        kv "python3" "$(first_line python3 --version)"
        python_ok_311 || warn "Python 3.11 or newer is needed by scripts/verify-patches.sh (tomllib); verify-patches will fail"
    else
        warn "python3 not found (needed by the Makefile, dev/ready.py and this report)"
        fatal=1
    fi
    if have make; then kv "make" "$(first_line make --version)"; else warn "make not found"; fatal=1; fi
    kv "git" "$(first_line git --version)"

    if have docker; then
        kv "docker client" "$(first_line docker version --format '{{.Client.Version}}')"
        v="$(timeout 20 docker version --format '{{.Server.Version}}' 2>/dev/null || true)"
        if [ -n "$v" ]; then
            kv "docker server" "$v ($(timeout 20 docker info --format '{{.NCPU}} CPUs, {{.MemTotal}} bytes of memory' 2>/dev/null || echo ?))"
        else
            warn "the Docker daemon is not reachable (is it running, is your user in the docker group?): engines and dev-check will fail"
        fi
        v="$(docker compose version --short 2>/dev/null || true)"
        if [ -n "$v" ]; then kv "docker compose" "$v"; else warn "the Docker Compose v2 plugin is missing"; fi
    else
        warn "docker not found: the engines stage and the dev-check step will fail"
    fi

    if have dbus-daemon; then
        kv "dbus-daemon" "$(first_line dbus-daemon --version)"
    else
        warn "dbus-daemon not found: the lock-watcher tests skip themselves (sudo apt-get install dbus-daemon)"
    fi
    for t in gitleaks shellcheck cargo-deb cargo-about; do
        if have "$t"; then
            kv "$t" "$("$t" --version 2>/dev/null | grep -m1 -iE '[0-9]+\.[0-9]+' || true)"
        else
            kv "$t" "not installed"
        fi
    done
    if ! have gitleaks; then
        if [ "$NO_SECRET_SCAN" = 1 ]; then
            warn "gitleaks not installed; --no-secret-scan: the archive will be written without a secret scan"
        else
            warn "gitleaks not installed: no report.tar.gz will be written (install gitleaks, or rerun with --no-secret-scan)"
        fi
    fi

    # Session and display (no display names beyond the session type).
    kv "Session type" "${XDG_SESSION_TYPE:-unknown}"
    kv "Desktop" "${XDG_CURRENT_DESKTOP:-unknown}"
    kv "WAYLAND_DISPLAY set" "$([ -n "${WAYLAND_DISPLAY:-}" ] && echo yes || echo no)"
    kv "DISPLAY set" "$([ -n "${DISPLAY:-}" ] && echo yes || echo no)"
    have_display || warn "no display (WAYLAND_DISPLAY and DISPLAY unset): gui-smoke will be skipped"
    local scale=""
    if have gsettings; then
        v="$(timeout 5 gsettings get org.gnome.desktop.interface scaling-factor 2>/dev/null || true)"
        [ -n "$v" ] && scale+="GNOME scaling-factor ${v#uint32 }; "
        v="$(timeout 5 gsettings get org.gnome.desktop.interface text-scaling-factor 2>/dev/null || true)"
        [ -n "$v" ] && scale+="text-scaling-factor $v; "
    fi
    if [ -n "${DISPLAY:-}" ] && have xrdb; then
        v="$(timeout 5 xrdb -query 2>/dev/null | awk '/^Xft.dpi:/ { print $2 }' || true)"
        [ -n "$v" ] && scale+="Xft.dpi $v; "
    fi
    for e in GDK_SCALE GDK_DPI_SCALE QT_SCALE_FACTOR; do
        [ -n "${!e:-}" ] && scale+="$e=${!e}; "
    done
    kv "Display scale" "${scale:-not detected}"
    if [ -n "${DISPLAY:-}" ] && have xrandr; then
        v="$(timeout 5 xrandr --current 2>/dev/null | awk '/\*/ { print $1 }' | paste -sd, - || true)"
        [ -n "$v" ] && kv "Screen modes (xrandr)" "$v"
    fi

    # GPU: only an allowlist of key / value lines is kept (no device or driver UUIDs, no
    # serial numbers, no full dumps in the log).
    local lines
    if have_display && have glxinfo; then
        lines="$(timeout 15 glxinfo -B 2>/dev/null | grep -E '^[[:space:]]*(direct rendering|Vendor|Device|Version|Accelerated|Video memory|Unified memory|OpenGL (vendor|renderer|version|core profile version|ES profile version) string):' | sed 's/^[[:space:]]*//' || true)"
        printf 'glxinfo -B (filtered):\n%s\n' "${lines:-no answer}" >>"$log"
        v="$(grep -E '^OpenGL (renderer|version) string' <<<"$lines" | paste -sd ';' - || true)"
        kv "OpenGL (glxinfo -B)" "${v:-no answer}"
    else
        kv "OpenGL (glxinfo -B)" "not available (mesa-utils not installed, or no display)"
    fi
    if have vulkaninfo; then
        lines="$(timeout 20 vulkaninfo --summary 2>/dev/null | grep -E '^[[:space:]]*(apiVersion|driverVersion|vendorID|deviceID|deviceType|deviceName|driverID|driverName|driverInfo|conformanceVersion)[[:space:]]*=' | sed 's/^[[:space:]]*//; s/[[:space:]][[:space:]]*/ /g' || true)"
        printf 'vulkaninfo --summary (filtered):\n%s\n' "${lines:-no device listed}" >>"$log"
        v="$(grep -E '^(deviceType|deviceName|driverName|driverInfo|apiVersion) =' <<<"$lines" | paste -sd ';' - | sed 's/;/; /g' || true)"
        kv "Vulkan (vulkaninfo --summary)" "${v:-no device listed}"
    else
        kv "Vulkan (vulkaninfo --summary)" "not available (vulkan-tools not installed)"
    fi
    if [ "$SCREENSHOT" = 1 ]; then
        if [ -n "${DISPLAY:-}" ] && have import && { have xdotool || have wmctrl; }; then
            kv "Screenshot" "requested (--screenshot): the window of the launched process only (found by pid with xdotool / wmctrl, captured with import)"
        else
            warn "--screenshot: no window capture here (needs DISPLAY, ImageMagick import and xdotool or wmctrl): no screenshot will be taken, and no whole-screen capture instead"
        fi
    else
        kv "Screenshot" "not requested (--screenshot to take one)"
    fi
    if [ "$SCENARIO_SHOTS" = 1 ]; then
        if [ -n "${DISPLAY:-}" ] && have import && { have xdotool || have wmctrl; }; then
            kv "Scenario screenshots" "at each checkpoint, the test window only (XWayland or X11: WAYLAND_DISPLAY unset for the run)"
        else
            warn "scenario checkpoint screenshots need DISPLAY (X11 or XWayland), ImageMagick import and xdotool or wmctrl: the scenarios will run without screenshots"
        fi
    else
        kv "Scenario screenshots" "off (--no-screenshot)"
    fi

    cat "$md" >>"$log"
    if [ "$fatal" = 1 ]; then
        reason env "a required tool is missing (see the warnings in the Environment section)"
        return 1
    fi
    return 0
}

# ---------------------------------------------------------------------------------------------
# Stage: static
# ---------------------------------------------------------------------------------------------

stage_static() {
    local log=$1 step sublog rc failed=() t0
    note static "| Step | Result | Duration | Log |"
    note static "|---|---|---|---|"
    for step in "${STATIC_STEPS[@]}"; do
        sublog="$REPORT/static-$step.log"
        : >"$sublog"
        t0=$SECONDS
        say "    static: $step"
        rc=0
        if [ "$step" = verify-patches ] && [ "$DRY_RUN" = 0 ] && ! python_ok_311; then
            echo "scripts/verify-patches.sh needs Python 3.11 or newer (tomllib); found: $(python3 --version 2>&1)" >>"$sublog"
            rc=1
        elif [ "$step" = deny ] && [ "$DRY_RUN" = 0 ] && ! cargo deny --version >/dev/null 2>&1; then
            echo "cargo-deny is not installed: cargo install cargo-deny --locked (CI uses 0.20.2)" >>"$sublog"
            rc=1
        else
            run "$sublog" make --no-print-directory "$step" "TIMEOUT=$CARGO_TIMEOUT" || rc=$?
        fi
        {
            printf '\n===== %s (exit %s, %s) =====\n' "$step" "$rc" "$(fmt_dur $((SECONDS - t0)))"
            cat "$sublog"
        } >>"$log"
        if [ "$rc" = 0 ]; then
            note static "| $step | PASS | $(fmt_dur $((SECONDS - t0))) | \`static-$step.log\` |"
        else
            note static "| $step | FAIL (exit $rc) | $(fmt_dur $((SECONDS - t0))) | \`static-$step.log\` |"
            failed+=("$step")
            [ "$rc" = 124 ] && note static "" && note static "$step hit the time limit (--timeout $CARGO_TIMEOUT)."
        fi
    done
    if [ "${#failed[@]}" -gt 0 ]; then
        reason static "failed steps: ${failed[*]}"
        printf '%s\n' "${failed[@]}" >"$NOTES/static.failed"
        return 1
    fi
}

# ---------------------------------------------------------------------------------------------
# Stage: unit
# ---------------------------------------------------------------------------------------------

stage_unit() {
    local log=$1 rc=0 dbus=()
    if have dbus-daemon; then
        dbus=(env NINESQL_REQUIRE_DBUS=1)
        note unit "NINESQL_REQUIRE_DBUS=1 (dbus-daemon found): the lock-watcher tests must run."
    else
        note unit "dbus-daemon not found: the lock-watcher tests skipped themselves (sudo apt-get install dbus-daemon)."
    fi
    # --no-fail-fast: every test binary runs, so the report lists every failure, not the first.
    run "$log" "${dbus[@]}" make --no-print-directory test "TIMEOUT=$CARGO_TIMEOUT" TEST_ARGS=--no-fail-fast || rc=$?
    if [ "$rc" != 0 ]; then
        note_test_failures unit "$log"
        if [ "$rc" = 124 ]; then reason unit "cargo test hit the time limit (--timeout $CARGO_TIMEOUT)"
        else reason unit "cargo test --workspace failed (exit $rc)"; fi
        return 1
    fi
    grep -E '^test result: ' "$log" | awk '{ p += $4; f += $6; ig += $8 } END { if (NR) printf "Totals over %d test binaries: %d passed, %d failed, %d ignored.\n", NR, p, f, ig }' >>"$NOTES/unit.md" || true
}

# ---------------------------------------------------------------------------------------------
# Stage: engines
# ---------------------------------------------------------------------------------------------

compose() { docker compose -f dev/compose.yaml "$@"; }

# The startup plan for a list of engine ids, from dev/engines.json:
#   UP <profile> <ids,...> <service> <service>...   engines to start, per profile
#   PORT <port>                                      host ports they publish
#   NOSERVICE <id>                                   file engines and ssh (nothing to start)
#   UNKNOWN <id>
engine_plan() {
    python3 -I - "$ROOT/dev/engines.json" "$1" <<'EOF'
import json, sys
doc = json.load(open(sys.argv[1], encoding="utf-8"))
by_id = {e["id"]: e for e in doc["engines"]}
groups = {}
for eid in [x for x in sys.argv[2].split(",") if x]:
    if eid == "ssh":  # the core and ssh tests start their own OpenSSH containers
        print(f"NOSERVICE {eid}")
        continue
    e = by_id.get(eid)
    if e is None:
        print(f"UNKNOWN {eid}")
        continue
    if not e["service"]:
        print(f"NOSERVICE {eid}")
        continue
    groups.setdefault(e["profile"], []).append(e)
    if e.get("port"):
        print(f"PORT {e['port']}")
    for p in (e.get("extra_ports") or {}).values():
        print(f"PORT {p}")
for profile in ("core", "sql", "nosql", "heavy"):
    es = groups.get(profile)
    if es:
        print("UP", profile, ",".join(e["id"] for e in es), " ".join(e["service"] for e in es))
EOF
}

port_busy() { timeout 2 bash -c "exec 3<>/dev/tcp/127.0.0.1/$1" 2>/dev/null; }

# running_compose_projects <plan>: the compose projects of the containers that publish the
# plan's host ports, one per line.
running_compose_projects() {
    local kind a
    while read -r kind a; do
        [ "$kind" = PORT ] || continue
        docker ps --filter "publish=$a" --format '{{.Label "com.docker.compose.project"}}' 2>/dev/null </dev/null || true
    done <<<"$1" | grep -E '^[a-z0-9][a-z0-9_-]*$' | sort -u
}

stage_engines() {
    local log=$1 rc=0 plan kind a b rest busy=() started=0 p projects collect=1
    export COMPOSE_PROJECT_NAME="$COMPOSE_PROJECT"

    if [ "$DRY_RUN" = 0 ]; then
        if ! have docker || ! timeout 20 docker info >/dev/null 2>&1; then
            echo "Docker is not available or its daemon is not reachable." >>"$log"
            reason engines "Docker is not available or its daemon is not reachable"
            return 1
        fi
    fi
    plan="$(engine_plan "$ENGINES")" || { reason engines "cannot read dev/engines.json"; return 1; }
    printf 'Plan:\n%s\n' "$plan" >>"$log"
    if grep -q '^UNKNOWN ' <<<"$plan"; then
        reason engines "unknown engine ids: $(grep '^UNKNOWN ' <<<"$plan" | cut -d' ' -f2 | paste -sd, -)"
        return 1
    fi
    # With --engines-running, the state and the logs come from the project that is actually up
    # (make dev-up uses ninesql-dev), not from this script's default project.
    if [ "$ENGINES_RUNNING" = 1 ] && [ "$COMPOSE_PROJECT_SET" = 0 ] && [ "$DRY_RUN" = 0 ]; then
        projects="$(running_compose_projects "$plan")"
        case "$(grep -c . <<<"$projects")" in
            1) COMPOSE_PROJECT_NAME="$projects" ;;
            0) collect=0
               note engines "No running compose project publishes the dev ports: container state and logs not collected." ;;
            *) collect=0
               note engines "Several compose projects publish the dev ports ($(paste -sd, - <<<"$projects")): container state and logs not collected; rerun with --compose-project." ;;
        esac
    fi
    note engines "Required engines: \`$ENGINES\`; compose project \`$COMPOSE_PROJECT_NAME\`."

    teardown() {
        [ "$started" = 1 ] || return 0
        started=0
        if [ "$KEEP_ENGINES" = 1 ]; then
            note engines "Engines left running (--keep-engines); stop them with: COMPOSE_PROJECT_NAME=$COMPOSE_PROJECT_NAME docker compose -f dev/compose.yaml --profile all down"
            return 0
        fi
        say "    engines: stopping the containers"
        if [ "$CLEAN_VOLUMES" = 1 ]; then
            run "$log" compose --profile all down -v || true
        else
            run "$log" compose --profile all down || true
        fi
    }
    trap 'teardown; exit 130' INT TERM

    if [ "$ENGINES_RUNNING" = 0 ]; then
        # The host ports are fixed by dev/compose.yaml, so another project's containers (make
        # dev-up uses ninesql-dev, CI uses ninesql-ci-drivers) would make `up` fail.
        if [ "$DRY_RUN" = 0 ]; then
            while read -r kind a; do
                [ "$kind" = PORT ] && port_busy "$a" </dev/null && busy+=("$a")
            done <<<"$plan"
            if [ "${#busy[@]}" -gt 0 ]; then
                {
                    echo "Host ports already in use: ${busy[*]}"
                    for p in "${busy[@]}"; do
                        docker ps --filter "publish=$p" --format "  port $p: container {{.Names}}, compose project {{.Label \"com.docker.compose.project\"}}" 2>/dev/null || true
                    done
                } >>"$log"
                reason engines "host ports already in use (${busy[*]}): stop the other containers (make dev-down) or rerun with --engines-running"
                note engines "Ports in use before the stage: ${busy[*]} (details in the log)."
                return 1
            fi
        fi
        started=1
        while read -r kind a b rest; do
            [ "$kind" = UP ] || continue
            say "    engines: pulling and starting $b"
            # shellcheck disable=SC2086 # $rest is a list of service names
            run "$log" timeout "$DEV_TIMEOUT" docker compose -f dev/compose.yaml --profile "$a" pull --quiet $rest || rc=$?
            [ "$rc" = 0 ] || break
            run "$log" python3 dev/ready.py up "$a" --engines "$b" --timeout "$DEV_TIMEOUT" || rc=$?
            [ "$rc" = 0 ] || break
        done <<<"$plan"
        if [ "$rc" != 0 ]; then
            reason engines "the engines did not start or were not seeded in time (exit $rc)"
        fi
    else
        note engines "--engines-running: the stage started and stopped nothing."
    fi

    if [ "$rc" = 0 ]; then
        say "    engines: running the driver and core engine tests"
        run "$log" make --no-print-directory test-engines-run "REQUIRE_ENGINES=$ENGINES" "TIMEOUT=$CARGO_TIMEOUT" TEST_ARGS=--no-fail-fast || rc=$?
        if [ "$rc" != 0 ]; then
            note_test_failures engines "$log"
            if [ "$rc" = 124 ]; then reason engines "the engine tests hit the time limit (--timeout $CARGO_TIMEOUT)"
            else reason engines "the engine tests failed (exit $rc)"; fi
        fi
    fi

    # State and container logs, whatever happened.
    if [ "$collect" = 1 ]; then
        run "$REPORT/engines-compose-ps.txt" compose --profile all ps --all || true
        say "    engines: collecting the last $LOG_LINES log lines of each container"
        run "$REPORT/engines-containers.log" compose --profile all logs --no-color --timestamps --tail "$LOG_LINES" || true
        note engines "Container state: \`engines-compose-ps.txt\`; last $LOG_LINES lines per container: \`engines-containers.log\`."
    fi
    teardown
    trap - INT TERM
    [ "$rc" = 0 ]
}

# ---------------------------------------------------------------------------------------------
# Stage: gui-smoke
# ---------------------------------------------------------------------------------------------

APP_ID="io.github.yil00.NineSQL"

# find_windows <pid>: the X11 ids of the visible windows of process <pid> (their _NET_WM_PID),
# one per line, or nothing. The search is by pid, never by window class, so another NineSQL
# window on the desktop (the maintainer's own) can never be picked.
find_windows() {
    local pid=$1 ids=""
    if have xdotool; then
        ids="$(timeout 5 xdotool search --onlyvisible --pid "$pid" 2>/dev/null || true)"
    fi
    if [ -z "$ids" ] && have wmctrl; then
        ids="$(timeout 5 wmctrl -lp 2>/dev/null | awk -v p="$pid" '$3 == p { print $1 }' || true)"
    fi
    grep -E '^(0x[0-9a-fA-F]+|[0-9]+)$' <<<"$ids" | sort -u || true
}

# screenshot_window <file> <pid> [settle]: the window of the launched process only (X11, or
# XWayland), raised first so no other window covers it, then [settle] seconds (default 1) for
# the raise to be drawn. Nothing is clicked or typed. There is no whole-screen fallback: when
# no single window of <pid> can be captured, nothing is taken and SHOT_WHY says why.
SHOT_WHY=""
screenshot_window() {
    local out=$1 pid=$2 settle=${3:-1} ids n id
    if [ -z "${DISPLAY:-}" ]; then
        SHOT_WHY="no X11 display (DISPLAY unset); a Wayland-native window cannot be captured alone"
        return 1
    fi
    if ! have import; then SHOT_WHY="ImageMagick import is not installed"; return 1; fi
    if ! have xdotool && ! have wmctrl; then SHOT_WHY="neither xdotool nor wmctrl is installed"; return 1; fi
    ids="$(find_windows "$pid")"
    n="$(grep -c . <<<"$ids" || true)"
    if [ "$n" = 0 ]; then
        SHOT_WHY="no visible X11 window of the launched process was found (searched by its pid, _NET_WM_PID; a Wayland-native window has none)"
        return 1
    fi
    if [ "$n" != 1 ]; then
        SHOT_WHY="$n visible windows belong to the launched process; none was picked"
        return 1
    fi
    id="$ids"
    if have xdotool; then
        timeout 5 xdotool windowactivate --sync "$id" >/dev/null 2>&1 || true
    else
        timeout 5 wmctrl -i -a "$id" >/dev/null 2>&1 || true
    fi
    sleep "$settle"
    if ! kill -0 "$pid" 2>/dev/null; then SHOT_WHY="the process exited before the capture"; return 1; fi
    if timeout 15 import -window "$id" "$out" 2>/dev/null && [ -s "$out" ]; then return 0; fi
    SHOT_WHY="ImageMagick import could not capture the window"
    return 1
}

stage_gui_smoke() {
    local log=$1 bin="$TARGET_DIR/release/ninesql" rc=0 tmp="" pid="" code="" fails=() i hits shot
    local out="$REPORT/gui-smoke-stdout.log" err="$REPORT/gui-smoke-stderr.log"

    if ! have_display; then
        echo "No display: WAYLAND_DISPLAY and DISPLAY are unset." >>"$log"
        reason gui-smoke "no display (WAYLAND_DISPLAY and DISPLAY are unset)"
        if [ "$DRY_RUN" = 1 ]; then
            say "    gui-smoke: no display; with one it would run:"
            DRY_RUN_GUI_ONLY=1
        else
            return 77
        fi
    fi

    cleanup_gui() {
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then kill -KILL "$pid" 2>/dev/null || true; fi
        if [ -n "$tmp" ] && [ "$KEEP_TEMP" = 0 ]; then rm -rf "$tmp"; fi
    }
    trap 'cleanup_gui; exit 130' INT TERM

    say "    gui-smoke: building the release binary"
    run "$log" make --no-print-directory release "TIMEOUT=$CARGO_TIMEOUT" || {
        reason gui-smoke "make release failed"
        return 1
    }

    # Arguments answered before GPUI starts (packaging install test does the same).
    run "$log" "$bin" --version || fails+=("--version failed")
    run "$log" "$bin" --help || fails+=("--help failed")
    if [ "$DRY_RUN" = 0 ]; then
        "$bin" --no-such-option >>"$log" 2>&1 && code=0 || code=$?
        echo "unknown argument exit status: $code (expected 2)" >>"$log"
        [ "$code" = 2 ] || fails+=("an unknown argument exited with $code, not 2")
    fi

    if [ "$DRY_RUN" = 1 ]; then
        say "    [dry-run] mkdir temp HOME / XDG_CONFIG_HOME / XDG_DATA_HOME / XDG_STATE_HOME / XDG_CACHE_HOME (0700)"
        say "    [dry-run] HOME=<temp> NINESQL_LOG=ninesql=debug,info $bin &   (alive check after ${GUI_WAIT} s)"
        if [ "$SCREENSHOT" = 1 ]; then
            say "    [dry-run] screenshot of the launched process's window, found by pid (xdotool search --pid / wmctrl -lp, import -window <id>); none when no single window matches, never the whole screen"
        else
            say "    [dry-run] no screenshot (--screenshot to take one)"
        fi
        say "    [dry-run] kill -TERM <pid>; wait up to 10 s; I1 scan of the temp XDG directories"
        if [ "${DRY_RUN_GUI_ONLY:-0}" = 1 ]; then return 77; fi
        return 0
    fi

    tmp="$(mktemp -d -t ninesql-e2e-XXXXXX)"
    mkdir -m 0700 "$tmp/home" "$tmp/config" "$tmp/data" "$tmp/state" "$tmp/cache"
    echo "Temporary HOME and XDG directories under $tmp" >>"$log"
    say "    gui-smoke: starting ninesql for ${GUI_WAIT} s (a window opens; leave it alone)"
    # A temporary HOME too: the first-run DBeaver detection reads known locations under $HOME,
    # so the user's real DBeaver connection names never reach the window (or a screenshot).
    # NINESQL_LOG (ADR-0029 §3.4; RUST_LOG is ignored), never at trace: trace may carry query
    # text (AGENTS.md). It also turns the file log on, under $tmp/state/ninesql/logs/.
    env HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config" XDG_DATA_HOME="$tmp/data" \
        XDG_STATE_HOME="$tmp/state" XDG_CACHE_HOME="$tmp/cache" NINESQL_LOG="ninesql=debug,info" \
        "$bin" </dev/null >"$out" 2>"$err" &
    pid=$!
    for ((i = 0; i < GUI_WAIT; i++)); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 1
    done
    if kill -0 "$pid" 2>/dev/null; then
        echo "alive after ${GUI_WAIT} s" >>"$log"
        if [ -n "${DISPLAY:-}" ] && have wmctrl; then
            if timeout 5 wmctrl -lpx 2>/dev/null | awk -v p="$pid" -v c="$APP_ID" '$3 == p && index($4, c) { f = 1 } END { exit !f }'; then
                note gui-smoke "A window of the launched process with WM_CLASS $APP_ID was found (wmctrl)."
            else
                note gui-smoke "No window of the launched process with WM_CLASS $APP_ID was listed by wmctrl (informational; normal on a Wayland-native window)."
            fi
        fi
        if [ "$SCREENSHOT" = 1 ]; then
            shot="$REPORT/gui-smoke-window.png"
            if screenshot_window "$shot" "$pid"; then
                note gui-smoke "Screenshot (--screenshot): \`gui-smoke-window.png\`, the window of the launched process only (check it before sending)."
            else
                rm -f "$shot"
                say "    gui-smoke: screenshot not taken: $SHOT_WHY"
                note gui-smoke "Screenshot (--screenshot): not taken: $SHOT_WHY. No whole-screen capture is ever taken instead."
            fi
        else
            note gui-smoke "No screenshot (pass --screenshot to take one)."
        fi
        kill -TERM "$pid" 2>/dev/null || true
        for ((i = 0; i < 10; i++)); do
            kill -0 "$pid" 2>/dev/null || break
            sleep 1
        done
        if kill -0 "$pid" 2>/dev/null; then
            kill -KILL "$pid" 2>/dev/null || true
            fails+=("ninesql did not exit within 10 s of SIGTERM (killed)")
        fi
        wait "$pid" 2>/dev/null && code=0 || code=$?
        note gui-smoke "Stayed up ${GUI_WAIT} s; exit status after SIGTERM: $code (143 = ended by SIGTERM)."
    else
        wait "$pid" 2>/dev/null && code=0 || code=$?
        fails+=("ninesql exited after less than ${GUI_WAIT} s with status $code")
    fi
    pid=""
    echo "exit status: $code" >>"$log"
    if grep -qE 'internal error \(panic\)|panicked at' "$err" 2>/dev/null; then
        fails+=("a panic was reported on stderr")
    fi
    note gui-smoke "stderr: \`gui-smoke-stderr.log\` ($(wc -l <"$err") lines), stdout: \`gui-smoke-stdout.log\`. The file log (NINESQL_LOG) is saved as \`gui-smoke-ninesql.log\` when written."

    # What the run wrote (names and sizes, relative to the temp directory), and its log file.
    {
        echo "Files written under the temporary HOME and XDG directories:"
        (cd "$tmp" && find . -type f -printf '  %p (%s bytes, mode %m)\n' | sort)
    } >>"$log"
    if [ -f "$tmp/state/ninesql/logs/ninesql.log" ]; then
        cp "$tmp/state/ninesql/logs/ninesql.log" "$REPORT/gui-smoke-ninesql.log"
    fi

    # I1 (modest): the run makes no connection, so nothing secret should be on disk. Look for
    # obvious secret patterns in every file it wrote; list the files, never the matches.
    local q="[\"']" assign
    assign="(password|passwd|secret|token)${q}?[[:space:]]*[:=][[:space:]]*${q}?[^\"'[:space:],}]{4,}"
    hits="$(grep -rlaEi -e 'BEGIN [A-Z ]*PRIVATE KEY' -e 'nine_dev_only' -e "$assign" "$tmp" 2>/dev/null |
        sed "s|^$tmp/||" || true)"
    if [ -n "$hits" ]; then
        fails+=("I1 check: secret-like text in: $(paste -sd, - <<<"$hits")")
    else
        note gui-smoke "I1 check: no secret-like text in the files the run wrote."
    fi

    if [ "$KEEP_TEMP" = 1 ]; then note gui-smoke "Temporary directories kept (--keep-temp)."; fi
    cleanup_gui
    trap - INT TERM
    if [ "${#fails[@]}" -gt 0 ]; then
        reason gui-smoke "$(printf '%s; ' "${fails[@]}")"
        rc=1
    fi
    return "$rc"
}

# ---------------------------------------------------------------------------------------------
# Stage: scenarios
# ---------------------------------------------------------------------------------------------

E2E_TARGET_DIR="$ROOT/target/e2e-build"

# scenario_engines <scenario...>: the dev engine ids the scenarios' [[connection]] tables name
# (sqlite excepted), comma-separated, or nothing.
scenario_engines() {
    local sc
    for sc in "$@"; do
        sed -n 's/^[[:space:]]*engine[[:space:]]*=[[:space:]]*"\([a-z0-9]*\)".*/\1/p' "$SCENARIO_DIR/$sc.toml"
    done | { grep -vx sqlite || true; } | LC_ALL=C sort -u | paste -sd, -
}

# scenario_results <results.json>: "<outcome> <steps passed> <steps failed> <checkpoints
# reached> <checkpoints planned>" from the runner's results.json, then one line per step that
# did not pass ("- step <n> `<name>` (<kind>): <outcome>: <message>"), Markdown-safe.
scenario_results() {
    python3 -I - "$1" <<'EOF'
import json, re, sys
def clean(v, n=200):
    v = re.sub(r"[\x00-\x1f\x7f|`]", " ", str(v if v is not None else ""))
    return v[:n]
try:
    with open(sys.argv[1], encoding="utf-8") as fh:
        doc = json.load(fh)
    if not isinstance(doc, dict):
        raise ValueError("not an object")
except (OSError, ValueError):
    print("unreadable 0 0 0 0")
    sys.exit(0)
steps = [s for s in (doc.get("steps") or []) if isinstance(s, dict)]
passed = sum(1 for s in steps if s.get("outcome") == "pass")
failed = sum(1 for s in steps if s.get("outcome") in ("fail", "timeout"))
reached = len(doc.get("checkpoints") or [])
planned = len(doc.get("planned_checkpoints") or [])
print(clean(doc.get("outcome") or "unknown", 20).replace(" ", "-"), passed, failed, reached, planned)
for s in steps:
    if s.get("outcome") in ("fail", "timeout"):
        print(f"- step {clean(s.get('index'), 8)} `{clean(s.get('name'), 80)}` ({clean(s.get('kind'), 20)}): "
              f"{clean(s.get('outcome'), 20)}: {clean(s.get('message'))}")
if doc.get("message"):
    print(f"- message: {clean(doc.get('message'))}")
EOF
}

# proc_start <pid>: the start time of process <pid> (field 22 of /proc/<pid>/stat), or nothing.
proc_start() {
    local s f
    s="$(cat "/proc/$1/stat" 2>/dev/null)" || return 0
    s="${s##*) }"
    read -ra f <<<"$s"
    printf '%s' "${f[19]:-}"
}

# child_alive <pid> <start>: the process is still the one started (same start time), so a
# signal never reaches another process that reused the pid. With no start time, kill -0 only.
child_alive() {
    kill -0 "$1" 2>/dev/null || return 1
    [ -z "$2" ] && return 0
    [ "$(proc_start "$1")" = "$2" ]
}

stage_scenarios() {
    local log=$1 bin="$E2E_TARGET_DIR/release/ninesql" rc=0 v sc fails=() skipped=0 ran=0
    local engines plan kind a b rest busy=() owners started=0 shots_why="" runs_root fifo t0
    local run pid="" code line fd deadline left timed_out n name shot taken r_out r_line extra
    local dest outcome passed failed reached planned result errf start="" partial
    export COMPOSE_PROJECT_NAME="$COMPOSE_PROJECT"

    if [ "${#SCENARIO_LIST[@]}" -eq 0 ]; then
        reason scenarios "no scenario file in e2e/scenarios"
        return 1
    fi
    # The test window runs on X11 or XWayland only (ADR-0029 §2.7): WAYLAND_DISPLAY is unset for
    # the run, so an X server (DISPLAY) is needed.
    if [ -z "${DISPLAY:-}" ]; then
        echo "No X11 display: DISPLAY is unset (the scenario windows run on X11 or XWayland)." >>"$log"
        reason scenarios "no X11 display (DISPLAY unset; the scenarios run on X11 or XWayland only)"
        if [ "$DRY_RUN" = 1 ]; then
            say "    scenarios: no X11 display; with one it would run:"
        else
            return 77
        fi
    fi

    # Checkpoint screenshots: the test window only, by default, when the tools are there.
    if [ "$SCENARIO_SHOTS" = 0 ]; then
        shots_why="off (--no-screenshot)"
    elif ! have import; then
        shots_why="ImageMagick import is not installed"
    elif ! have xdotool && ! have wmctrl; then
        shots_why="neither xdotool nor wmctrl is installed"
    fi

    note scenarios "Scenarios: ${SCENARIO_LIST[*]}."
    cleanup_scenarios() {
        if [ -n "$pid" ] && child_alive "$pid" "$start"; then kill -KILL "$pid" 2>/dev/null || true; fi
        if [ "$started" = 1 ] && [ "$KEEP_ENGINES" = 0 ]; then
            started=0
            say "    scenarios: stopping the containers"
            run "$log" compose --profile all down || true
        fi
    }
    trap 'cleanup_scenarios; exit 130' INT TERM

    # 1. The test build, in its own target directory: target/release/ninesql is never replaced.
    if [ -n "${E2E_TEST_SCENARIO_BIN:-}" ]; then
        bin="$E2E_TEST_SCENARIO_BIN"
        echo "Test hook: E2E_TEST_SCENARIO_BIN, no build" >>"$log"
    else
        say "    scenarios: building the e2e-scenarios test binary (target/e2e-build)"
        run "$log" env CARGO_TARGET_DIR="$E2E_TARGET_DIR" timeout "$CARGO_TIMEOUT" \
            cargo build --release --locked -p ninesql-ui --bin ninesql --features e2e-scenarios || {
            reason scenarios "the e2e-scenarios test build failed"
            cleanup_scenarios
            return 1
        }
    fi
    if [ "$DRY_RUN" = 0 ]; then
        v="$("$bin" --version 2>&1 </dev/null | head -n 1 || true)"
        echo "test build --version: $v" >>"$log"
        case "$v" in
            *"e2e-scenarios test build"*) note scenarios "Test build: \`$v\`." ;;
            *) reason scenarios "the binary built with --features e2e-scenarios does not say it is a test build"
               cleanup_scenarios
               return 1 ;;
        esac
    else
        say "    [dry-run] $bin --version   (must say: e2e-scenarios test build, not for distribution)"
    fi

    # 2. The dev engines the scenarios name (sqlite needs none).
    engines="$(scenario_engines "${SCENARIO_LIST[@]}")"
    if [ -z "$engines" ]; then
        note scenarios "No dev engine needed."
    elif [ "$ENGINES_RUNNING" = 1 ]; then
        note scenarios "Dev engines \`$engines\`: --engines-running, nothing started (a scenario whose engine does not answer is skipped)."
    elif [ "$DRY_RUN" = 0 ] && { ! have docker || ! timeout 20 docker info >/dev/null 2>&1; }; then
        note scenarios "Docker is not available: the scenarios that need \`$engines\` are skipped."
    else
        plan="$(engine_plan "$engines")" || plan=""
        printf 'Engine plan:\n%s\n' "$plan" >>"$log"
        if [ "$DRY_RUN" = 0 ]; then
            while read -r kind a; do
                [ "$kind" = PORT ] && port_busy "$a" </dev/null && busy+=("$a")
            done <<<"$plan"
        fi
        if [ "${#busy[@]}" -gt 0 ]; then
            owners="$(running_compose_projects "$plan")"
            if [ "$owners" = "$COMPOSE_PROJECT_NAME" ]; then
                # Left up by the engines stage (--keep-engines) or an earlier run of this script.
                note scenarios "Dev engines \`$engines\`: already up in compose project \`$COMPOSE_PROJECT_NAME\`, reused."
            else
                reason scenarios "host ports already in use (${busy[*]}) by another compose project: stop it (make dev-down) or rerun with --engines-running"
                cleanup_scenarios
                return 1
            fi
        else
            started=1
            while read -r kind a b rest; do
                [ "$kind" = UP ] || continue
                say "    scenarios: starting $b"
                # shellcheck disable=SC2086 # $rest is a list of service names
                run "$log" timeout "$DEV_TIMEOUT" docker compose -f dev/compose.yaml --profile "$a" pull --quiet $rest || rc=$?
                [ "$rc" = 0 ] || break
                run "$log" python3 dev/ready.py up "$a" --engines "$b" --timeout "$DEV_TIMEOUT" || rc=$?
                [ "$rc" = 0 ] || break
            done <<<"$plan"
            if [ "$rc" != 0 ]; then
                fails+=("the dev engines did not start (exit $rc), so their scenarios were skipped")
                rc=0
            fi
            note scenarios "Dev engines \`$engines\`: started in compose project \`$COMPOSE_PROJECT_NAME\` for this stage."
        fi
    fi

    # 3. Each scenario in a fresh 0700 run directory, target/e2e-runs/<time>/<scenario>, with
    # HOME and the four XDG directories inside it (the runner refuses anything else).
    runs_root="$OUT_BASE/e2e-runs/$STAMP"
    note scenarios ""
    note scenarios "| Scenario | Result | Exit | Steps passed | Steps failed | Checkpoints reached / planned | Screenshots | Duration |"
    note scenarios "|---|---|---|---|---|---|---|---|"
    for sc in "${SCENARIO_LIST[@]}"; do
        dest="$REPORT/scenarios/$sc"
        run="$runs_root/$sc"
        if [ "$DRY_RUN" = 1 ]; then
            say "    [dry-run] mkdir -m 0700 $run/{home,config,data,state,cache,out}"
            say "    [dry-run] env -u WAYLAND_DISPLAY HOME=$run/home XDG_CONFIG_HOME=… XDG_DATA_HOME=… XDG_STATE_HOME=… XDG_CACHE_HOME=… NINESQL_LOG=ninesql=debug,info $bin --e2e-run $SCENARIO_DIR/$sc.toml --e2e-out $run/out   (limit ${SCENARIO_TIMEOUT} s)"
            if [ -z "$shots_why" ]; then
                say "    [dry-run]   at each 'ninesql-e2e checkpoint <n> <name>' line: import -window <window of the launched pid> $dest/checkpoint-<n>-<name>.png"
            else
                say "    [dry-run]   no checkpoint screenshots: $shots_why"
            fi
            continue
        fi
        say "    scenarios: $sc (a window opens; leave it alone)"
        mkdir -p "$runs_root" "$dest"
        chmod 0700 "$OUT_BASE/e2e-runs" "$runs_root"
        rm -rf "$run"
        mkdir -m 0700 "$run"
        mkdir -m 0700 "$run/home" "$run/config" "$run/data" "$run/state" "$run/cache" "$run/out"
        # The pipe of the runner's standard output and the file of its standard error lie outside
        # the run directory, which the runner's I1 scan reads whole. The standard error reaches
        # the report only after the run, and only when the runner found no secret (exit 3).
        fifo="$runs_root/.$sc.stdout"
        errf="$runs_root/.$sc.stderr"
        rm -f "$fifo" "$errf"
        mkfifo -m 0600 "$fifo"
        t0=$SECONDS
        timed_out=0
        taken=0
        # WAYLAND_DISPLAY unset: GPUI uses its X11 backend (XWayland on a Wayland desktop), so the
        # window can be captured alone (ADR-0029 §2.7). NINESQL_LOG, never trace (AGENTS.md).
        # env execs the binary, so $! is the pid of ninesql itself (the screenshot looks it up).
        env -u WAYLAND_DISPLAY HOME="$run/home" XDG_CONFIG_HOME="$run/config" XDG_DATA_HOME="$run/data" \
            XDG_STATE_HOME="$run/state" XDG_CACHE_HOME="$run/cache" NINESQL_LOG="ninesql=debug,info" \
            NINESQL_E2E_COMMIT="$(git rev-parse HEAD 2>/dev/null || echo unknown)" \
            "$bin" --e2e-run "$SCENARIO_DIR/$sc.toml" --e2e-out "$run/out" \
            </dev/null >"$fifo" 2>"$errf" &
        pid=$!
        start="$(proc_start "$pid")"
        # Opened read-write, so this open never blocks, even when the runner dies before it
        # opens its end; the end of its output is therefore its exit, not an end of file.
        exec {fd}<>"$fifo"
        : >"$dest/stdout.log"
        deadline=$((SECONDS + SCENARIO_TIMEOUT))
        partial=""
        while :; do
            left=$((deadline - SECONDS))
            if [ "$left" -le 0 ]; then timed_out=1; break; fi
            line=""
            r_line=0
            IFS= read -r -t 1 -u "$fd" line || r_line=$?
            if [ "$r_line" != 0 ]; then
                # A timeout keeps any partial line; once the runner has exited, a timeout means
                # the pipe is drained. Any other status is a read error.
                partial+="$line"
                if [ "$r_line" -gt 128 ] && child_alive "$pid" "$start"; then continue; fi
                break
            fi
            line="$partial$line"
            partial=""
            printf '%s\n' "$line" >>"$dest/stdout.log"
            # A checkpoint: the runner holds the UI still for checkpoint_hold_ms (1 500 ms by
            # default) after this line; capture the launched process's window within it.
            if [[ "$line" =~ ^ninesql-e2e\ checkpoint\ ([0-9]{1,4})\ ([a-z0-9-]{1,64})$ ]]; then
                n="${BASH_REMATCH[1]}"
                name="${BASH_REMATCH[2]}"
                [ -z "$shots_why" ] || continue
                shot="$dest/checkpoint-$(printf '%02d' "$((10#$n))")-$name.png"
                if screenshot_window "$shot" "$pid" 0.2; then
                    taken=$((taken + 1))
                else
                    rm -f "$shot"
                    printf 'checkpoint %s %s: no screenshot: %s\n' "$n" "$name" "$SHOT_WHY" >>"$log"
                fi
            fi
        done
        exec {fd}<&-
        if [ -n "$partial" ]; then printf '%s\n' "$partial" >>"$dest/stdout.log"; fi
        # The process should be ending; it still gets only the time left.
        while [ "$timed_out" = 0 ] && child_alive "$pid" "$start"; do
            [ "$SECONDS" -lt "$deadline" ] || timed_out=1
            [ "$timed_out" = 1 ] || sleep 0.2
        done
        if [ "$timed_out" = 1 ]; then
            if child_alive "$pid" "$start"; then kill -TERM "$pid" 2>/dev/null || true; fi
            for ((n = 0; n < 10; n++)); do
                child_alive "$pid" "$start" || break
                sleep 1
            done
            if child_alive "$pid" "$start"; then kill -KILL "$pid" 2>/dev/null || true; fi
        fi
        wait "$pid" 2>/dev/null && code=0 || code=$?
        pid=""
        rm -f "$fifo"
        ran=$((ran + 1))

        # Results and the run's file log.
        if [ -f "$run/out/results.json" ] && [ ! -L "$run/out/results.json" ]; then
            cp "$run/out/results.json" "$dest/results.json"
        fi
        if [ "$code" = 3 ]; then
            # The runner's I1 scan found a password: neither its standard error nor the run's
            # file log reaches the report, and the marker stops any archive of the report, now
            # and in any later --check-report run (checked at the end of this script).
            printf '%s\n' "$STDERR_WITHHELD" >"$dest/stderr.log"
            printf '%s\n' "A scenario run ($sc) found a dev password or its master password in a file it wrote (runner exit 3). scripts/e2e-local.sh never archives this report, with --check-report and --no-secret-scan too." >"$SECRET_MARKER"
        else
            if [ -f "$errf" ] && [ ! -L "$errf" ]; then cat "$errf" >"$dest/stderr.log"; else : >"$dest/stderr.log"; fi
            if [ -f "$run/state/ninesql/logs/ninesql.log" ] && [ ! -L "$run/state/ninesql/logs/ninesql.log" ]; then
                cp "$run/state/ninesql/logs/ninesql.log" "$dest/ninesql.log"
            fi
        fi
        if [ "$KEEP_TEMP" = 0 ]; then rm -f "$errf"; fi
        if [ -f "$dest/results.json" ]; then
            r_out="$(scenario_results "$dest/results.json")"
        else
            r_out="none 0 0 0 0"
        fi
        read -r outcome passed failed reached planned <<<"$(head -n 1 <<<"$r_out")"
        extra="$(tail -n +2 <<<"$r_out")"
        case "$code" in
            0) result=PASS ;;
            1) result=FAIL ;;
            2) result="FAIL (refused)" ;;
            3) result="FAIL (SECRET FOUND)" ;;
            4) result="FAIL (setup error)" ;;
            5) result=SKIP ;;
            *) result="FAIL (exit $code)" ;;
        esac
        [ "$timed_out" = 1 ] && result="FAIL (time limit ${SCENARIO_TIMEOUT} s)"
        if grep -qE 'internal error \(panic\)|panicked at' "$dest/stderr.log" 2>/dev/null && [ "$result" = PASS ]; then
            result="FAIL (panic on stderr)"
        fi
        case "$result" in
            FAIL*) fails+=("$sc: $result") ;;
            SKIP) skipped=$((skipped + 1)) ;;
        esac
        if [ "$code" = 3 ]; then
            say ""
            say "    !!! scenarios: $sc: the runner found a dev password or the run's master password in a file it wrote (I1)."
            say "    !!! Its standard error and the run's file log are NOT copied into the report, and no archive"
            say "    !!! of this report will be written, now or by a later --check-report (scenarios/SECRET-FOUND)."
            say "    !!! The file names (never the values) are under i1_scan in $dest/results.json."
            say ""
            note scenarios "**$sc: SECRET FOUND** (exit 3): a dev password or the run's master password appeared in a file of the run directory (I1). The file names are under \`i1_scan\` in \`scenarios/$sc/results.json\`; the runner's standard error and the file log were not copied, and this report is never archived (\`scenarios/SECRET-FOUND\`, also with \`--check-report\` and \`--no-secret-scan\`). Report it as a bug."
        fi
        [ -n "$shots_why" ] && taken="none: $shots_why"
        note scenarios "| $sc | $result | $code | $passed | $failed | $reached / $planned | $taken | $(fmt_dur $((SECONDS - t0))) |"
        printf 'scenario %s: %s (exit %s; steps passed %s, failed %s; checkpoints %s / %s; screenshots %s)\n' \
            "$sc" "$result" "$code" "$passed" "$failed" "$reached" "$planned" "$taken" >>"$log"
        if [ -n "$extra" ] && [ "$result" != PASS ]; then
            {
                printf -- '- **%s** (%s):\n' "$sc" "$outcome"
                while IFS= read -r line; do printf '  %s\n' "$line"; done <<<"$extra"
            } >>"$NOTES/scenarios.failures"
        fi
        if [ -s "$dest/stderr.log" ] && [ ! -f "$dest/results.json" ]; then
            printf -- '- **%s**: stderr: %s\n' "$sc" "$(head -c 300 "$dest/stderr.log" | tr -d '\000-\037|`')" >>"$NOTES/scenarios.failures"
        fi
        if [ "$KEEP_TEMP" = 0 ]; then rm -rf "$run"; fi
    done
    if [ "$DRY_RUN" = 0 ]; then
        if [ -f "$NOTES/scenarios.failures" ]; then
            note scenarios ""
            note scenarios "Steps that did not pass, and runner messages:"
            note scenarios ""
            cat "$NOTES/scenarios.failures" >>"$NOTES/scenarios.md"
            rm -f "$NOTES/scenarios.failures"
        fi
        note scenarios ""
        note scenarios "Per scenario, in \`scenarios/<name>/\`: \`results.json\` (the runner's results), \`stdout.log\` (the checkpoint lines), \`stderr.log\`, \`ninesql.log\` (the file log, \`NINESQL_LOG=ninesql=debug,info\`) and \`checkpoint-<n>-<name>.png\` (the test window only, taken during the runner's 1 500 ms hold; a slow capture can show the state just after the checkpoint). Runner exit statuses: 0 pass, 1 a step failed, 2 refused, 3 secret found, 4 setup error, 5 skipped (a dev engine did not answer)."
        if [ "$KEEP_TEMP" = 1 ]; then
            note scenarios "Run directories kept (--keep-temp): \`target/e2e-runs/$STAMP/\`."
        else
            rmdir "$runs_root" 2>/dev/null || true
        fi
    fi
    cleanup_scenarios
    trap - INT TERM
    if [ "$DRY_RUN" = 1 ]; then
        [ -n "${DISPLAY:-}" ] || return 77
        return 0
    fi
    if [ "${#fails[@]}" -gt 0 ]; then
        reason scenarios "$(printf '%s; ' "${fails[@]}")"
        return 1
    fi
    if [ "$ran" -gt 0 ] && [ "$skipped" = "$ran" ]; then
        reason scenarios "every scenario was skipped (a dev engine did not answer)"
        return 77
    fi
    return 0
}

# ---------------------------------------------------------------------------------------------
# Stage: deb
# ---------------------------------------------------------------------------------------------

stage_deb() {
    local log=$1 debs=()
    say "    deb: make deb (Ubuntu 22.04 builder; long)"
    run "$log" make --no-print-directory deb "TIMEOUT=$CARGO_TIMEOUT" || {
        reason deb "make deb failed (see packaging/README.md for cargo-deb 3.8.0 and cargo-about 0.9.3)"
        return 1
    }
    if [ "$DRY_RUN" = 0 ]; then
        shopt -s nullglob
        debs=("$ROOT"/target/debian/ninesql_*_amd64.deb)
        shopt -u nullglob
        if [ "${#debs[@]}" -eq 0 ]; then
            reason deb "make deb wrote no target/debian/ninesql_*_amd64.deb"
            return 1
        fi
        note deb "Package: \`$(basename "${debs[-1]}")\`, sha256 \`$(sha256sum "${debs[-1]}" | cut -c1-64)\`."
    fi
    say "    deb: install tests (Debian 12 / 13, Ubuntu 22.04 / 24.04)"
    run "$log" make --no-print-directory deb-install-test || {
        reason deb "the install tests failed (a BLOCKED distribution means its archive was unreachable)"
        return 1
    }
}

# ---------------------------------------------------------------------------------------------
# Run the stages
# ---------------------------------------------------------------------------------------------

declare -A RESULT EXIT DURATION LOGFILE
INTERRUPTED=0
trap 'INTERRUPTED=1' INT

say "NineSQL local end-to-end run: ${STAGES[*]}"
say "Report: $REPORT"
[ "$DRY_RUN" = 1 ] && say "(dry run: commands are printed, not run)"

n=0
stop=0
for stage in "${STAGES[@]}"; do
    n=$((n + 1))
    logfile="$REPORT/$(printf '%02d' "$n")-$stage.log"
    LOGFILE[$stage]="$(basename "$logfile")"
    if [ "$stop" = 1 ]; then
        RESULT[$stage]="NOT RUN"; EXIT[$stage]="-"; DURATION[$stage]="-"
        continue
    fi
    say "[$stage] running (log: ${LOGFILE[$stage]})"
    printf '# stage %s, started %s\n' "$stage" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"$logfile"
    t0=$SECONDS
    set +e
    ( set +e; "stage_${stage//-/_}" "$logfile" )
    rc=$?
    set -e
    DURATION[$stage]=$((SECONDS - t0))
    EXIT[$stage]=$rc
    case "$rc" in
        0) if [ "$DRY_RUN" = 1 ] && [ "$stage" != env ]; then RESULT[$stage]=DRY-RUN; else RESULT[$stage]=PASS; fi ;;
        77) RESULT[$stage]=SKIP ;;
        130) RESULT[$stage]=INTERRUPTED; INTERRUPTED=1 ;;
        *) RESULT[$stage]=FAIL ;;
    esac
    printf '# stage %s: %s (exit %s, %s)\n' "$stage" "${RESULT[$stage]}" "$rc" "$(fmt_dur "${DURATION[$stage]}")" >>"$logfile"
    line="[$stage] ${RESULT[$stage]} ($(fmt_dur "${DURATION[$stage]}"))"
    [ -f "$NOTES/$stage.reason" ] && [ "$rc" != 0 ] && line+=": $(cat "$NOTES/$stage.reason")"
    say "$line"
    if [ "$INTERRUPTED" = 1 ] || { [ "$FAIL_FAST" = 1 ] && [ "${RESULT[$stage]}" = FAIL ]; }; then
        stop=1
    fi
done
trap - INT

# ---------------------------------------------------------------------------------------------
# Scrub, summary, secret scan, archive
# ---------------------------------------------------------------------------------------------

# Python helpers shared by the scrubber and the secret checks (prepended to their scripts).
# report_files: the text files of the report, regular files only: a symbolic link (to a file
# or a directory) or any other non-regular entry is never read nor written, and os.walk does
# not descend into linked directories. replace_text: writes a new 0600 file next to the old
# one and renames it over, so a hard link is broken, never written through.
PY_REPORT_FILES='
import os, stat, tempfile
def report_files(report):
    for dirpath, dirnames, files in os.walk(report):
        dirnames.sort()
        for f in sorted(files):
            if f.endswith((".png", ".gz")):
                continue
            p = os.path.join(dirpath, f)
            try:
                st = os.lstat(p)
            except OSError:
                continue
            if stat.S_ISREG(st.st_mode):
                yield p
def read_text(p, errors="surrogateescape"):
    fd = os.open(p, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    if not stat.S_ISREG(os.fstat(fd).st_mode):
        os.close(fd)
        raise OSError("not a regular file")
    with os.fdopen(fd, encoding="utf-8", errors=errors) as fh:
        return fh.read()
def replace_text(p, text):
    fd, tmp = tempfile.mkstemp(dir=os.path.dirname(p), prefix=".scrub-")
    try:
        with os.fdopen(fd, "w", encoding="utf-8", errors="surrogateescape") as fh:
            fh.write(text)
        os.replace(tmp, p)
    except BaseException:
        os.unlink(tmp)
        raise
'

# Replaces the repository path with <repo>, $HOME with ~, the user name with <user>, the host
# name with <host>, IPv4 addresses (127.x and 0.x kept) and IPv6 global, unique local and
# link-local addresses with <ip>, and MAC addresses with <mac>, in every text file of the
# report.
scrub() {
    local user host fqdn
    user="$(id -un 2>/dev/null || echo "")"
    host="$(hostname 2>/dev/null || cat /proc/sys/kernel/hostname 2>/dev/null || echo "")"
    fqdn="$(hostname -f 2>/dev/null || echo "")"
    { printf '%s\n' "$PY_REPORT_FILES"; cat; } <<'EOF' | python3 -I - "$REPORT" "$ROOT" "${HOME:-}" "$user" "$host" "$fqdn"
import os, re, sys
report, repo, home, user, host, fqdn = sys.argv[1:7]
# Names that identify nobody; replacing them would garble the logs (root:root, mysql root…).
generic = {"", "root", "user", "admin", "runner", "ubuntu", "debian", "nobody", "dev", "test", "localhost"}
pairs = []
for path, token in ((repo, "<repo>"), (os.path.realpath(repo), "<repo>"), (home, "~"), (os.path.realpath(home) if home else "", "~")):
    if path and path != "/":
        pairs.append((re.compile(re.escape(path.rstrip("/")) + r"(?=/|\b|$)"), token))
for name, token in ((fqdn, "<host>"), (host, "<host>"), (user, "<user>")):
    if name and name.lower() not in generic and len(name) >= 2:
        pairs.append((re.compile(r"(?<![A-Za-z0-9_.-])" + re.escape(name) + r"(?![A-Za-z0-9_-])"), token))
import ipaddress
octet = r"(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)"
ipv4 = re.compile(r"(?<![\w.])" + octet + r"(?:\." + octet + r"){3}(?!\w|\.\d)")
# Only the vulkaninfo fields the env stage keeps, exactly as it writes them ("apiVersion = ");
# any other four-part number, "version: 10.1.2.3" included, becomes <ip>. A dotted number
# with a component above 255 never matches ipv4, so it stays as it is.
version_label = re.compile(r"\b(?:conformanceVersion|apiVersion|driverVersion) = $")
def sub_ipv4(m):
    first = m.group(0).split(".")[0]
    # Loopback and 0.x stay readable; so does a vulkaninfo version ("conformanceVersion = 1.4.0.0").
    if first in ("127", "0") or version_label.search(m.string, max(0, m.start() - 24), m.start()):
        return m.group(0)
    return "<ip>"
# Candidates: hex groups with at least two colons and an optional zone (%eth0); each is checked
# with ipaddress and replaced only when global, unique local or link-local, so Rust paths
# (std::io), times (12:34:56) and ::1 are left alone.
ipv6 = re.compile(r"(?<![\w:.%])[0-9A-Fa-f]{0,4}(?::[0-9A-Fa-f]{0,4}){2,7}(?:%[\w.-]+)?(?![\w:])")
nets = [ipaddress.ip_network(n) for n in ("2000::/3", "fc00::/7", "fe80::/10")]
def sub_ipv6(m):
    text = m.group(0)
    try:
        ip = ipaddress.IPv6Address(text.partition("%")[0])
    except ValueError:
        return text
    return "<ip>" if any(ip in n for n in nets) else text
mac = re.compile(r"(?<![\w:-])[0-9A-Fa-f]{2}([:-])[0-9A-Fa-f]{2}(?:\1[0-9A-Fa-f]{2}){4}(?![\w:-])")
for p in report_files(report):
    try:
        data = read_text(p)
    except OSError:
        continue
    new = data
    for rx, token in pairs:
        new = rx.sub(token, new)
    new = mac.sub("<mac>", new)
    new = ipv6.sub(sub_ipv6, new)
    new = ipv4.sub(sub_ipv4, new)
    if new != data:
        replace_text(p, new)
EOF
}

# redact_dev_passwords: replaces every distinct password of dev/engines.json (its "password"
# fields and the "password: <value>" of its notes) with <dev-password> in the report's text
# files; prints the number of files changed.
redact_dev_passwords() {
    { printf '%s\n' "$PY_REPORT_FILES"; cat; } <<'EOF' | python3 -I - "$REPORT" "$ROOT/dev/engines.json"
import json, os, re, sys
report, engines = sys.argv[1:3]
found = set()
def walk(v):
    if isinstance(v, dict):
        for k, x in v.items():
            if k == "password" and isinstance(x, str) and x:
                found.add(x)
            walk(x)
    elif isinstance(v, list):
        for x in v:
            walk(x)
    elif isinstance(v, str):
        for m in re.finditer(r"password:\s*([^\s,;]+)", v):
            found.add(m.group(1).rstrip("."))
walk(json.load(open(engines, encoding="utf-8")))
words = sorted((w for w in found if len(w) >= 4), key=len, reverse=True)
if not words:
    sys.exit("no password found in dev/engines.json")
rx = re.compile("|".join(re.escape(w) for w in words))
changed = 0
for p in report_files(report):
    data = read_text(p)
    new = rx.sub("<dev-password>", data)
    if new != data:
        replace_text(p, new)
        changed += 1
print(changed)
EOF
}

# credential_uris: prints "<file>:<line>" (never the match) for every URI with a user and a
# password left in the report's text files; redacted dev passwords do not count. The scheme is
# matched in any case (HTTP://), the user may be empty (redis://:password@host), and the
# password runs to the next "@" on the same line, spaces included, but not across another
# "://" (so "http://host:80/ then pg://…" is not read as one password).
credential_uris() {
    { printf '%s\n' "$PY_REPORT_FILES"; cat; } <<'EOF' | python3 -I - "$REPORT"
import os, re, sys
report = sys.argv[1]
rx = re.compile(r"(?i)[a-z][a-z0-9+.-]*://[^/:@\s]*:((?:(?!://)[^@\r\n])+)@")
for p in report_files(report):
    for n, line in enumerate(read_text(p, errors="replace").split("\n"), 1):
        if any(m.group(1) != "<dev-password>" for m in rx.finditer(line)):
            print(f"{os.path.relpath(p, report)}:{n}")
EOF
}

relevant_tail() { # <log>: error-looking lines, then the last 40 lines, without build noise
    local log=$1
    if grep -qE '^(error|FAIL|failed|thread .* panicked|ERROR)' "$log" 2>/dev/null; then
        echo "First error lines:"
        grep -E '^(error|FAIL|failed|thread .* panicked|ERROR)' "$log" | head -n 15
        echo "..."
        echo "Last 40 lines:"
    fi
    grep -vE '^[[:space:]]*(Compiling|Checking|Downloaded|Downloading|Fresh|Locking|Adding|Updating) ' "$log" | tail -n 40
}

write_summary() {
    local branch dirty changed total_s nfail=0 stage r
    branch="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)"
    changed="$(git status --porcelain 2>/dev/null | wc -l | tr -d ' ')"
    if [ "$changed" = 0 ]; then dirty="clean"; else dirty="dirty ($changed changed or untracked paths)"; fi
    total_s=$(( $(date -u +%s) - STARTED_EPOCH ))
    for stage in "${STAGES[@]}"; do
        case "${RESULT[$stage]}" in FAIL|INTERRUPTED) nfail=$((nfail + 1)) ;; esac
    done
    {
        echo "$REPORT_TITLE"
        echo
        echo "- Result: **$([ "$nfail" = 0 ] && echo PASS || echo "FAIL ($nfail stage(s))")**$([ "$DRY_RUN" = 1 ] && echo " (dry run: nothing was executed)")"
        echo "- Commit: \`$(git rev-parse HEAD 2>/dev/null || echo unknown)\` on \`$branch\`, working tree $dirty"
        echo "- Started: $STARTED_UTC, took $(fmt_dur "$total_s")"
        echo "- Command: \`scripts/e2e-local.sh${ORIG_ARGS:+ $ORIG_ARGS}\`"
        echo
        echo "## Stages"
        echo
        echo "| Stage | Result | Exit | Duration | Log |"
        echo "|---|---|---|---|---|"
        for stage in "${STAGES[@]}"; do
            r="${RESULT[$stage]}"
            local d="${DURATION[$stage]}"
            [ "$d" != "-" ] && d="$(fmt_dur "$d")"
            echo "| $stage | $r | ${EXIT[$stage]} | $d | \`${LOGFILE[$stage]}\` |"
        done

        if [ "$nfail" != 0 ]; then
            echo
            echo "## Failures"
            for stage in "${STAGES[@]}"; do
                case "${RESULT[$stage]}" in FAIL|INTERRUPTED) ;; *) continue ;; esac
                echo
                echo "### $stage"
                echo
                [ -f "$NOTES/$stage.reason" ] && echo "Reason: $(cat "$NOTES/$stage.reason")" && echo
                if [ "$stage" = static ] && [ -f "$NOTES/static.failed" ]; then
                    while read -r step; do
                        echo "#### static: $step (\`static-$step.log\`)"
                        echo
                        echo '```text'
                        relevant_tail "$REPORT/static-$step.log"
                        echo '```'
                        echo
                    done <"$NOTES/static.failed"
                else
                    echo "Last relevant lines of \`${LOGFILE[$stage]}\`:"
                    echo
                    echo '```text'
                    relevant_tail "$REPORT/${LOGFILE[$stage]}"
                    echo '```'
                fi
            done
        fi

        [ "${#STAGES[@]}" -gt 1 ] || [ "${STAGES[0]}" != env ] && echo && echo "## Stage details"
        for stage in "${STAGES[@]}"; do
            [ "$stage" = env ] && continue
            echo
            echo "### $stage: ${RESULT[$stage]}"
            echo
            if [ "${RESULT[$stage]}" = SKIP ] && [ -f "$NOTES/$stage.reason" ]; then
                echo "Skipped: $(cat "$NOTES/$stage.reason")"
                echo
            fi
            [ -f "$NOTES/$stage.md" ] && cat "$NOTES/$stage.md"
            [ -f "$NOTES/$stage.md" ] || echo "No details."
        done

        echo
        echo "## Environment"
        echo
        if [ -f "$NOTES/env.md" ]; then cat "$NOTES/env.md"; else echo "Not collected (env stage skipped)."; fi
        echo
        echo "---"
        echo "Generated by scripts/e2e-local.sh. Paths are shown relative to <repo> or ~; the user name, host name, IP addresses (loopback kept) and MAC addresses are replaced in text files. Screenshots (gui-smoke-window.png with --screenshot, scenarios/*/checkpoint-*.png) are part of the archive and are not scrubbed: they show the NineSQL test window only, with dev data, but a popup over the window can appear in them."
    } >"$SUMMARY"
    return "$nfail"
}

if [ -z "$CHECK_REPORT" ]; then
    scrub
    set +e
    write_summary
    failures=$?
    set -e
else
    failures=0
fi
scrub
SCENARIO_SECRET_FOUND=0
if [ -e "$SECRET_MARKER" ] || [ -L "$SECRET_MARKER" ] || grep -qF 'FAIL (SECRET FOUND)' "$SUMMARY" 2>/dev/null; then
    SCENARIO_SECRET_FOUND=1
fi
rm -rf "$NOTES"

# Secret checks of the report, then the archive. The archive is written only when every check
# passed and gitleaks scanned the report clean, or with --no-secret-scan (fail closed).
archive_ok=1
scan_notes=()
if [ "$SCENARIO_SECRET_FOUND" = 1 ]; then
    archive_ok=0
    failures=$((failures + 1))
    scan_notes+=("a scenario run found a dev password or its master password in a file it wrote (runner exit 3, \`scenarios/SECRET-FOUND\`): this report is never archived, whatever the other checks say and with \`--no-secret-scan\` too")
    say ""
    say "FAIL: a scenario run found a password in a file it wrote (scenarios/SECRET-FOUND, and the scenarios"
    say "      section of summary.md): this report is never archived, --check-report and --no-secret-scan included."
    say "      Report it as a bug; do not send this report."
fi
if ! redacted="$(redact_dev_passwords)"; then
    redacted=0
    archive_ok=0
    failures=$((failures + 1))
    scan_notes+=("the dev-password redaction failed: no archive")
fi
if [ "$redacted" != 0 ]; then
    scan_notes+=("dev passwords of dev/engines.json found in $redacted file(s) and replaced with \`<dev-password>\`")
fi
if ! uris="$(credential_uris)"; then
    uris="(the URI check failed)"
fi
if [ -n "$uris" ]; then
    archive_ok=0
    failures=$((failures + 1))
    printf '%s\n' "$uris" >"$REPORT/credential-uris.txt"
    scan_notes+=("URIs with a user and a password found on $(grep -c . <<<"$uris") line(s), listed in \`credential-uris.txt\` without the values: no archive")
    say ""
    say "FAIL: the report holds URIs with a user and a password; no archive was written."
    say "      Lines (file:line, values not shown): $REPORT/credential-uris.txt"
fi
if [ "$NO_SECRET_SCAN" = 1 ]; then
    scan_notes+=("gitleaks scan skipped (--no-secret-scan)")
    say "WARNING: --no-secret-scan: the report was scrubbed but not scanned by gitleaks; read it before sending it."
elif have gitleaks; then
    gl_rc=0
    gitleaks dir "$REPORT" --redact --no-banner --exit-code 42 >"$REPORT/gitleaks.log" 2>&1 || gl_rc=$?
    if [ "$gl_rc" != 0 ] && [ "$gl_rc" != 42 ]; then
        gl_rc=0
        gitleaks detect --no-git --source "$REPORT" --redact --no-banner --exit-code 42 >"$REPORT/gitleaks.log" 2>&1 || gl_rc=$?
    fi
    if [ "$gl_rc" = 0 ]; then
        scan_notes+=("gitleaks: no finding")
    elif [ "$gl_rc" = 42 ]; then
        archive_ok=0
        failures=$((failures + 1))
        scan_notes+=("gitleaks found secret-like text (\`gitleaks.log\`, redacted): no archive")
        say ""
        say "FAIL: gitleaks found secret-like text in the report; no archive was written."
        say "      Findings (redacted): $REPORT/gitleaks.log"
        say "      Remove or fix the lines it names, or send only the parts that are safe."
    else
        archive_ok=0
        failures=$((failures + 1))
        scan_notes+=("gitleaks could not scan the report (exit $gl_rc): no archive")
        say ""
        say "FAIL: gitleaks could not scan the report (exit $gl_rc, see gitleaks.log); no archive was written."
    fi
else
    archive_ok=0
    scan_notes+=("gitleaks is not installed: no archive (install gitleaks, or rerun with --no-secret-scan)")
    say ""
    say "No archive: gitleaks is not installed, so the report was not scanned for secrets."
    say "      Install gitleaks and rerun, or rerun with --no-secret-scan once you have read the report."
fi
{
    echo
    echo "## Secret checks"
    echo
    printf -- '- %s\n' "${scan_notes[@]}"
    if [ "$archive_ok" = 1 ]; then echo "- Archive: \`report.tar.gz\`"; else echo "- Archive: not written"; fi
} >>"$SUMMARY"

if [ "$archive_ok" = 1 ]; then
    # A new file (mktemp, O_EXCL) next to the report, so a planted name cannot redirect tar.
    tmp_archive="$(mktemp "$(dirname "$REPORT")/.$(basename "$REPORT").tar.gz.XXXXXX")"
    tar -czf "$tmp_archive" --exclude="$(basename "$REPORT")/report.tar.gz" \
        -C "$(dirname "$REPORT")" "$(basename "$REPORT")"
    mv -fT "$tmp_archive" "$ARCHIVE"
fi

say ""
say "Summary: $SUMMARY"
[ "$archive_ok" = 1 ] && say "Archive: $ARCHIVE"
[ -f "$REPORT/gui-smoke-window.png" ] && say "Note: the report holds a screenshot of the NineSQL window (gui-smoke-window.png); check it before sending."
if [ -d "$REPORT/scenarios" ] && [ -n "$(find "$REPORT/scenarios" -name '*.png' -print -quit 2>/dev/null)" ]; then
    say "Note: the report holds checkpoint screenshots of the test windows (scenarios/*/*.png): dev data only, but not scrubbed; a popup over the window can show in them."
fi
# Only in a report written by an older version of this script, which could fall back to it.
[ -f "$REPORT/gui-smoke-screen.png" ] && say "Note: the report holds a screenshot of your WHOLE screen (gui-smoke-screen.png); check it, or delete it."
if [ "$archive_ok" = 1 ]; then
    say "Next: send summary.md (or the archive) back to the session."
else
    say "Do not send anything before fixing the lines listed above."
fi
[ "$failures" = 0 ]
