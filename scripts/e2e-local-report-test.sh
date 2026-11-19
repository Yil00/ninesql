#!/usr/bin/env bash
# Test of the report scrubber and secret checks of scripts/e2e-local.sh, on fake report
# directories (scripts/e2e-local.sh --check-report). Needs no display, Docker or cargo.
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
EOF
    cat >"$d/engines-containers.log" <<'EOF'
mysql  | root password: nine_dev_only_root
pg     | connecting to postgres://nine:nine_dev_only@127.0.0.1:15432/nine
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
for v in 10.1.2.3 192.168.1.20 8.8.4.4 203.0.113.7 2001:db8:85a3::8a2e:370:7334 fd12:3456:789a::1 fe80::1ff:fe23:4567:890a 00:1a:2b:3c:4d:5e 00-1A-2B-3C-4D-5F; do
    expect_not_in "$env_log" "$v" "scrubbed $v"
done
for v in 127.0.0.1:15432 0.0.0.0 '[::1]:15432' std::io::Error 12:34:56 dead::beef 'conformanceVersion = 1.4.0.0' '<mac>' '<ip>:5432'; do
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
expect_in "$d/credential-uris.txt" "engines-containers.log:3" "credential URI located by file:line"
expect_not_in "$d/credential-uris.txt" S3cretValue "credential URI value not listed"
expect_not_in "$WORK/out3.txt" S3cretValue "credential URI value not printed"

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

if [ "$FAILS" = 0 ]; then echo "All report checks passed."; else echo "$FAILS check(s) failed."; exit 1; fi
