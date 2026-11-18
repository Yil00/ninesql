# ADR-0020: Phase 1 driver decisions

- **Status**: Accepted (maintainer, 2026-11-15). Dated note on §2 (2026-11-16, phase 5): the MySQL text literal is now spelled `CONCAT(_utf8mb4 X'…')`, rule unchanged.
- **Date**: 2026-11-15

## Context
Phase 1 (P1-B, the driver-API PR of 2026-11-15 and wave 3) landed the pgwire, mysqlwire and sqlite drivers and the connect capabilities of the driver API. The security reviews of those waves, and the P1-E end review, listed several security-relevant decisions that were made in code and documented in [docs/03](../03-tech-stack.md#driver-clients), [docs/05](../05-security.md) and [docs/08](../08-engine-capabilities.md), but recorded in no ADR ([ROADMAP](../ROADMAP.md#phase-1-follow-ups), row "ADR questions"). This ADR records the driver-side decisions as they are implemented; it changes no behaviour. The core, secrets and storage decisions are in [ADR-0021](0021-phase-1-core-secrets-and-storage-decisions.md).

Invariants involved: I1 (no secret in clear), I3 (engine-side read-only), I4 (DBeaver files never modified), I5 (TLS verified by default), I6 (bounded results), I8 (honest capabilities).

## Decision
### 1. mysqlwire: a hand-written protocol client
1. `ninesql-driver-mysqlwire` implements the MySQL client/server protocol itself on top of `tokio` and `tokio-rustls` (`ring` provider): packets, handshake, authentication and the text protocol. No protocol crate, no C library, no OpenSSL ([ADR-0011](0011-tls-and-ssh-defaults.md)).
2. The subset is fixed: protocol 4.1 with plugin authentication, one text-protocol statement (`COM_QUERY`) per call, multiple results for `CALL` only. Never `CLIENT_MULTI_STATEMENTS`, never `CLIENT_LOCAL_FILES` (a `LOAD DATA LOCAL` request is answered with an empty file and the statement fails), no compression, no prepared statements (`COM_STMT_*`), never `CLIENT_IGNORE_SPACE`.
3. Packets are read as a stream across the 16 MiB packet boundary, so values are cut to the budget while rows are read (I6).

### 2. mysqlwire: parameters inlined client-side as hexadecimal literals
1. With the text protocol, bind parameters are inlined by the driver as literals that cannot leave their token in any `sql_mode` or character set: hexadecimal strings (`CONVERT(X'…' USING utf8mb4)` for text and JSON, `X'…'` for bytes), digits (negative numbers in parentheses), and `DATE` / `TIMESTAMP` literals.
2. Placeholders are found with the session's lexical rules, so a `?` inside a string, an identifier or a comment is never replaced; the numbers of placeholders and parameters must match.
3. Values with no faithful MySQL literal are refused: UTC timestamps, NaN and infinities.
4. The driver's own queries follow the same rule: user input (filters, schema and table names, cursors) reaches the SQL text only as hexadecimal literals compared as binary strings.

   > **Note (2026-11-16, phase 5).** The rule of this section is unchanged: values still reach the statement text only as hexadecimal literals. Its text form is now spelled `CONCAT(_utf8mb4 X'…')` instead of `CONVERT(X'…' USING utf8mb4)` (P5-B re-review, commit 2b64682): the `_utf8mb4` introducer makes the string coercible, so the column's collation applies in comparisons (a `latin1_swedish_ci` or `_as_cs` key), and `CONCAT` makes it a string expression (alone, an introduced hex literal is stored into `JSON` as binary and read as a bit literal by TiDB). The empty string is `_utf8mb4''`. The same form is used by the DML previews ([ADR-0027](0027-phase-5-editing-transfer-and-export-decisions.md) §1.2, accepted; [docs/08](../08-engine-capabilities.md#mysqlwire-mysql-and-mariadb-p1-b)).

### 3. mysqlwire: the password only over a protected transport
1. A transport is **protected** when TLS is `verify-ca` or `verify-full`, or when it is an SSH tunnel whose hops all verify host keys and whose local end is loopback. TLS `require` is not protected.
2. Challenge-response scrambles (`caching_sha2_password` fast path, `mysql_native_password`) are allowed over any transport.
3. Exchanges that send the password itself are refused over an unprotected transport, with a message saying how to connect (verified TLS once): **full `caching_sha2_password` authentication** (the server has no cached verifier for the account, typically the first connection after a server restart) and `mysql_clear_password`.
4. The server's RSA public key is **never requested**: a man in the middle could send his own. `allowPublicKeyRetrieval` is a reserved property. `sha256_password`, `client_ed25519`, `auth_gssapi_client` and other plugins are refused. At most one plugin switch is accepted.

### 4. Engine mismatch refused
1. A connection declares its engine. mysqlwire checks the server against it on the cleartext greeting and again with `VERSION()` inside the (possibly TLS) session.
2. A MariaDB server reached through a connection declared MySQL, or the reverse, fails the connect with a message asking to choose the matching engine: capabilities, the access-mode variable and the dialect differ between the two (I8).
3. A TiDB server is refused until TiDB has its own tested overrides (P4-C).

### 5. TLS modes against the driver's declaration
`Capabilities::tls` is the exact set of TLS modes a driver supports. On network endpoints, `ninesql-core` (`Connection::validate_for_driver`, at save, at import and in the connect flow) applies:
1. A driver that declares **no** TLS (empty set) accepts only `disable`. A connection saved as `verify-full` must not look encrypted when it is not (I5, I8).
2. A driver that declares TLS accepts exactly the modes it declares. One that declares TLS **without** `disable` (a TLS-only driver) refuses `disable`.
3. File endpoints are not checked against `Capabilities::tls`.

### 6. `ScramOnly` also refuses trust authentication
`PasswordExchange::ScramOnly` means "only a SCRAM exchange in which the server proves it knows the verifier", as libpq's `require_auth=scram-sha-256`. pgwire therefore refuses, before anything derived from the password is sent:
1. a request for md5 or a cleartext password;
2. a server that sends `AuthenticationOk` without a verified SCRAM exchange (trust authentication).

It stops method downgrades only. Without channel binding (`SCRAM-SHA-256-PLUS` is a follow-up), a man in the middle that relays the exchange passes when TLS is below `verify-full`. A driver that cannot enforce the policy does not declare it and refuses it at connect (mysqlwire: no MySQL exchange proves that the server knows the verifier).

### 7. `ConnectCapabilities::default()` is the most conservative declaration
`ConnectCapabilities::default()` accepts nothing a driver has not opted into: an empty property allowlist, `AuthKind::None` only, `PasswordExchange::Any` only, `verify_ca_requires_ca_file = true`, no client certificates and no encrypted client keys. A driver that forgets a field refuses the setting; it never accepts a setting it does not enforce (I8).

### 8. DBeaver files: immutable open preferred over `-wal` freshness (note on ADR-0006)
This is a note on [ADR-0006](0006-dbeaver-interoperability.md) §1 (I4); it does not change that ADR.
1. When `ConnectParams::inside_dbeaver_workspace` is set on a file endpoint, file drivers open the file read-only whatever `read_only` says, never create it, and never create or modify a file next to it. Network drivers ignore the flag.
2. sqlite opens such a file read-only **and immutable** (`?mode=ro&immutable=1`, the URI built by the driver from the canonical path, never from user text), whatever its journal mode: SQLite creates no journal, `-wal` or `-shm` file next to it.
3. Freshness is given up for I4: pages committed only in a `-wal` file (a writer that crashed or is still running) or in a hot journal are **not visible** in that session, and its first notice says so. If another program writes to the database while the session is open, results may be stale or fail with `SQLITE_CORRUPT`; the file is never damaged.

### 9. sqlite: protections in every session
Every sqlite session, read-only or not, runs with:
1. `SQLITE_DBCONFIG_DEFENSIVE` (no `writable_schema` writes, no `journal_mode = OFF`, no shadow-table writes);
2. `SQLITE_DBCONFIG_TRUSTED_SCHEMA` off: views and triggers stored in a hostile file cannot call functions with side effects or virtual tables;
3. double-quoted string literals off (`SQLITE_DBCONFIG_DQS_DML` and `DQS_DDL`): a double-quoted name is always an identifier, so an edit's `WHERE "id" = ?` cannot match every row;
4. `SQLITE_LIMIT_LENGTH` set per statement to the caller's `max_bytes` (at least 16 MiB, at most 10⁹ bytes): a single value larger than the result's byte budget fails the statement instead of being loaded (I6);
5. the pragmas that would undo these denied by the authorizer, and `load_extension` never enabled;
6. foreign keys **on** by default (the bundled build's default, unlike the `sqlite3` shell), switchable with the `foreign_keys` driver property.

The thread model of the sqlite driver is in the [amendment of ADR-0004](0004-driver-api-runtime-and-results.md#amendment-sqlite-session-threads-2026-11-15).

## Consequences
- mysqlwire needs no new dependency and keeps control of `KILL QUERY` on its control connection, the access-mode read-back and the session's lexical modes. The protocol code is NineSQL's to maintain and test: fake servers in its unit tests cover the authentication refusals, the plugin switch limit and the handshake flags.
- No server-side prepared statements on MySQL / MariaDB: every parameterized statement is parsed again by the server. Revisit if the P3 performance tests show a cost.
- A MySQL 8 account with no cached verifier cannot connect over plain TCP or TLS `require` until it has connected once over verified TLS. The error message says so.
- Connecting to the wrong engine of the MySQL family is an error, not a silently wrong capability set.
- A connection whose TLS mode the driver does not support cannot be saved or imported. The DBeaver import (P2-C) must map such modes or report them.
- `ScramOnly` must not be presented as a protection against a man in the middle until channel binding exists (ROADMAP follow-up).
- New drivers start from `ConnectCapabilities::default()` and opt into each setting their tests prove.
- An SQLite file inside a DBeaver workspace may show stale data while DBeaver writes to it.
- Tests to keep: the mysqlwire authentication refusals over unprotected transports (fake servers and against `dev/`), the wrong-engine test, the `ScramOnly` trust refusal (scripted server), the `validate_for_driver` TLS rules, the sqlite immutable open and the bypass list of docs/08.

## Rejected alternatives
- **`mysql_async`**: it reads each row whole (no value cutting while rows are read), and its protocol crate `mysql_common` brings a large tree (compression, big-number and date crates) for features the driver does not use ([docs/03](../03-tech-stack.md#driver-clients)).
- **Server-side prepared statements for parameters**: a second protocol path to write and secure, for no gain in the MVP subset.
- **Escaping parameters as quoted strings**: their meaning depends on `NO_BACKSLASH_ESCAPES`, `ANSI_QUOTES` and the character set; hexadecimal literals do not.
- **Requesting the server's RSA key for `caching_sha2_password` full authentication**: the key is not authenticated over an unprotected transport, so a man in the middle could get the password.
- **Accepting a MariaDB server under a MySQL connection, or the reverse**: the declared capabilities and the access-mode variable would be wrong.
- **Accepting any TLS mode and ignoring it in drivers without TLS**: the saved connection would claim an encryption it does not have.
- **`ScramOnly` that allows trust**: a server (or a man in the middle) could skip authentication and the user would believe SCRAM was used.
- **A permissive `ConnectCapabilities::default()`**: a driver that forgets a field would accept settings it does not enforce.
- **A normal read-only open of DBeaver files to see `-wal` pages**: SQLite would create or modify `-shm` (and possibly `-wal`) files inside the DBeaver workspace, against I4.
