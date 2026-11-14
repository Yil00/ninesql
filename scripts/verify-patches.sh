#!/usr/bin/env bash
# Verify NineSQL's patched third-party crates (docs/audits/gpui-snapshot.md, X11 IME entry).
#
# Fails when:
#   - a [patch.*] entry of the root Cargo.toml names a crate outside the allowlist below, or does
#     not point to crates/patches/<crate> (git, registry or other path sources are refused);
#     a [replace] section, a [patch] or `paths` override in .cargo/config*, or a directory under
#     crates/patches/ without a patch entry also fail;
#   - Cargo.lock resolves a patched crate from a registry (a `source =` line), or at a version
#     other than the patched copy's;
#   - the crates.io archive of that version (Cargo's cache, else static.crates.io) does not
#     match the sha256 recorded for it in docs/audits/gpui-snapshot.md;
#   - a file of crates/patches/<crate> differs from the archive without a "Modified for NineSQL"
#     marker in its first 5 lines, is missing, or is added without being listed in EXTRAS.
#
# Usage: scripts/verify-patches.sh [work-dir]
#   work-dir defaults to a fresh mktemp directory; nothing is written to the repository.
# Needs: bash, curl, tar, sha256sum, diff, python3 >= 3.11 (tomllib).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="${1:-$(mktemp -d -t verify-patches-XXXXXX)}"
mkdir -p "$WORK"
CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
AUDIT="$ROOT/docs/audits/gpui-snapshot.md"
MARKER="Modified for NineSQL"

# Crates NineSQL may patch. Adding one needs a security-reviewer review and an audit entry.
ALLOWED="xim-ctext zed-xim"
# Files a patched copy may add to its archive ("<crate>/<path>"), each listed in the audit entry.
# xim-ctext/LICENSE: the xim-rs repository MIT license, absent from the xim-ctext archive.
EXTRAS="xim-ctext/LICENSE"

failures=0
fail() { echo "FAIL: $*"; failures=$((failures + 1)); }

# 1. [patch.*] entries: "<crate> <path>" lines, or "BAD <reason>" lines.
patch_entries() {
    python3 -I - "$ROOT/Cargo.toml" <<'EOF'
import sys, tomllib
manifest = tomllib.load(open(sys.argv[1], "rb"))
if "replace" in manifest:
    print("BAD [replace] section in the root Cargo.toml")
for registry, entries in manifest.get("patch", {}).items():
    for name, spec in entries.items():
        if not isinstance(spec, dict) or set(spec) != {"path"}:
            print(f"BAD [patch.{registry}] {name}: only {{ path = ... }} is allowed, got {spec!r}")
        elif registry != "crates-io":
            print(f"BAD [patch.{registry}] {name}: only [patch.crates-io] is allowed")
        else:
            print(f"{name} {spec['path']}")
EOF
}

patched=()
while read -r name path; do
    if [ "$name" = BAD ]; then
        fail "$path"
        continue
    fi
    if [[ " $ALLOWED " != *" $name "* ]]; then
        fail "[patch.crates-io] $name is not in the allowlist ($ALLOWED)"
    elif [ "$path" != "crates/patches/$name" ]; then
        fail "[patch.crates-io] $name points to '$path', expected 'crates/patches/$name'"
    else
        patched+=("$name")
    fi
done < <(patch_entries)

for cfg in "$ROOT"/.cargo/config "$ROOT"/.cargo/config.toml; do
    if [ -f "$cfg" ] && grep -qE '^\s*(\[patch|paths\s*=)' "$cfg"; then
        fail "$cfg overrides dependency sources ([patch] or paths)"
    fi
done

