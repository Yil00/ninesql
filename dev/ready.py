#!/usr/bin/env python3
"""Start, wait for and validate the NineSQL dev databases (ADR-0009).

Usage (from the repository root):
    python3 dev/ready.py check                 # engines.json <-> compose.yaml, no containers
    python3 dev/ready.py up <profile>          # docker compose up -d, then wait
    python3 dev/ready.py wait <profile>        # wait for running services
    python3 dev/ready.py up sql --engines timescaledb,tidb   # only these engines of the profile
    python3 dev/ready.py down                  # stop everything, delete volumes

A profile is ready when every engine container of the profile reports
"healthy" (the healthchecks query the seed marker table, so healthy means
seeded) and every one-shot "<engine>-seed" container has exited with 0.

Standard library only (Python 3.9+). Never prints credentials.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time
from pathlib import Path

DEV_DIR = Path(__file__).resolve().parent
COMPOSE_FILE = DEV_DIR / "compose.yaml"
ENGINES_FILE = DEV_DIR / "engines.json"
ALL_PROFILES = ("core", "sql", "nosql", "heavy", "all")


def compose(*args: str, capture: bool = False) -> subprocess.CompletedProcess:
    cmd = ["docker", "compose", "-f", str(COMPOSE_FILE), *args]
    return subprocess.run(cmd, check=False, text=True, capture_output=capture)


def load_engines() -> dict:
    with ENGINES_FILE.open(encoding="utf-8") as f:
        return json.load(f)


def profile_engines(doc: dict, profile: str) -> list[dict]:
    included = doc["profiles"][profile]
    return [e for e in doc["engines"] if e["profile"] in included]


def select_engines(doc: dict, profile: str, only: list[str] | None) -> list[dict]:
    """The engines of `profile`, restricted to the ids in `only` when given."""
    engines = profile_engines(doc, profile)
    if only is None:
        return engines
    known = {e["id"] for e in engines}
    unknown = [i for i in only if i not in known]
    if unknown:
        raise SystemExit(f"error: not in profile '{profile}': {', '.join(unknown)}")
    return [e for e in engines if e["id"] in only]


def expected_services(doc: dict, profile: str, only: list[str] | None = None) -> tuple[list[str], list[str]]:
    engines = [e for e in select_engines(doc, profile, only) if e["service"]]
    services = [e["service"] for e in engines]
    seeds = [e["readiness"]["seed_service"] for e in engines if e["readiness"]["seed_service"]]
    return services, seeds


# --------------------------------------------------------------- check ---

def check() -> int:
    doc = load_engines()
    errors: list[str] = []
    proc = compose("--profile", "all", "config", "--format", "json", capture=True)
    if proc.returncode != 0:
        print(proc.stderr, file=sys.stderr)
        return 1
    config = json.loads(proc.stdout)
    services = config["services"]

    # Every service: digest-pinned image, loopback-only ports, known profiles.
    for name, svc in services.items():
        image = svc.get("image", "")
        if "@sha256:" not in image or ":" not in image.split("@")[0]:
            errors.append(f"{name}: image must be 'name:tag@sha256:digest', got {image!r}")
        for port in svc.get("ports", []):
            if port.get("host_ip") != "127.0.0.1":
                errors.append(f"{name}: port {port.get('target')} must bind 127.0.0.1, got {port.get('host_ip')!r}")
        profiles = set(svc.get("profiles", []))
        if not profiles or not profiles <= set(ALL_PROFILES) or "all" not in profiles:
            errors.append(f"{name}: profiles {sorted(profiles)} must be a subset of {ALL_PROFILES} and include 'all'")

    # Every engine of engines.json matches its service.
    seen_ports: dict[int, str] = {}
    referenced = set()
    for e in doc["engines"]:
        eid = e["id"]
        for f in e["seed"]:
            if not (DEV_DIR / f).is_file():
                errors.append(f"{eid}: seed file {f} does not exist")
        if e["service"] is None:
            if e["image"] or e["port"]:
                errors.append(f"{eid}: file engine must have no image and no port")
            continue
        svc = services.get(e["service"])
        if svc is None:
            errors.append(f"{eid}: service {e['service']!r} missing from compose.yaml")
            continue
        referenced.add(e["service"])
        if svc["image"] != e["image"]:
            errors.append(f"{eid}: image {e['image']!r} != compose {svc['image']!r}")
        published = {int(p["published"]): int(p["target"]) for p in svc.get("ports", [])}
        wanted = [e["port"], *e.get("extra_ports", {}).values()]
        for port in wanted:
            if port not in published:
                errors.append(f"{eid}: port {port} not published by service {e['service']}")
            if port in seen_ports:
                errors.append(f"{eid}: port {port} already used by {seen_ports[port]}")
            seen_ports[port] = eid
        expected_profiles = {
            "core": {"core", "sql", "all"},
            "sql": {"sql", "all"},
            "nosql": {"nosql", "all"},
            "heavy": {"heavy", "all"},
        }[e["profile"]]
        if set(svc.get("profiles", [])) != expected_profiles:
            errors.append(f"{eid}: service profiles {svc.get('profiles')} != {sorted(expected_profiles)}")
        if "healthcheck" not in svc:
            errors.append(f"{eid}: service has no healthcheck")
        seed = e["readiness"]["seed_service"]
        if seed:
            referenced.add(seed)
            seed_svc = services.get(seed)
            if seed_svc is None:
                errors.append(f"{eid}: seed service {seed!r} missing from compose.yaml")
            elif set(seed_svc.get("profiles", [])) != expected_profiles:
                errors.append(f"{seed}: profiles must match its engine's")
            elif seed_svc.get("ports"):
                errors.append(f"{seed}: a seed service must not publish ports")
        # A one-shot service that makes test-only TLS certificates for an image without
        # openssl: the engine waits for it (depends_on), so `up` starts it with its engine.
        tls_service = (e.get("tls") or {}).get("service")
        if tls_service:
            referenced.add(tls_service)
            tls_svc = services.get(tls_service)
            if tls_svc is None:
                errors.append(f"{eid}: TLS service {tls_service!r} missing from compose.yaml")
            elif set(tls_svc.get("profiles", [])) != expected_profiles:
                errors.append(f"{tls_service}: profiles must match its engine's")
            elif tls_svc.get("ports"):
                errors.append(f"{tls_service}: a TLS service must not publish ports")
            elif (svc.get("depends_on") or {}).get(tls_service, {}).get("condition") != "service_completed_successfully":
                errors.append(f"{eid}: must depend on {tls_service} with condition service_completed_successfully")

    for name in services:
        if name not in referenced:
            errors.append(f"{name}: service not referenced by engines.json")

    ids = [e["id"] for e in doc["engines"]]
    if len(ids) != 20 or len(set(ids)) != 20:
        errors.append(f"engines.json must list the 20 v1 engines exactly once (ADR-0003), got {len(ids)}")

    for err in errors:
        print(f"error: {err}", file=sys.stderr)
    if errors:
        return 1
    print(f"ok: {len(ids)} engines, {len(services)} services, all images pinned, all ports on 127.0.0.1")
    return 0


# ---------------------------------------------------------------- wait ---

def container_states() -> dict[str, dict]:
    proc = compose("--profile", "all", "ps", "--all", "--format", "json", capture=True)
    if proc.returncode != 0:
        raise RuntimeError(proc.stderr.strip())
    out = proc.stdout.strip()
    if not out:
        return {}
    # Compose prints either a JSON array or one JSON object per line.
    items = json.loads(out) if out.startswith("[") else [json.loads(line) for line in out.splitlines()]
    return {item["Service"]: item for item in items}


def wait(profile: str, timeout: int, only: list[str] | None = None) -> int:
    doc = load_engines()
    services, seeds = expected_services(doc, profile, only)
    deadline = time.monotonic() + timeout
    start = time.monotonic()
    reported: set[str] = set()
    while True:
        states = container_states()
        pending: list[str] = []
        failed: list[str] = []
        for name in services:
            st = states.get(name)
            if st is None:
                failed.append(f"{name}: not created")
            elif st.get("State") != "running":
                failed.append(f"{name}: {st.get('State')} ({st.get('Status')})")
            elif st.get("Health") == "unhealthy":
                failed.append(f"{name}: unhealthy")
            elif st.get("Health") != "healthy":
                pending.append(name)
            elif name not in reported:
                reported.add(name)
                print(f"  ready  {name:<22} {time.monotonic() - start:6.0f}s", flush=True)
        for name in seeds:
            st = states.get(name)
            if st is None:
                failed.append(f"{name}: not created")
            elif st.get("State") == "exited":
                if st.get("ExitCode") != 0:
                    failed.append(f"{name}: exited with {st.get('ExitCode')}")
                elif name not in reported:
                    reported.add(name)
                    print(f"  seeded {name:<22} {time.monotonic() - start:6.0f}s", flush=True)
            else:
                pending.append(name)
        if failed:
            for f in failed:
                print(f"error: {f}", file=sys.stderr)
            for name in sorted({f.split(":")[0] for f in failed}):
                compose("logs", "--no-color", "--tail", "40", name)
            return 1
        if not pending:
            print(f"profile '{profile}' ready: {len(services)} engines in {time.monotonic() - start:.0f}s")
            return 0
        if time.monotonic() > deadline:
            print(f"error: timeout after {timeout}s, still waiting for: {', '.join(pending)}", file=sys.stderr)
            for name in pending:
                compose("logs", "--no-color", "--tail", "40", name)
            return 1
        time.sleep(3)


def up(profile: str, timeout: int, only: list[str] | None = None) -> int:
    services: list[str] = []
    if only is not None:
        engines, seeds = expected_services(load_engines(), profile, only)
        services = engines + seeds
    proc = compose("--profile", profile, "up", "--detach", *services)
    if proc.returncode != 0:
        return proc.returncode
    return wait(profile, timeout, only)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="cmd", required=True)
    sub.add_parser("check", help="validate engines.json against compose.yaml")
    for name in ("up", "wait"):
        p = sub.add_parser(name)
        p.add_argument("profile", choices=ALL_PROFILES)
        p.add_argument("--timeout", type=int, default=900, help="seconds (default 900)")
        p.add_argument(
            "--engines",
            help="comma-separated engine ids of the profile to start or wait for (default: all)",
        )
    sub.add_parser("down", help="stop all services and delete their volumes")
    args = parser.parse_args()
    if args.cmd == "check":
        return check()
    only = [e.strip() for e in args.engines.split(",") if e.strip()] if getattr(args, "engines", None) else None
    if args.cmd == "up":
        return up(args.profile, args.timeout, only)
    if args.cmd == "wait":
        return wait(args.profile, args.timeout, only)
    return compose("--profile", "all", "down", "--volumes", "--remove-orphans").returncode


if __name__ == "__main__":
    sys.exit(main())
