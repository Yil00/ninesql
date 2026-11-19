<!-- Release notes template, filled in by .github/workflows/release.yml (job "release"):
     @VERSION@, @TAG@, @COMMIT@ and @REPOSITORY@ are replaced. The maintainer adds the
     CHANGELOG section before publishing the draft. -->
NineSQL @VERSION@.

Built by the release workflow from commit `@COMMIT@` (tag `@TAG@`). The signatures are bound
to that commit: check that it is the commit of the tag (`git rev-parse @TAG@^{commit}` in a
clone of https://github.com/@REPOSITORY@) before you trust them.

## Verify the files

```bash
cosign verify-blob --bundle <file>.sigstore.json \
  --certificate-identity "https://github.com/@REPOSITORY@/.github/workflows/release.yml@refs/tags/@TAG@" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-github-workflow-sha @COMMIT@ \
  <file>
sha256sum -c SHA256SUMS --ignore-missing
```

Every file is signed: the `.deb`, the CycloneDX SBOM (`.cdx.json`) and `SHA256SUMS`. A
signature made from another workflow, another tag or another commit fails this check.

## Changes

See [CHANGELOG.md](https://github.com/@REPOSITORY@/blob/@TAG@/CHANGELOG.md).
