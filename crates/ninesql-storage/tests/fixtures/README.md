# Storage migration fixtures

`sample/` holds every schema version of the synthetic `sample` document used by
`tests/migrations.rs` to exercise the migration engine (ADR-0008 §2):

| File | Schema | Shape |
|------|--------|-------|
| `v1.json` | 1 | `{ "host": "…", "port": 5432 }` per item, list under `items` |
| `v2.json` | 2 | `host` renamed `hostname` |
| `v3.json` | 3 | `port` moved under `endpoint`, `tags` added (default empty) |
| `v3.toml` | 3 | Same as `v3.json`, TOML syntax |

Real documents (for example `connections.json` in `ninesql-core`) keep a fixture of every previous
schema version next to their own tests. Fixtures never contain real hosts or passwords.
