#!/usr/bin/env bash
# Regenerates one synthetic DBeaver fixture workspace with a real DBeaver CE build.
#
# DBeaver is only a test-data generator: it is downloaded outside the repository,
# never linked, never committed (invariant I7). Every value written below is fake
# (hosts under .invalid, made-up users and passwords). See README.md next to this file.
#
# Usage:
#   generate.sh <version> <dbeaver-tarball> <scratch-dir> <fixture-out-dir>
#
#   <version>          DBeaver version, one of the keys of SHA256 below (e.g. 24.3.5)
#   <dbeaver-tarball>  the Linux x86_64 tarball downloaded from https://dbeaver.io/files/<version>/
#                      (verified against SHA256 before it is extracted)
#   <scratch-dir>      new or empty directory outside the repository (install, HOME, workspace)
#   <fixture-out-dir>  e.g. crates/ninesql-dbeaver/tests/fixtures/dbeaver-24.3.5
#
# Requirements: Xvfb, xdotool, python3 (no third-party modules), GTK 3 runtime libraries,
# sha256sum, setsid, and `unshare` (util-linux) or `bwrap` for network isolation.
# Steps:
#   1. verify and extract the tarball, start DBeaver under Xvfb in a clean environment
#      without network access and create connections with `-con`;
#   2. apply the documented hand edits (hand-edit.py) for what `-con` cannot express;
#   3. start DBeaver again with one more `-con`, which makes DBeaver re-read the hand-edited
#      files and re-serialize the whole registry in its own format (round trip);
#   4. copy only the files NineSQL reads into <fixture-out-dir>.
set -euo pipefail

