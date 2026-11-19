#!/usr/bin/env python3
"""Writes the CycloneDX SBOM of the ninesql binary (P6-A, ADR-0010 §4).

    python3 packaging/sbom.py OUTPUT.cdx.json

Runs cargo-cyclonedx 0.5.10 on crates/ninesql-ui (JSON, CycloneDX 1.5, target
x86_64-unknown-linux-gnu, no build dependencies), then keeps only the crates linked into the
binary: cargo-cyclonedx also lists the root crate's dev-dependencies (test-only crates such as
proptest), so components outside `cargo tree -e normal` for that target are removed, with
their entries in the dependency graph. Offline (`--frozen`): crates must be fetched already.
SOURCE_DATE_EPOCH, when set, is the SBOM's timestamp.
"""

from __future__ import annotations

import json
import os
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

    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(bom, indent=2, sort_keys=False) + "\n", encoding="utf-8")
    print(f"{output}: {len(components)} components ({len(removed)} test-only removed)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
