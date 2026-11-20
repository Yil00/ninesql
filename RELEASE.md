# Release process – NineSQL

Inspired by [Portabase](https://github.com/Portabase/portabase) and DataBastion: `main` / `dev` branches, Conventional Commits, changelog generated at release, release triggered by merging `dev` into `main`.

## 1. Branches
| Branch | Role | Protected | Branches from | Merges into |
|---|---|---|---|---|
| `main` | Released code. Each merge = one release | Yes: PR required, green `CI result`, no force-push | — | — |
| `dev` | Integration of the next version | Yes: PR required, green `CI result`, no force-push, no deletion | `main` | `main` (release PR) |
| `feat/<id>-<slug>`, `fix/…`, `docs/…`, `chore/…`, `ci/…`, `refactor/…`, `perf/…`, `test/…` | Work branches | No | `dev` | `dev` |
| `hotfix/<slug>` | Urgent fix on the released version | No | `main` | `main`, then back-merged into `dev` |

- Merge strategy: **squash** into `dev`; **merge commit** from `dev` into `main` and for the back-merge.
- Work branches are deleted after merge.

## 2. Versions
- One version for the whole workspace (`Cargo.toml` `[workspace.package] version`).
- SemVer computed from commits: `fix:` / `perf:` → patch, `feat:` → minor, breaking → minor before 1.0, major after.
- Pre-releases: `0.1.0-alpha.N`, `-beta.N`, `-rc.N`.

## 3. Artifacts (from v0.1.0)
Built only by the tag-push workflow [`release.yml`](.github/workflows/release.yml) ([ADR-0010](docs/adr/0010-packaging-and-distribution.md) and its [P6-A amendment](docs/adr/0010-packaging-and-distribution.md#amendment-p6-a-packaging-2026-11-19)), started when the maintainer pushes a `vX.Y.Z` tag:
- the `.deb` (amd64), built twice from two checkouts and required to be byte-identical (reproducible within one run), then install-tested on Debian 12 / 13 and Ubuntu 22.04 / 24.04; a binary built with the `e2e-scenarios` test feature is refused at compile time, by its marker before packaging and again before signing, and by the install test ([ADR-0029](docs/adr/0029-automated-ui-end-to-end-tests-and-application-logging.md) §2.2, [packaging/README.md](packaging/README.md#the-deb));
- a CycloneDX SBOM of the linked crates;
- `SHA256SUMS` of the `.deb` and the SBOM;
- a cosign keyless signature bundle (`<file>.sigstore.json`) for each of these files;
- a **draft** GitHub release holding them, with the commit the signatures are bound to in its notes; the maintainer publishes it (§4).

v0.1.0 ships as this signed `.deb` on the GitHub release (maintainer decision, 2026-11-19). The workflow does not build a Flatpak bundle and does not update an APT repository: the signed APT repository and Flathub come in a later release, and will be fed from the published draft by hand ([packaging/README.md](packaging/README.md#p6-b-hooks-maintainer)).

## 4. Release steps
1. Bump `[workspace.package] version` in `Cargo.toml` to `X.Y.Z` (the workflow refuses a tag that does not match it). Open a release PR `dev` → `main`; CI green; CHANGELOG section generated.
2. Merge with a merge commit.
3. The maintainer creates and pushes the tag `vX.Y.Z` on the merge commit, from their own account (a tag pushed with `GITHUB_TOKEN` does not start workflows; the tag ruleset lets only the maintainer push `v*` tags).
4. The release workflow ([`release.yml`](.github/workflows/release.yml)) builds the `.deb` twice and compares the builds, runs the install test, then waits for the maintainer's approval of the `release` environment, signs the files and creates a **draft** release whose notes print the commit the signatures are bound to.
5. The maintainer checks the draft (files, `cosign verify-blob` as in [SECURITY.md](SECURITY.md#release-signing), the commit), adds the CHANGELOG section to the notes and publishes it.
6. Back-merge `main` into `dev`.

## 5. Repository configuration (one-time, maintainer)
- Default branch `dev`; branch rulesets on `main` and `dev` as in the table above, with `CI result` as the required check.
- Private vulnerability reporting enabled (Security tab).
- Actions: read-only default `GITHUB_TOKEN` permissions; workflows request what they need.
- A tag ruleset on `refs/tags/v*` that only the maintainer can bypass, and a `release` environment with the maintainer as required reviewer and a deployment policy limited to `v*` tags ([packaging/README.md](packaging/README.md#who-can-sign-a-release)).
- Squash merge allowed for PRs into `dev`; "delete branch on merge" enabled.

Agents never create tags or releases, and never push to `main` or `dev` ([AGENTS.md](AGENTS.md#git)).
