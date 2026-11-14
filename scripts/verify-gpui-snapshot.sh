#!/usr/bin/env bash
# Verify that the GPUI crates NineSQL takes from crates.io are faithful snapshots (ADR-0013).
#
# For every crate of the pinned set found in Cargo.lock:
#   - the .crate archive in Cargo's cache matches the Cargo.lock checksum;
#   - gpui-pre-*: each file equals its source in zed-industries/zed at the revision recorded in
#     the crate's `package.metadata.gpui-pre.zed-rev` (directory crates/<zed-crate>);
#   - gpui-kit family (gpui-kit, gpui-base, gpui-component, gpui-component-macros,
#     gpui-kit-assets): each file equals longbridge/gpui-component at tag v<version>;
#   - gpui-pre-reqwest: each file equals the zed-reqwest crate published on crates.io.
# Cargo.toml, Cargo.toml.orig, .cargo_vcs_info.json and .cargo-ok are ignored (rewritten by
# `cargo package`). A file that differs from, or is absent in, its source must carry a
# "Modified for gpui-pre" marker in its first 5 lines, otherwise the script fails.
# It also prints the crates.io owners of each crate (warning only if crates.io is unreachable).
#
# Usage: scripts/verify-gpui-snapshot.sh [work-dir]
#   work-dir defaults to a fresh mktemp directory; nothing is written to the repository.
# Needs: bash, git (https access to github.com), curl, tar, sha256sum, python3 (TOML/JSON).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOCK="$ROOT/Cargo.lock"
WORK="${1:-$(mktemp -d -t gpui-snapshot-XXXXXX)}"
mkdir -p "$WORK"
CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
ZED_URL="https://github.com/zed-industries/zed"
KIT_URL="https://github.com/longbridge/gpui-component"
KIT_FAMILY="gpui-kit gpui-base gpui-component gpui-component-macros gpui-kit-assets"
MARKER="Modified for gpui-pre"

failures=0
fail() { echo "FAIL: $*"; failures=$((failures + 1)); }
warn() { echo "WARN: $*"; }

# "name version checksum" for the crates of the pinned set, from Cargo.lock.
lock_entries() {
    python3 -I - "$LOCK" "$KIT_FAMILY" <<'EOF'
import sys, tomllib
lock = tomllib.load(open(sys.argv[1], "rb"))
family = set(sys.argv[2].split())
for p in lock.get("package", []):
    n = p["name"]
    if n.startswith("gpui-pre") or n in family:
        print(n, p["version"], p.get("checksum", "-"))
EOF
}

# Extract a .crate from Cargo's cache (fetching it first if needed) and check its checksum.
extract_crate() {
    local name="$1" version="$2" sum="$3" archive
    archive="$(ls "$CARGO_HOME"/registry/cache/*/"$name-$version.crate" 2>/dev/null | head -1 || true)"
    if [ -z "$archive" ]; then
        (cd "$ROOT" && cargo fetch --locked >/dev/null 2>&1) || true
        archive="$(ls "$CARGO_HOME"/registry/cache/*/"$name-$version.crate" 2>/dev/null | head -1 || true)"
    fi
    [ -n "$archive" ] || { fail "$name $version: .crate not in Cargo's cache"; return 1; }
    if [ "$sum" != "-" ] && [ "$(sha256sum "$archive" | cut -d' ' -f1)" != "$sum" ]; then
        fail "$name $version: archive checksum differs from Cargo.lock"
        return 1
    fi
    mkdir -p "$WORK/crates"
    tar -xzf "$archive" -C "$WORK/crates"
}

# Value of package.metadata.gpui-pre.<key> in an extracted crate.
gpui_pre_meta() {
    python3 -I - "$1/Cargo.toml" "$2" <<'EOF'
import sys, tomllib
meta = tomllib.load(open(sys.argv[1], "rb")).get("package", {}).get("metadata", {}).get("gpui-pre", {})
print(meta.get(sys.argv[2], ""))
EOF
}

