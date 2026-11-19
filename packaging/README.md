# Packaging

How NineSQL is packaged for Debian / Ubuntu and Flatpak (P6-A, [ADR-0010](../docs/adr/0010-packaging-and-distribution.md)). Owner: `core-engineer`. The release infrastructure (APT signing key, the signed repository on GitHub Pages, Flathub) is the maintainer's (P6-B): see [P6-B hooks](#p6-b-hooks-maintainer).

| Path | Content |
|---|---|
| `linux/io.github.yil00.NineSQL.desktop` | Desktop entry (shared by the `.deb` and the Flatpak) |
| `linux/io.github.yil00.NineSQL.metainfo.xml` | AppStream metadata |
| `linux/ninesql.1` | Man page (`man -l packaging/linux/ninesql.1`) |
| `linux/icons/*.png`, `linux/render-icons.sh` | hicolor PNG icons rendered from [`docs/assets/logo.svg`](../docs/assets/logo.svg) (the scalable icon is the SVG itself); rerun the script only when the logo changes |
| `deb/build.sh` | Builds the `.deb` |
| `deb/builder.Dockerfile` | The Ubuntu 22.04 build environment |
| `deb/about.toml`, `deb/third-party-licenses.hbs` | The third-party license file (cargo-about) |
| `deb/install-test.sh` | Install test on Debian 12 / 13 and Ubuntu 22.04 / 24.04 |
| `deb/check-reproducible.sh` | Builds twice and compares the hashes |
| `sbom.py` | CycloneDX SBOM of the binary |
| `flatpak/io.github.yil00.NineSQL.yml` | Flatpak manifest |
| `flatpak/lint-manifest.py`, `flatpak/prepare-sources.sh`, `flatpak/verify-sources.py` | Manifest checks; the generated Flatpak sources and their check against `Cargo.lock` |
| `release-notes.md` | The release notes template, filled in by the release workflow |
| [`../.github/workflows/release.yml`](../.github/workflows/release.yml) | The tag-push release workflow |

The application id is **`io.github.yil00.NineSQL`** everywhere: the desktop file's name, the icons, the AppStream id, the Flatpak id and the window's Wayland app id / X11 `WM_CLASS` (`crates/ninesql-ui/src/app.rs`), so the desktop shows the window with NineSQL's name and icon. Changing it later moves the Flatpak's data directory (`~/.var/app/<id>/`), so it is settled before the first release.

## The `.deb`
```bash
make deb          # = packaging/deb/build.sh; writes target/debian/ninesql_<version>-1_amd64.deb
```
Needs Docker, rustup with the repository's toolchain, and on the host exactly cargo-deb 3.8.0 and cargo-about 0.9.3 (`cargo install --locked cargo-deb@3.8.0` and `cargo install --locked cargo-about@0.9.3 --features cli`): `build.sh` checks both versions and stops with the install command when they differ, since they shape the package and its license file. Network: `make deb` pulls the builder's base image and its apt packages (Docker) and the crates (`cargo fetch`); only the compile step runs offline.

**Reproduce a release locally.** Release builds use the toolchain pinned in `release.yml` (`RUSTUP_TOOLCHAIN`, 1.100.0 today) while `rust-toolchain.toml` follows `stable`, so set it explicitly: `rustup toolchain install 1.100.0` then `NINESQL_RUST_TOOLCHAIN=1.100.0 make deb`. `build.sh` warns when the toolchain is not the release one. The builder's apt packages are not pinned (below), so a local rebuild matches a release only while the Ubuntu 22.04 archive serves the same packages.

**Why the builder container.** The binary is linked in an Ubuntu 22.04 container because 22.04 has the oldest glibc of the release targets (2.35; Debian 12 has 2.36). A binary linked on a newer system does not start on the older ones: built on Ubuntu 24.04 it would need glibc 2.39. `build.sh` checks the highest glibc symbol version the binary needs and fails above 2.35. The container holds no toolchain and no crate: the host's rustup toolchain, the source tree and the host's cargo registry cache and index (`registry/cache`, `registry/index`) are mounted read-only; the extracted crate sources (`registry/src`) and the rest of the cargo home are the container's own, under the build directory; `cargo build --release --locked --offline` runs under `--network none`, so the build cannot change what the host's cargo later trusts (the build directory therefore holds its own copy of the extracted sources, about 1.4 GB). Before it, `cargo fetch --locked` on the host downloads the crates (crates.io) and `docker build` pulls the base image and the apt packages.

