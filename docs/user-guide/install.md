# Install

## Packages (v0.1.0, not released yet)
No version of NineSQL has been released, and no package is published yet. The v0.1.0 MVP is planned to ship as ([ADR-0010](../adr/0010-packaging-and-distribution.md)):
- a signed **`.deb`** package for Debian 12 / 13 and Ubuntu 22.04 / 24.04 (amd64), attached to each GitHub release and later served from a signed APT repository;
- a **Flatpak**, later on Flathub.

There is deliberately no `curl … | bash` installer. The packaging files and their details are in [packaging/README.md](../../packaging/README.md).

### The `.deb`
The package exists in the repository (phase 6, P6-A) but no release has been made, so for now you build it yourself and install it with `apt`, which pulls its dependencies: [Build and install the `.deb`](../tutorials/build-and-run.md#5-build-and-install-the-deb) (needs Docker and two pinned packaging tools; `make deb`, then `sudo apt install ./target/debian/ninesql_<version>-1_amd64.deb`).

The package installs `/usr/bin/ninesql`, a desktop entry (NineSQL appears in the applications menu), icons, AppStream metadata, a man page (`man ninesql`) and the licenses of the bundled crates. It has no install or removal script and adds no service, update check or APT source. `ninesql --version` and `ninesql --help` work without a display. `sudo apt purge ninesql` removes it; your workspace, settings and vault stay in your home directory ([below](#where-ninesql-keeps-its-files)).

What has been checked: the install test (install, `--version` / `--help`, desktop and AppStream validation, man page, purge) passed in Ubuntu 22.04 and 24.04 containers; Debian 12 and 13 were not reachable from the build sandbox and are checked by the release workflow. Opening a window from the package on each distribution is part of the final tests (P6-F). The APT repository and its signing key do not exist yet (P6-B).

### The Flatpak
The manifest (`packaging/flatpak/io.github.yil00.NineSQL.yml`, application id `io.github.yil00.NineSQL`) exists and passes its lint, but **the Flatpak has not been built yet** and is not on Flathub. [packaging/README.md](../../packaging/README.md#flatpak) gives the local build commands for those who want to try it. Its permissions are below.

### Verify a release file
When a release exists, each file comes with a `SHA256SUMS` file and a cosign keyless signature (`*.sigstore.json`) made by the repository's release workflow on the release tag. Verify the `.deb` before installing it, with the commit printed in the release notes:
```bash
cosign verify-blob --bundle ninesql_0.1.0-1_amd64.deb.sigstore.json \
  --certificate-identity "https://github.com/Yil00/ninesql/.github/workflows/release.yml@refs/tags/v0.1.0" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-github-workflow-sha <commit from the release notes> \
  ninesql_0.1.0-1_amd64.deb
sha256sum -c SHA256SUMS --ignore-missing
```
What each option proves, and how to check that the commit is the tag's and is on `main`, is in [packaging/README.md](../../packaging/README.md#verify-a-release-file) and [SECURITY.md](../../SECURITY.md#release-signing). The release workflow has not run yet: no release tag exists.

## Build from source (today)
Until a release exists, build NineSQL from the repository: [Build and run NineSQL from source](../tutorials/build-and-run.md) gives the toolchain (Rust 1.89 or newer through rustup), the Debian / Ubuntu packages to install, and the `make build` / `make run` commands. An optimised binary comes from `make release` (`target/release/ninesql`).

To try NineSQL without your own servers, [Start the dev databases](../tutorials/dev-databases.md) runs the supported engines in containers with a sample dataset ("nine").

## System requirements
- **Linux**: Debian 12 / 13 or Ubuntu 22.04 / 24.04 are the release targets, amd64 only for now. The `.deb`'s binary is linked against glibc 2.35 so it starts on all four, but opening a window has not been checked on Ubuntu 22.04 and Debian 12 yet: GPUI may need newer graphics libraries than they ship ([docs/10](../10-supported-platforms.md)).
- **A GPU with Vulkan 1.2 drivers** (Mesa `radv` or `anv`, or the NVIDIA proprietary driver) and a **Wayland or X11** session. Software Vulkan (`lavapipe`) works but is slow.
- **Optional**: a Secret Service provider (GNOME Keyring, KWallet, KeePassXC). NineSQL's keyring backend is off in default builds for now, so saved passwords go to its own encrypted vault ([First start and secrets](first-start-and-secrets.md)).
- **Optional**: systemd-logind on the system bus, so saved passwords are locked when the computer sleeps or the screen locks. Without it only the idle lock and "Lock secrets now" apply.
- **macOS** builds are kept compiling for interface development only; it is not a release target. **Windows** is not supported.

The Flatpak will need network access (used only for your own connections) and logind on the system bus. It does not get the Secret Service while NineSQL's keyring backend is off, so the Flatpak keeps saved passwords in the encrypted vault. It reads `~/.ssh/known_hosts` and `~/.local/share/DBeaverData` read-only, and any other file only through the file chooser. A SQLite file chosen through the file chooser opens read-only unless it lies directly in your home folder or NineSQL can check its real location for a DBeaver workspace; to edit one elsewhere, give the sandbox its folder (`flatpak override --user --filesystem=<folder> io.github.yil00.NineSQL`) and open it again. The reasons are in [docs/10](../10-supported-platforms.md#flatpak-permissions) and [packaging/README.md](../../packaging/README.md#permissions).

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
