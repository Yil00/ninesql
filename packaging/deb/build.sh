#!/usr/bin/env bash
# Builds the NineSQL .deb (P6-A, ADR-0010 §1 and §5). Run from anywhere; see packaging/README.md.
#
#   packaging/deb/build.sh
#
# Steps:
#   1. `cargo fetch --locked` on the host (the only step that uses the network: crates.io).
#   2. Build the builder image (packaging/deb/builder.Dockerfile, Ubuntu 22.04, glibc 2.35).
#   3. `cargo build --release --locked --offline -p ninesql-ui --bin ninesql` in that image under
#      `--network none`, with the host's Rust toolchain, the source tree and the host's cargo
#      registry cache and index mounted read-only (the extracted crate sources are the
#      container's own, under the build dir). Paths inside the container are fixed (/src, /cargo, /toolchain,
#      /target) and remapped with --remap-path-prefix, so the binary does not depend on where
#      the checkout or the cargo home lives on the host.
#   4. Check that the binary needs no glibc symbol newer than 2.35 (Ubuntu 22.04).
#   5. Generate the man page (gzip -n) and the third-party license file (cargo-about, offline,
#      from the crates' own license files) next to the binary.
#   6. `cargo deb --no-build` on the host, with mtimes clamped to SOURCE_DATE_EPOCH.
#
# Environment (all optional):
#   CARGO_TARGET_DIR      host target directory (default <repo>/target)
#   NINESQL_DEB_BUILD_DIR the container's target directory on the host
#                         (default $CARGO_TARGET_DIR/deb-build); the reproducibility check
#                         builds twice with two different values
#   NINESQL_DEB_OUT       output directory of the .deb (default $CARGO_TARGET_DIR/debian)
#   SOURCE_DATE_EPOCH     default: the commit time of HEAD
#   NINESQL_BUILDER_BASE  base image override: the same image from a mirror, which must name the
#                         digest pinned in builder.Dockerfile (`<registry>/ubuntu@sha256:<digest>`);
#                         any other value is refused. Another base image means editing the
#                         Dockerfile, and the result is then no longer the release build.
#   NINESQL_RUST_TOOLCHAIN rustup toolchain to mount (default: the one rust-toolchain.toml selects,
#                         `stable`). To reproduce a release, set it to the toolchain pinned in
#                         .github/workflows/release.yml: NINESQL_RUST_TOOLCHAIN=1.100.0
#   TIMEOUT               time limit of the container build, in seconds (default 3000)
#
# Tools needed on the host: docker, rustup with the toolchain installed, exactly cargo-deb 3.8.0
# and cargo-about 0.9.3 (`cargo install --locked cargo-deb@3.8.0` and
# `cargo install --locked cargo-about@0.9.3 --features cli`; checked), gzip, sha256sum.
set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
TARGET_BASE=${CARGO_TARGET_DIR:-$REPO/target}
BUILD_DIR=${NINESQL_DEB_BUILD_DIR:-$TARGET_BASE/deb-build}
OUT=${NINESQL_DEB_OUT:-$TARGET_BASE/debian}
TIMEOUT=${TIMEOUT:-3000}
IMAGE=ninesql-deb-builder:jammy
CARGO_HOME_DIR=${CARGO_HOME:-$HOME/.cargo}
# Highest glibc symbol version the binary may need: Ubuntu 22.04 ships 2.35, Debian 12 2.36.
MAX_GLIBC=2.35

log() { printf '==> %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

for tool in docker rustup cargo gzip sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is not installed"
done
# The packaging tools shape the .deb and its license file: exact versions, the same as the release
# workflow's CARGO_DEB_VERSION / CARGO_ABOUT_VERSION (bump both together).
CARGO_DEB_VERSION=3.8.0
CARGO_ABOUT_VERSION=0.9.3
# Toolchain of release builds (RUSTUP_TOOLCHAIN in .github/workflows/release.yml).
RELEASE_TOOLCHAIN=1.100.0
have=$(cargo deb --version 2>/dev/null | awk '{print $2}') || true
[ "$have" = "$CARGO_DEB_VERSION" ] || die "cargo-deb $CARGO_DEB_VERSION is required, found ${have:-none} \
(cargo install --locked --force cargo-deb@$CARGO_DEB_VERSION)"
have=$(cargo about --version 2>/dev/null | awk '{print $2}') || true
[ "$have" = "$CARGO_ABOUT_VERSION" ] || die "cargo-about $CARGO_ABOUT_VERSION is required, found ${have:-none} \
(cargo install --locked --force cargo-about@$CARGO_ABOUT_VERSION --features cli)"

cd "$REPO"
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}
if [ -n "${NINESQL_RUST_TOOLCHAIN:-}" ]; then
    RUSTC=$(rustup which --toolchain "$NINESQL_RUST_TOOLCHAIN" rustc)
else
    RUSTC=$(rustup which rustc)
fi
TOOLCHAIN_DIR=$(cd "$(dirname "$RUSTC")/.." && pwd)
log "toolchain: $("$RUSTC" -V) ($TOOLCHAIN_DIR)"
case "$("$RUSTC" -V)" in
    "rustc $RELEASE_TOOLCHAIN "*) ;;
    *) log "warning: not the release toolchain $RELEASE_TOOLCHAIN: the .deb will not match a release \
(NINESQL_RUST_TOOLCHAIN=$RELEASE_TOOLCHAIN to reproduce one)" ;;
esac
log "SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH"

