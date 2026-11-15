# ADR-0007: Production safety model

- **Status**: Accepted. Note (P1-D core, 2026-11-14): §3 is extended to init SQL, which is classified and confirmed when it would run at connect, not refused when saved ([docs/02](../02-architecture.md#confirmations-adr-0007-adr-0011)). Amended (classifier, accepted by the maintainer on 2026-11-15) in the [section at the end](#amendment-classifier-2026-11-15): two deliberate over-classifications of `EXPLAIN` / `DESCRIBE` and `ANALYZE`, and `CREATE OR REPLACE` as destructive.
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

## Amendment (classifier, 2026-11-15)
**Status: Accepted** (maintainer, 2026-11-15). It adds to Decision §3 and changes none of its points. Raised by the P1-E review (M1). Behaviour as implemented by commit 3291ca0 (`fix(driver-api): classify running EXPLAIN forms and CREATE OR REPLACE`, branch `fix/p2-pre-classifier`, security-reviewed; merged into the pre-phase-2 integration branch on 2026-11-15, it reaches `dev` with that PR): the shared statement classifier of `ninesql-driver-api` and its conformance lists `POSTGRES_EXPLAIN_WRITES` and `MYSQL_EXPLAIN_WRITES`.

**Context.** §3 classifies statements before execution, and the same classes decide what a read-only session refuses client-side (I3, on top of the engine-side mechanism). Some `EXPLAIN` forms run the statement they explain: PostgreSQL `EXPLAIN ANALYZE`, with the option also written quoted or `U&`-escaped inside a parenthesised list (`EXPLAIN ("analyze") DELETE …`, `EXPLAIN (U&"analyze") DELETE …`), MySQL `EXPLAIN ANALYZE`, MariaDB `EXPLAIN ANALYZE` and `DESC ANALYZE`, and MariaDB `ANALYZE <statement>`. The classifier used to detect `ANALYZE` as a bare word only, so `EXPLAIN ("analyze") DELETE FROM t` was a read and skipped the production confirmation while PostgreSQL deleted the rows. Reading every option list exactly as each engine does would make the classifier depend on engine and version details it cannot see.

**Decision.** The classifier errs on the side of confirmation, in two deliberate over-classifications:
1. An `EXPLAIN` / `DESCRIBE` / `DESC` with a **parenthesised option list**, or with `ANALYZE` / `ANALYSE` **in any form** (bare, quoted, backquoted, `U&`-escaped), is classified as the statement it would run: the riskiest of the statements starting at any explainable keyword after it, never below `Read`, and `Unknown` when nothing explainable follows the options or the list is unbalanced. So `EXPLAIN (FORMAT JSON) DELETE FROM t` asks for confirmation on a production connection and is refused in a read-only session, although PostgreSQL does not run the `DELETE` for it.
2. **Every statement starting with `ANALYZE` / `ANALYSE` is `Unknown`**, whatever follows (MariaDB `ANALYZE <statement>` runs the statement; `ANALYZE [TABLE] t` writes statistics on PostgreSQL, MySQL and SQLite). So MariaDB `ANALYZE SELECT …` is refused in read-only sessions and confirmed on production, although it only reads.
3. A plain `EXPLAIN` (no option list, no `ANALYZE`), SQLite `EXPLAIN [QUERY PLAN]` and `DESCRIBE t` stay reads.
4. `CREATE OR REPLACE …` is `Destructive`, not `Ddl`: it silently overwrites an existing object, which the production rule (confirm destructive statements) would otherwise not confirm.

**Consequences.**
- No form of `EXPLAIN` or `ANALYZE` that runs a write can skip the production confirmation or pass the client-side read-only check.
- False positives, accepted: an `EXPLAIN` with options but without `ANALYZE` asks for a confirmation it does not need, and `ANALYZE SELECT` cannot be used in a read-only session on MariaDB. A user who needs a plan in a read-only session uses a plain `EXPLAIN`.
- The conformance lists `POSTGRES_EXPLAIN_WRITES` and `MYSQL_EXPLAIN_WRITES` are part of `writes_for(engine)`, so each driver's read-only tests also check that these forms are refused engine-side.
- Narrowing either rule later (for example, reading PostgreSQL option lists exactly) needs a new amendment or ADR and a security review.

**Rejected alternatives.**
- **Parse each engine's `EXPLAIN` options exactly**: depends on engine and version (option names, boolean spellings, quoting), and a single misread option hides a write from the confirmation.
- **Treat every `EXPLAIN` as a read**: lets `EXPLAIN ANALYZE DELETE` through without confirmation.
- **Treat every `EXPLAIN` as `Unknown`**: plain `EXPLAIN SELECT` and `DESCRIBE t`, which never run anything, would need confirmation and be refused in read-only sessions.

