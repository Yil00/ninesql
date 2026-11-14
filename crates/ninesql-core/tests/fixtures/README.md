# Core workspace fixtures

One file per published schema version of each workspace document (ADR-0008 §2). When a
`schema_version` is bumped, the previous version's fixture stays here and `tests/format.rs` keeps
loading it through the migration chain.

| File | Document | Schema |
|------|----------|--------|
| `connections/v1.json` | `connections.json` | 1 |
| `connection-types/v1.json` | `connection-types.json` | 1 |
| `project/v1.json` | `project.json` | 1 (current: 2, adds `id`) |

Hosts use `example.test`; no fixture contains a real host or any secret value, only secret
references.
