#!/usr/bin/env python3
"""Writes the CycloneDX SBOM of the ninesql binary (P6-A, ADR-0010 §4).

    python3 packaging/sbom.py OUTPUT.cdx.json

Runs cargo-cyclonedx 0.5.10 on crates/ninesql-ui (JSON, CycloneDX 1.5, target
x86_64-unknown-linux-gnu, no build dependencies), then keeps only the crates linked into the
binary: cargo-cyclonedx also lists the root crate's dev-dependencies (test-only crates such as
proptest), so components outside `cargo tree -e normal` for that target are removed, with
their entries in the dependency graph. Offline (`--frozen`): crates must be fetched already.
SOURCE_DATE_EPOCH, when set, is the SBOM's timestamp.

C code a crate compiles in is not a crate, so cargo-cyclonedx does not list it. The bundled
FreeType of freetype-sys, compiled statically in the .deb's Ubuntu 22.04 builder (its system
FreeType is older than the crate's floor), is added as its own component, with the version read
from the crate's bundled `freetype.h` and a dependency edge from freetype-sys (P6-E review L4).
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
MANIFEST = REPO / "crates/ninesql-ui/Cargo.toml"
TARGET = "x86_64-unknown-linux-gnu"
GENERATED_NAME = "ninesql.cdx.json"
GENERATED = MANIFEST.parent / GENERATED_NAME


def generated_files() -> list[Path]:
    """cargo-cyclonedx writes one SBOM next to the manifest of every workspace member."""
    return sorted((REPO / "crates").rglob(GENERATED_NAME))


def linked_crates() -> set[tuple[str, str]]:
    out = subprocess.run(
        [
            "cargo", "tree", "--frozen", "--manifest-path", str(MANIFEST),
            "-e", "normal", "--target", TARGET, "--prefix", "none", "--format", "{p}",
        ],
        check=True, capture_output=True, text=True, cwd=REPO,
    ).stdout
    crates = set()
    for line in out.splitlines():
        parts = line.split()
        if len(parts) >= 2 and parts[1].startswith("v"):
            crates.add((parts[0], parts[1][1:]))
    return crates


FREETYPE_CRATE = "freetype-sys"
# FreeType's own terms (freetype2/LICENSE.TXT): the FTL or the GPLv2 "(any later version can be
# used also)". NineSQL takes it under the FTL (I7, ADR-0002); the property records that choice.
FREETYPE_LICENSE = "FTL OR GPL-2.0-or-later"


def crate_manifest(name: str, version: str) -> Path:
    out = subprocess.run(
        [
            "cargo", "metadata", "--frozen", "--format-version", "1",
            "--manifest-path", str(MANIFEST), "--filter-platform", TARGET,
        ],
        check=True, capture_output=True, text=True, cwd=REPO,
    ).stdout
    for package in json.loads(out)["packages"]:
        if package["name"] == name and package["version"] == version:
            return Path(package["manifest_path"])
    raise SystemExit(f"{name} {version} not found in cargo metadata")


def bundled_freetype_version(crate_dir: Path) -> str:
    header = (crate_dir / "freetype2/include/freetype/freetype.h").read_text(encoding="utf-8")
    parts = []
    for macro in ("FREETYPE_MAJOR", "FREETYPE_MINOR", "FREETYPE_PATCH"):
        match = re.search(rf"^#define\s+{macro}\s+(\d+)\s*$", header, re.MULTILINE)
        if not match:
            raise SystemExit(f"{macro} not found in the bundled freetype.h of {crate_dir}")
        parts.append(match.group(1))
    return ".".join(parts)


def add_bundled_freetype(bom: dict) -> str | None:
    """Adds the FreeType C library freetype-sys compiles in; returns its version, if linked."""
    crate = next(
        (c for c in bom["components"] if c.get("name") == FREETYPE_CRATE), None
    )
    if crate is None:
        return None
    crate_dir = crate_manifest(FREETYPE_CRATE, crate["version"]).parent
    version = bundled_freetype_version(crate_dir)
    ref = f"bundled-freetype@{version}"
    bom["components"].append({
        "type": "library",
        "bom-ref": ref,
        "name": "freetype",
        "version": version,
        "description": f"FreeType C library bundled by {FREETYPE_CRATE} {crate['version']} "
                       "and compiled statically into the binary",
        "licenses": [{"expression": FREETYPE_LICENSE}],
        "purl": f"pkg:generic/freetype@{version}",
        "externalReferences": [{"type": "website", "url": "https://freetype.org"}],
        "properties": [{"name": "ninesql:license-elected", "value": "FTL"}],
    })
    for dep in bom.setdefault("dependencies", []):
        if dep.get("ref") == crate.get("bom-ref"):
            dep.setdefault("dependsOn", []).append(ref)
            break
    else:
        bom["dependencies"].append({"ref": crate.get("bom-ref"), "dependsOn": [ref]})
    bom["dependencies"].append({"ref": ref, "dependsOn": []})
    return version


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    output = Path(sys.argv[1]).resolve()
    stale = generated_files()
    if stale:
        print(f"refusing to run: {stale[0]} exists (not written by this script)", file=sys.stderr)
        return 1
    env = dict(os.environ, CARGO_NET_OFFLINE="true")
    try:
        subprocess.run(
            [
                "cargo", "cyclonedx", "--manifest-path", str(MANIFEST), "-f", "json",
                "--spec-version", "1.5", "--target", TARGET, "--no-build-deps",
                "--override-filename", "ninesql.cdx", "-q",
            ],
            check=True, cwd=REPO, env=env,
        )
        bom = json.loads(GENERATED.read_text(encoding="utf-8"))
    finally:
        for path in generated_files():
            path.unlink(missing_ok=True)

    keep = linked_crates()
    removed = set()
    components = []
    for component in bom.get("components", []):
        if (component.get("name"), component.get("version")) in keep:
            components.append(component)
        else:
            removed.add(component.get("bom-ref"))
    bom["components"] = components
    dependencies = []
    for dep in bom.get("dependencies", []):
        if dep.get("ref") in removed:
            continue
        dep["dependsOn"] = [r for r in dep.get("dependsOn", []) if r not in removed]
        dependencies.append(dep)
    bom["dependencies"] = dependencies
    freetype = add_bundled_freetype(bom)
    if freetype is None:
        print(f"note: {FREETYPE_CRATE} is not linked, no bundled FreeType component", file=sys.stderr)

    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(bom, indent=2, sort_keys=False) + "\n", encoding="utf-8")
    print(
        f"{output}: {len(bom['components'])} components ({len(removed)} test-only removed"
        f"{f', bundled FreeType {freetype} added' if freetype else ''})"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