log "1/6 cargo fetch --locked"
cargo fetch --locked

log "2/6 builder image"
PINNED_DIGEST=$(sed -n 's/^ARG BASE=.*@\(sha256:[0-9a-f]\{64\}\)$/\1/p' packaging/deb/builder.Dockerfile)
[ -n "$PINNED_DIGEST" ] || die "builder.Dockerfile does not pin its base image by digest"
build_args=()
if [ -n "${NINESQL_BUILDER_BASE:-}" ]; then
    case "$NINESQL_BUILDER_BASE" in
        *@"$PINNED_DIGEST") ;;
        *) die "NINESQL_BUILDER_BASE must name the pinned digest (…@$PINNED_DIGEST): another base \
image is not the release build (edit builder.Dockerfile instead)" ;;
    esac
    build_args+=(--build-arg "BASE=$NINESQL_BUILDER_BASE")
fi
docker build -q -t "$IMAGE" "${build_args[@]}" -f packaging/deb/builder.Dockerfile packaging/deb >/dev/null

log "3/6 cargo build in the builder (no network)"
# The host's downloaded crates (registry/cache) and index (registry/index) are mounted
# read-only: the build cannot change what the host's cargo later trusts. The extracted sources
# (registry/src) and the rest of the cargo home are the container's own, under the build dir.
[ -d "$CARGO_HOME_DIR/registry/cache" ] && [ -d "$CARGO_HOME_DIR/registry/index" ] \
    || die "no cargo registry in $CARGO_HOME_DIR (cargo fetch failed?)"
mkdir -p "$BUILD_DIR" "$BUILD_DIR/.cargo-home/registry/src"
mounts=(
    -v "$REPO:/src:ro"
    -v "$TOOLCHAIN_DIR:/toolchain:ro"
    -v "$BUILD_DIR:/target"
    -v "$BUILD_DIR/.cargo-home:/cargo"
    -v "$CARGO_HOME_DIR/registry/cache:/cargo/registry/cache:ro"
    -v "$CARGO_HOME_DIR/registry/index:/cargo/registry/index:ro"
)
if [ -d "$CARGO_HOME_DIR/git/db" ]; then
    mounts+=(-v "$CARGO_HOME_DIR/git/db:/cargo/git/db:ro")
fi
remap="--remap-path-prefix=/src=/ninesql --remap-path-prefix=/cargo=/cargo --remap-path-prefix=/toolchain=/rust --remap-path-prefix=/target=/target"
docker run --rm --network none \
    -u "$(id -u):$(id -g)" \
    "${mounts[@]}" \
    -w /src \
    -e HOME=/tmp \
    -e PATH=/toolchain/bin:/usr/local/bin:/usr/bin:/bin \
    -e CARGO_HOME=/cargo \
    -e CARGO_TARGET_DIR=/target \
    -e CARGO_TERM_COLOR=never \
    -e CARGO_INCREMENTAL=0 \
    -e SOURCE_DATE_EPOCH \
    -e RUSTFLAGS="$remap" \
    "$IMAGE" \
    timeout "$TIMEOUT" cargo build --release --locked --offline -p ninesql-ui --bin ninesql

BIN=$BUILD_DIR/release/ninesql
[ -x "$BIN" ] || die "the build did not produce $BIN"

log "4/6 glibc check (<= $MAX_GLIBC)"
needed=$(docker run --rm --network none -v "$BIN:/ninesql:ro" "$IMAGE" \
    sh -c "objdump -T /ninesql | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -n 1")
log "highest glibc symbol version needed: $needed"
if [ "$(printf '%s\n%s\n' "$needed" "$MAX_GLIBC" | sort -V | tail -n 1)" != "$MAX_GLIBC" ]; then
    die "the binary needs glibc $needed, newer than $MAX_GLIBC (Ubuntu 22.04)"
fi

log "5/6 man page and third-party licenses"
# gzip -n: no file name and no time stamp in the header (reproducible).
gzip -n -9 -c packaging/linux/ninesql.1 > "$BUILD_DIR/release/ninesql.1.gz"
# The crates linked into the binary, on its target, without build and dev dependencies
# (packaging/deb/about.toml); --frozen: no network, license texts come from the crates.
cargo about generate --frozen --fail \
    -c packaging/deb/about.toml \
    -m crates/ninesql-ui/Cargo.toml \
    -o "$BUILD_DIR/release/THIRD-PARTY-LICENSES.txt" \
    packaging/deb/third-party-licenses.hbs

log "6/6 cargo deb"
version=$(cargo metadata --format-version 1 --no-deps --locked --offline \
    | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="ninesql-ui"))')
mkdir -p "$OUT"
deb=$(CARGO_TARGET_DIR=$BUILD_DIR cargo deb -p ninesql-ui --no-build --no-strip --locked --offline \
    --deb-version "$version-1" -o "$OUT")
deb=${deb##*$'\n'}
[ -f "$deb" ] || die "cargo deb did not report the .deb it wrote"
(cd "$(dirname "$deb")" && sha256sum "$(basename "$deb")")
printf '%s\n' "$deb"