**Base image.** `builder.Dockerfile` pins `ubuntu:22.04` by digest. `NINESQL_BUILDER_BASE` may pull that same digest from a mirror (`mirror.gcr.io/library/ubuntu@sha256:<digest>`); `build.sh` refuses any value that does not end with the pinned digest. Another base image means editing the Dockerfile, and such a build is not the release build.

**Why cargo-deb.** ADR-0010 §1 names it; it builds the package from `[package.metadata.deb]` in `crates/ninesql-ui/Cargo.toml` (the manifest of the package that owns the binary, which is where cargo-deb reads it), clamps every file's time stamp to `SOURCE_DATE_EPOCH`, writes `root:root` owners and compresses the archive itself (no `dpkg-deb`), and generates `/usr/share/doc/ninesql/copyright` from `LICENSE`. It is a build tool (MIT), never linked into NineSQL, so I7 is unaffected. `--no-build` uses the binary from the builder; `--no-strip` keeps the release profile's symbol table (`strip = "debuginfo"`) and avoids a host `strip`.

**Contents.**
| File | |
|---|---|
| `/usr/bin/ninesql` | The binary. `ninesql --version` and `ninesql --help` answer before any window, display or GPU |
| `/usr/share/applications/io.github.yil00.NineSQL.desktop` | Categories `Development;Database;` |
| `/usr/share/metainfo/io.github.yil00.NineSQL.metainfo.xml` | AppStream |
| `/usr/share/icons/hicolor/{scalable,32x32,48x48,64x64,128x128,256x256,512x512}/apps/io.github.yil00.NineSQL.*` | Icons |
| `/usr/share/man/man1/ninesql.1.gz` | Man page (`gzip -n`, no time stamp) |
| `/usr/share/doc/ninesql/copyright`, `NOTICE`, `THIRD-PARTY-LICENSES.txt` | NineSQL's license and notice; the licenses of the crates linked into the binary (about 620) |

- **Dependencies** are listed by hand (`libc6 (>= 2.35), libgcc-s1, libxcb1, libxkbcommon0, libxkbcommon-x11-0, libvulkan1, libwayland-client0, libfontconfig1`; recommends `mesa-vulkan-drivers | vulkan-icd`): the binary links libc, libgcc, libxcb and libxkbcommon(-x11), and GPUI loads Vulkan, Wayland and fontconfig at run time, which `dpkg-shlibdeps` cannot see. The names exist on all four targets (the install test resolves them).
- **No `md5sums`.** cargo-deb 3.8.0 has no option to write the control archive's `md5sums` file, so `dpkg --verify ninesql` and `debsums` cannot check the installed files. The `.deb` itself is covered by `SHA256SUMS` and its cosign signature; adding `md5sums` would mean repacking the archive outside cargo-deb (follow-up, if a later cargo-deb supports it).
- **No maintainer script** (ADR-0010 §1): nothing runs at install or removal, so nothing can touch the network; dpkg triggers refresh the desktop, icon and man caches. The install test fails if one appears.
- **No phone-home (I2).** The package adds no service, no timer, no update check and no APT source.
- **Third-party licenses.** `cargo about generate --frozen --fail` takes the license texts from the crates themselves (no network) for the crates linked into the binary on `x86_64-unknown-linux-gnu`, without build and dev dependencies. `about.toml` accepts exactly the licenses `deny.toml` allows, so a crate under another license fails the build as it fails `cargo deny check`. SQLite (public domain) and other notices are in `NOTICE`.
- **MIME type.** `.sql` is already `application/sql` in shared-mime-info, so the package ships no MIME definition. The desktop file does not declare `MimeType=application/sql` yet: `ninesql` does not open a file given on its command line, and offering it under "Open with" would do nothing (I8). Once the binary opens a script passed as an argument (follow-up, `ui-engineer`), add `MimeType=application/sql;` and `Exec=ninesql %F`.

