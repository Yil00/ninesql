# ADR-0014: Dev environment profiles follow ADR-0009

- **Status**: Accepted (maintainer, 2026-11-14)
- **Date**: 2026-11-14

## Context
Two accepted ADRs describe the compose profiles of `dev/` differently:

- [ADR-0003](0003-supported-engines-v1.md), Consequences: "Heavy engines (Elasticsearch, OpenSearch, Cassandra, Neo4j, Trino, YugabyteDB) are in a separate compose profile."
- [ADR-0009](0009-container-first-test-environment.md), Decision §2: the `heavy` profile holds Elasticsearch, OpenSearch, Cassandra and Neo4j; Trino and YugabyteDB are in the `sql` profile.

Both statements are about the same thing, the profiles of `dev/compose.yaml`, so they cannot both hold. ADR-0003 decides which engines are supported; its sentence on profiles is a side remark written before the test environment was designed. ADR-0009 is the decision dedicated to the test environment and sets the profile list explicitly.

The P0-C implementation (`dev/compose.yaml`, `dev/engines.json`, `.github/workflows/dev-env.yml`, `dev/README.md`) follows ADR-0009. Keeping YugabyteDB in `sql` matters: it is an MVP engine ([docs/04](../04-mvp-scope.md)), and ADR-0009 §5 runs `heavy` only nightly, on demand and on PRs that touch its drivers, so moving YugabyteDB to `heavy` would take an MVP engine out of the per-PR matrix. Trino (v0.2) belongs with the other SQL engines for the same reason.

YugabyteDB and Trino are still resource-hungry: the `sql` profile needs about 6 GB of RAM and 12 GB of disk, and YugabyteDB refuses writes when the disk is nearly full ([dev/README.md](../../dev/README.md)). That is what ADR-0003 meant by "heavy"; it is a resource note, not a profile assignment.

## Decision
1. The compose profiles of `dev/` are those of ADR-0009 §2. Where ADR-0003 and ADR-0009 disagree on profiles, ADR-0009 governs.
2. This supersedes only the sentence of ADR-0003's Consequences quoted above. ADR-0003 is otherwise unchanged, and so is ADR-0009.
3. Trino and YugabyteDB stay in `sql`. Their resource needs are documented in `dev/README.md`, not expressed by a profile.
4. Moving an engine between profiles later is a change to ADR-0009's list and needs a new ADR.

## Consequences
- No change to `dev/compose.yaml`, `dev/engines.json`, the CI workflow or [docs/08](../08-engine-capabilities.md), which already follow ADR-0009.
- The `sql` CI job carries YugabyteDB and Trino on every PR that touches `dev/` or a driver; its runtime and disk use must stay within the job's `timeout-minutes` and the runner's disk (the job frees disk space before pulling images).
- When accepted, the ADR index marks ADR-0003 as partially superseded by this ADR.

## Rejected alternatives
- **Follow ADR-0003 and move Trino and YugabyteDB to `heavy`**: takes an MVP engine out of the per-PR CI matrix, and contradicts the implemented and documented environment.
- **Edit ADR-0003's text**: accepted ADRs are not modified; a superseding ADR keeps the history.
- **A sixth profile for resource-hungry SQL engines**: adds a CI job and a profile name for two engines without a need shown by P0-C.
