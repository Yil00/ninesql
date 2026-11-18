# ADR-0005: Secrets storage

- **Status**: Accepted. Partially superseded by [ADR-0012](0012-secret-service-client-and-msrv.md) for the client crate (`secret-service` 6 instead of `keyring`) and the Secret Service item attributes; everything else stands. Note (P1-D, 2026-11-14): the vault of §2 is implemented as format v1; its bounds and known limits are documented in the rustdoc of `ninesql_secrets::vault` and in [docs/05](../05-security.md#vault-format-v1). Amended (P1-D wiring, 2026-11-15) by the [section](#amendment-p1-d-wiring-2026-11-15) below: locking on sleep and screen lock in default builds, and the Flatpak permissions it needs; the decision above is unchanged. A further amendment, [lock watcher reconnect (2026-11-18)](#amendment-lock-watcher-reconnect-2026-11-18), is **Accepted (maintainer, 2026-11-18)**: it changes point 4 of the P1-D wiring amendment. Note on §3 (["ask every time", 2026-11-18](#note-on-3-ask-every-time-2026-11-18)): an "ask every time" secret is asked at every connect and never cached in memory; the decision is unchanged.
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
- A watcher that lost the bus is not restarted until the next start of the application (ROADMAP follow-up). Changed by the accepted [lock watcher reconnect amendment](#amendment-lock-watcher-reconnect-2026-11-18).

**Rejected alternatives.**
- **Keep the watcher behind `secret-service`**: default builds would get no sleep or screen-lock locking until the Secret Service backend is validated.
- **Make `secret-service` default-on to get the watcher**: contradicts ADR-0012, which keeps the release-candidate client off until the desktop round trip and the stable 6.x.

## Amendment (lock watcher reconnect, 2026-11-18)
**Status: Accepted (maintainer, 2026-11-18).** It changes point 4 of the [P1-D wiring amendment](#amendment-p1-d-wiring-2026-11-15) ("When it loses the bus connection, it locks once and stops (fail-closed). It is not restarted.", and the list of bus calls) and its last consequence; points 1 to 3, 5 and 6 are unchanged. Raised by the Phase 1 follow-up "Wave 3 (secrets)" and its security review. Behaviour as implemented: `crates/ninesql-secrets/src/lock_signals.rs` (`supervise`, `BusLink::watch_signals`, `RECONNECT_FIRST`, `RECONNECT_MAX`, `WatcherState`), `CoreServices::lock_watcher_status`, [docs/02](../02-architecture.md#secrets-adr-0005-p1-d) and [docs/05](../05-security.md#vault-format-v1).

**Context.** A watcher that lost the system bus locked once and stopped for the rest of the session, leaving only the idle lock even after the bus came back (a `dbus-broker` / `dbus-daemon` restart). A logind that left the bus was waited for, but nothing was locked meanwhile, and events missed during either outage were never looked for.

**Decision.**
1. **Reconnect.** When the bus connection is lost, the watcher still locks once at once (`LockTrigger::Requested`), then reconnects with a bounded exponential back-off: 1 s doubling to 60 s, reset only after a connection that stayed up 60 s, each attempt bounded by the 5 s setup timeout (values and reasons in [docs/02](../02-architecture.md#secrets-adr-0005-p1-d) and [docs/05](../05-security.md#vault-format-v1)). On success it subscribes again to `NameOwnerChanged`, `PrepareForSleep`, the session's `Lock` and its `LockedHint` changes, and takes the sleep delay inhibitor again. The session found before is kept when the lookup fails on a redial, and looked up again (subscribing to the new path) once logind answers; a lookup that fails then is retried twice (after 1 s, then 2 s) on the watcher's timer thread.
2. **logind leaving the bus** locks once (`Requested`); nothing is watched until a new owner appears.
3. **Missed screen lock.** After a bus outage, and when logind returns, the session's `LockedHint` is read; `true` locks at once. A read that fails or times out (5 s method timeout) is taken as locked (fail-closed).
4. **Missed suspend.** `CLOCK_BOOTTIME` minus `CLOCK_MONOTONIC` (time suspended since boot) is recorded when the bus or logind goes away; if it grew by more than 2 s when it is back, a suspend was missed and the secrets are locked (`LockTrigger::Sleep`). A suspend shorter than the 2 s slack is not seen.
5. **Bus calls.** Besides the calls of point 4 of the P1-D wiring amendment, the watcher reads the session's `LockedHint` with `org.freedesktop.DBus.Properties.Get` on logind, and repeats the session lookup after logind returns. It still talks only to `org.freedesktop.login1` and to the bus daemon.
6. **State.** `DesktopLockWatcher::state()` (`Watching`, `LogindAbsent`, `Reconnecting`, `Stopped`) and `session_lock_lost()` (a session watched before that is not watched any more, or whose lookup failed again once logind was back, so the path still watched may no longer exist; cleared when a lookup succeeds) are reported live by `CoreServices::lock_watcher_status`, so the UI's problem entry appears while the watcher does not watch fully and clears when it is back. Every wait of the watcher runs on one timer thread started with it.

**Consequences.**
- The secrets are locked on every bus or logind outage, including a logind restart during a package upgrade: the user unlocks the vault again.
- While the bus or logind is away, sleep and screen lock are not watched (the idle lock still applies). A screen lock followed by an unlock within the outage is not seen.
- A watcher that could not start at all (no system bus, or no permission, point 5 of the P1-D wiring amendment) is not retried.
- At most one connection attempt a minute while the bus stays away.

**Rejected alternatives.**
- **Stop after the first loss** (the previous behaviour): leaves default builds without sleep and screen-lock locking for the rest of the session after a transient bus restart.
- **A fixed retry interval**: too short loads the bus daemon while it is down, too long leaves a returned bus unnoticed.
- **Treat an unreadable `LockedHint` as unlocked**: fails open on exactly the case the check is for.

## Note on §3 ("ask every time", 2026-11-18)
Decided by the maintainer on 2026-11-18; it records a decision and changes none of the points above. A secret saved as "ask every time" is asked at **every** connect and is never cached in memory between connects: the connect flow does not use the `SessionSecretCache`, and NineSQL keeps no answer for a later connect (`SessionManager::reconnect`, not offered by the UI yet, holds the answers only while it runs and drops them on a secrets lock). "For the session only" in point 3 is read that way. This is a final decision, not a missing feature; caching answers for the application's lifetime would need a new ADR. Raised by the Phase 1 follow-up "Wave 3 (core)" ([ROADMAP](../ROADMAP.md#phase-1-follow-ups)).
