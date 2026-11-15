# Build and run NineSQL from source

NineSQL has no release yet: you build it from the repository. This page covers Linux (Debian, Ubuntu), the platforms NineSQL targets, and macOS, which is kept compiling so contributors on a Mac can iterate on the interface ([docs/10](../10-supported-platforms.md)).

What you get today: the main window (navigator, editor tabs and splits, status bar, several windows), the command palette, themes, the connection dialog, and sessions to PostgreSQL, MySQL, MariaDB and SQLite. Not built yet: running SQL and seeing results (phase 3), the DBeaver import screen (phase 2), packages (phase 6).

## 1. Install the toolchain

### Linux (Debian, Ubuntu)
1. Install Rust **1.89 or newer** with [rustup](https://rustup.rs). The `rustc` package of Debian 12 and Ubuntu 24.04 is too old. The repository pins the `stable` channel in `rust-toolchain.toml`, so rustup picks the right toolchain by itself.
2. Install the system packages GPUI needs. `make deps-debian` prints the list without installing anything:
   ```bash
   sudo apt install build-essential pkg-config clang cmake libvulkan-dev libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libx11-xcb-dev libxcb1-dev libfontconfig-dev libfreetype-dev libzstd-dev libasound2-dev
   ```
3. To run the window you need a GPU with Vulkan 1.2 drivers (Mesa `radv` or `anv`, or the NVIDIA proprietary driver) and a Wayland or X11 session. Software Vulkan (`lavapipe`) works but is slow.

### macOS (UI iteration only)
macOS is a development platform, not a release target. `make deps-macos` prints the list:
- Rust 1.89 or newer with [rustup](https://rustup.rs).
- The Xcode Command Line Tools (`xcode-select --install`). GPUI renders with Metal; if the build stops at the Metal shader step, install the full Xcode app from the App Store and select it with `sudo xcode-select -s /Applications/Xcode.app`.
- For the dev databases: Docker Desktop or OrbStack.
- `make` is the one macOS ships (`/usr/bin/make`); GNU `gmake` from Homebrew works too. macOS has no `timeout` command: the Makefile then runs long commands without a time limit, or uses `gtimeout` if GNU coreutils is installed (`brew install coreutils`).

A required CI job builds, tests and lints `ninesql-ui` on Apple Silicon ([docs/10](../10-supported-platforms.md)); Intel Macs are not built in CI. Report a failure as an issue.

## 2. Get the code and build
```bash
git clone https://github.com/Yil00/ninesql.git
cd ninesql
git switch dev            # development happens on dev; main holds releases
make build                # = cargo build --workspace --locked
```
The first build compiles GPUI and takes a long time. `make build` stops after `TIMEOUT` seconds (900 by default); raise it if your machine needs more: `make build TIMEOUT=1800`.

## 3. Run
```bash
make run                  # = cargo run --locked -p ninesql-ui --bin ninesql
```
For an optimised binary, `make release` builds `target/release/ninesql`; start it directly.

Only one NineSQL process can use the workspace at a time: a second one is refused (the workspace lock of [docs/02](../02-architecture.md#storage)).

## 4. Find your way in the window
| Action | Default keys |
|---|---|
| Command palette (every action is listed there) | `Ctrl+Shift+P` or `F1` |
| New connection | `Ctrl+Alt+N` |
| Show or hide the database navigator | `Ctrl+B` |
| Move focus to the navigator | `Ctrl+K` then `Ctrl+N` |
| New editor tab (empty: statements cannot be run yet) | `Ctrl+T` |
| Close the editor tab | `Ctrl+W` |
| Split right / split down | `Ctrl+\` / `Ctrl+K` then `Ctrl+\` |
| Toggle light / dark theme | `Ctrl+K` then `Ctrl+T` |
| Font size larger / smaller / reset | `Ctrl+=` / `Ctrl+-` / `Ctrl+0` |
| Show settings problems | `Ctrl+K` then `Ctrl+E` |
| New window / close window / quit | `Ctrl+Shift+N` / `Ctrl+Shift+W` / `Ctrl+Q` |

The connection actions are in [Create your first connection](first-connection.md), the secret actions in [Secrets](secrets.md).

On macOS the default bindings use **Ctrl** too, not Cmd: the defaults are the Linux ones on every platform.

## Settings and key bindings
NineSQL reads `$XDG_CONFIG_HOME/ninesql/settings.toml` (by default `~/.config/ninesql/settings.toml`, on macOS too). The file is optional; changes you make in the window (theme, fonts, zoom) are written back to it, keeping your comments. An example:
```toml
schema_version = 1

[appearance]
theme = "dark"                  # "light", "dark" or "system"
ui_font_size = 14               # pixels, 8 to 32
mono_font_size = 13

[keybindings]
"ninesql::NewEditorTab" = "ctrl-n"              # replaces the defaults of this action
SplitDown = ["ctrl-k ctrl-down", "alt-shift-2"] # short names work too
ToggleNavigator = []                            # unbinds
```
An invalid value or binding never stops NineSQL: the default is used and the problem is listed by "Show settings problems" (`Ctrl+K` then `Ctrl+E`).

## Where NineSQL keeps its files
| Path (defaults) | Content |
|---|---|
| `~/.config/ninesql/settings.toml` | Appearance and key bindings |
| `~/.local/share/ninesql/workspace/<project>/` | `connections.json`, `connection-types.json`, `project.json` |
| `~/.local/share/ninesql/vault.bin` | The encrypted vault, if you created one ([Secrets](secrets.md)) |
| `~/.local/share/ninesql/known_hosts` | SSH host keys you trusted in NineSQL |

None of these files ever holds a password ([invariant I1](../../CONTEXT.md#invariants-non-negotiable)). The `XDG_CONFIG_HOME` and `XDG_DATA_HOME` variables move them.

## macOS: what behaves differently
- **Passwords go to the vault.** The keyring backend is the Linux Secret Service, which macOS does not have (and the backend is off by default on Linux too for now). NineSQL saves passwords in its encrypted vault, protected by a master password ([Secrets](secrets.md)).
- **No locking on sleep or screen lock.** That watcher listens to systemd-logind, which macOS does not have. The vault still locks after 15 minutes without use and with "Lock secrets now" (`Ctrl+Alt+L`). "Show connection problems" (`Ctrl+K` then `Ctrl+L`) lists it as "Sleep and screen lock are not watched…".
- Packaging (`.deb`, Flatpak) and the Secret Service are tested on Linux only.

## Next
- [Start the dev databases](dev-databases.md), then [create your first connection](first-connection.md).
