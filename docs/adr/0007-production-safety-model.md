# ADR-0007: Production safety model

- **Status**: Accepted. Note (P1-D core, 2026-11-14): §3 is extended to init SQL, which is classified and confirmed when it would run at connect, not refused when saved ([docs/02](../02-architecture.md#confirmations-adr-0007-adr-0011)).
- **Date**: 2026-11-14

## Context
DBeaver offers connection types (Development, Test, Production) with a color, an auto-commit default, "confirm SQL execution" and a read-only flag. The read-only flag is mostly enforced on the client side through JDBC `setReadOnly`, whose effect depends on the driver. Incidents with GUI clients often come from a statement run in the wrong tab.

## Decision
1. **Connection types**: Development, Test, Production built in (DBeaver-compatible names and default colors), user-defined types allowed. Each type sets: color, auto-commit default, confirm destructive statements, confirm every write, read-only default, and whether query text may be logged.
2. **Read-only is engine-side** (I3). When a session is read-only, the driver applies the engine's mechanism (e.g. `SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY` on PostgreSQL, `SET SESSION TRANSACTION READ ONLY` on MySQL / MariaDB, `PRAGMA query_only = ON` on SQLite, a read-only open mode for file engines, the engine's read-only user or option where nothing else exists). An engine without any engine-side mechanism declares `read_only: ClientSideOnly` in its capabilities, and the UI says so in plain words.
3. **Destructive statement guard**: on connections whose type asks for confirmation, statements are classified by the dialect's parser before execution (`DROP`, `TRUNCATE`, `DELETE` / `UPDATE` without `WHERE`, `ALTER`, `GRANT` / `REVOKE`, and engine-specific equivalents such as `FLUSHALL` on Redis or `dropDatabase` on MongoDB). The dialog shows the exact statement and the connection name typed in color. Unparseable statements count as destructive.
4. **Visual identity**: the connection color frames the editor tab and the results; the type name is always visible in the status bar.
5. **Manual commit by default for Production**, with a pending-transaction indicator and a confirmation when closing a tab with an open transaction.
6. Production connections never log query text, even at `trace` level.

## Consequences
- Each dialect needs a statement classifier good enough to find destructive statements across comments, case, multiple statements and CTEs; it gets property tests and a corpus of tricky cases.
- The engine-side mechanism of each engine is listed in [docs/08-engine-capabilities.md](../08-engine-capabilities.md).
- Users with a wrongly typed connection are still at risk: the import from DBeaver keeps their existing types, and the connection dialog asks for the type explicitly when creating a connection.

## Rejected alternatives
- **Client-side read-only only**: bypassable by any statement the client fails to classify.
- **Regex-based detection**: misses comments, quoting and multi-statement scripts.
