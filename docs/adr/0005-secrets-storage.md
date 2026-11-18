# ADR-0005: Secrets storage

- **Status**: Accepted. Partially superseded by [ADR-0012](0012-secret-service-client-and-msrv.md) for the client crate (`secret-service` 6 instead of `keyring`) and the Secret Service item attributes; everything else stands. Note (P1-D, 2026-11-14): the vault of §2 is implemented as format v1; its bounds and known limits are documented in the rustdoc of `ninesql_secrets::vault` and in [docs/05](../05-security.md#vault-format-v1). Amended (P1-D wiring, 2026-11-15) by the [section at the end](#amendment-p1-d-wiring-2026-11-15): locking on sleep and screen lock in default builds, and the Flatpak permissions it needs; the decision above is unchanged.
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

## Amendment (P1-D wiring, 2026-11-15)
Approved by the maintainer. It adds to the decision above and changes none of its points. Behaviour as implemented: `crates/ninesql-secrets/Cargo.toml`, `crates/ninesql-secrets/src/lock_signals.rs`, [docs/02](../02-architecture.md#secrets-adr-0005-p1-d), [docs/05](../05-security.md#vault-format-v1) and [docs/10](../10-supported-platforms.md#flatpak-permissions).

**Context.** The vault of point 2 and the "ask every time" cache of point 3 hold decrypted secrets in memory. Locking them when the machine sleeps or the screen locks first sat behind the `secret-service` feature, which stays off by default under [ADR-0012](0012-secret-service-client-and-msrv.md). Default builds therefore had only the idle lock.

**Decision.**
1. Locking secrets on sleep and screen lock (`DesktopLockWatcher`, logind on the system bus) sits behind its own Cargo feature, `desktop-lock`, in `ninesql-secrets` and `ninesql-core`. It is **on by default**. It depends on zbus 6 only: the same zbus as the Secret Service client, with no Secret Service code and no crypto.
2. The `secret-service` feature implies `desktop-lock`. It stays off by default, per ADR-0012.
3. On sleep (`PrepareForSleep(true)`) and on screen lock of the current logind session (its `Lock` signal, or its `LockedHint` property turning true), the vault is locked and the "ask every time" cache cleared.
4. On its own system-bus connection, the watcher talks only to `org.freedesktop.login1` (session lookup, `Inhibit`) and to the bus daemon (`GetNameOwner`, `NameOwnerChanged`). It holds a logind sleep **delay inhibitor**, so the suspend waits (up to logind's `InhibitDelayMaxSec`) until the lock has run. It accepts only broadcast signals (no destination header) whose sender is the current unique owner of `org.freedesktop.login1`. When it loses the bus connection, it locks once and stops (fail-closed). It is not restarted.
5. A failure to start the watcher (no system bus, or no permission to reach logind) is not fatal. The idle lock still applies: vault idle timeout 15 min by default, checked every 30 s (`CoreConfig`). `CoreServices::lock_watcher_status` reports whether sleep and screen lock are watched.
6. **Flatpak** (completes point 5 above and [ADR-0010](0010-packaging-and-distribution.md) §3): besides `--talk-name=org.freedesktop.secrets`, the manifest declares `--system-talk-name=org.freedesktop.login1` for the watcher. It also declares `--share=network`. That permission is all-or-nothing in Flatpak, so the limit to the user's connections and their SSH / proxy hops (I2) is enforced by NineSQL itself, not by the sandbox.

**Consequences.**
- Default builds lock secrets on sleep and screen lock without enabling the Secret Service backend, which is not validated yet (ADR-0012).
- Default builds open a system-bus connection to logind. That is local D-Bus traffic, not network traffic, so I2 is unaffected.
- On a system without logind, or in a Flatpak without the logind permission, only the idle lock and the explicit `CoreServices::lock_now` apply. The UI can tell from `lock_watcher_status`.
- A watcher that lost the bus is not restarted until the next start of the application (ROADMAP follow-up).

*Note (2026-11-18, Phase 1 follow-up "Wave 3 (secrets)").* This changes the last sentence of point 4 and the last consequence above; the rest of the amendment stands. A watcher that loses the system-bus connection still locks once at once (fail-closed), then reconnects on its own instead of stopping: a bounded exponential back-off of 1 s doubling to 60 s (reset only after a connection that stayed up 60 s, so a flapping bus is not retried every second; each attempt bounded by the 5 s setup timeout), and on success it subscribes again to `PrepareForSleep`, `Lock`, the `LockedHint` changes and `NameOwnerChanged`, and takes the delay inhibitor again. Back on the bus it locks at once when the session's `LockedHint` is already true, or when the machine was suspended during the outage (`CLOCK_BOOTTIME` ahead of `CLOCK_MONOTONIC` by more than 2 s beyond what it was at the loss), since those events were not seen. When logind (rather than the bus) leaves, the watcher now also locks once and waits for a new owner, then reads `LockedHint` the same way. On the bus it still talks only to logind (now also reading the session's `LockedHint` with `Properties.Get`) and to the bus daemon. `lock_watcher_status` reports the state live, so the UI's problem entry clears when the watcher is back. Only a bus lost after a successful start is retried: a watcher that could not start is not (point 5 unchanged). A screen lock followed by an unlock while the bus is away is not seen.

**Rejected alternatives.**
- **Keep the watcher behind `secret-service`**: default builds would get no sleep or screen-lock locking until the Secret Service backend is validated.
- **Make `secret-service` default-on to get the watcher**: contradicts ADR-0012, which keeps the release-candidate client off until the desktop round trip and the stable 6.x.
