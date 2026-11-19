# Security policy

## Supported versions
| Version | Supported |
|---------|-----------|
| No release yet | — |
| Unreleased commits of `main` / `dev` | No |

From v0.1.0, the latest `0.x` minor version receives security fixes as patch releases.

## Reporting a vulnerability
**Do not open a public issue, discussion or pull request.** Report privately through GitHub Security Advisories: in this repository, **Security** tab → **Report a vulnerability**.

Please include: the affected version or commit, the component (UI, a driver, secrets, SSH / TLS, DBeaver import, packaging), reproduction steps and the estimated impact. Never send real credentials or real DBeaver workspaces: reproduce with the `dev/` containers and synthetic files.

## Commitment
- Acknowledgment within 72 h
- Initial assessment within 7 days
- Fix and advisory coordinated with the reporter, published together with the patch release

## Scope
In scope: the `ninesql` binary and its crates, the stored workspace files, the encrypted vault, the SSH / TLS behavior, the DBeaver importer / exporter, the `.deb`, the APT repository metadata and the Flatpak manifest.

Out of scope: vulnerabilities of the database engines themselves, the `dev/` test environment (dev-only passwords by design), and setups that turn off a documented safeguard (TLS `require` / `disable`, accepted unknown SSH host keys, read-only off on a production connection).

## Release signing
Release assets (the `.deb`, its CycloneDX SBOM and `SHA256SUMS`) are signed by the tag-push workflow with cosign keyless signatures (Sigstore, GitHub Actions OIDC). Each file has a `<file>.sigstore.json` bundle. A valid signature must have:
- certificate identity `https://github.com/Yil00/ninesql/.github/workflows/release.yml@refs/tags/vX.Y.Z` (the release's tag);
- certificate OIDC issuer `https://token.actions.githubusercontent.com`;
- the commit the release was built from, printed in the release notes, as the workflow SHA; in a clone, that commit must be the tag's and be on `main` (`git merge-base --is-ancestor <commit> origin/main`).

```bash
cosign verify-blob --bundle ninesql_X.Y.Z-1_amd64.deb.sigstore.json \
  --certificate-identity "https://github.com/Yil00/ninesql/.github/workflows/release.yml@refs/tags/vX.Y.Z" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-github-workflow-sha <commit from the release notes> \
  ninesql_X.Y.Z-1_amd64.deb
sha256sum -c SHA256SUMS --ignore-missing
git fetch origin main --tags && git merge-base --is-ancestor <commit> origin/main
```

Do not accept a signature checked with a looser identity (a regular expression, another workflow or another ref). What really decides who can sign is the tag ruleset that lets only the maintainer push `v*` tags: the workflow that runs is the one in the tagged commit, so a tag pushed by anyone else could carry a workflow without the approval step. The signing job also runs in a `release` environment that needs the maintainer's approval ([packaging/README.md](packaging/README.md#who-can-sign-a-release)). The APT repository signing key fingerprint will be published here with v0.1.0.
