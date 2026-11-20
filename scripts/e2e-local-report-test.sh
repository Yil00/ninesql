#!/usr/bin/env bash
# Test of the report scrubber and secret checks of scripts/e2e-local.sh, on fake report
# directories (scripts/e2e-local.sh --check-report), and of its scenarios stage with a fake
# scenario runner. Needs no display, Docker or cargo.
# Usage: scripts/e2e-local-report-test.sh
set -euo pipefail
umask 077

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
E2E="$ROOT/scripts/e2e-local.sh"
WORK="$(mktemp -d -t ninesql-e2e-report-test-XXXXXX)"
trap 'rm -rf "$WORK"' EXIT
FAILS=0

fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
ok() { echo "ok:   $*"; }

# A PATH with no gitleaks, and one with a fake gitleaks that exits with $FAKE_GITLEAKS_RC.
mkdir "$WORK/bin-none" "$WORK/bin-fake"
cat >"$WORK/bin-fake/gitleaks" <<'EOF'
#!/bin/sh
echo "fake gitleaks $*"
exit "${FAKE_GITLEAKS_RC:-0}"
EOF
chmod 0700 "$WORK/bin-fake/gitleaks"
NOGL_PATH="$WORK/bin-none:$(printf '%s' "$PATH" | tr ':' '\n' | while read -r d; do [ -x "$d/gitleaks" ] || printf '%s:' "$d"; done)"
NOGL_PATH="${NOGL_PATH%:}"

# new_report <name>: a fake report directory with addresses, dev passwords and a dev URI.
new_report() {
    local d="$WORK/$1"
    mkdir "$d"
    cat >"$d/01-env.log" <<'EOF'
private 10.1.2.3 and 192.168.1.20, public 8.8.4.4 and 203.0.113.7:5432
loopback 127.0.0.1:15432 and 0.0.0.0 and [::1]:15432
global 2001:db8:85a3::8a2e:370:7334, ula fd12:3456:789a::1, link-local fe80::1ff:fe23:4567:890a%eth0
mac 00:1a:2b:3c:4d:5e and 00-1A-2B-3C-4D-5F
rust path std::io::Error, time 12:34:56, hash dead::beef
conformanceVersion = 1.4.0.0
driverVersion = 10.20.30.40
version: 10.4.5.6 and Version 192.168.7.8
build 1.2.300.4
EOF
    cat >"$d/engines-containers.log" <<'EOF'
mysql  | root password: nine_dev_only_root
pg     | connecting to postgres://nine:nine_dev_only@127.0.0.1:15432/nine
web    | see http://example.com:8080/docs then pg://nine:nine_dev_only@127.0.0.1/nine
EOF
    printf '# NineSQL local end-to-end report\n' >"$d/summary.md"
    printf '%s\n' "$d"
}

# check <description> <command...>: ok when the command succeeds.
check() {
    local what=$1
    shift
    if "$@"; then ok "$what"; else fail "$what"; fi
}
expect_in() { check "$3" grep -qF -- "$2" "$1"; }
absent() { ! grep -qF -- "$1" "$2"; }
expect_not_in() { check "$3" absent "$2" "$1"; }
mode_is() { [ "$(stat -c %a "$1" 2>/dev/null)" = "$2" ]; }

# 1. Scrubbing and redaction, no gitleaks: no archive, exit 0.
d="$(new_report no-gitleaks)"
rc=0
PATH="$NOGL_PATH" "$E2E" --check-report "$d" >"$WORK/out1.txt" 2>&1 || rc=$?
check "no gitleaks: exit 0 (got $rc)" [ "$rc" = 0 ]
check "no gitleaks: no archive" [ ! -e "$d/report.tar.gz" ]
env_log="$d/01-env.log"
for v in 10.1.2.3 192.168.1.20 10.4.5.6 192.168.7.8 8.8.4.4 203.0.113.7 2001:db8:85a3::8a2e:370:7334 fd12:3456:789a::1 fe80::1ff:fe23:4567:890a 00:1a:2b:3c:4d:5e 00-1A-2B-3C-4D-5F; do
    expect_not_in "$env_log" "$v" "scrubbed $v"