# Blobless, shallow fetch of <url> at <rev> into <dest> (trees only, nothing checked out).
shallow_fetch() {
    local url="$1" rev="$2" dest="$3"
    if [ ! -d "$dest/.git" ]; then
        git init -q "$dest"
        git -C "$dest" remote add origin "$url"
    fi
    git -C "$dest" fetch -q --depth 1 --filter=blob:none origin "$rev"
}

# Materialize only <dirs> of the fetched revision.
sparse_checkout() {
    local dest="$1"
    shift
    git -C "$dest" sparse-checkout set --cone "$@"
    git -C "$dest" checkout -q --detach FETCH_HEAD
}

# Directory of the crate named <name> in the fetched zed revision: crates/<name>, or nested
# (for example crates/refineable/derive_refineable).
zed_crate_dir() {
    if git -C "$WORK/zed" cat-file -e "FETCH_HEAD:crates/$1/Cargo.toml" 2>/dev/null; then
        echo "crates/$1"
    else
        git -C "$WORK/zed" ls-tree -r --name-only FETCH_HEAD | grep -E "(^|/)$1/Cargo.toml$" |
            head -1 | xargs -r dirname
    fi
}

# `readme` path of a crate manifest (Cargo.toml.orig), empty if none.
manifest_readme() {
    python3 -I -c '
import sys, tomllib
r = tomllib.load(open(sys.argv[1], "rb")).get("package", {}).get("readme", "")
print(r if isinstance(r, str) else "")' "$1" 2>/dev/null || true
}

# Compare an extracted crate with its source directory.
# Two packaging effects are resolved before a file counts as changed:
#   - the README that `cargo package` copies from `readme = "../../README.md"`;
#   - files vendored under vendor/<crate>/... (4th argument: zed checkout holding that crate).
compare_dirs() {
    local label="$1" crate_dir="$2" src_dir="$3" vendor_root="${4:-}"
    local rel ref readme changed=0 unmarked=0 missing=0
    if [ ! -d "$src_dir" ]; then
        fail "$label: source directory $src_dir not found"
        return
    fi
    readme="$(manifest_readme "$crate_dir/Cargo.toml.orig")"
    while IFS= read -r -d '' file; do
        rel="${file#"$crate_dir"/}"
        case "$rel" in
            Cargo.toml | Cargo.toml.orig | .cargo_vcs_info.json | .cargo-ok | Cargo.lock) continue ;;
        esac
        ref="$src_dir/$rel"
        if [ ! -e "$ref" ] && [ -n "$readme" ] && [ "$rel" = "$(basename "$readme")" ]; then
            ref="$src_dir/$readme"
        fi
        if [ ! -e "$ref" ] && [ -n "$vendor_root" ] && [[ "$rel" == vendor/*/* ]]; then
            ref="$vendor_root/$(zed_crate_dir "$(cut -d/ -f2 <<<"$rel")")/${rel#vendor/*/}"
        fi
        if [ -e "$ref" ] && cmp -s "$file" "$ref"; then
            continue
        fi
        changed=$((changed + 1))
        [ -e "$ref" ] || missing=$((missing + 1))
        if ! head -n 5 "$file" | grep -q "$MARKER"; then
            unmarked=$((unmarked + 1))
            echo "    unmarked change: $rel$([ -e "$ref" ] || echo ' (not in source)')"
        fi
    done < <(find "$crate_dir" -type f -print0 | sort -z)
    if [ "$unmarked" -gt 0 ]; then
        fail "$label: $unmarked changed file(s) without a '$MARKER' marker ($changed changed, $missing not in source)"
    else
        echo "OK:   $label: $changed changed file(s), all marked ($missing not in source)"
    fi
}

owners() {
    local name="$1" out
    if out="$(curl -fsS --max-time 15 -A "ninesql-verify-gpui-snapshot" \
        "https://crates.io/api/v1/crates/$name/owners" 2>/dev/null)"; then
        printf '%s' "$out" | python3 -I -c 'import json,sys; print(", ".join(u["login"] for u in json.load(sys.stdin)["users"]))'
    else
        echo "unknown (crates.io unreachable)"
        warn "could not read crates.io owners of $name"
    fi
}

