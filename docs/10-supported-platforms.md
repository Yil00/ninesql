# Supported platforms

## Release targets (v0.1.0)
| Distribution | Versions | Architecture | Status |
|---|---|---|---|
| Debian | 12 (bookworm), 13 (trixie) | amd64 (arm64 later) | 🗓 planned |
| Ubuntu | 22.04 LTS, 24.04 LTS (and the current interim release) | amd64 (arm64 later) | 🗓 planned |
| Any distribution with Flatpak | — | amd64 | 🗓 planned |

Ubuntu 22.04 and Debian 12 support depends on GPUI's Vulkan and Wayland requirements, which may need newer Mesa or system libraries than they ship. The P0-A spike ran on Ubuntu 24.04 only ([docs/spikes/p0-a-gpui.md](spikes/p0-a-gpui.md)); building on 22.04 and Debian 12 and finding a Vulkan adapter there are still to check (ROADMAP [phase 0 follow-ups](ROADMAP.md#phase-0-follow-ups)).

The `.deb` binary is linked in an Ubuntu 22.04 container, the oldest glibc of the targets (2.35), and the build fails if it needs a newer glibc symbol; a binary linked on Ubuntu 24.04 would need glibc 2.39 and would not start on Debian 12 or Ubuntu 22.04. The install test (`packaging/deb/install-test.sh`) installs the package, runs `ninesql --version` and validates the desktop and AppStream files in a container of each target ([packaging/README.md](../packaging/README.md)).

## Runtime requirements
- A GPU with Vulkan 1.2+ drivers (Mesa `radv`, `anv`, NVIDIA proprietary). Software Vulkan (`lavapipe`) works for testing but is slow.
- Wayland or X11 session.
- A Secret Service provider (GNOME Keyring, KWallet ≥ 5.97 with Secret Service, KeePassXC) for saved passwords; otherwise NineSQL uses its encrypted vault.
- systemd-logind on the system bus, to lock saved secrets on sleep and screen lock ([docs/05](05-security.md#vault-format-v1)); without it only the idle lock applies.

### Flatpak permissions
The Flatpak manifest (`packaging/flatpak/io.github.yil00.NineSQL.yml`, checked by `make flatpak-lint`) declares these permissions, and only these:
- `--socket=wayland`, `--socket=fallback-x11` and `--share=ipc`: the display, X11 only without a Wayland compositor;
- `--device=dri`: GPU rendering;
- `--share=network`: network access, used only for the user's connections and their SSH / proxy hops (I2). Flatpak cannot narrow it: the permission is all-or-nothing, so the restriction is NineSQL's own;
- `--system-talk-name=org.freedesktop.login1`: logind on the system bus, so the vault is locked and the "ask every time" cache cleared on sleep and screen lock (`DesktopLockWatcher`, default-on `desktop-lock` feature). Without it the watcher cannot start: NineSQL reports it and relies on the idle lock;
- `--filesystem=~/.ssh/known_hosts:ro`: OpenSSH's host keys, read-only, that file only;
- `--filesystem=~/.local/share/DBeaverData:ro`: the DBeaver import's first-run offer, read-only, which also makes the sandbox enforce I4 there.

`--talk-name=org.freedesktop.secrets` (the Secret Service, [ADR-0005](adr/0005-secrets-storage.md)) is declared only by a build with the `secret-service` feature, which is off by default ([ADR-0012](adr/0012-secret-service-client-and-msrv.md)): the default Flatpak keeps saved passwords in the encrypted vault. Files (scripts, exports, imports, SQLite databases, SSH keys) go through the file chooser portal; the SSH agent and Unix-socket databases need an explicit `flatpak override` ([packaging/README.md](../packaging/README.md#permissions)).

## Build requirements (Debian / Ubuntu)
Rust **1.89 or newer**, installed with `rustup` (the repository pins the `stable` channel in `rust-toolchain.toml`; 1.89 is the floor, [ADR-0012](adr/0012-secret-service-client-and-msrv.md)). The default `rustc` package of Debian 12 and Ubuntu 24.04 is older than that and cannot build NineSQL. This does not affect them as release targets: the `.deb` is built in CI with stable Rust and the Flatpak with the Flatpak Rust SDK extension, so users need no Rust toolchain.

System packages: the list below builds the P0-A spike on Ubuntu 24.04. The spike found that the binary only links `libxcb`, `libxcb-xkb`, `libxkbcommon` and `libxkbcommon-x11` (Vulkan, Wayland and fontconfig are loaded at run time), so the list will be trimmed after a green CI run confirms it:
```bash
sudo apt install build-essential pkg-config clang cmake libvulkan-dev libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libx11-xcb-dev libxcb1-dev libfontconfig-dev libfreetype-dev libzstd-dev libasound2-dev
```

## Development only
- **macOS** (Apple Silicon and Intel): builds are kept compiling so contributors on Macs can iterate on the UI (GPUI renders with Metal). Not a release target; Linux-specific parts (Secret Service, Flatpak, `.deb`) are tested on Linux only.
  - Build requirements: Rust 1.89 or newer through `rustup`; the Xcode Command Line Tools (`xcode-select --install`), or the full Xcode if the Metal shader step of the GPUI build needs it; Docker Desktop or OrbStack for the dev databases. `make deps-macos` prints this list, `make deps-debian` the Debian / Ubuntu one above.
  - Status: the `macos` CI job builds, tests and lints `ninesql-ui` on an Apple Silicon runner (`macos-15`) and is required to pass (part of the `CI result` check, green since PR #9); Intel Macs are not built in CI, and the application has not been run on macOS by the project yet. Cross-compiling from Linux does not work without the Apple SDK (`ring` needs it). `deny.toml` checks Linux targets only, so macOS-only dependencies are not license-checked ([ROADMAP](ROADMAP.md#phase-1-follow-ups)).
  - Behaviour: saved passwords go to the encrypted vault (the Secret Service is Linux-only); sleep and screen-lock locking does not run (no logind) and the connection problems list says so; default key bindings use Ctrl as on Linux ([tutorial](tutorials/build-and-run.md#macos-what-behaves-differently)).
- **Windows**: not supported.