done
for v in 127.0.0.1:15432 0.0.0.0 '[::1]:15432' std::io::Error 12:34:56 dead::beef 'conformanceVersion = 1.4.0.0' 'driverVersion = 10.20.30.40' 1.2.300.4 'version: <ip>' '<mac>' '<ip>:5432'; do
    expect_in "$env_log" "$v" "kept $v"
done
expect_not_in "$d/engines-containers.log" nine_dev_only "dev passwords redacted"
expect_in "$d/engines-containers.log" 'postgres://nine:<dev-password>@127.0.0.1' "dev URI redacted, not refused"
expect_in "$d/summary.md" "dev passwords of dev/engines.json found in 1 file(s)" "redaction noted in the summary"
expect_in "$d/summary.md" "gitleaks is not installed: no archive" "missing gitleaks noted in the summary"
check "rewritten file is 0600" mode_is "$env_log" 600

# 2. --no-secret-scan: archive written.
d="$(new_report no-scan)"
rc=0
PATH="$NOGL_PATH" "$E2E" --check-report "$d" --no-secret-scan >"$WORK/out2.txt" 2>&1 || rc=$?
check "--no-secret-scan: exit 0 (got $rc)" [ "$rc" = 0 ]
check "--no-secret-scan: archive written" [ -s "$d/report.tar.gz" ]
check "archive is 0600" mode_is "$d/report.tar.gz" 600

# 3. A credential URI that is not a dev one: fail, no archive, values not listed.
d="$(new_report foreign-uri)"
echo 'jdbc url mysql://alice:S3cretValue@db.example.com/prod' >>"$d/engines-containers.log"
rc=0
PATH="$NOGL_PATH" "$E2E" --check-report "$d" --no-secret-scan >"$WORK/out3.txt" 2>&1 || rc=$?
check "credential URI: exit 1 (got $rc)" [ "$rc" = 1 ]
check "credential URI: no archive" [ ! -e "$d/report.tar.gz" ]
expect_in "$d/credential-uris.txt" "engines-containers.log:4" "credential URI located by file:line"
expect_not_in "$d/credential-uris.txt" S3cretValue "credential URI value not listed"
expect_not_in "$WORK/out3.txt" S3cretValue "credential URI value not printed"
expect_in "$WORK/out3.txt" "Do not send anything before fixing the lines listed above" "refused archive: told not to send"
expect_not_in "$WORK/out3.txt" "Next: send summary.md" "refused archive: no 'send' hint"
expect_in "$WORK/out2.txt" "Next: send summary.md" "archive written: 'send' hint"

# 3b. Credential URIs the first check missed: empty user, upper-case scheme, spaces.
d="$(new_report uri-forms)"
cat >>"$d/engines-containers.log" <<'EOF'
cache  | redis://:S3cretRedis@cache.example.com:6379
proxy  | HTTP://bob:Pa55wordUpper@proxy.example.com
app    | Ftp://carol:pass with spaces@files.example.com/x
EOF
rc=0
PATH="$NOGL_PATH" "$E2E" --check-report "$d" --no-secret-scan >"$WORK/out3b.txt" 2>&1 || rc=$?
check "URI forms: exit 1 (got $rc)" [ "$rc" = 1 ]
check "URI forms: no archive" [ ! -e "$d/report.tar.gz" ]
for n in 4 5 6; do
    expect_in "$d/credential-uris.txt" "engines-containers.log:$n" "URI form on line $n found"
done
check "URI forms: exactly 3 lines" [ "$(grep -c . "$d/credential-uris.txt")" = 3 ]
for v in S3cretRedis Pa55wordUpper 'pass with spaces'; do
    expect_not_in "$d/credential-uris.txt" "$v" "URI form value '$v' not listed"
