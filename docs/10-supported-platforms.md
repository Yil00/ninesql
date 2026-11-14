# Supported platforms

## Release targets (v0.1.0)
| Distribution | Versions | Architecture | Status |
|---|---|---|---|
| Debian | 12 (bookworm), 13 (trixie) | amd64 (arm64 later) | 🗓 planned |
| Ubuntu | 22.04 LTS, 24.04 LTS (and the current interim release) | amd64 (arm64 later) | 🗓 planned |
| Any distribution with Flatpak | — | amd64 | 🗓 planned |

Ubuntu 22.04 and Debian 12 support depends on GPUI's Vulkan and Wayland requirements, which may need newer Mesa or system libraries than they ship. The P0-A spike ran on Ubuntu 24.04 only ([docs/spikes/p0-a-gpui.md](spikes/p0-a-gpui.md)); building on 22.04 and Debian 12 and finding a Vulkan adapter there are still to check (ROADMAP [phase 0 follow-ups](ROADMAP.md#phase-0-follow-ups)).

## Runtime requirements
- A GPU with Vulkan 1.2+ drivers (Mesa `radv`, `anv`, NVIDIA proprietary). Software Vulkan (`lavapipe`) works for testing but is slow.
- Wayland or X11 session.
- A Secret Service provider (GNOME Keyring, KWallet ≥ 5.97 with Secret Service, KeePassXC) for saved passwords; otherwise NineSQL uses its encrypted vault.

## Build requirements (Debian / Ubuntu)
Rust **1.89 or newer**, installed with `rustup` (the repository pins the `stable` channel in `rust-toolchain.toml`; 1.89 is the floor, [ADR-0012](adr/0012-secret-service-client-and-msrv.md)). The default `rustc` package of Debian 12 and Ubuntu 24.04 is older than that and cannot build NineSQL. This does not affect them as release targets: the `.deb` is built in CI with stable Rust and the Flatpak with the Flatpak Rust SDK extension, so users need no Rust toolchain.

System packages: the list below builds the P0-A spike on Ubuntu 24.04. The spike found that the binary only links `libxcb`, `libxcb-xkb`, `libxkbcommon` and `libxkbcommon-x11` (Vulkan, Wayland and fontconfig are loaded at run time), so the list will be trimmed after a green CI run confirms it:
```bash
sudo apt install build-essential pkg-config clang cmake libvulkan-dev libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libx11-xcb-dev libxcb1-dev libfontconfig-dev libfreetype-dev libzstd-dev libasound2-dev
```

## Development only
- **macOS** (Apple Silicon and Intel): builds are kept compiling so contributors on Macs can iterate on the UI (GPUI renders with Metal). Not a release target; Linux-specific parts (Secret Service, Flatpak, `.deb`) are tested on Linux only.
- **Windows**: not supported.
