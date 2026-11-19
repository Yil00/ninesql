# First start and secrets

## The first window
`ninesql` opens one window: the database navigator on the left, an editor area with tabs, and a status bar. With no connection saved yet, the navigator says: No connections yet. Use "New connection" (Ctrl+Alt+N) or "Import from DBeaver" (Ctrl+K Ctrl+I).

- If a DBeaver workspace is found in one of its usual places, a box above the editor offers "Import your DBeaver connections?" ([Import from DBeaver](import-from-dbeaver.md)). "Don't show again" turns the offer off for good.
- The command palette (`Ctrl+Shift+P` or `F1`) lists every action with its keys.
- Themes (light, dark, system), font sizes and key bindings come from `~/.config/ninesql/settings.toml`; changes made in the window (theme, fonts, zoom) are written back to it, keeping your comments. An invalid value never stops NineSQL: the default is used and "Show settings problems" (`Ctrl+K` then `Ctrl+E`) lists the problem.
- Only one NineSQL process can use the workspace at a time. A second one is refused ("another NineSQL process is using this workspace …"); open a new window from the running one instead ("New window", `Ctrl+Shift+N`).
- If a previous session saved scripts, they reopen, but nothing connects by itself ([SQL editor](sql-editor.md#script-files-and-session-restore)).

## Where passwords go
NineSQL never writes a password, passphrase or token to a file in clear ([invariant I1](../../CONTEXT.md#invariants-non-negotiable)). For each password field of a connection you choose between:

| Choice | Where the secret lives | When to use it |
|---|---|---|
| **"Save in keyring"** | The desktop keyring through the Secret Service (GNOME Keyring, KWallet, KeePassXC) | Only in builds with the keyring backend, which is **off in default builds** until it has been validated on GNOME and KDE desktops |
| **"Save in vault"** | NineSQL's own encrypted file, `vault.bin`, unlocked with a master password | The default: what default builds use, on Linux and macOS |
| **"Ask every time"** | Nowhere | You type the password at every connect; it is used for that connect only and is never kept in memory afterwards |

At startup NineSQL picks the store for new secrets in this order: the keyring when a Secret Service answers (in builds that have the backend), otherwise the vault. It never falls back to a plain file. The dialog never displays a saved password, not even masked: it says "Saved in vault", "Saved in keyring", "Not saved" or "Asked at every connect", and password fields have no reveal button and cannot be copied.

## The vault
- **Created** the first time you save a password with "Save in vault": the "Create the vault" prompt asks for a **master password**, typed twice, at least **12 characters** long and not only spaces. It is never saved anywhere. **There is no recovery**: without it, the passwords in the vault are lost (the connections themselves stay; you type the passwords again).
- **Choose it well**: someone who copies `vault.bin` can try to guess the master password offline, slowed only by the key derivation (Argon2id). The file is readable by you only and encrypted with XChaCha20-Poly1305.
- **Locked at every start.** NineSQL asks for the master password ("Unlock the vault") when a connect needs a password from the vault, before anything is sent to the network. After three wrong attempts it waits between attempts (1 s, doubling up to 60 s) while it runs.

## Locking
The vault locks itself, and any typed password still in memory is forgotten:

| When | Applies |
|---|---|
| You run "Lock secrets now" (`Ctrl+Alt+L`) | Always |
| The vault is not used for 15 minutes | Always (time spent suspended counts on Linux) |
| The computer goes to sleep | Linux with systemd-logind on the system bus |
| The screen locks | Linux with systemd-logind on the system bus |

After a lock, a "Secrets locked" notice says why and the status bar shows "Secrets locked (…)". **Open sessions stay open**: drivers keep no password once connected, so a running statement or an open transaction goes on; only the next connect asks for the master password again. If NineSQL loses the system bus, it locks at once and watches again once the bus is back. When sleep and screen lock are not watched (macOS, a Flatpak without the logind permission, a system without logind), "Show connection problems" (`Ctrl+K` then `Ctrl+L`) says so: lock by hand before leaving the computer.

## What never reaches the disk
- Passwords, passphrases and the master password are not in `settings.toml`, `connections.json` (it holds only a reference to the stored secret), logs, history, exports or the DBeaver export.
- The query history never stores parameter values, nothing at all for production connections, and no statement holding a credential word (`PASSWORD`, `IDENTIFIED`, `SECRET`, `TOKEN`…) or a URL with a password.
- A connection URL pasted into the dialog that carries a password is refused.
- Something that looks like a secret added by hand to `connections.json` is removed on load, the file is rewritten, and "Show connection problems" lists it.

The limits (malware running as you, text left in memory by the input component, a replaced `vault.bin`) are in [Known limitations](known-limitations.md#secrets) and [docs/05](../05-security.md#what-ninesql-does-not-protect-against).

Step by step: [Secrets: keyring, vault and locking](../tutorials/secrets.md).

## Next
- [Import from DBeaver](import-from-dbeaver.md), or [create a connection](connections.md).
