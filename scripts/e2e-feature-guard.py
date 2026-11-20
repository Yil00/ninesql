#!/usr/bin/env python3
"""Manifest guard of the end-to-end scenario runner (ADR-0029 §2.2, guard 3).

    python3 scripts/e2e-feature-guard.py [--metadata FILE]

Reads `cargo metadata --format-version 1 --no-deps` (run here, or the JSON in FILE, `-` for
stdin) and fails (exit 1) when the test-only `e2e-scenarios` feature of `ninesql-ui` or of
`ninesql-core` could reach a normal build:

- `e2e-scenarios` is reachable from the `default` feature of `ninesql-ui` or of `ninesql-core`
  (directly, through other features of the crate, or, for `ninesql-ui`, by enabling
  `ninesql-core/e2e-scenarios`);
- a workspace crate turns it on for `ninesql-ui` or `ninesql-core` in a normal or build
  dependency (`features = ["e2e-scenarios"]`; dev-dependencies are allowed: tests only);
- a feature of any workspace crate enables `ninesql-ui/e2e-scenarios` or
  `ninesql-core/e2e-scenarios` (also the `?/` form), except the `e2e-scenarios` feature of
  `ninesql-ui` itself, which may enable the core's guards (ADR-0029 §2.5).

It passes whether or not the crates define the feature. Exit status 2 on a usage or read error.
Run by `make e2e-feature-guard` (part of `make check`) and the CI `deny` job; tested by
scripts/e2e-feature-guard-test.py.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

FEATURE = "e2e-scenarios"
GUARDED = ("ninesql-ui", "ninesql-core")
REPO = Path(__file__).resolve().parent.parent


def dep_keys(package: dict) -> dict[str, str]:
    """Maps the key a crate uses for each dependency (its rename, else its name) to the name."""
    return {(d.get("rename") or d["name"]): d["name"] for d in package.get("dependencies", [])}


def enabled_dep_feature(entry: str, keys: dict[str, str]) -> tuple[str, str] | None:
    """For a feature entry `dep/feat` or `dep?/feat`, returns (dependency name, feat)."""
    if "/" not in entry or entry.startswith("dep:"):
        return None
    dep, feat = entry.split("/", 1)
    dep = dep.rstrip("?")
    return keys.get(dep, dep), feat


def default_closure(package: dict) -> set[str]:
    """Every feature entry reachable from `default` inside the crate's feature table."""
    features = package.get("features", {})
    seen: set[str] = set()
    todo = list(features.get("default", []))
    while todo:
        entry = todo.pop()
        if entry in seen:
            continue
        seen.add(entry)
        if entry in features:
            todo.extend(features[entry])
    return seen


def check(metadata: dict) -> list[str]:
    errors: list[str] = []
    members = set(metadata.get("workspace_members", []))
    packages = [p for p in metadata.get("packages", []) if not members or p["id"] in members]
    names = {p["name"] for p in packages}
    for guarded in GUARDED:
        if guarded not in names:
            errors.append(f"{guarded} is not a workspace member: the guard checks nothing")

    for package in packages:
        name = package["name"]
        keys = dep_keys(package)

        if name in GUARDED:
            for entry in sorted(default_closure(package)):
                target = enabled_dep_feature(entry, keys)
                if entry == FEATURE or (target is not None and target[0] in GUARDED and target[1] == FEATURE):
                    errors.append(f"{name}: `{entry}` is reachable from the default feature")

        for dep in package.get("dependencies", []):
            if dep["name"] in GUARDED and dep.get("kind") != "dev" and FEATURE in dep.get("features", []):
                kind = dep.get("kind") or "normal"
                errors.append(f"{name}: enables {dep['name']}/{FEATURE} in a {kind} dependency")

        for feature, entries in sorted(package.get("features", {}).items()):
            for entry in entries:
                target = enabled_dep_feature(entry, keys)
                if target is None or target[0] not in GUARDED or target[1] != FEATURE:
                    continue
                if name == "ninesql-ui" and feature == FEATURE and target[0] == "ninesql-core":
                    continue  # the UI's test feature turns on the core's guards
                errors.append(f"{name}: feature `{feature}` enables `{entry}`")
    return errors


def load(path: str | None) -> dict:
    if path == "-":
        return json.load(sys.stdin)
    if path is not None:
        return json.loads(Path(path).read_text(encoding="utf-8"))
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps", "--locked"],
        cwd=REPO,
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(out.stdout)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--metadata", help="cargo metadata JSON file (`-` for stdin)")
    args = parser.parse_args(argv)
    try:
        metadata = load(args.metadata)
    except (OSError, ValueError, subprocess.CalledProcessError) as err:
        detail = getattr(err, "stderr", "") or err
        print(f"error: cannot read cargo metadata: {detail}", file=sys.stderr)
        return 2
    errors = check(metadata)
    for error in errors:
        print(f"error: {error} (ADR-0029 §2.2: {FEATURE} is a test build only)", file=sys.stderr)
    if errors:
        return 1
    print(f"ok: {FEATURE} is off by default and enabled by no workspace crate")
    return 0


if __name__ == "__main__":
    sys.exit(main())
