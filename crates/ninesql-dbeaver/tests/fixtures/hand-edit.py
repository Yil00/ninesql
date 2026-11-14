#!/usr/bin/env python3
"""Hand edits applied by generate.sh between the two DBeaver runs.

DBeaver's `-con` command-line option cannot set a connection type, the read-only
flag, network handlers (SSH, TLS), bootstrap queries or descriptions in any of the
versions used here (handler.* parameters are only used to match existing
connections). This script adds them to data-sources.json in DBeaver's own
structure; generate.sh then starts DBeaver again so that DBeaver re-reads and
re-serializes the file. Whatever survives that round trip is DBeaver's own output.

All values are fake. Connections are matched by name, never by generated id.
Usage: hand-edit.py <path/to/data-sources.json>
"""

import json
import sys


def by_name(connections, name):
    for conn in connections.values():
        if conn.get("name") == name:
            return conn
    raise SystemExit(f"connection {name!r} not found")


def main(path):
    with open(path, encoding="utf-8") as f:
        data = json.load(f)
    conns = data["connections"]

    # Folder description.
    data["folders"].setdefault("Team A", {})["description"] = "Synthetic fixture folder"

    # Custom connection type, used by clickhouse-analytics.
    data.setdefault("connection-types", {})["staging"] = {
        "name": "Staging",
        "color": "0,128,128",
        "description": "Custom connection type (synthetic fixture)",
        "auto-commit": False,
        "confirm-execute": True,
        "confirm-data-change": True,
        "auto-close-transactions": False,
    }

    # pg-dev: bootstrap (default schema, init query).
    pg_dev = by_name(conns, "pg-dev")
    pg_dev["configuration"]["bootstrap"] = {
        "defaultSchema": "app",
        "query": ["SET statement_timeout = 30000"],
    }

    # pg-prod-ssh: production type, read-only, SSH tunnel with password authentication.
    pg_prod = by_name(conns, "pg-prod-ssh")
    pg_prod["read-only"] = True
    pg_prod["configuration"]["type"] = "prod"
    pg_prod["configuration"]["handlers"] = {
        "ssh_tunnel": {
            "type": "TUNNEL",
            "enabled": True,
            "save-password": True,
            "user": "nine_ssh",
            "password": "fixture-ssh-not-a-secret",
            "properties": {
                "host": "bastion.example.invalid",
                "port": "22",
                "authType": "PASSWORD",
                "implementation": "sshj",
            },
        }
    }

    # pg-url-tls: PostgreSQL TLS handler, verify-full with certificate paths.
    pg_url = by_name(conns, "pg-url-tls")
    pg_url["configuration"]["handlers"] = {
        "postgre_ssl": {
            "type": "CONFIG",
            "enabled": True,
            "save-password": False,
            "properties": {
                "sslMode": "verify-full",
                "ssl.method": "CERTIFICATES",
                "ssl.ca.cert": "/home/nine_user/certs/fixture-ca.pem",
                "ssl.client.cert": "/home/nine_user/certs/fixture-client.pem",
                "ssl.client.key": "/home/nine_user/certs/fixture-client.key",
            },
        }
    }

    # mysql-test: test type.
    by_name(conns, "mysql-test")["configuration"]["type"] = "test"

    # mariadb-ssh-key: SSH tunnel with a public key (saved passphrase) and MySQL TLS
    # handler that does not verify the server certificate.
    mariadb = by_name(conns, "mariadb-ssh-key")
    mariadb["configuration"]["handlers"] = {
        "ssh_tunnel": {
            "type": "TUNNEL",
            "enabled": True,
            "save-password": True,
            "user": "nine_ssh",
            "password": "fixture-passphrase-not-a-secret",
            "properties": {
                "host": "bastion.example.invalid",
                "port": "2222",
                "authType": "PUBLIC_KEY",
                "keyPath": "/home/nine_user/.ssh/id_fixture_ed25519",
                "implementation": "sshj",
            },
        },
        "mysql_ssl": {
            "type": "CONFIG",
            "enabled": True,
            "save-password": False,
            "properties": {
                "ssl.require": True,
                "ssl.verify.server": False,
                "ssl.public.key.retrieve": False,
                "ssl.ca.cert": "/home/nine_user/certs/fixture-ca.pem",
            },
        },
    }

    # sqlite-local: description.
    by_name(conns, "sqlite-local")["description"] = "Local SQLite file (synthetic fixture)"

    # clickhouse-analytics: custom connection type.
    by_name(conns, "clickhouse-analytics")["configuration"]["type"] = "staging"

    with open(path, "w", encoding="utf-8") as f:
        json.dump(data, f, indent="\t")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    main(sys.argv[1])
