#!/usr/bin/env bash
# Tests of packaging/check-no-e2e-marker.sh on fake binaries (ADR-0029 §2.2, guard 2).
#
#   packaging/check-no-e2e-marker-test.sh
set -euo pipefail

CHECK=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-no-e2e-marker.sh
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
status_of() { local rc=0; "$CHECK" "$@" 2>/dev/null || rc=$?; printf '%s' "$rc"; }

# A release-like binary: NUL bytes, no newline, no marker.
printf '\177ELF\0\0ninesql 0.1.0\0\0' > "$tmp/clean"
# A test build: the marker between NUL bytes, as a `#[used] static` leaves it.
printf '\177ELF\0\0NINESQL-E2E-SCENARIOS-TEST-BUILD\0ninesql\0' > "$tmp/test-build"
# A near miss is not the marker.
printf '\177ELF\0NINESQL-E2E-SCENARIOS-TEST\0' > "$tmp/near-miss"

[ "$(status_of "$tmp/clean")" = 0 ] || fail "a clean binary was refused"
[ "$(status_of "$tmp/near-miss")" = 0 ] || fail "a near miss was refused"
[ "$(status_of "$tmp/test-build")" = 1 ] || fail "a test build was accepted"
[ "$(status_of "$tmp/clean" "$tmp/test-build")" = 1 ] || fail "a test build among others was accepted"
[ "$(status_of "$tmp/missing")" = 2 ] || fail "a missing file was not an error"
[ "$(status_of)" = 2 ] || fail "no argument was not a usage error"
echo "ok: check-no-e2e-marker.sh (6 cases)"