## Install test
```bash
make deb-install-test    # = packaging/deb/install-test.sh target/debian/ninesql_*_amd64.deb
```
For each of Debian 12, Debian 13, Ubuntu 22.04 and Ubuntu 24.04, in a fresh container: apt installs the `.deb` and resolves its dependencies, no maintainer script exists, `ninesql --version` / `--help` run and an unknown argument exits with status 2, `desktop-file-validate` and `appstreamcli validate --no-net` pass, the man page renders with no `groff` warning, the icons and license files are there, and `apt-get purge` leaves no file behind. A distribution whose archive cannot be reached is reported `BLOCKED`, not passed. `NINESQL_TEST_REGISTRY=mirror.gcr.io/library` pulls the images from a mirror when Docker Hub rate-limits.

## Reproducibility
```bash
make deb-reproducible    # = packaging/deb/check-reproducible.sh
```
Builds the package twice, once from the checkout and once from a copy at another path into an empty build directory, with the same `SOURCE_DATE_EPOCH` (the commit time of `HEAD`), and compares the SHA-256. What makes it reproducible: the pinned builder image (by digest), the same rustup toolchain on both builds (the check uses whichever toolchain `NINESQL_RUST_TOOLCHAIN` or `rust-toolchain.toml` selects, so to compare with a release set `NINESQL_RUST_TOOLCHAIN=1.100.0`), `--locked --offline`, fixed paths inside the container (`/src`, `/cargo`, `/target`, `/toolchain`) remapped with `--remap-path-prefix`, `CARGO_INCREMENTAL=0`, `codegen-units = 1`, cargo-deb's clamped time stamps and `gzip -n`. The release workflow does the same on two runners.

Not pinned yet (follow-up): the apt packages of the builder (linker, C compiler, `-dev` libraries) come from the Ubuntu 22.04 archive at image-build time, so a rebuild months later may differ: reproducibility is proven within one workflow run (two builds compared), not over time. Pinning them through `snapshot.ubuntu.com` would let a third party rebuild an old tag bit for bit.

**What the install test does not cover.** It runs `--version` and `--help`, which answer before any window. The libraries GPUI loads with `dlopen` at run time (the Vulkan loader and ICD, `libwayland-client`, fontconfig, the X11 / xkbcommon pieces it opens lazily) are exercised only when a window opens, so a missing or incompatible one shows up only in the GUI run of the release candidate on each distribution (P6-F), not in CI.

