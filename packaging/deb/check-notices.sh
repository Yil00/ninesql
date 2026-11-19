#!/usr/bin/env bash
# Fails when the generated third-party license file lacks a notice that cargo-about cannot find
# by itself (P6-E review M1, ADR-0002 rule 5). The texts come from the clarifications in
# packaging/deb/about.toml; cargo-about only warns when one of them stops matching (a crate
# bump whose license file changed) and silently falls back to scanning, so this check is what
# makes such a drift fail the build.
#
#   packaging/deb/check-notices.sh THIRD-PARTY-LICENSES.txt
#
# Run by packaging/deb/build.sh and packaging/flatpak/prepare-sources.sh after generation.
set -euo pipefail

[ "$#" -eq 1 ] || { echo "usage: $0 THIRD-PARTY-LICENSES.txt" >&2; exit 2; }
file=$1
[ -s "$file" ] || { echo "error: $file is missing or empty" >&2; exit 1; }

missing=0
# Each entry: a fixed string that must appear, then what it stands for.
while IFS='|' read -r needle what; do
    if ! grep -qF -- "$needle" "$file"; then
        echo "error: $file lacks $what (\"$needle\")" >&2
        missing=1
    fi
done <<'LIST'
The FreeType Project LICENSE|the FreeType License (FTL.TXT, bundled FreeType in freetype-sys)
--- end of FTL.TXT ---|the end of the FreeType License text
Francesco Zappa Nardelli|the FreeType BDF / PCF driver notices
Copyright (c) 2026 Lucide Icons and Contributors|the Lucide ISC license (gpui-kit-assets icons)
The following Lucide icons are derived from the Feather project|the Feather-derived Lucide icon list
Copyright (c) 2013-present Cole Bemis|the Feather MIT license (gpui-kit-assets icons)
LIST

if [ "$missing" -ne 0 ]; then
    echo "error: a clarification in packaging/deb/about.toml no longer matches (run cargo about with RUST_LOG=warn to see which)" >&2
    exit 1
fi
echo "third-party notices: FreeType (FTL) and Lucide / Feather present"
