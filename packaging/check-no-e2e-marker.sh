#!/usr/bin/env bash
# Refuses an end-to-end scenario test build (ADR-0029 §2.2, guard 2).
#
#   packaging/check-no-e2e-marker.sh path/to/ninesql [...]
#
# A binary built with the `e2e-scenarios` feature of ninesql-ui embeds the fixed marker
# NINESQL-E2E-SCENARIOS-TEST-BUILD (a `#[used] static`, kept by the optimiser and the linker).
# Such a binary must never be packaged, signed or shipped. Exit status: 1 when any file holds
# the marker, 2 on a usage error or an unreadable file, 0 otherwise. Run by
# packaging/deb/build.sh before cargo-deb packages the binary; release.yml runs it again on the
# built binary and on the binary extracted from the .deb, before signing.
set -euo pipefail

MARKER=NINESQL-E2E-SCENARIOS-TEST-BUILD

[ $# -ge 1 ] || { printf 'usage: %s path/to/ninesql [...]\n' "$0" >&2; exit 2; }
status=0
for bin in "$@"; do
    if [ ! -f "$bin" ] || [ ! -r "$bin" ]; then
        printf 'error: %s is not a readable file\n' "$bin" >&2
        exit 2
    fi
    # grep -c prints the count and exits 1 when it is 0; any other status is a read error.
    rc=0
    count=$(grep -a -c -F -- "$MARKER" "$bin") || rc=$?
    if [ "$rc" -gt 1 ]; then
        printf 'error: cannot read %s\n' "$bin" >&2
        exit 2
    fi
    if [ "${count:-0}" -ne 0 ]; then
        printf 'error: %s is an e2e-scenarios test build (marker %s found): it must not be packaged (ADR-0029 §2.2)\n' \
            "$bin" "$MARKER" >&2
        status=1
    else
        printf 'ok: %s holds no e2e-scenarios marker\n' "$bin" >&2
    fi
done
exit "$status"
