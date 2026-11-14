#!/usr/bin/env bash
# Regenerates one synthetic DBeaver fixture workspace with a real DBeaver CE build.
#
# DBeaver is only a test-data generator: it is downloaded outside the repository,
# never linked, never committed (invariant I7). Every value written below is fake
# (hosts under .invalid, made-up users and passwords). See README.md next to this file.
#
# Usage:
#   generate.sh <version> <dbeaver-install-dir> <scratch-dir> <fixture-out-dir>
#
#   <version>             DBeaver version, e.g. 24.3.5 (used for logging only)
#   <dbeaver-install-dir> extracted tarball directory containing the `dbeaver` launcher
#   <scratch-dir>         new, empty directory for the throw-away HOME and workspace
#   <fixture-out-dir>     e.g. crates/ninesql-dbeaver/tests/fixtures/dbeaver-24.3.5
#
# Requirements: Xvfb, xdotool, python3 (no third-party modules), GTK 3 runtime libraries.
# Steps:
#   1. start DBeaver under Xvfb with a fresh workspace and create connections with `-con`;
#   2. apply the documented hand edits (hand-edit.py) for what `-con` cannot express;
#   3. start DBeaver again with one more `-con`, which makes DBeaver re-read the hand-edited
#      files and re-serialize the whole registry in its own format (round trip);
#   4. copy only the files NineSQL reads into <fixture-out-dir>.
set -euo pipefail

if [[ $# -ne 4 ]]; then
  sed -n '2,14p' "$0" >&2
  exit 2
fi

VERSION=$1
DBEAVER_DIR=$(cd "$2" && pwd)
SCRATCH=$(mkdir -p "$3" && cd "$3" && pwd)
OUT=$4
HERE=$(cd "$(dirname "$0")" && pwd)
DISPLAY_NUM=${DISPLAY_NUM:-:99}

WS="$SCRATCH/workspace6"
FAKE_HOME="$SCRATCH/home"
mkdir -p "$WS" "$FAKE_HOME"
if [[ -e "$WS/General" ]]; then
  echo "scratch dir $SCRATCH is not empty" >&2
  exit 1
fi

# DBeaver must not reach the network: drop proxy settings inherited from the environment.
unset JAVA_TOOL_OPTIONS HTTPS_PROXY HTTP_PROXY https_proxy http_proxy ALL_PROXY all_proxy NO_PROXY no_proxy

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

log() { echo "[$VERSION] $*" >&2; }

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
  DISPLAY=$DISPLAY_NUM HOME="$FAKE_HOME" \
    "$DBEAVER_DIR/dbeaver" -nosplash --launcher.suppressErrors -data "$WS" "${args[@]}" -quit \
    -vmargs "${VMARGS[@]}" >>"$SCRATCH/dbeaver.log" 2>&1 &
  local pid=$! i
  for i in $(seq 1 180); do
    kill -0 "$pid" 2>/dev/null || break
    dismiss_dialogs
    sleep 2
    if [[ $(connection_count) -ge $expected ]]; then
      # Connections are saved; give DBeaver up to 30 s to finish and exit on its own.
      for i in $(seq 1 15); do
        kill -0 "$pid" 2>/dev/null || break
        dismiss_dialogs
        sleep 2
      done
      break
    fi
  done
  if kill -0 "$pid" 2>/dev/null; then
    log "DBeaver did not exit by itself; stopping it"
    kill -TERM "$pid" 2>/dev/null || true
    pkill -TERM -f -- "-data $WS" || true
    sleep 5
    pkill -KILL -f -- "-data $WS" || true
  fi
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
