# Architecture Decision Records

An ADR records a settled decision, its context and the alternatives it beat. Coding agents follow accepted ADRs and **never work around them**: a change of direction goes through a new ADR that supersedes the old one ([AGENTS.md](../../AGENTS.md)).

New ADR: copy [template.md](template.md), take the next number, status `Proposed`, open a PR. The maintainer accepts it.

| # | Title | Status |
|---|-------|--------|
| [0001](0001-native-stack-rust-gpui.md) | Native stack – Rust and GPUI | Accepted, partially superseded by [0013](0013-gpui-source-and-supply-chain.md) |
| [0002](0002-licensing-and-dependency-policy.md) | Licensing and dependency policy | Accepted, partially superseded by [0013](0013-gpui-source-and-supply-chain.md), partially amended by [0015](0015-allow-0bsd-license.md) |
| [0003](0003-supported-engines-v1.md) | Supported engines for v1 and driver families | Accepted, partially superseded by [0014](0014-dev-environment-profiles.md) |
| [0004](0004-driver-api-runtime-and-results.md) | Driver API, async runtime and result model | Accepted |
| [0005](0005-secrets-storage.md) | Secrets storage | Accepted, partially superseded by [0012](0012-secret-service-client-and-msrv.md) |
| [0006](0006-dbeaver-interoperability.md) | DBeaver interoperability | Accepted |
| [0007](0007-production-safety-model.md) | Production safety model | Accepted |
| [0008](0008-workspace-and-config-storage.md) | Workspace and configuration storage | Accepted |
| [0009](0009-container-first-test-environment.md) | Container-first test environment, runnable in CI and cloud agent sandboxes | Accepted |
| [0010](0010-packaging-and-distribution.md) | Packaging and distribution | Accepted |
| [0011](0011-tls-and-ssh-defaults.md) | TLS and SSH defaults | Accepted, partially amended by [0016](0016-ssh-rsa-and-host-key-policies.md) |
| [0012](0012-secret-service-client-and-msrv.md) | Secret Service client crate and minimum Rust version | Accepted |
| [0013](0013-gpui-source-and-supply-chain.md) | GPUI source (gpui-kit / gpui-pre from crates.io) and supply-chain controls | Accepted |
| [0014](0014-dev-environment-profiles.md) | Dev environment profiles follow ADR-0009 | Accepted |
| [0015](0015-allow-0bsd-license.md) | Allow the 0BSD license | Accepted |
| [0016](0016-ssh-rsa-and-host-key-policies.md) | SSH – RSA keys disabled until russh moves to rsa ≥ 0.10, and non-strict host-key policies | Accepted |