## SBOM
```bash
make sbom                # = python3 packaging/sbom.py target/ninesql.cdx.json
```
CycloneDX 1.5 JSON from `cargo cyclonedx` 0.5.10, limited to the crates linked into the binary (cargo-cyclonedx also lists the root crate's test-only dependencies, which `sbom.py` removes with their graph entries). Its timestamp is `SOURCE_DATE_EPOCH`.

## Release workflow
[`release.yml`](../.github/workflows/release.yml) runs on the push of a tag `vX.Y.Z` or `vX.Y.Z-…` only; never on pull requests or branches. The maintainer pushes the tag (a tag created with `GITHUB_TOKEN` does not start workflows). Jobs:
1. **build** (two replicas, two checkout paths, `contents: read`): checks that the tag matches the workspace version and is on `main`, installs the pinned toolchain (`RUSTUP_TOOLCHAIN`, bumped in its own PR) and the packaging tools from crates.io (`--locked`), runs `packaging/deb/build.sh`, and writes the SBOM;
2. **reproducible** (no permission): the two `.deb` files must be byte-identical;
3. **install-test**: `packaging/deb/install-test.sh` on the four distributions;
4. **sign** (`id-token: write` only, environment `release`, after the three others): `SHA256SUMS` and cosign keyless signatures (`cosign sign-blob --bundle`, Sigstore with the workflow's OIDC identity) of the `.deb`, the SBOM and `SHA256SUMS`; it has no write access to the repository;
5. **release** (`contents: write` only, environment `release`, after **sign**): a **draft** GitHub release with the files, the signatures and the notes from [`release-notes.md`](release-notes.md), which print the commit the signatures are bound to; it has no OIDC token, so it cannot sign. The maintainer checks the draft and publishes it.

Every action is pinned to a commit SHA.

### Verify a release file
The Sigstore certificate names the workflow file, the tag and the commit. Check all three: the identity and issuer prove the signature came from this repository's `release.yml` on that tag, and the commit (printed in the release notes) proves which sources it was built from. In a clone, check that the commit is the tag's and that it is on `main`: `git rev-parse vX.Y.Z^{commit}`, then `git merge-base --is-ancestor <commit> origin/main` (after `git fetch origin main --tags`). The workflow checks the latter too, but a workflow changed in the tagged commit could skip that check.
```bash
cosign verify-blob --bundle ninesql_0.1.0-1_amd64.deb.sigstore.json \
  --certificate-identity "https://github.com/Yil00/ninesql/.github/workflows/release.yml@refs/tags/v0.1.0" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-github-workflow-sha <commit from the release notes> \
  ninesql_0.1.0-1_amd64.deb
sha256sum -c SHA256SUMS --ignore-missing
```

### Who can sign a release
Anyone who can run `release.yml` on a `v*` tag can obtain a valid signature, so the repository settings decide who can sign, and the **tag ruleset is what really decides**: the workflow that runs is the one in the tagged commit, so someone able to push a `v*` tag on a commit of their own could remove the `environment:` line and the `main` check from it and sign without any approval. The `release` environment is a second gate for tags pushed by the maintainer. They cannot be set from the repository's files; the maintainer creates them before the first release (P6-B):
1. **Tag ruleset**: Settings → Rules → Rulesets → New tag ruleset, target `refs/tags/v*`, enforcement active, restrict creations, updates and deletions, with only the maintainer (repository admin role) in the bypass list. Nobody else can push, move or delete a release tag.
2. **`release` environment**: Settings → Environments → New environment `release`, with **required reviewers** set to the maintainer (leave "Prevent self-review" off while the maintainer, who pushes the tag, is the only reviewer), and **deployment branches and tags** set to "Selected branches and tags" with the single tag rule `v*`. The **sign** and **release** jobs wait for the maintainer's approval in each run, and cannot run from any other ref.
3. Keep the Actions default `GITHUB_TOKEN` permissions read-only (RELEASE.md §5).

## Flatpak
[`flatpak/io.github.yil00.NineSQL.yml`](flatpak/io.github.yil00.NineSQL.yml) builds the same binary with the freedesktop 25.08 runtime and the Rust stable SDK extension, offline (`cargo --offline --locked` on the sources listed in `cargo-sources.json`). `make flatpak-lint` checks it without flatpak-builder. The rust-stable extension follows Rust's stable channel and is not pinned, so a Flatpak build is not reproducible in the `.deb`'s sense. It has not been built by the project yet (no flatpak-builder in the cloud sandbox); to build it locally:
```bash
packaging/flatpak/prepare-sources.sh     # cargo-sources.json and THIRD-PARTY-LICENSES.txt (network)
flatpak install --user flathub org.freedesktop.Platform//25.08 org.freedesktop.Sdk//25.08 \
  org.freedesktop.Sdk.Extension.rust-stable//25.08
flatpak-builder --user --install --force-clean packaging/flatpak/build-dir \
  packaging/flatpak/io.github.yil00.NineSQL.yml
flatpak run io.github.yil00.NineSQL --version
```
`prepare-sources.sh` runs on the maintainer's machine, not in CI: it downloads `flatpak-cargo-generator.py` at a pinned commit (SHA-256 checked) and installs its Python dependencies from PyPI at exact versions (`aiohttp==3.14.5`, `tomlkit==0.15.2`) but without hashes, their own dependencies resolved by pip, in a throwaway virtual environment. The result does not rest on them: `verify-sources.py` (standard library only) then checks that `cargo-sources.json` is exactly what `Cargo.lock` implies (every crates.io archive with its `Cargo.lock` checksum, the vendored-sources config, nothing else) and the script fails otherwise.

### Permissions
| Permission | Why |
|---|---|
| `--socket=wayland`, `--socket=fallback-x11`, `--share=ipc` | Display: Wayland, X11 only without a Wayland compositor (`ipc` for X11 shared memory) |
| `--device=dri` | GPU rendering (Vulkan) |
| `--share=network` | The user's connections and their SSH / proxy hops. Flatpak grants it all-or-nothing; the limit to declared connections is NineSQL's own (I2) |
| `--system-talk-name=org.freedesktop.login1` | Lock the vault and drop "ask every time" secrets on sleep and screen lock ([ADR-0005 amendment](../docs/adr/0005-secrets-storage.md#amendment-p1-d-wiring-2026-11-15)); without it only the idle lock applies |
| `--filesystem=~/.ssh/known_hosts:ro` | OpenSSH's host keys, read-only, the file only (never `~/.ssh`, which holds private keys); without it host keys are checked against NineSQL's own `known_hosts` only |
| `--filesystem=~/.local/share/DBeaverData:ro` | The first-run DBeaver import offer finds the workspace, and the sandbox makes that path read-only (I4). It also exposes DBeaver's `credentials-config.json` (obfuscated, not encrypted) to the sandbox, which the import reads anyway |

Not granted:
- `--talk-name=org.freedesktop.secrets`: the Secret Service backend is off by default ([ADR-0012](../docs/adr/0012-secret-service-client-and-msrv.md)), so the Flatpak keeps saved passwords in the encrypted vault (`~/.var/app/io.github.yil00.NineSQL/data/ninesql/vault.bin`). A build with `--features ninesql-core/secret-service` adds it; `lint-manifest.py` requires both or neither.
- Home or host file access: scripts, exports, CSV imports, SQLite files and SSH key files are chosen through the file chooser portal. Other DBeaver locations (DBeaver's own Flatpak, the Snap) are chosen with the folder picker.
- The SSH agent (opt-in, ADR-0010 §3): `flatpak override --user --socket=ssh-auth io.github.yil00.NineSQL`.
- A database on a Unix socket: `flatpak override --user --filesystem=/run/postgresql io.github.yil00.NineSQL` (or the socket's directory).

**Files chosen through the portal (I4).** The sandbox enforces I4 only for `~/.local/share/DBeaverData`: a file chosen through the file chooser portal gets a `/run/user/<uid>/doc/<id>/…` path (`/run/flatpak/doc/…` inside the sandbox) and the portal's grant is **read-write** on the host file, wherever it is. NineSQL therefore asks the Documents portal for the host path (`GetHostPaths`, at most 2 s) and runs its DBeaver checks there (P6-A review M1, [ADR-0010 amendment](../docs/adr/0010-packaging-and-distribution.md#amendment-p6-a-packaging-2026-11-19)):
- the portal does not answer, fails or does not know the file: the file counts as inside a DBeaver workspace;
- a known DBeaver location, a `DBeaverData` folder or a DBeaver project / workspace marker on the host path: inside;
- otherwise, inside the sandbox the marker check cannot see the host's folders, so the file still counts as inside, unless it lies directly in the home folder.

"Inside" means a SQLite database opens read-only. An export (which only creates a new file) is still allowed where the check could not tell, but never replaces a file there. To edit a SQLite file elsewhere in the Flatpak, give the sandbox its folder (`flatpak override --user --filesystem=~/data io.github.yil00.NineSQL`) and open it again: the file chooser then returns its real path, which NineSQL checks normally.

Known limits, to check when the Flatpak is first built: an SSH key file chosen through the portal is reachable only while the portal keeps the grant; the portal rule above is unit-tested on synthetic paths only. The sandbox's own data directory replaces `~/.config/ninesql` and `~/.local/share/ninesql`, so a Flatpak NineSQL does not see the workspace of a `.deb` NineSQL.

## P6-B hooks (maintainer)
- **APT repository.** Take the `.deb` from the draft release (after `cosign verify-blob`), add it to the repository (for example `reprepro` or `aptly`), sign `Release` / `InRelease` with the dedicated OpenPGP key and publish on GitHub Pages; users add it with a `signed-by=/usr/share/keyrings/ninesql-archive-keyring.gpg` source, never `apt-key` (ADR-0010 §2). The fingerprint goes to `SECURITY.md` and the README. A Pages job can be added to `release.yml` after **publish**, with `pages: write` on that job only and the signing key in an environment secret.
- **Tag ruleset and `release` environment.** Create them before the first tag: [Who can sign a release](#who-can-sign-a-release).
- **Tag.** The maintainer pushes the release tag from their own account after merging the release PR (RELEASE.md §4): a tag created with `GITHUB_TOKEN` does not start `release.yml`.
- **Maintainer field.** `maintainer` in `crates/ninesql-ui/Cargo.toml` is a GitHub no-reply address; replace it with a project address.
- **Flathub.** Before submitting: build and run the manifest, switch its source to a `git` source with the tag and commit, add screenshots and a `<releases>` entry to the AppStream file (screenshots come with the visual pass of P6-F), and confirm the app id with Flathub's verification of `io.github.yil00`.
- **Flatpak bundle** (RELEASE.md §3): once the manifest builds, a `flatpak build-bundle` job can join `release.yml`.
- **AppStream releases.** The release PR adds `<release version="X.Y.Z" date="…"/>` to the metainfo file.