done

# 3c. Symbolic links and non-regular files in the report are never read nor written.
d="$(new_report symlinks)"
printf 'outside 10.9.8.7 mysql://eve:OutsideSecret@db.example.com/x\n' >"$WORK/outside.txt"
cp "$WORK/outside.txt" "$WORK/outside.orig"
ln -s "$WORK/outside.txt" "$d/linked.log"
mkdir "$WORK/outside-dir"
cp "$WORK/outside.txt" "$WORK/outside-dir/inner.log"
ln -s "$WORK/outside-dir" "$d/linked-dir"
mkfifo "$d/pipe.log"
rc=0
PATH="$NOGL_PATH" timeout 60 "$E2E" --check-report "$d" --no-secret-scan >"$WORK/out3c.txt" 2>&1 || rc=$?
check "symlinks: exit 0 (got $rc)" [ "$rc" = 0 ]
check "symlinks: linked file untouched" cmp -s "$WORK/outside.txt" "$WORK/outside.orig"
check "symlinks: file in linked dir untouched" cmp -s "$WORK/outside-dir/inner.log" "$WORK/outside.orig"
check "symlinks: link still a link" [ -L "$d/linked.log" ]
check "symlinks: archive written" [ -s "$d/report.tar.gz" ]
archive_lacks() { ! tar -xzOf "$1" 2>/dev/null | grep -q -- "$2"; }
check "symlinks: archive stores the link, not the target" archive_lacks "$d/report.tar.gz" OutsideSecret

# 3d. Hard link: the rewrite replaces the report's name, never the linked file.
d="$(new_report hardlink)"
printf 'hard 10.7.7.7\n' >"$WORK/hard-target.txt"
ln "$WORK/hard-target.txt" "$d/hard.log"
rc=0
PATH="$NOGL_PATH" "$E2E" --check-report "$d" --no-secret-scan >"$WORK/out3d.txt" 2>&1 || rc=$?
check "hard link: exit 0 (got $rc)" [ "$rc" = 0 ]
expect_in "$WORK/hard-target.txt" "10.7.7.7" "hard link: linked file untouched"
expect_not_in "$d/hard.log" "10.7.7.7" "hard link: report copy scrubbed"

# 3e. Directories that are not a report are refused (exit 2) and left alone.
refused() { # <description> <dir>
    local rc=0
    PATH="$NOGL_PATH" "$E2E" --check-report "$2" --no-secret-scan >"$WORK/out-refused.txt" 2>&1 || rc=$?
    check "$1: refused with exit 2 (got $rc)" [ "$rc" = 2 ]
    check "$1: no archive" [ ! -e "$2/report.tar.gz" ]
}
mkdir "$WORK/not-a-report"
printf 'private 10.1.2.3\n' >"$WORK/not-a-report/file.log"
refused "no summary.md" "$WORK/not-a-report"
expect_in "$WORK/not-a-report/file.log" "10.1.2.3" "no summary.md: files untouched"
printf 'some other title\n' >"$WORK/not-a-report/summary.md"
refused "summary.md with another title" "$WORK/not-a-report"
d="$(new_report summary-link)"
mv "$d/summary.md" "$WORK/summary-target.md"
ln -s "$WORK/summary-target.md" "$d/summary.md"
refused "summary.md is a symbolic link" "$d"
d="$(new_report output-link)"
ln -s "$WORK/gitleaks-target.log" "$d/gitleaks.log"
refused "gitleaks.log is a symbolic link" "$d"
check "gitleaks.log link target not created" [ ! -e "$WORK/gitleaks-target.log" ]

