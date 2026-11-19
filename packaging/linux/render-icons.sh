#!/usr/bin/env bash
# Renders the hicolor PNG icons of the .deb and the Flatpak from the project logo
# (docs/assets/logo.svg) with rsvg-convert in an Ubuntu 24.04 container (P6-A).
# The PNGs are committed, so a package build needs no SVG renderer; run this again only when
# the logo changes. The scalable icon is the SVG itself.
#
#   packaging/linux/render-icons.sh
#
# NINESQL_ICON_IMAGE overrides the image, e.g. mirror.gcr.io/library/ubuntu:24.04.
set -euo pipefail
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
IMAGE=${NINESQL_ICON_IMAGE:-docker.io/library/ubuntu:24.04}
docker run --rm -u 0 -v "$REPO:/src" -w /src "$IMAGE" sh -euc '
  apt-get update -qq >/dev/null
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends librsvg2-bin >/dev/null
  for size in 32 48 64 128 256 512; do
    rsvg-convert -w "$size" -h "$size" -o "packaging/linux/icons/${size}.png" docs/assets/logo.svg
  done
  chown '"$(id -u):$(id -g)"' packaging/linux/icons/*.png
'
ls -l "$REPO"/packaging/linux/icons/*.png
