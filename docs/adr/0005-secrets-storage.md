# ADR-0005: Secrets storage

- **Status**: Accepted. Partially superseded by [ADR-0012](0012-secret-service-client-and-msrv.md) for the client crate (`secret-service` 6 instead of `keyring`) and the Secret Service item attributes; everything else stands. Note (P1-D, 2026-11-14): the vault of §2 is implemented as format v1; its bounds and known limits are documented in the rustdoc of `ninesql_secrets::vault` and in [docs/05](../05-security.md#vault-format-v1).
- **Date**: 2026-11-14

## Context
A database client holds the keys to production. DBeaver Community stores saved passwords in `credentials-config.json`, encrypted with a key that is the same for every installation and public in its source code: anyone who can read the file can decrypt it. NineSQL must do better (I1) while still working on minimal desktops without a keyring daemon.

## Decision
1. **Primary store**: the freedesktop **Secret Service** API (GNOME Keyring, KWallet ≥ 5.97 / KeePassXC with Secret Service enabled), through the `keyring` crate's Secret Service backend (pure-Rust D-Bus, no `libsecret` C dependency when possible). Items use `service = "ninesql"` and attributes `connection_id`, `kind` (`password`, `ssh_password`, `ssh_key_passphrase`, `token`, `client_key_passphrase`).
2. **Fallback store**: when no Secret Service answers, NineSQL offers an **encrypted vault** (`$XDG_DATA_HOME/ninesql/vault.bin`, mode `0600`): key derived from a master password with **Argon2id** (parameters stored in the header, default m = 64 MiB, t = 3, p = 1), entries sealed with **XChaCha20-Poly1305**, the whole file authenticated. The vault is locked at startup and after a configurable idle time.
3. **Never on disk in clear**: workspace files store only a secret reference (`{ "store": "keyring" | "vault", "id": "…" }`). "Do not save password" asks at connect time and keeps the secret in memory for the session only.
4. **In memory**: secrets are held in `zeroize`-on-drop types (`secrecy::SecretString`), never `Debug`-printed, passed to drivers only at connect time.
5. **Flatpak**: access to Secret Service goes through the `org.freedesktop.secrets` D-Bus name (`--talk-name`), declared in the manifest; the vault fallback works inside the sandbox.
6. Private SSH keys stay where the user keeps them (file path or `ssh-agent`); NineSQL stores only the path and, if asked, the passphrase in the store.

## Consequences
- Headless or exotic sessions get a master-password prompt instead of silent plaintext storage.
- Importing from DBeaver moves passwords into the keyring or the vault (ADR-0006); the import report says how many were moved.
- Tests need a Secret Service in CI: a `gnome-keyring` or KeePassXC D-Bus service in a container, plus an in-memory store for unit tests behind a test-only feature.

## Rejected alternatives
- **A fixed or machine-derived key** (DBeaver's approach): obfuscation, not protection.
- **Plaintext with `0600` permissions**: readable by any process of the user and by backups.
- **`libsecret` through C bindings**: works, but adds a C dependency and `unsafe` where a pure-Rust D-Bus client exists.
