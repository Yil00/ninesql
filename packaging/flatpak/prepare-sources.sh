#!/usr/bin/env bash
# Writes the two generated inputs of the Flatpak manifest next to it (P6-A):
#   cargo-sources.json         every crate of Cargo.lock as a source with its checksum, so
#                              flatpak-builder builds offline (flatpak-cargo-generator);
#   THIRD-PARTY-LICENSES.txt   the third-party license file, as in the .deb (cargo-about).
#
#   packaging/flatpak/prepare-sources.sh
#
# Network: downloads flatpak-cargo-generator.py from flatpak-builder-tools at a pinned commit
# (its SHA-256 is checked) and its two Python dependencies from PyPI into a throwaway virtual
# environment; crates.io entries are generated from Cargo.lock alone. Needs python3 with venv,
# curl, sha256sum and cargo-about 0.9.3 (`cargo install --locked cargo-about@0.9.3 --features cli`).
set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
HERE=$REPO/packaging/flatpak
# flatpak/flatpak-builder-tools, master on 2026-11-19.
TOOLS_COMMIT=9d86d336a39b9af38b86f114b6ce5820fe260f91
GENERATOR_SHA256=0a2db6be87d75910facef28ab46d4d6460802e8419ab850d0caa6a364d26b380

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/tool"
curl -sSfL -o "$WORK/tool/flatpak-cargo-generator.py" \
    "https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/$TOOLS_COMMIT/cargo/flatpak-cargo-generator.py"
echo "$GENERATOR_SHA256  $WORK/tool/flatpak-cargo-generator.py" | sha256sum -c --quiet

python3 -m venv "$WORK/venv"
"$WORK/venv/bin/pip" install --quiet --disable-pip-version-check "aiohttp>=3.9,<4" "tomlkit>=0.12,<1"
# -I: isolated mode (no current directory or user site on sys.path).
"$WORK/venv/bin/python" -I "$WORK/tool/flatpak-cargo-generator.py" \
    "$REPO/Cargo.lock" -o "$HERE/cargo-sources.json"

cd "$REPO"
cargo about generate --frozen --fail \
    -c packaging/deb/about.toml \
    -m crates/ninesql-ui/Cargo.toml \
    -o "$HERE/THIRD-PARTY-LICENSES.txt" \
    packaging/deb/third-party-licenses.hbs
sed -i 's|/usr/share/doc/ninesql/copyright|/app/share/licenses/io.github.yil00.NineSQL/LICENSE|; s|/usr/share/doc/ninesql/NOTICE|/app/share/licenses/io.github.yil00.NineSQL/NOTICE|' \
    "$HERE/THIRD-PARTY-LICENSES.txt"
echo "wrote $HERE/cargo-sources.json and $HERE/THIRD-PARTY-LICENSES.txt"
