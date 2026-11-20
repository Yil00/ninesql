# Known limitations

What NineSQL does not do yet, or does only partly, as of phase 6. The list is built from the project's open items ([CONTEXT.md](../../CONTEXT.md#where-the-project-stands), the follow-up tables of the [ROADMAP](../ROADMAP.md)) and rephrased for users. NineSQL declares per engine what it supports and hides the rest rather than faking it ([invariant I8](../../CONTEXT.md#invariants-non-negotiable)); this page lists the gaps you may still run into.

"When" says:
- **v0.1.0 (P6)**: planned before the first release, in phase 6;
- **Later**: not scheduled for v0.1.0; it may come in a later version, or stay a documented limit.

## Release and platform
| Limitation | When |
|---|---|
| No release and no published package yet: you build from source, or build the `.deb` yourself with `make deb` ([Install](install.md#the-deb)). The release workflow (signed `.deb`, SBOM) has not run yet; v0.1.0 will be a cosign-signed `.deb` attached to the GitHub release | v0.1.0 (P6) |
| No signed APT repository and no Flatpak: v0.1.0 ships only the `.deb` on the GitHub release, so updates are downloaded and verified by hand (the Flatpak has not been built yet) | Later |
| In the Flatpak (once built), a SQLite file chosen through the file chooser opens read-only unless it lies directly in your home folder or its folder is granted with `flatpak override` | Later |
| The `.deb` has no `md5sums`, so `dpkg --verify` and `debsums` cannot check the installed files (the package itself is covered by `SHA256SUMS` and its signature) | Later |
| "Open with NineSQL" is not offered for `.sql` files: `ninesql` does not open a file given on its command line yet | Later |
| The interface was written and tested without a display: its visual check on real Linux desktops (Wayland and X11), with screenshots and a manual run of the main scenarios, has not been done | v0.1.0 (P6) |
| Start time, idle memory, time to the first rows on screen and scrolling smoothness have **not been measured** on real hardware; no figure is published for them. The only published numbers are core-level test timings (time to the first rows of a 1 M-row query, export and import speeds), each with its method and commit | v0.1.0 (P6) |
| Accessibility with the Orca screen reader has not been tested | v0.1.0 (P6) |
| The end-of-MVP security review (2026-11-19) found no critical or high issue; its verdict is "ready for v0.1.0 with conditions", and the conditions (missing license notices, crash output that could print internal values, release-workflow checks, among others) are being fixed before the release | v0.1.0 (P6) |
| The `.deb` carries its own copy of the FreeType font library, which does not receive Debian or Ubuntu security updates: a FreeType fix reaches you only with a new NineSQL release | Later |
| Opening a window on Ubuntu 22.04 and Debian 12 is not checked yet (GPUI may need newer graphics libraries); the `.deb` installs and runs `--version` on Ubuntu 22.04 | v0.1.0 (P6) |
| macOS is a development platform only (no locking on sleep or screen lock, no keyring, macOS-only dependencies not license-checked); Windows is not supported; arm64 is not built | Later |

## Secrets
| Limitation | When |
|---|---|
| The desktop keyring backend (Secret Service) is off in default builds: saved passwords go to the encrypted vault, which you unlock with a master password after each start. It stays off until it has been validated on GNOME and KDE (Debian 12, Ubuntu 24.04) and its client library reaches a stable release | Later |
| Text typed in a password field, a master password field or the parameters panel may stay in memory after the field is cleared (the text input component does not wipe its buffer); NineSQL's own copies of a run's parameter values are not wiped either | Later |
| A `vault.bin` replaced by an older copy is not detected | Later |
| After NineSQL's lock watcher recovers from a lost system bus, the "not watched" entry of "Show connection problems" can stay until the next repaint (it errs on the safe side) | Later |

## Connections
| Limitation | When |
|---|---|
| **TiDB has no TLS**: every TiDB connection is unencrypted. The password itself is not sent in clear, but someone who can capture the traffic can record the login exchange (`mysql_native_password` or `caching_sha2_password` scramble) and try to guess a weak password offline. Queries and results also cross in clear and can be **altered on the way**, including the answers to the checks that keep a read-only TiDB session read-only, so a read-only TiDB session is only as trustworthy as the network path. Use an SSH tunnel for any TiDB that is not on your own machine | Later |
| **QuestDB has no TLS** (open-source QuestDB offers none) and takes passwords in clear: the password is sent only through a strict SSH tunnel, and crosses the last leg in clear when the tunnel ends on a bastion in front of QuestDB. Tunnel to the QuestDB host itself when you can | Later |
| QuestDB's read-only check is verified on open-source QuestDB only; QuestDB Enterprise has its own access control | Later |
| No SCRAM channel binding (`SCRAM-SHA-256-PLUS`): "SCRAM only" stops downgrades, not a relaying man in the middle below "Verify CA" | Later |
| While an SSH tunnel is open, its local end listens on `127.0.0.1` on a random port that **other users of the same machine** can connect to: they get a network path to the database through your tunnel (not your credentials; the database still asks them to log in). Avoid SSH tunnels on shared multi-user machines, or use database accounts with strong passwords there ([docs/05](../05-security.md#accepted-risks)) | Later |
| RSA SSH key files are refused (use Ed25519, ECDSA or the SSH agent); no proxies (SOCKS5, HTTP), network profiles, Kerberos or encrypted client keys | Later |
| No reconnect is offered when a session is lost (also after a paused result was closed for idleness): connect again by hand | Later |
| A server-side idle setting (PostgreSQL `idle_in_transaction_session_timeout` on the role, a lowered MySQL `net_write_timeout`) can end a session while a result waits, before NineSQL's own idle limit | Later |
| A connection's default schema and catalog (from a DBeaver import) are stored but not applied at connect | Later |

## Safety and read-only
| Limitation | When |
|---|---|
| Production confirmations look for a missing `WHERE`, not a useless one: `DELETE FROM t WHERE 1=1`, `UPDATE t SET … WHERE true` or a `MERGE … WHEN MATCHED THEN DELETE` count as ordinary writes and are not confirmed (production connections still start in manual commit, so you can roll back) | Later |
| The statement classifier is lexical: a user-defined function that writes, called from a `SELECT`, reads as a read, so it is not confirmed on production and passes the export and filter read checks (read-only sessions still refuse its writes in the engine) | Later |
| A single statement of 64 KiB or more is shown as its head and tail in the production confirmation; a destructive part can sit in the hidden middle | Later |
| The credential rule of the history is lexical: a password built by concatenation could be recorded (never for production connections) | Later |
| TiDB: a privilege granted to your account by another session after connect is only checked at the next connect, and a statement can run while a privileged user lifts the instance's read-only mode, before NineSQL notices | Later |
| SQLite (and any engine without an engine-side read-only transaction) exports with none and without a warning; the export's read check still applies | Later |
| A raw `WHERE` filter holding bidirectional control characters is shown with markers but not refused | Later |

## SQL editor
| Limitation | When |
|---|---|
| No history view (the history file is kept for it), no preferences page (edit `settings.toml`), no auto-commit toggle | Later |
| No keyboard action to fold (mouse only); no completion of foreign-key joins; no `${var}` variables | Later |
| Untitled tabs and unsaved edits are not restored after a restart (by design: they could hold a password) | Later |
| No rename, move or delete of scripts from NineSQL, no script list in the navigator; object editors are not restored; a script changed on disk since the quit is skipped, not reopened | Later |
| No screen to create snippets (edit `snippets.json`); the formatter does not break `AND` / `OR` and has no settings; the per-statement run summary is not shown | Later |
| No "Explain" action or visual plans: type and run `EXPLAIN` yourself | Later (phase 10) |
| "Commit transaction" waits while a paged result keeps the session busy, and up to 30 s while the navigator loads metadata on the same session (closing the tab ends that wait; the commit is then not sent) | Later |

## Navigator
| Limitation | When |
|---|---|
| One database per PostgreSQL connection: other databases of the server are not browsed | Later |
| SQLite: no constraints group, foreign keys named `fk_<id>`; MariaDB and TiDB sequences listed without start and increment | Later |
| DDL leaves parts out on some engines: PostgreSQL collations, privileges and identity options; a hypertable's `create_hypertable` call; PostgreSQL base types and foreign tables have no DDL | Later |
| With very large catalogs, completion is rebuilt on the interface thread after every metadata change (up to 250 000 objects), and one metadata load can hold about 320 MiB briefly | Later |
| Filters of a deleted connection stay in `navigator.json` (harmless); load limits are not configurable | Later |
| MySQL, MariaDB, TiDB: a one-shot `SET TRANSACTION …` (next transaction only) is set again after the navigator reads, except when a stored routine ran it, or, in auto-commit mode, when a `SELECT` without a table ran between it and the navigator read; the next transaction may then run without those settings | Later |
| The navigator's "Not connected" hint always reads "Ctrl+K Ctrl+O", even when "Connect" is rebound | Later |
| No create / alter / drop forms, dependencies, users and roles, schema compare or ER diagrams | Later (phase 10) |

## Results, editing and transfer
| Limitation | When |
|---|---|
| Opening a DBeaver export in DBeaver itself has not been checked by the project yet (including how DBeaver merges TLS settings, and jump hosts before DBeaver 24.3) | v0.1.0 (P6) |
| No in-cell editing (cells are edited in a dialog); inserted rows are not drawn in the grid until the table is read again | Later |
| QuestDB declares no row editing (no unique keys, no `DELETE`) | Later (engine limit) |
| The server-side filter history is kept in memory only; GIF and animated PNG values are shown as hex, not previewed; the client-side sort stops at 200 000 loaded rows | Later |
| Plain `Ctrl+C` puts a `'` in front of a single copied cell that starts like a formula (`+33 6…`, `@alice`), and there is no unguarded "Copy value" | Later |
| Under a client-side sort, a page arriving while you edit the loaded rows of a paused result can move the selection to another row (the preview and confirmation still show the exact statements) | Later |
| CSV import: UTF-8 only, comma and `"` only, existing tables only. A table altered to MyISAM after the navigator loaded it still reads as transactional until you refresh it; PostgreSQL foreign tables count as rollback-safe even through a non-transactional foreign data wrapper; on MyISAM the committed-row count is a lower bound | Later |
| Transfers use an existing session (open the connection twice to keep the navigator free during a long import) | Later |
| QuestDB is exported to DBeaver as PostgreSQL; TiDB has no accent- and case-sensitive collation (engine limit) | Later |
| XLSX, XML and Parquet export, table-to-table transfer, saved transfer tasks | Later (phase 10) |

## DBeaver import
| Limitation | When |
|---|---|
| The DBeaver file decoders have been fuzzed for about 40 to 45 minutes in total per decoder (the longest run 25 minutes), with no crash; a longer campaign is still to run | Later |
| DBeaver SQL scripts are not copied; empty DBeaver folders are not created | Later |
| No screen to relax a built-in connection type that an import made stricter | Later |
| A read blocked on a stalled network or FUSE file cannot be cancelled until the file answers | Later |
| Hard links to DBeaver files, and DBeaver workspaces in custom places never picked for an import, are not recognised as DBeaver files | Later |

## Engines
Only the eight MVP engines are supported: PostgreSQL, MySQL, MariaDB, SQLite, TimescaleDB, YugabyteDB, QuestDB and TiDB. DuckDB, ClickHouse, Trino, Firebird and InfluxDB 3 are planned for v0.2; MongoDB, Redis and Valkey for v0.3; Elasticsearch, OpenSearch, Cassandra and Neo4j for v0.4 ([ROADMAP](../ROADMAP.md)). Per-engine limits are in the [capability matrix](../08-engine-capabilities.md).
