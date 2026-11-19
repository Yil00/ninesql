# Install

## Packages (v0.1.0, not released yet)
No version of NineSQL has been released. The v0.1.0 MVP is planned to ship as ([ADR-0010](../adr/0010-packaging-and-distribution.md)):
- a signed **`.deb`** package for Debian 12 / 13 and Ubuntu 22.04 / 24.04 (amd64), from a signed APT repository;
- a **Flatpak** on Flathub.

The packages are being built in phase 6 (ROADMAP P6-A, P6-B); installation commands will be added here once they are published. There is deliberately no `curl … | bash` installer.

Until then you can build the `.deb` yourself from the repository and install it with `apt`: [Build and install the `.deb`](../tutorials/build-and-run.md#5-build-and-install-the-deb) (needs Docker; details in [packaging/README.md](../../packaging/README.md#the-deb)).

## Build from source (today)
Until a release exists, build NineSQL from the repository: [Build and run NineSQL from source](../tutorials/build-and-run.md) gives the toolchain (Rust 1.89 or newer through rustup), the Debian / Ubuntu packages to install, and the `make build` / `make run` commands. An optimised binary comes from `make release` (`target/release/ninesql`).

To try NineSQL without your own servers, [Start the dev databases](../tutorials/dev-databases.md) runs the supported engines in containers with a sample dataset ("nine").

## System requirements
- **Linux**: Debian 12 / 13 or Ubuntu 22.04 / 24.04 are the release targets, amd64 only for now. Ubuntu 22.04 and Debian 12 are not checked yet: GPUI may need newer graphics libraries than they ship ([docs/10](../10-supported-platforms.md)).
- **A GPU with Vulkan 1.2 drivers** (Mesa `radv` or `anv`, or the NVIDIA proprietary driver) and a **Wayland or X11** session. Software Vulkan (`lavapipe`) works but is slow.
- **Optional**: a Secret Service provider (GNOME Keyring, KWallet, KeePassXC). NineSQL's keyring backend is off in default builds for now, so saved passwords go to its own encrypted vault ([First start and secrets](first-start-and-secrets.md)).
- **Optional**: systemd-logind on the system bus, so saved passwords are locked when the computer sleeps or the screen locks. Without it only the idle lock and "Lock secrets now" apply.
- **macOS** builds are kept compiling for interface development only; it is not a release target. **Windows** is not supported.

The Flatpak will need network access (used only for your own connections) and logind on the system bus. It does not get the Secret Service while NineSQL's keyring backend is off, so the Flatpak keeps saved passwords in the encrypted vault. It reads `~/.ssh/known_hosts` and `~/.local/share/DBeaverData` read-only, and any other file only through the file chooser. The reasons are in [docs/10](../10-supported-platforms.md#flatpak-permissions) and [packaging/README.md](../../packaging/README.md#permissions).

## What NineSQL needs from the network
Nothing but your own connections. NineSQL sends no telemetry, has no account, checks for no updates and contacts no server except the databases you declare and their SSH hops ([invariant I2](../../CONTEXT.md#invariants-non-negotiable)).

## Where NineSQL keeps its files
NineSQL follows the XDG directories (`XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME` move them):

| Path (defaults) | Content |
|---|---|
| `~/.config/ninesql/settings.toml` | Appearance, key bindings and preferences |
| `~/.local/share/ninesql/workspace/<project>/` | Connections, connection types, navigator filters, snippets, scripts and DBeaver import reports |
| `~/.local/share/ninesql/vault.bin` | The encrypted vault, once created |
| `~/.local/share/ninesql/known_hosts` | SSH host keys you trusted in NineSQL |
| `~/.local/state/ninesql/history.sqlite` | Query history (readable by you only, never parameter values) |
| `~/.local/state/ninesql/session.json` | Which saved scripts to reopen at the next start (never editor text) |

None of these files holds a password ([invariant I1](../../CONTEXT.md#invariants-non-negotiable)). The full list is in [Build and run](../tutorials/build-and-run.md#where-ninesql-keeps-its-files).

**Removing NineSQL's data.** Deleting these folders deletes your connections, scripts and history. Saved passwords in the vault are gone with `vault.bin`; passwords saved in a desktop keyring (when that backend is enabled) stay in the keyring until you delete them there. NineSQL never touches your DBeaver files, so a DBeaver workspace is unaffected either way.

## Next
- [First start and secrets](first-start-and-secrets.md).
