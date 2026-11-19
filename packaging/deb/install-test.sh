#!/usr/bin/env bash
# Install test of the NineSQL .deb (P6-A, ADR-0010 §7) in containers of every release target of
# docs/10-supported-platforms.md: Debian 12 and 13, Ubuntu 22.04 and 24.04.
#
#   packaging/deb/install-test.sh path/to/ninesql_<version>_amd64.deb [distribution ...]
#
# Distributions: debian:12 debian:13 ubuntu:22.04 ubuntu:24.04 (default: all four).
# For each one, in a fresh container:
#   1. apt installs the package and resolves its dependencies from the distribution's archive;
#   2. the package has no maintainer script (ADR-0010 §1: nothing runs at install or removal);
#   3. `ninesql --version` and `ninesql --help` run (no display needed) and an unknown argument
#      exits with status 2;
#   4. the desktop file passes `desktop-file-validate` and the AppStream file
#      `appstreamcli validate --no-net`; the man page renders without warnings;
#   5. `apt-get purge` removes the package and leaves none of its files behind.
#
# Environment:
#   NINESQL_TEST_REGISTRY  image prefix (default docker.io/library), e.g. mirror.gcr.io/library
#   NINESQL_TEST_LOGS      directory of the per-distribution logs (default: a temporary one)
#   TIMEOUT                time limit per distribution, in seconds (default 900)
#
# Exit status: 0 when every distribution passes; a distribution whose archive cannot be reached
# is reported BLOCKED and fails the run too. The images are the distribution tags (Debian:
# the -slim ones; they move with point releases); `docker rmi` them afterwards if you pulled them only for this.
set -euo pipefail

die() { printf 'error: %s\n' "$*" >&2; exit 1; }

[ $# -ge 1 ] || die "usage: $0 path/to/ninesql.deb [distribution ...]"
DEB=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
[ -f "$DEB" ] || die "$1 is not a file"
shift
DISTROS=("$@")
[ ${#DISTROS[@]} -gt 0 ] || DISTROS=(debian:12 debian:13 ubuntu:22.04 ubuntu:24.04)
REGISTRY=${NINESQL_TEST_REGISTRY:-docker.io/library}
LOGS=${NINESQL_TEST_LOGS:-$(mktemp -d)}
TIMEOUT=${TIMEOUT:-900}
mkdir -p "$LOGS"
command -v docker >/dev/null 2>&1 || die "docker is not installed"

# Runs inside the container, as root. Every check prints "ok: …" or fails the script.
read -r -d '' CHECKS <<'EOF' || true
set -eu
export DEBIAN_FRONTEND=noninteractive
. /etc/os-release
echo "== $PRETTY_NAME"
# Minimal images drop /usr/share/doc and /usr/share/man at install (dpkg path-exclude); lift
# that so the package's man page and copyright files can be checked.
rm -f /etc/dpkg/dpkg.cfg.d/docker /etc/dpkg/dpkg.cfg.d/excludes
apt-get update -qq
# `apt-get update` exits 0 when every index failed to download: tell an unreachable archive
# (exit 3, reported BLOCKED) from a package problem.
if ! apt-cache policy desktop-file-utils | grep -q 'Candidate: [0-9]'; then
  echo "BLOCKED: the distribution archive is unreachable from this container"; exit 3
fi
# The validators first, so the package install below starts from the distribution's base.
apt-get install -y -qq --no-install-recommends desktop-file-utils appstream groff-base >/dev/null
echo "ok: validators installed"

apt-get install -y -qq --no-install-recommends /pkg/ninesql.deb >/tmp/install.log 2>&1 \
  || { cat /tmp/install.log; exit 1; }
echo "ok: installed $(dpkg-query -W -f='${Package} ${Version}' ninesql)"
echo "   Depends: $(dpkg-query -W -f='${Depends}' ninesql)"

for script in preinst postinst prerm postrm config; do
  if [ -e "/var/lib/dpkg/info/ninesql.$script" ]; then
    echo "FAIL: maintainer script $script present"; exit 1
  fi
done
echo "ok: no maintainer script"

version=$(ninesql --version)
echo "ok: ninesql --version: $version"
case "$version" in "ninesql "*) ;; *) echo "FAIL: unexpected --version output"; exit 1;; esac
ninesql --help >/dev/null
echo "ok: ninesql --help"
set +e
ninesql --no-such-flag >/dev/null 2>&1
status=$?
set -e
[ "$status" -eq 2 ] || { echo "FAIL: unknown argument exited with $status, not 2"; exit 1; }
echo "ok: unknown argument refused (exit 2)"

desktop-file-validate /usr/share/applications/io.github.yil00.NineSQL.desktop
echo "ok: desktop-file-validate"
appstreamcli validate --no-net /usr/share/metainfo/io.github.yil00.NineSQL.metainfo.xml
echo "ok: appstreamcli validate ($(appstreamcli --version | head -n 1))"
warnings=$(zcat /usr/share/man/man1/ninesql.1.gz | groff -man -ww -z 2>&1)
[ -z "$warnings" ] || { echo "FAIL: man page warnings:"; echo "$warnings"; exit 1; }
echo "ok: man page renders without warnings"
for icon in scalable/apps/io.github.yil00.NineSQL.svg 128x128/apps/io.github.yil00.NineSQL.png; do
  [ -s "/usr/share/icons/hicolor/$icon" ] || { echo "FAIL: icon $icon missing"; exit 1; }
done
[ -s /usr/share/doc/ninesql/THIRD-PARTY-LICENSES.txt ] || { echo "FAIL: no third-party licenses"; exit 1; }
[ -s /usr/share/doc/ninesql/copyright ] || { echo "FAIL: no copyright file"; exit 1; }
echo "ok: icons, copyright and third-party licenses present"

dpkg -L ninesql | sort > /tmp/files
apt-get purge -y -qq ninesql >/dev/null
if dpkg-query -W ninesql >/dev/null 2>&1 && [ "$(dpkg-query -W -f='${Status}' ninesql)" != "unknown ok not-installed" ]; then
  echo "FAIL: ninesql still known to dpkg after purge"; exit 1
fi
left=0
while read -r path; do
  if [ -f "$path" ] || [ -L "$path" ]; then echo "FAIL: left behind: $path"; left=1; fi
done < /tmp/files
[ "$left" -eq 0 ] || exit 1
echo "ok: purged, no file left behind"
EOF

declare -A RESULT
overall=0
for distro in "${DISTROS[@]}"; do
    # Debian: the -slim images (same archive, smaller download).
    case "$distro" in
        debian:*) image="$REGISTRY/$distro-slim" ;;
        *) image="$REGISTRY/$distro" ;;
    esac
    log="$LOGS/${distro//[:\/]/-}.log"
    printf '==> %s (%s)\n' "$distro" "$image"
    status=0
    timeout "$TIMEOUT" docker run --rm -v "$DEB:/pkg/ninesql.deb:ro" "$image" sh -c "$CHECKS" \
        >"$log" 2>&1 || status=$?
    case "$status" in
        0) RESULT[$distro]=PASS ;;
        3) RESULT[$distro]="BLOCKED (archive unreachable, nothing tested)"; overall=1 ;;
        *) RESULT[$distro]=FAIL; overall=1 ;;
    esac
    sed 's/^/    /' "$log"
done

printf '\nSummary (logs in %s):\n' "$LOGS"
for distro in "${DISTROS[@]}"; do
    printf '  %-14s %s\n' "$distro" "${RESULT[$distro]}"
done
exit "$overall"