for dir in "$ROOT"/crates/patches/*/; do
    [ -d "$dir" ] || continue
    name="$(basename "$dir")"
    if [[ " ${patched[*]} " != *" $name "* ]]; then
        fail "crates/patches/$name has no valid [patch.crates-io] entry"
    fi
done

# 2-4. Per patched crate: Cargo.lock, archive checksum, file-by-file diff.
for name in "${patched[@]}"; do
    copy="$ROOT/crates/patches/$name"
    version="$(python3 -I -c 'import sys, tomllib; print(tomllib.load(open(sys.argv[1], "rb"))["package"]["version"])' "$copy/Cargo.toml")"
    echo "== $name $version"

    lock_report="$(python3 -I - "$ROOT/Cargo.lock" "$name" "$version" <<'EOF'
import sys, tomllib
lock, name, version = tomllib.load(open(sys.argv[1], "rb")), sys.argv[2], sys.argv[3]
entries = [p for p in lock.get("package", []) if p["name"] == name]
if not entries:
    print(f"{name} is not in Cargo.lock (patch unused?)")
for p in entries:
    if "source" in p:
        print(f"{name} {p['version']} is resolved from {p['source']}, so the patch is bypassed")
    elif p["version"] != version:
        print(f"{name} is locked at {p['version']}, the patched copy is {version}")
EOF
)"
    if [ -n "$lock_report" ]; then
        while read -r line; do fail "Cargo.lock: $line"; done <<<"$lock_report"
    fi

    expected="$(grep -oE "\`$name $version\`: \`[0-9a-f]{64}\`" "$AUDIT" | grep -oE '[0-9a-f]{64}' | head -n 1 || true)"
    if [ -z "$expected" ]; then
        fail "$name $version: no sha256 recorded in docs/audits/gpui-snapshot.md"
        continue
    fi

    archive="$WORK/$name-$version.crate"
    cached="$(ls "$CARGO_HOME"/registry/cache/*/"$name-$version.crate" 2>/dev/null | head -n 1 || true)"
    if [ -n "$cached" ]; then
        cp "$cached" "$archive"
    else
        curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
            -o "$archive" "https://static.crates.io/crates/$name/$name-$version.crate"
    fi
    actual="$(sha256sum "$archive" | cut -d' ' -f1)"
    if [ "$actual" != "$expected" ]; then
        fail "$name $version: archive sha256 $actual, audit log records $expected"
        continue
    fi

    extract="$WORK/extract-$name"
    rm -rf "$extract"
    mkdir -p "$extract"
    tar -xzf "$archive" -C "$extract" --no-same-owner --no-same-permissions
    upstream="$extract/$name-$version"
    if [ ! -d "$upstream" ]; then
        fail "$name $version: archive has no $name-$version/ directory"
        continue
    fi

    changed=0
    while IFS= read -r rel; do
        if [ ! -e "$copy/$rel" ]; then
            fail "$name: $rel is in the crates.io archive but missing from crates/patches/$name"
        elif ! cmp -s "$upstream/$rel" "$copy/$rel"; then
            changed=$((changed + 1))
            if head -n 5 "$copy/$rel" | grep -qF "$MARKER"; then
                echo "  modified (marked): $rel"
            else
                fail "$name: $rel differs from the crates.io archive without a '$MARKER' marker in its first 5 lines"
            fi
        fi
    done < <(cd "$upstream" && find . -type f | sed 's#^\./##' | sort)

    while IFS= read -r rel; do
        if [ ! -e "$upstream/$rel" ]; then
            if [[ " $EXTRAS " == *" $name/$rel "* ]]; then
                echo "  added (allowed): $rel"
            else
                fail "$name: $rel is not in the crates.io archive and not an allowed extra"
            fi
        fi
    done < <(cd "$copy" && find . -type f | sed 's#^\./##' | sort)

    if [ "$changed" -eq 0 ]; then
        echo "  note: no file differs from the archive (is the patch still needed?)"
    fi
done

if [ "${#patched[@]}" -eq 0 ]; then
    echo "verify-patches: no patched crate"
fi
if [ "$failures" -gt 0 ]; then
    echo "verify-patches: $failures failure(s)"
    exit 1
fi
echo "verify-patches: all patched crates match their crates.io archives"
