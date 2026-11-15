# ADR-0010: Packaging and distribution

- **Status**: Accepted. Note (P1-D wiring, 2026-11-15): the Flatpak permissions of §3 also include `--system-talk-name=org.freedesktop.login1` (logind on the system bus, to lock secrets on sleep and screen lock), and "network" is `--share=network`, which Flatpak grants all-or-nothing: NineSQL itself limits network use to the user's connections and their SSH / proxy hops (I2). See the [amendment of ADR-0005](0005-secrets-storage.md#amendment-p1-d-wiring-2026-11-15) and [docs/10](../10-supported-platforms.md#flatpak-permissions).
- **Date**: 2026-11-14

## Context
NineSQL targets Debian and Ubuntu desktops. Users of a security-minded database client should be able to verify what they install. A `curl … | bash` installer runs unverified code as the user, often with `sudo`.

## Decision
1. **`.deb`** built with `cargo-deb`, for `amd64` (and `arm64` when CI capacity allows): binary, `.desktop` file, hicolor SVG / PNG icons, AppStream metainfo, MIME type for `.sql`, man page, third-party license file. No maintainer script that touches the network.
2. **Signed APT repository** served from GitHub Pages (or equivalent static hosting): `Release` / `InRelease` signed with a dedicated OpenPGP key whose fingerprint is published in `SECURITY.md` and the README; installation uses a `signed-by=` keyring file in `/usr/share/keyrings/`, never `apt-key`.
3. **Flatpak** published on Flathub, with the narrowest permissions that work: network, Wayland / X11 fallback, GPU (`--device=dri`), Secret Service D-Bus name, file access through portals (no `--filesystem=home` by default). The SSH agent socket is opt-in and documented.
4. **Release assets** on GitHub Releases: `.deb`, `SHA256SUMS`, signatures (Sigstore / cosign keyless from the tag-push workflow), SBOM (CycloneDX).
5. **Reproducible builds**: pinned toolchain, `--locked`, `SOURCE_DATE_EPOCH`; CI checks that two builds of the same tag produce the same `.deb`.
6. **No `curl | bash` installer**, ever. A one-line command in the docs is allowed only when it downloads a signed key and adds the APT source, written out in full.
7. **Supported platforms** are listed in [docs/10-supported-platforms.md](../10-supported-platforms.md) and tested in CI with an install test in containers of each distribution (package installs, binary starts with `--version`, desktop file valid).

## Consequences
- Release work (keys, Flathub submission, Pages setup) is a maintainer task in P6 and cannot be delegated to agents.
- Flatpak sandboxing complicates SSH agent access and local socket connections (e.g. PostgreSQL over a Unix socket); the user guide documents the overrides.

## Rejected alternatives
- **Snap**: an option for later; Flatpak first because it is distribution-neutral and preferred on Debian.
- **AppImage only**: no sandbox, no update channel, weaker integration.
- **Upstream Debian packaging first**: the long-term goal, but Debian's Rust packaging policy (every crate packaged) makes it a post-1.0 effort.
