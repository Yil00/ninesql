#!/usr/bin/env python3
"""Checks the Flatpak manifest of NineSQL (P6-A) without flatpak-builder.

    python3 packaging/flatpak/lint-manifest.py [manifest.yml]

- the manifest parses (YAML) and has the keys flatpak-builder needs;
- the app id matches the desktop file, the AppStream file, the icons and the UI's Wayland app id;
- every finish-arg is on the allow-list below, which records why each one is needed
  (ADR-0010 §3, ADR-0005 amendment "P1-D wiring" point 6); no home or host file access, no
  session-bus name besides the ones listed, no Secret Service unless the build enables the
  `secret-service` feature;
- the build runs cargo offline and locked (sources come from cargo-sources.json).

Exit status 0 when every check passes. Needs PyYAML (python3-yaml).
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

try:
    import yaml
except ImportError:  # pragma: no cover - environment check
    sys.exit("PyYAML is required (apt install python3-yaml)")

REPO = Path(__file__).resolve().parents[2]
DEFAULT_MANIFEST = REPO / "packaging/flatpak/io.github.yil00.NineSQL.yml"

# finish-arg -> why it is needed. Anything else fails the lint.
ALLOWED_FINISH_ARGS = {
    "--socket=wayland": "display (Wayland)",
    "--socket=fallback-x11": "display (X11 only without Wayland)",
    "--share=ipc": "X11 shared memory",
    "--device=dri": "GPU rendering (Vulkan)",
    "--share=network": "declared connections and their SSH / proxy hops only (I2)",
    "--system-talk-name=org.freedesktop.login1": "lock secrets on sleep and screen lock",
    "--filesystem=~/.ssh/known_hosts:ro": "OpenSSH host keys, read-only, the file only",
    "--filesystem=~/.local/share/DBeaverData:ro": "DBeaver import, read-only (I4)",
}
# Only with `--features ninesql-core/secret-service` in the build commands (ADR-0012).
SECRET_SERVICE_ARG = "--talk-name=org.freedesktop.secrets"
FORBIDDEN_PATTERNS = [
    (re.compile(r"^--filesystem=(home|host|host-os|host-etc|~|/)(:.*)?$"), "broad file access"),
    (re.compile(r"^--filesystem=~/\.ssh(/)?(:.*)?$"), "~/.ssh holds private keys"),
    (re.compile(r"^--socket=(session-bus|system-bus|x11)$"), "whole bus or unconditional X11"),
    (re.compile(r"^--device=all$"), "all devices"),
    (re.compile(r"^--talk-name=org\.freedesktop\.Flatpak$"), "sandbox escape"),
]


def enables_secret_service(line: str) -> bool:
    """Whether a build command line is a `cargo build` enabling the secret-service feature."""
    return bool(
        re.search(r"\bcargo\b.*\bbuild\b", line)
        and re.search(r"(?<!\S)(?:-F|--features)[= ]?\S*.*secret-service", line)
    )


def self_test() -> None:
    """Both flag forms count, and only on the cargo build line (review L4, re-review N4)."""
    build = "cargo --offline build --release --locked -p ninesql-ui --bin ninesql"
    for flags in (
        "--features ninesql-core/secret-service",
        "--features=ninesql-core/secret-service",
        "-F ninesql-core/secret-service",
        "-F=ninesql-core/secret-service",
        "-Fninesql-core/secret-service",
        "--features 'mock,ninesql-core/secret-service'",
    ):
        assert enables_secret_service(f"{build} {flags}"), flags
    for line in (
        build,
        f"{build} # secret-service",
        "echo --features ninesql-core/secret-service",
        "cargo --offline fetch --locked --features ninesql-core/secret-service",
    ):
        assert not enables_secret_service(line), line


def check(manifest_path: Path) -> list[str]:
    errors: list[str] = []
    data = yaml.safe_load(manifest_path.read_text(encoding="utf-8"))
    if not isinstance(data, dict):
        return [f"{manifest_path}: not a YAML mapping"]

    for key in ("id", "runtime", "runtime-version", "sdk", "command", "finish-args", "modules"):
        if key not in data:
            errors.append(f"missing key: {key}")
    if errors:
        return errors

    app_id = data["id"]
    if not re.fullmatch(r"[A-Za-z][A-Za-z0-9_-]*(\.[A-Za-z][A-Za-z0-9_-]*){2,}", app_id):
        errors.append(f"app id {app_id!r} is not a reverse-DNS name with at least 3 parts")
    if manifest_path.stem != app_id:
        errors.append(f"manifest file name {manifest_path.name} does not match the app id")

    linux = REPO / "packaging/linux"
    for path in (linux / f"{app_id}.desktop", linux / f"{app_id}.metainfo.xml"):
        if not path.is_file():
            errors.append(f"missing {path.relative_to(REPO)}")
    desktop = linux / f"{app_id}.desktop"
    if desktop.is_file():
        text = desktop.read_text(encoding="utf-8")
        for line in (f"Icon={app_id}", f"StartupWMClass={app_id}", f"Exec={data['command']}"):
            if line not in text.splitlines():
                errors.append(f"{desktop.name}: expected the line {line!r}")
    metainfo = linux / f"{app_id}.metainfo.xml"
    if metainfo.is_file():
        text = metainfo.read_text(encoding="utf-8")
        if f"<id>{app_id}</id>" not in text:
            errors.append(f"{metainfo.name}: <id> is not {app_id}")
        if f'<launchable type="desktop-id">{app_id}.desktop</launchable>' not in text:
            errors.append(f"{metainfo.name}: launchable is not {app_id}.desktop")
    app_rs = REPO / "crates/ninesql-ui/src/app.rs"
    if f'app_id: Some("{app_id}".to_owned())' not in app_rs.read_text(encoding="utf-8"):
        errors.append("crates/ninesql-ui/src/app.rs: the window app_id differs from the app id")

    build_commands = [
        command
        for module in data["modules"]
        if isinstance(module, dict)
        for command in module.get("build-commands", [])
    ]
    joined = "\n".join(build_commands)
    # The feature is enabled only on the `cargo build` line (review L4): a comment or another
    # command naming secret-service does not count. `-F x`, `-F=x`, `-Fx`, `--features x` and
    # `--features=x` all enable it (re-review N4).
    secret_service_built = any(enables_secret_service(line) for line in joined.splitlines())

    for arg in data["finish-args"]:
        for pattern, why in FORBIDDEN_PATTERNS:
            if pattern.match(arg):
                errors.append(f"finish-arg {arg}: forbidden ({why})")
        if arg == SECRET_SERVICE_ARG:
            if not secret_service_built:
                errors.append(
                    f"finish-arg {arg}: the build does not enable the secret-service feature"
                )
            continue
        if arg not in ALLOWED_FINISH_ARGS:
            errors.append(f"finish-arg {arg}: not on the allow-list of lint-manifest.py")
    if secret_service_built and SECRET_SERVICE_ARG not in data["finish-args"]:
        errors.append(f"the build enables secret-service but {SECRET_SERVICE_ARG} is missing")
    for required in ("--share=network", "--system-talk-name=org.freedesktop.login1"):
        if required not in data["finish-args"]:
            errors.append(f"missing finish-arg {required} (ADR-0005 amendment, P1-D wiring)")

    cargo_lines = [line for line in joined.splitlines() if re.search(r"\bcargo\b", line)]
    if not cargo_lines:
        errors.append("no cargo command in the build")
    for line in cargo_lines:
        if "--offline" not in line or "--locked" not in line:
            errors.append(f"cargo must run --offline --locked: {line.strip()}")
    for module in data["modules"]:
        if not isinstance(module, dict):
            continue
        sources = module.get("sources", [])
        if "cargo-sources.json" not in sources:
            errors.append(f"module {module.get('name')}: cargo-sources.json is not a source")
        for source in sources:
            if isinstance(source, dict) and source.get("type") in ("git", "archive", "file"):
                if "url" in source and not ("sha256" in source or "commit" in source):
                    errors.append(f"source {source.get('url')}: no sha256 or commit")
    if data.get("build-options", {}).get("env", {}).get("CARGO_NET_OFFLINE") != "true":
        errors.append("build-options.env.CARGO_NET_OFFLINE must be 'true'")
    return errors


def main() -> int:
    self_test()
    manifest = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else DEFAULT_MANIFEST
    errors = check(manifest)
    for error in errors:
        print(f"error: {error}", file=sys.stderr)
    if errors:
        return 1
    shown = manifest.relative_to(REPO) if manifest.is_relative_to(REPO) else manifest
    print(f"ok: {shown} ({len(ALLOWED_FINISH_ARGS)} permissions on the list)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
