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
Built only by the tag-push workflow ([ADR-0010](docs/adr/0010-packaging-and-distribution.md)): `.deb` (amd64), `SHA256SUMS`, cosign signatures, CycloneDX SBOM, Flatpak bundle; the APT repository is updated from the same run.

## 4. Release steps
1. Open a release PR `dev` → `main`; CI green; CHANGELOG section generated.
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
