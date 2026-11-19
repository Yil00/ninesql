#!/usr/bin/env python3
"""Checks a generated cargo-sources.json against Cargo.lock alone (P6-A review L5).

    python3 -I packaging/flatpak/verify-sources.py [cargo-sources.json] [Cargo.lock]

flatpak-cargo-generator runs with Python packages from PyPI (prepare-sources.sh). Whatever
they did, this check, which uses the standard library only, accepts the output only if it is
exactly what Cargo.lock implies: for every crates.io package, its `.crate` archive from
static.crates.io with the Cargo.lock checksum and its `.cargo-checksum.json`, and one cargo
config that replaces crates.io with those vendored sources; nothing else (no other host, no
git source, no extra file). Needs Python 3.11 or newer (tomllib).
"""

from __future__ import annotations

import json
import sys
import tomllib
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
CRATES_IO_SOURCE = "registry+https://github.com/rust-lang/crates.io-index"
CRATES_URL = "https://static.crates.io/crates"
VENDOR = "cargo/vendor"


def expected(lock: dict) -> tuple[list[dict], dict]:
    sources: list[dict] = []
    for package in lock.get("package", []):
        source = package.get("source")
        if source is None:
            continue  # A workspace member.
        if source != CRATES_IO_SOURCE:
            raise SystemExit(f"error: {package['name']}: unsupported source {source}")
        name, version, checksum = package["name"], package["version"], package["checksum"]
        dest = f"{VENDOR}/{name}-{version}"
        sources.append(
            {
                "type": "archive",
                "archive-type": "tar-gzip",
                "url": f"{CRATES_URL}/{name}/{name}-{version}.crate",
                "sha256": checksum,
                "dest": dest,
            }
        )
        sources.append(
            {
                "type": "inline",
                "contents": {"package": checksum, "files": {}},
                "dest": dest,
                "dest-filename": ".cargo-checksum.json",
            }
        )
    config = {
        "source": {
            "vendored-sources": {"directory": VENDOR},
            "crates-io": {"replace-with": "vendored-sources"},
        }
    }
    return sources, config


def main() -> int:
    sources_path = Path(sys.argv[1]) if len(sys.argv) > 1 else REPO / "packaging/flatpak/cargo-sources.json"
    lock_path = Path(sys.argv[2]) if len(sys.argv) > 2 else REPO / "Cargo.lock"
    lock = tomllib.loads(lock_path.read_text(encoding="utf-8"))
    actual = json.loads(sources_path.read_text(encoding="utf-8"))
    if not isinstance(actual, list) or not actual:
        print(f"error: {sources_path}: not a list of sources", file=sys.stderr)
        return 1
    want, want_config = expected(lock)

    *crates, config = actual
    errors: list[str] = []
    if not (
        isinstance(config, dict)
        and config.get("type") == "inline"
        and config.get("dest") == "cargo"
        and config.get("dest-filename") == "config"
        and set(config) == {"type", "contents", "dest", "dest-filename"}
    ):
        errors.append("the last source is not the cargo config")
    else:
        try:
            if tomllib.loads(config["contents"]) != want_config:
                errors.append("the cargo config does more than replace crates.io with the vendored sources")
        except tomllib.TOMLDecodeError as e:
            errors.append(f"the cargo config is not TOML: {e}")

    def normalized(entry: object) -> object:
        if isinstance(entry, dict) and entry.get("type") == "inline" and entry.get("dest-filename") == ".cargo-checksum.json":
            try:
                return {**entry, "contents": json.loads(entry["contents"])}
            except (KeyError, TypeError, json.JSONDecodeError):
                return entry
        return entry

    got = [normalized(e) for e in crates]
    key = lambda e: json.dumps(e, sort_keys=True)  # noqa: E731
    got_set = {key(e) for e in got}
    want_set = {key(e) for e in want}
    if len(got_set) != len(got):
        errors.append("duplicate sources")
    for extra in sorted(got_set - want_set):
        errors.append(f"unexpected source: {extra}")
    for missing in sorted(want_set - got_set):
        errors.append(f"missing source: {missing}")
    for error in errors[:50]:
        print(f"error: {error}", file=sys.stderr)
    if errors:
        print(f"error: {len(errors)} problem(s) in {sources_path}", file=sys.stderr)
        return 1
    print(f"ok: {sources_path.name} matches Cargo.lock ({len(want) // 2} crates)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