if [[ $# -ne 4 ]]; then
  sed -n '2,16p' "$0" >&2
  exit 2
fi

# SHA-256 of the tarballs the committed fixtures were generated from (also in README.md).
declare -A SHA256=(
  [22.3.5]=52f153eb33d012c029c628cef5ffec3ca2ed2eb3dd3022b2ab6f99ec565d2847
  [23.3.5]=156ba81158992bd21d8ce1c4906f191246ca509b9336125dbf224843d97e9cf5
  [24.3.5]=30a4c3f5d42d84b7aad023ba77c95a572e9fad3f5e96870c169e9c687d39dca9
  [25.3.5]=a548cb336b4f215b86898a6fd1481f52720d3d1c7375b06f00f7de5f856cae7f
  [26.2.4]=a0dce46bb01cd2447c2d51388a39d3cda7ac58930fc558ebf3945aca5b859360
)

VERSION=$1
TARBALL=$2
OUT=$4
HERE=$(cd "$(dirname "$0")" && pwd)
DISPLAY_NUM=${DISPLAY_NUM:-:99}

log() { echo "[$VERSION] $*" >&2; }
die() {
  log "error: $*"
  exit 1
}

# --- scratch directory: new or empty, never inside the repository ------------------------
SCRATCH=$(realpath -m "$3")
if REPO=$(git -C "$HERE" rev-parse --show-toplevel 2>/dev/null); then
  REPO=$(realpath "$REPO")
  case "$SCRATCH/" in
    "$REPO"/*) die "scratch dir $SCRATCH is inside the repository $REPO" ;;
  esac
fi
mkdir -p "$SCRATCH"
if [[ -n "$(ls -A "$SCRATCH")" ]]; then
  die "scratch dir $SCRATCH is not empty"
fi

# --- verify and extract the tarball ------------------------------------------------------
EXPECTED=${SHA256[$VERSION]:-}
[[ -n "$EXPECTED" ]] || die "no known SHA-256 for version $VERSION; add it to SHA256 first"
ACTUAL=$(sha256sum "$TARBALL" | cut -d' ' -f1)
[[ "$ACTUAL" == "$EXPECTED" ]] || die "SHA-256 mismatch for $TARBALL (got $ACTUAL)"
mkdir "$SCRATCH/install"
tar -xzf "$TARBALL" -C "$SCRATCH/install"
DBEAVER_DIR="$SCRATCH/install/dbeaver"
[[ -x "$DBEAVER_DIR/dbeaver" ]] || die "no dbeaver launcher in $TARBALL"

WS="$SCRATCH/workspace6"
FAKE_HOME="$SCRATCH/home"
mkdir -p "$WS" "$FAKE_HOME"

# --- isolation ---------------------------------------------------------------------------
# DBeaver runs with an empty environment (no proxy variables, no D-Bus session, no SSH
# agent, XDG directories inside the throw-away HOME) and, when the system allows it, in its
# own network namespace (no interface but a down loopback). Xvfb is reached over its Unix
# socket, which works across network namespaces.
CLEAN_ENV=(env -i
  PATH=/usr/local/bin:/usr/bin:/bin
  HOME="$FAKE_HOME" USER=nine_user LOGNAME=nine_user LANG=C.UTF-8
  DISPLAY="$DISPLAY_NUM"
  XDG_DATA_HOME="$FAKE_HOME/.local/share"
  XDG_CONFIG_HOME="$FAKE_HOME/.config"
  XDG_CACHE_HOME="$FAKE_HOME/.cache"
  XDG_RUNTIME_DIR="$FAKE_HOME/.run")
mkdir -p "$FAKE_HOME/.run" && chmod 700 "$FAKE_HOME/.run"
if unshare -rn true 2>/dev/null; then
  NETNS=(unshare -rn)
  log "network isolation: unshare -rn"
elif bwrap --unshare-net --dev-bind / / true 2>/dev/null; then
  NETNS=(bwrap --unshare-net --dev-bind / /)
  log "network isolation: bwrap --unshare-net"
else
  NETNS=()
  log "WARNING: no network namespace available (unshare / bwrap); DBeaver runs with the"
  log "WARNING: host network. Only the environment is cleaned; run offline if this matters."
fi

# DBeaver 26.x opens a modal "Product Configuration" wizard on first start, which renders
# blank under Xvfb and blocks `-con`; the `product.config.disable` system property (read by
# DBeaver's ProductConfigUtils) suppresses it. Older versions ignore the property.
VMARGS=(-Duser.name=nine_user -Duser.home="$FAKE_HOME" -Dproduct.config.disable=true)

# --- connection specs (all values are fake) ---------------------------------------------
CONS=(
  # PostgreSQL, saved password, folder, custom driver properties.
  "driver=postgres-jdbc|name=pg-dev|folder=Team A|host=pg-dev.example.invalid|port=5432|database=nine|user=nine_user|password=fixture-not-a-secret|savePassword=true|prop.ApplicationName=ninesql-fixture|prop.connectTimeout=7|save=true|connect=false"
  # PostgreSQL nested folder; SSH tunnel, prod type and read-only flag added by hand-edit.py.
  "driver=postgres-jdbc|name=pg-prod-ssh|folder=Team A/Production|host=pg-prod.example.invalid|port=5432|database=nine_prod|user=nine_ro|password=fixture-prod-not-a-secret|savePassword=true|save=true|connect=false"
  # PostgreSQL defined by a JDBC URL only, password not saved; TLS handler added by hand-edit.py.
  "driver=postgres-jdbc|name=pg-url-tls|folder=Team B|url=jdbc:postgresql://pg-url.example.invalid:5433/nine_url?sslmode=verify-full|user=nine_user|savePassword=false|save=true|connect=false"
  # TimescaleDB driver of the PostgreSQL provider.
  "driver=postgres-timescale-jdbc|name=timescale-metrics|folder=Team B|host=tsdb.example.invalid|port=5432|database=metrics|user=nine_user|password=fixture-not-a-secret|savePassword=true|save=true|connect=false"
  # MySQL 8, manual commit, driver property; typed test by hand-edit.py.
  "driver=mysql8|name=mysql-test|folder=Team B|host=mysql.example.invalid|port=3306|database=nine|user=nine_user|password=fixture-not-a-secret|savePassword=true|autoCommit=false|prop.useSSL=true|prop.connectTimeout=5000|save=true|connect=false"
  # MariaDB, database password not saved; SSH (public key) and TLS handlers added by hand-edit.py.
  "driver=mariaDB|name=mariadb-ssh-key|host=mariadb.example.invalid|port=3306|database=nine|user=nine_user|savePassword=false|save=true|connect=false"
  # SQLite file (fake path, the file does not exist).
  "driver=sqlite_jdbc|name=sqlite-local|database=/home/nine_user/data/fixture.sqlite|save=true|connect=false"
  # ClickHouse (v0.2 engine); custom connection type by hand-edit.py.
  "driver=com_clickhouse|name=clickhouse-analytics|folder=Analytics|host=clickhouse.example.invalid|port=8123|database=default|user=nine_user|password=fixture-not-a-secret|savePassword=true|save=true|connect=false"
  # Oracle: engine outside the NineSQL v1 set (must be imported disabled).
  "driver=oracle_thin|name=oracle-legacy|folder=Legacy|host=oracle.example.invalid|port=1521|database=ORCLPDB1|user=nine_user|password=fixture-not-a-secret|savePassword=true|save=true|connect=false"
)
ROUNDTRIP_CON="driver=postgres-jdbc|name=roundtrip-marker|host=marker.example.invalid|port=5432|database=nine|user=nine_user|savePassword=false|save=true|connect=false"

start_xvfb() {
  if ! DISPLAY=$DISPLAY_NUM xdotool getmouselocation >/dev/null 2>&1; then
    Xvfb "$DISPLAY_NUM" -screen 0 1280x1024x24 -nolisten tcp >"$SCRATCH/xvfb.log" 2>&1 &
    XVFB_PID=$!
    trap 'kill "$XVFB_PID" 2>/dev/null || true' EXIT
    sleep 2
  fi
}

# Closes first-run dialogs (sample database, tips, ...) that would block command processing.
dismiss_dialogs() {
  local w name
  for w in $(DISPLAY=$DISPLAY_NUM xdotool search --onlyvisible --name '.' 2>/dev/null); do
    name=$(DISPLAY=$DISPLAY_NUM xdotool getwindowname "$w" 2>/dev/null || true)
    case "$name" in
      "Create sample database"*|"Tip of the day"*|"Tips"*|"Welcome"*|"Data Sharing"*|"Driver settings"*)
        DISPLAY=$DISPLAY_NUM xdotool key --window "$w" Escape || true ;;
    esac
  done
}

connection_count() {
  python3 -I -c 'import json,sys
try:
    print(len(json.load(open(sys.argv[1], encoding="utf-8")).get("connections", {})))
except Exception:
    print(0)' "$WS/General/.dbeaver/data-sources.json"
}

# run_dbeaver <expected-connection-count> <con-spec>...
# `-quit` makes DBeaver exit by itself once the command-line commands are processed
# (the launcher then reports "JVM terminated. Exit code=255", which is expected).
run_dbeaver() {
  local expected=$1
  shift
  local args=() spec
  for spec in "$@"; do args+=(-con "$spec"); done
  log "starting DBeaver, expecting $expected connections"
  # setsid: DBeaver (launcher + JVM) gets its own process group, so it can be stopped as a
  # whole without matching command lines. setsid, unshare and env all exec in place, so
  # $! is the group leader.
  setsid "${NETNS[@]}" "${CLEAN_ENV[@]}" \
    "$DBEAVER_DIR/dbeaver" -nosplash --launcher.suppressErrors -data "$WS" "${args[@]}" -quit \
    -vmargs "${VMARGS[@]}" >>"$SCRATCH/dbeaver.log" 2>&1 &
  local pgid=$! i
  for i in $(seq 1 180); do
    kill -0 -- "-$pgid" 2>/dev/null || break
    dismiss_dialogs
    sleep 2
    if [[ $(connection_count) -ge $expected ]]; then
      # Connections are saved; give DBeaver up to 30 s to finish and exit on its own.
      for i in $(seq 1 15); do
        kill -0 -- "-$pgid" 2>/dev/null || break
        dismiss_dialogs
        sleep 2
      done
      break
    fi
  done
  if kill -0 -- "-$pgid" 2>/dev/null; then
    log "DBeaver did not exit by itself; stopping it"
    kill -TERM -- "-$pgid" 2>/dev/null || true
    sleep 5
    kill -KILL -- "-$pgid" 2>/dev/null || true
  fi
  wait "$pgid" 2>/dev/null || true
  if [[ $(connection_count) -ne $expected ]]; then
    log "error: $(connection_count) connections written, expected $expected"
    exit 1
  fi
}

start_xvfb
run_dbeaver "${#CONS[@]}" "${CONS[@]}"
cp -a "$WS/General/.dbeaver" "$SCRATCH/phase1-dbeaver"

if [[ "${SKIP_HAND_EDIT:-0}" != "1" ]]; then
  python3 -I "$HERE/hand-edit.py" "$WS/General/.dbeaver/data-sources.json"
  run_dbeaver "$((${#CONS[@]} + 1))" "$ROUNDTRIP_CON"
fi

mkdir -p "$OUT/workspace6/General/.dbeaver" "$OUT/workspace6/.metadata"
cp "$WS/General/.project" "$OUT/workspace6/General/.project"
cp "$WS/General/.dbeaver/"*.json "$OUT/workspace6/General/.dbeaver/"
# Only the product name / version lines are kept: they are what workspace detection can use.
# With `-data`, DBeaver 25.x writes them to the default workspace location under HOME instead
# of the `-data` workspace, so both files are read (in a default install they are the same file).
PROPS=$(grep -hE '^product-(name|version)=' "$WS/.metadata/dbeaver-workspace.properties" \
  "$FAKE_HOME/.local/share/DBeaverData/workspace6/.metadata/dbeaver-workspace.properties" 2>/dev/null |
  sort -u || true)
if ! grep -q '^product-version=' <<<"$PROPS"; then
  log "error: product-version not found in dbeaver-workspace.properties"
  exit 1
fi
printf '%s\n' "$PROPS" >"$OUT/workspace6/.metadata/dbeaver-workspace.properties"
log "fixture written to $OUT"