echo "Work directory: $WORK"
mapfile -t entries < <(lock_entries)
[ "${#entries[@]}" -gt 0 ] || { echo "No gpui-pre / gpui-kit crate in Cargo.lock"; exit 1; }

zed_rev=""
zed_crates=()
kit_version=""
for entry in "${entries[@]}"; do
    read -r name version sum <<<"$entry"
    extract_crate "$name" "$version" "$sum" || continue
    dir="$WORK/crates/$name-$version"
    case "$name" in
        gpui-pre-reqwest) ;;
        gpui-pre*)
            rev="$(gpui_pre_meta "$dir" zed-rev)"
            [ -n "$rev" ] || { fail "$name: no package.metadata.gpui-pre.zed-rev"; continue; }
            if [ -n "$zed_rev" ] && [ "$rev" != "$zed_rev" ]; then
                fail "$name: zed-rev $rev differs from $zed_rev (mixed snapshots)"
            fi
            zed_rev="$rev"
            zed_crates+=("$(gpui_pre_meta "$dir" zed-crate)")
            ;;
        *) kit_version="$version" ;;
    esac
done

if [ -n "$zed_rev" ]; then
    echo "Checking out $ZED_URL at $zed_rev (sparse: ${#zed_crates[@]} crates)..."
    shallow_fetch "$ZED_URL" "$zed_rev" "$WORK/zed"
    zed_dirs=()
    for zc in "${zed_crates[@]}"; do
        d="$(zed_crate_dir "$zc")"
        if [ -n "$d" ]; then zed_dirs+=("$d"); else fail "zed crate $zc not found at $zed_rev"; fi
    done
    sparse_checkout "$WORK/zed" "${zed_dirs[@]}"
fi
if [ -n "$kit_version" ]; then
    echo "Checking out $KIT_URL at v$kit_version..."
    shallow_fetch "$KIT_URL" "refs/tags/v$kit_version" "$WORK/gpui-component"
    sparse_checkout "$WORK/gpui-component" crates
fi

for entry in "${entries[@]}"; do
    read -r name version _ <<<"$entry"
    dir="$WORK/crates/$name-$version"
    [ -d "$dir" ] || continue
    case "$name" in
        gpui-pre-reqwest)
            # Snapshot of Zed's reqwest fork, published by Zed as zed-reqwest <version>-zed.
            ref="zed-reqwest-$version-zed"
            if [ ! -d "$WORK/ref/$ref" ]; then
                mkdir -p "$WORK/ref"
                if curl -fsSL --max-time 60 -A "ninesql-verify-gpui-snapshot" \
                    "https://static.crates.io/crates/zed-reqwest/$ref.crate" -o "$WORK/ref/$ref.crate"; then
                    tar -xzf "$WORK/ref/$ref.crate" -C "$WORK/ref"
                else
                    fail "$name: cannot download $ref from crates.io"
                    continue
                fi
            fi
            compare_dirs "$name $version vs $ref" "$dir" "$WORK/ref/$ref"
            ;;
        gpui-pre*)
            zd="$(zed_crate_dir "$(gpui_pre_meta "$dir" zed-crate)")"
            compare_dirs "$name $version vs zed@${zed_rev:0:7} $zd" "$dir" "$WORK/zed/$zd" "$WORK/zed"
            ;;
        *)
            src="$(grep -l -E "^name = \"$name\"" "$WORK"/gpui-component/crates/*/Cargo.toml 2>/dev/null | head -1 || true)"
            if [ -z "$src" ]; then
                fail "$name: no crate of that name in gpui-component v$kit_version"
                continue
            fi
            compare_dirs "$name $version vs gpui-component v$kit_version $(dirname "${src#"$WORK"/gpui-component/}")" \
                "$dir" "$(dirname "$src")"
            ;;
    esac
done

echo
echo "crates.io owners:"
for entry in "${entries[@]}"; do
    read -r name version _ <<<"$entry"
    printf '  %-28s %-10s %s\n' "$name" "$version" "$(owners "$name")"
done

echo
if [ "$failures" -gt 0 ]; then
    echo "verify-gpui-snapshot: $failures failure(s)"
    exit 1
fi
echo "verify-gpui-snapshot: all crates match their sources"
