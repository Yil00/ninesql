# Secrets: keyring, vault and locking

This page explains where NineSQL keeps database passwords, SSH passwords and key passphrases, how the encrypted vault works from the user's side, and when it locks. The design is in [docs/02](../02-architecture.md#secrets-adr-0005-p1-d) and [docs/05](../05-security.md#vault-format-v1); the decision is [ADR-0005](../adr/0005-secrets-storage.md).

The rule behind everything here: **no secret in clear on disk** ([invariant I1](../../CONTEXT.md#invariants-non-negotiable)).

## 1. Where a password goes
For each password field of the connection dialog you choose:
- **Save**: the password goes to a secret store. The button names the store new secrets go to:
  - **"Save in keyring"**: the desktop keyring through the Secret Service (GNOME Keyring, KWallet, KeePassXC). This backend is **off in default builds** until it has been validated on GNOME and KDE desktops ([ROADMAP](../ROADMAP.md#phase-0-follow-ups)); macOS has no Secret Service at all.
  - **"Save in vault"**: NineSQL's encrypted vault. This is what default builds use, on Linux and macOS.
  - "Save (no keyring or vault available)" means neither store can be used; only "Ask every time" works then.
- **"Ask every time"**: nothing is saved. At each connect a "Credentials" dialog asks for it; the answer is used for that connect only, then wiped from memory.

At startup NineSQL picks the store in this order: keyring if a Secret Service answers (in builds that have the backend), otherwise the vault; it never falls back to a plain file.

## 2. Create the vault
The vault is created the first time you save a password with "Save in vault":
1. A "Create the vault" prompt opens: "Passwords are saved in an encrypted vault on this computer. Choose its master password: it is never saved, and without it the saved passwords cannot be recovered."
2. Type the master password twice ("The two passwords differ: type them again." if they do not match) and press "Create vault". It must be at least 12 characters long; the prompt shows the rule, and a shorter password is refused with "The master password must be at least 12 characters long." A vault created earlier with a shorter password still unlocks with it.
3. The connection is then saved, with the password in the vault.

Choose a strong master password: someone who copies `vault.bin` can try to guess it offline, slowed only by the key derivation (Argon2id). There is no recovery if you forget it.

The vault is one file, `~/.local/share/ninesql/vault.bin` (under `$XDG_DATA_HOME`), readable only by you (mode `0600`), encrypted with XChaCha20-Poly1305 under a key derived from the master password.

## 3. Unlock the vault
The vault is **locked every time NineSQL starts**. You unlock it when it is needed:
- when you connect and a saved password of that connection is in the vault. NineSQL asks before anything is sent to the network, and before any "Ask every time" field;
- when you save a password to the vault while it is locked.

The "Unlock the vault" prompt says: "The vault is locked. Type its master password to use the passwords saved in it." Press "Unlock". After a wrong password ("Wrong master password."), the first three attempts are free; then NineSQL waits 1 s, doubling up to 60 s, between attempts ("Too many wrong attempts: try again in N s."). This delay applies within a running NineSQL; it is not a protection for a copied file.

The master password is typed into a masked field with no reveal and no copy, moved into the key derivation and wiped from memory right after.

## 4. Lock it
The vault locks itself, and the passwords typed for "Ask every time" are forgotten, in four cases:

| When | How | Available |
|---|---|---|
| You ask | "Lock secrets now", `Ctrl+Alt+L` (also in the command palette) | Always |
| The vault is not used for a while | After 15 minutes without use, checked every 30 s; on Linux, time spent suspended counts | Always |
| The computer goes to sleep | systemd-logind `PrepareForSleep`: NineSQL holds a short sleep delay so the lock happens before the suspend | Linux with logind on the system bus |
| The screen locks | logind session `Lock` signal, or its `LockedHint` turning true (set by the GNOME and KDE lockers) | Linux with logind on the system bus |

After a lock, a "Secrets locked" notification explains why ("the computer went to sleep", "the screen was locked", "the vault was not used for a while", "you asked for it") and the status bar shows "Secrets locked (…)". The next connect that needs a vault password asks for the master password again.

Locking does not close open sessions: the drivers keep no password once connected, so a session goes on until you disconnect, and only the next connect needs the vault.

**When sleep and screen lock are not watched**: on macOS, in a Flatpak without the `org.freedesktop.login1` system-bus permission ([docs/10](../10-supported-platforms.md#flatpak-permissions)), or on a Linux system without logind. Only the idle lock and "Lock secrets now" apply then, and "Show connection problems" (`Ctrl+K` then `Ctrl+L`) says so: "Sleep and screen lock are not watched…: use "Lock secrets now" before leaving the computer."

## 5. What never reaches the disk
- Passwords, passphrases and the master password are never written to NineSQL's files: not `settings.toml`, not `connections.json` (it holds only a reference to the stored secret), not logs, history or exports. Tests check that no password reaches any file under the workspace directories after create, connect, execute and close.
- The dialog never displays a saved password, not even masked; secret fields have no reveal button and cannot be copied.
- If NineSQL finds something that looks like a secret in `connections.json` (for example added by hand), it removes it, rewrites the file and lists it in "Show connection problems".
- A URL pasted in the dialog that holds a password is refused.

Limits, recorded in [docs/05](../05-security.md#what-ninesql-does-not-protect-against): malware running as you can read an unlocked vault or keyring from memory; text typed in a password field may stay in memory after the field is cleared (a known issue of the input component, tracked in the [ROADMAP](../ROADMAP.md#phase-1-follow-ups)); someone able to replace `vault.bin` can put an older copy back without NineSQL noticing.

## Next
- [Create your first connection](first-connection.md).
- [docs/05 – Security](../05-security.md) for the threat model.
