# ADR-0004: Driver API, async runtime and result model

- **Status**: Accepted. Amended (sqlite session threads, accepted by the maintainer on 2026-11-15) by the [section at the end](#amendment-sqlite-session-threads-2026-11-15): a synchronous client may own one dedicated thread per session instead of using `spawn_blocking`; the Decision above is unchanged.
- **Date**: 2026-11-14

## Context
The UI (GPUI) has its own foreground and background executors. Most Rust database clients are built on Tokio. Results range from a 3-row metadata query to millions of rows or nested documents. The UI must never block (I6) and must only offer what a driver supports (I8).

## Decision
### Crate and traits
`ninesql-driver-api` defines the contract every driver implements. It is the **single synchronization point** between UI and drivers (AGENTS.md) and is frozen at the end of P0-B. Main traits (names indicative, refined in P0-B):
- `Driver`: engine ids served, `capabilities(engine, server_version) -> Capabilities`, `connect(ConnectParams) -> Session`.
- `Session`: `execute(Statement, ExecOptions) -> ResultStream`, `cancel()`, transaction control (`begin`, `commit`, `rollback`, `set_auto_commit`), `set_read_only(bool)`, `metadata() -> &dyn Metadata`, `close()`.
- `Metadata`: lazy, paged listing of object kinds (catalogs, schemas, tables, views, columns, indexes, constraints, foreign keys, triggers, routines, sequences, types, users / roles…) and `ddl(object)` where supported.
- `Dialect`: identifier quoting, literal formatting, statement splitting, `LIMIT` / paging syntax, DML generation for edits, explain syntax.

`ConnectParams` carries a **secret handle** resolved by `ninesql-secrets` at connect time, never a password stored in the struct beyond the connect call (I1).

### Capabilities
`Capabilities` is a plain data struct of booleans and enums (metadata kinds, editing, transactions, savepoints, explain formats, server-side cancel, multiple result sets, read-only mechanism, query languages). The UI reads it to enable actions; [docs/08-engine-capabilities.md](../08-engine-capabilities.md) is the human-readable mirror and is updated in the same PR as any change.

### Runtime
- `ninesql-core` owns one **multi-thread Tokio runtime** (worker count bounded, default 4) dedicated to driver and network I/O (drivers, SSH tunnels, exports).
- UI code calls core services that `spawn` on that runtime and deliver results to GPUI entities through channels polled on the GPUI executor. No Tokio future is ever awaited on the GPUI foreground executor.
- Each query gets a timeout (default 0 = none for interactive queries, configurable per connection) and a cancel token wired to the engine's server-side cancel when it exists.

### Result model
- **Tabular** results: a stream of Apache Arrow `RecordBatch` (`arrow-array` / `arrow-schema` crates only, not the full `arrow` crate). Engine types map to Arrow types; a type without a faithful Arrow mapping becomes `Utf8` with the original type name kept in the field metadata (`ninesql.type`), so editing and display stay correct.
- **Document / key-value / graph** results: a stream of `Document` batches (an ordered JSON-like value tree that keeps engine types such as BSON ObjectId, dates and decimals). The grid can flatten documents into columns on demand.
- **Memory budget**: the caller sets `fetch_size` (default 200 rows, DBeaver's default) and `max_bytes` (default 256 MiB per result tab). The stream stops at the budget and reports `Truncated`; "fetch all" asks the user above the budget. Export bypasses the grid and streams directly to the writer (`ninesql-transfer`).
- Update counts, notices / warnings and multiple result sets are first-class stream items.

## Consequences
- Arrow gives a columnar layout the grid can read without copying, and CSV / JSON / Parquet export for free; it costs some compile time (limited by using the sub-crates).
- Drivers that only offer row APIs build Arrow batches through builders; that cost is measured in the P3 performance tests.
- A single runtime means a misbehaving driver can starve others: drivers must not block worker threads (use `spawn_blocking` for synchronous clients such as `rusqlite`, `duckdb`).

## Rejected alternatives
- **A generic `Vec<Vec<Value>>` row model**: simple, but doubles memory for wide results and makes columnar export slower.
- **`sqlx` as the common layer**: covers only PostgreSQL / MySQL / SQLite, hides server-side cancel and catalog specifics, and does not help with the 17 other engines.
- **GPUI's executor for driver I/O**: most clients require Tokio; mixing would make cancellation and timeouts unreliable.

## Amendment (sqlite session threads, 2026-11-15)
**Status: Accepted** (maintainer, 2026-11-15). It adds to the Consequences above and changes no point of the Decision. Raised by the P1-E review: `ninesql-driver-sqlite` does not follow the wording of the third Consequence. Behaviour as implemented: `crates/drivers/ninesql-driver-sqlite/src/worker.rs` and [docs/08](../08-engine-capabilities.md#sqlite-sqlite-p1-b).

**Context.** The third Consequence above says drivers built on a synchronous client must not block Tokio worker threads and names `spawn_blocking` as the way to run such clients (`rusqlite`, `duckdb`). A `rusqlite::Connection` is used by one thread at a time, and the sqlite driver streams results lazily under the caller's budget (I6): each poll of the `ResultStream` steps at most one batch, so a paused grid pauses the statement. A paused statement keeps its prepared statement, and so the thread that owns the connection, waiting for the next demand, for up to `PAUSED_RESULT_TIMEOUT` (300 s). Tokio's blocking pool is shared by the whole driver runtime (the core's secret reads also run there); a task that waits for minutes on a paused result would hold one of its threads for that long.

**Decision.**
1. A driver built on a synchronous client **may** give each session one dedicated OS thread that owns the client connection, instead of running each call through `spawn_blocking`. The rule of the third Consequence stands: no synchronous client call runs on a Tokio worker thread or on the GPUI foreground executor.
2. The async side talks to that thread through channels: commands go in, and results come back one batch per demand of the result stream (Tokio `oneshot`), so the thread never runs ahead of the consumer (I6).
3. The thread is bounded like any other driver I/O: cancel and timeout reach it (sqlite: `sqlite3_interrupt` and a flag read between rows and while a result is paused), the driver's own statements have a deadline (sqlite: 60 s), a thread that does not answer in time closes the session (sqlite: 120 s), and a result left paused longer than its limit is closed (sqlite: 300 s). Dropping the stream or the session stops a running statement. Closing the session ends the thread.
4. `ninesql-driver-sqlite` applies this: one thread named `ninesql-sqlite` per session, started by `connect`. A panic while opening is caught and reported as a connect error.

**Consequences.**
- One OS thread per open SQLite session, on top of the Tokio runtime's bounded workers and blocking pool. There is no separate cap on these threads: the number of open sessions bounds them.
- A paused result never takes a thread of the shared blocking pool, so it cannot starve the core's secret reads or other drivers' blocking calls.
- A navigator call while a result streams on the same session fails with `Busy` (the session's thread serves both).
- Later synchronous clients (DuckDB, P7) choose between `spawn_blocking` and a session thread by the same test: a client that must keep a statement open between demands uses a session thread.

**Rejected alternatives.**
- **`spawn_blocking` per call**: a paused result would hold a blocking-pool thread for up to 300 s, and the connection would have to move between pool threads between calls.
- **Reading the whole result in one blocking call**: breaks the memory budget (I6).

