#!/usr/bin/env bash
# Reproducibility check of the NineSQL .deb (P6-A, ADR-0010 §5): builds it twice and compares
# the SHA-256 of the two packages.
#
#   packaging/deb/check-reproducible.sh
#
# Build A: the checkout, in $CARGO_TARGET_DIR/deb-build (an earlier build there is reused, so
#          A can be incremental).
# Build B: a copy of the checkout at another path (tracked and untracked files, uncommitted
#          changes included, no target directory), in a fresh, empty build directory.
# Both use the same SOURCE_DATE_EPOCH (the commit time of HEAD unless set). The release
# workflow does the same on two separate runners.
#
# Environment: CARGO_TARGET_DIR (default <repo>/target), NINESQL_REPRO_KEEP=1 keeps build B's
# directories (otherwise deleted, they take a few GB), plus everything build.sh reads.
# Exit status 0 when the two .deb files are byte-identical.
set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
TARGET_BASE=${CARGO_TARGET_DIR:-$REPO/target}
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git -C "$REPO" log -1 --format=%ct)}
WORK=$TARGET_BASE/repro
SRC_B=$WORK/src-b
rm -rf "$WORK"
mkdir -p "$WORK"

# shellcheck disable=SC2329 # invoked by the trap below
cleanup() {
    if [ "${NINESQL_REPRO_KEEP:-0}" != 1 ]; then
        rm -rf "$SRC_B" "$WORK/build-b"
    fi
}
trap cleanup EXIT

echo "==> build A ($REPO)"
NINESQL_DEB_BUILD_DIR=$TARGET_BASE/deb-build NINESQL_DEB_OUT=$WORK/out-a \
    "$REPO/packaging/deb/build.sh" > "$WORK/build-a.log" 2>&1 \
    || { tail -n 40 "$WORK/build-a.log"; exit 1; }

echo "==> build B ($SRC_B)"
mkdir -p "$SRC_B"
# Tracked and untracked files (not ignored ones), as they are on disk.
git -C "$REPO" ls-files -z --cached --others --exclude-standard \
    | (cd "$REPO" && xargs -0 cp --parents -P -t "$SRC_B" 2>/dev/null || true)
NINESQL_DEB_BUILD_DIR=$WORK/build-b NINESQL_DEB_OUT=$WORK/out-b CARGO_TARGET_DIR=$TARGET_BASE \
    "$SRC_B/packaging/deb/build.sh" > "$WORK/build-b.log" 2>&1 \
    || { tail -n 40 "$WORK/build-b.log"; exit 1; }

a=$(ls "$WORK"/out-a/*.deb)
b=$(ls "$WORK"/out-b/*.deb)
sum_a=$(sha256sum "$a" | cut -d' ' -f1)
sum_b=$(sha256sum "$b" | cut -d' ' -f1)
echo "A: $sum_a  $a"
echo "B: $sum_b  $b"
if [ "$sum_a" = "$sum_b" ]; then
    echo "reproducible: the two packages are byte-identical"
    exit 0
fi
echo "NOT reproducible" >&2
if command -v diffoscope >/dev/null 2>&1; then
    diffoscope "$a" "$b" | head -n 200 >&2 || true
else
    diff <(dpkg-deb -c "$a") <(dpkg-deb -c "$b") >&2 || true
    cmp -l "$TARGET_BASE/deb-build/release/ninesql" "$WORK/build-b/release/ninesql" | head >&2 || true
fi
exit 1
