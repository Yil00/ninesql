#!/usr/bin/env python3
"""Tests of scripts/e2e-feature-guard.py on synthetic `cargo metadata` documents.

    python3 scripts/e2e-feature-guard-test.py
"""

from __future__ import annotations

import copy
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

_SPEC = importlib.util.spec_from_file_location(
    "e2e_feature_guard", Path(__file__).resolve().parent / "e2e-feature-guard.py"
)
assert _SPEC is not None and _SPEC.loader is not None
guard = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(guard)


def dep(name: str, features: list[str] | None = None, kind: str | None = None, rename: str | None = None) -> dict:
    return {"name": name, "features": features or [], "kind": kind, "rename": rename}


def package(name: str, features: dict | None = None, deps: list[dict] | None = None) -> dict:
    return {"id": f"path+file:///w/{name}#0.0.0", "name": name, "features": features or {}, "dependencies": deps or []}


def metadata(*packages: dict) -> dict:
    return {"packages": list(packages), "workspace_members": [p["id"] for p in packages]}


# The workspace as it stands once the UI feature lands: off by default, the UI's feature turns
# on the core's guards, the UI tests enable the core feature through a dev-dependency.
GOOD = metadata(
    package(
        "ninesql-core",
        {"default": ["desktop-lock"], "desktop-lock": ["dep:zbus"], "e2e-scenarios": ["tokio/net", "dep:async-trait"]},
        [dep("tokio"), dep("zbus")],
    ),
    package(
        "ninesql-ui",
        {"default": [], "e2e-scenarios": ["ninesql-core/e2e-scenarios"]},
        [dep("ninesql-core"), dep("ninesql-core", ["e2e-scenarios"], kind="dev")],
    ),
    package("ninesql-transfer", {}, [dep("ninesql-core")]),
)


class GuardTest(unittest.TestCase):
    def errors(self, meta: dict) -> list[str]:
        return guard.check(meta)

    def test_good_workspace_passes(self) -> None:
        self.assertEqual(self.errors(GOOD), [])

    def test_before_the_ui_feature_lands(self) -> None:
        meta = copy.deepcopy(GOOD)
        meta["packages"][1]["features"] = {}
        self.assertEqual(self.errors(meta), [])

    def test_ui_default_fails(self) -> None:
        meta = copy.deepcopy(GOOD)
        meta["packages"][1]["features"]["default"] = ["e2e-scenarios"]
        self.assertTrue(any("ninesql-ui" in e and "default" in e for e in self.errors(meta)))

    def test_core_default_fails(self) -> None:
        meta = copy.deepcopy(GOOD)
        meta["packages"][0]["features"]["default"].append("e2e-scenarios")
        self.assertTrue(any("ninesql-core" in e and "default" in e for e in self.errors(meta)))

    def test_default_through_another_feature_fails(self) -> None:
        meta = copy.deepcopy(GOOD)
        meta["packages"][1]["features"]["default"] = ["extras"]
        meta["packages"][1]["features"]["extras"] = ["e2e-scenarios"]
        self.assertTrue(any("default" in e for e in self.errors(meta)))

    def test_ui_default_enabling_core_feature_fails(self) -> None:
        meta = copy.deepcopy(GOOD)
        meta["packages"][1]["features"]["default"] = ["ninesql-core?/e2e-scenarios"]
        self.assertTrue(any("default" in e for e in self.errors(meta)))

    def test_normal_dependency_feature_fails(self) -> None:
        for target in guard.GUARDED:
            for kind in (None, "build"):
                meta = copy.deepcopy(GOOD)
                meta["packages"][2]["dependencies"].append(dep(target, ["e2e-scenarios"], kind=kind))
                self.assertTrue(
                    any(f"{target}/e2e-scenarios" in e for e in self.errors(meta)), (target, kind)
                )

    def test_renamed_dependency_feature_fails(self) -> None:
        meta = copy.deepcopy(GOOD)
        meta["packages"][2]["dependencies"].append(dep("ninesql-ui", rename="ui"))
        meta["packages"][2]["features"] = {"e2e": ["ui/e2e-scenarios"]}
        self.assertTrue(any("ui/e2e-scenarios" in e for e in self.errors(meta)))

    def test_other_crate_feature_enabling_it_fails(self) -> None:
        for entry in ("ninesql-ui/e2e-scenarios", "ninesql-core?/e2e-scenarios"):
            meta = copy.deepcopy(GOOD)
            meta["packages"][2]["features"] = {"testing": [entry]}
            self.assertTrue(any(entry in e for e in self.errors(meta)), entry)

    def test_other_ui_feature_enabling_core_feature_fails(self) -> None:
        meta = copy.deepcopy(GOOD)
        meta["packages"][1]["features"]["extras"] = ["ninesql-core/e2e-scenarios"]
        self.assertTrue(any("extras" in e for e in self.errors(meta)))

    def test_dev_dependency_allowed(self) -> None:
        meta = copy.deepcopy(GOOD)
        meta["packages"][2]["dependencies"].append(dep("ninesql-ui", ["e2e-scenarios"], kind="dev"))
        self.assertEqual(self.errors(meta), [])

    def test_missing_guarded_crate_fails(self) -> None:
        meta = copy.deepcopy(GOOD)
        del meta["packages"][1]
        meta["workspace_members"] = [p["id"] for p in meta["packages"]]
        self.assertTrue(any("not a workspace member" in e for e in self.errors(meta)))

    def test_main_exit_status_on_a_metadata_file(self) -> None:
        bad = copy.deepcopy(GOOD)
        bad["packages"][1]["features"]["default"] = ["e2e-scenarios"]
        with tempfile.TemporaryDirectory() as tmp:
            for meta, expected in ((GOOD, 0), (bad, 1)):
                path = Path(tmp) / "metadata.json"
                path.write_text(json.dumps(meta), encoding="utf-8")
                with redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
                    self.assertEqual(guard.main(["--metadata", str(path)]), expected)
            (Path(tmp) / "broken.json").write_text("{", encoding="utf-8")
            with redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
                self.assertEqual(guard.main(["--metadata", str(Path(tmp) / "broken.json")]), 2)


if __name__ == "__main__":
    sys.exit(0 if unittest.main(exit=False).result.wasSuccessful() else 1)