# 4. gitleaks clean: archive; gitleaks findings or error: fail, no archive.
for case in "0 0 yes" "42 1 no" "3 1 no"; do
    read -r glrc want archive <<<"$case"
    d="$(new_report "gitleaks-$glrc")"
    rc=0
    FAKE_GITLEAKS_RC="$glrc" PATH="$WORK/bin-fake:$NOGL_PATH" "$E2E" --check-report "$d" >"$WORK/out4.txt" 2>&1 || rc=$?
    check "gitleaks exit $glrc: script exit $want (got $rc)" [ "$rc" = "$want" ]
    if [ "$archive" = yes ]; then
        check "gitleaks exit $glrc: archive written" [ -s "$d/report.tar.gz" ]
    else
        check "gitleaks exit $glrc: no archive" [ ! -e "$d/report.tar.gz" ]
    fi
done

# 5. The scenarios stage, with a fake runner instead of the test build (E2E_TEST_SCENARIO_BIN)
# and fake xdotool / import, so its logic runs with no build, display or Docker.
mkdir "$WORK/bin-shot" "$WORK/scen"
cat >"$WORK/bin-shot/xdotool" <<'EOF'
#!/bin/sh
case "$1" in search) echo 4242 ;; esac
exit 0
EOF
cat >"$WORK/bin-shot/import" <<'EOF'
#!/bin/sh
for a in "$@"; do out=$a; done
printf 'fake png\n' >"$out"
EOF
cat >"$WORK/fake-ninesql" <<'EOF'
#!/usr/bin/env bash
# Fake scenario runner: the CLI and exit statuses of `ninesql --e2e-run` (e2e/README.md).
set -u
if [ "${1:-}" = --version ]; then echo "${FAKE_VERSION:-ninesql 0.0.0 (e2e-scenarios test build, not for distribution)}"; exit 0; fi
[ "${1:-}" = --e2e-run ] && [ "${3:-}" = --e2e-out ] && [ $# = 4 ] || exit 2
name="$(basename "$2" .toml)"
out=$4
run="$(dirname "$out")"
[ -z "${WAYLAND_DISPLAY:-}" ] || { echo "WAYLAND_DISPLAY must be unset" >&2; exit 2; }
for v in HOME XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME; do
    case "${!v}" in "$run"/*) ;; *) echo "$v is outside the run directory" >&2; exit 2 ;; esac
done
[ "$(stat -c %a "$run")" = 700 ] || { echo "run directory mode" >&2; exit 2; }
[ -d "$out" ] || { echo "no out directory" >&2; exit 2; }
mkdir -p "$XDG_STATE_HOME/ninesql/logs"
results() { # <outcome> <exit> <checkpoints json> <steps json>
    printf '{"schema_version":1,"scenario":"%s","outcome":"%s","exit_code":%s,"message":null,"checkpoints":%s,"planned_checkpoints":["start","done"],"steps":%s}\n' \
        "$name" "$1" "$2" "$3" "$4" >"$out/results.json"
}
checkpoint() { echo "ninesql-e2e checkpoint $1 $2"; sleep 1; }
pass_steps='[{"index":1,"name":"a","kind":"wait","outcome":"pass"},{"index":2,"name":"b","kind":"checkpoint","outcome":"pass"}]'
case "$name" in
    fake-pass)
        checkpoint 1 start
        checkpoint 2 done
        echo "connected with nine_dev_only" >"$XDG_STATE_HOME/ninesql/logs/ninesql.log"
        results pass 0 '["start","done"]' "$pass_steps"; exit 0 ;;
    fake-fail)
        checkpoint 1 start
        results fail 1 '["start"]' '[{"index":1,"name":"a","kind":"wait","outcome":"pass"},{"index":2,"name":"rows","kind":"expect","outcome":"fail","message":"grid.rows: expected 5 | got `0`"}]'
        exit 1 ;;
    fake-refused) echo "ninesql: scenario run stopped: the run directory must be owned by the current user with mode 0700" >&2; exit 2 ;;
    fake-secret)
        echo "MasterSecretXYZ" >"$XDG_STATE_HOME/ninesql/logs/ninesql.log"
        results secret-found 3 '[]' "$pass_steps"; exit 3 ;;
    fake-setup) results setup-error 4 '[]' '[]'; exit 4 ;;
    fake-skip) results skipped 5 '[]' '[]'; exit 5 ;;
    fake-hang) echo "ninesql-e2e checkpoint 1 start"; exec sleep 60 ;;
    fake-panic) echo "thread 'main' panicked at src/x.rs:1:1" >&2; results pass 0 '[]' "$pass_steps"; exit 0 ;;
esac
exit 2
EOF
chmod 0700 "$WORK/bin-shot/xdotool" "$WORK/bin-shot/import" "$WORK/fake-ninesql"
for s in fake-pass fake-fail fake-refused fake-secret fake-setup fake-skip fake-hang fake-panic; do
    printf 'schema_version = 1\nname = "%s"\n' "$s" >"$WORK/scen/$s.toml"
done
printf '[[connection]]\nname = "PG"\nengine = "postgresql"\n' >>"$WORK/scen/fake-skip.toml"

# scenarios <n> [options...]: runs only the scenarios stage into $WORK/sN; sets REP and RC.
scenarios() {
    local n=$1
    shift
    RC=0
    E2E_TEST_SCENARIO_BIN="${FAKE_BIN:-$WORK/fake-ninesql}" E2E_TEST_SCENARIO_DIR="$WORK/scen" \
        E2E_TEST_OUTPUT_DIR="$WORK/s$n" DISPLAY="${FAKE_DISPLAY-:99}" WAYLAND_DISPLAY=wayland-test \
        PATH="$WORK/bin-shot:$NOGL_PATH" timeout 300 "$E2E" --skip env --only scenarios \
        --engines-running --no-secret-scan "$@" >"$WORK/scen-out$n.txt" 2>&1 || RC=$?
    REP="$(find "$WORK/s$n/e2e-reports" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | head -n 1 || true)"
}

# 5a. Every exit status at once.
scenarios 1 --scenario-timeout 4
S="$REP/summary.md"
check "scenarios: script exit 1 (got $RC)" [ "$RC" = 1 ]
expect_in "$S" "| scenarios | FAIL |" "scenarios: stage FAIL"
expect_in "$S" "| fake-pass | PASS | 0 | 2 | 0 | 2 / 2 | 2 |" "scenarios: pass row"
expect_in "$S" "| fake-fail | FAIL | 1 | 1 | 1 | 1 / 2 | 1 |" "scenarios: fail row"
expect_in "$S" "| fake-refused | FAIL (refused) | 2 |" "scenarios: refused row"
expect_in "$S" "| fake-secret | FAIL (SECRET FOUND) | 3 |" "scenarios: secret row"
expect_in "$S" "| fake-setup | FAIL (setup error) | 4 |" "scenarios: setup-error row"
expect_in "$S" "| fake-skip | SKIP | 5 |" "scenarios: skipped row"
expect_in "$S" "| fake-hang | FAIL (time limit 4 s) |" "scenarios: time limit row"
expect_in "$S" "| fake-panic | FAIL (panic on stderr) | 0 |" "scenarios: panic row"
expect_in "$S" 'grid.rows: expected 5   got  0 ' "scenarios: failed step message, Markdown-safe"
expect_in "$S" "the run directory must be owned" "scenarios: refusal message from stderr"
expect_in "$S" "Dev engines \`postgresql\`: --engines-running" "scenarios: engines read from the scenario files"
check "scenarios: checkpoint 1 screenshot taken" [ -s "$REP/scenarios/fake-pass/checkpoint-01-start.png" ]
check "scenarios: checkpoint 2 screenshot taken" [ -s "$REP/scenarios/fake-pass/checkpoint-02-done.png" ]
check "scenarios: results.json collected" [ -s "$REP/scenarios/fake-fail/results.json" ]
expect_in "$REP/scenarios/fake-pass/stdout.log" "ninesql-e2e checkpoint 2 done" "scenarios: stdout collected"
expect_in "$REP/scenarios/fake-pass/ninesql.log" "<dev-password>" "scenarios: file log collected, dev password redacted"
check "scenarios: secret run's file log not collected" [ ! -e "$REP/scenarios/fake-secret/ninesql.log" ]
check "scenarios: secret found: no archive, even with --no-secret-scan" [ ! -e "$REP/report.tar.gz" ]
expect_in "$WORK/scen-out1.txt" "!!! scenarios: fake-secret" "scenarios: secret found: loud warning"
expect_in "$S" "runner exit 3): no archive" "scenarios: secret found: noted in the secret checks"
check "scenarios: run directories removed" [ -z "$(ls -A "$WORK/s1/e2e-runs" 2>/dev/null)" ]
expect_not_in "$S" "WAYLAND_DISPLAY must be unset" "scenarios: WAYLAND_DISPLAY unset for the run"

# 5b. Pass and skip: stage PASS, archive with the screenshots.
scenarios 2 --scenario fake-pass,fake-skip
check "scenarios pass: script exit 0 (got $RC)" [ "$RC" = 0 ]
expect_in "$REP/summary.md" "| scenarios | PASS |" "scenarios pass: stage PASS"
check "scenarios pass: archive written" [ -s "$REP/report.tar.gz" ]
archive_has() { tar -tzf "$1" 2>/dev/null | grep -q -- "$2"; }
check "scenarios pass: screenshots in the archive" archive_has "$REP/report.tar.gz" "scenarios/fake-pass/checkpoint-01-start.png"
expect_in "$WORK/scen-out2.txt" "checkpoint screenshots of the test windows" "scenarios pass: screenshot note"

# 5c. Only skipped scenarios: stage SKIP. No display: stage SKIP, nothing run.
scenarios 3 --scenario fake-skip
check "scenarios all skipped: script exit 0 (got $RC)" [ "$RC" = 0 ]
expect_in "$REP/summary.md" "| scenarios | SKIP |" "scenarios all skipped: stage SKIP"
FAKE_DISPLAY="" scenarios 4 --scenario fake-pass
check "scenarios no display: script exit 0 (got $RC)" [ "$RC" = 0 ]
expect_in "$REP/summary.md" "| scenarios | SKIP |" "scenarios no display: stage SKIP"
check "scenarios no display: nothing run" [ ! -e "$REP/scenarios" ]

# 5d. --no-screenshot; a binary that is not a test build; --dry-run; an unknown scenario.
scenarios 5 --scenario fake-pass --no-screenshot
check "scenarios --no-screenshot: exit 0 (got $RC)" [ "$RC" = 0 ]
check "scenarios --no-screenshot: no screenshot" [ -z "$(find "$REP" -name '*.png')" ]
expect_in "$REP/summary.md" "none: off (--no-screenshot)" "scenarios --no-screenshot: noted"
cat >"$WORK/not-test-build" <<'EOF'
#!/bin/sh
echo "ninesql 0.0.0"
EOF
chmod 0700 "$WORK/not-test-build"
FAKE_BIN="$WORK/not-test-build" scenarios 6 --scenario fake-pass
check "scenarios release binary: exit 1 (got $RC)" [ "$RC" = 1 ]
expect_in "$REP/summary.md" "does not say it is a test build" "scenarios release binary: refused"
scenarios 7 --dry-run
check "scenarios --dry-run: exit 0 (got $RC)" [ "$RC" = 0 ]
expect_in "$WORK/scen-out7.txt" "--e2e-run $WORK/scen/fake-pass.toml" "scenarios --dry-run: run command printed"
check "scenarios --dry-run: nothing run" [ ! -e "$REP/scenarios" ]
scenarios 8 --scenario no-such-scenario
check "scenarios unknown name: exit 2 (got $RC)" [ "$RC" = 2 ]

if [ "$FAILS" = 0 ]; then echo "All report checks passed."; else echo "$FAILS check(s) failed."; exit 1; fi
