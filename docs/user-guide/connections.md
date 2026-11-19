# Connections

A connection is a saved set of parameters to reach one database server or file: engine, endpoint, user, a reference to the stored password, TLS, SSH, connection type. Step by step, field by field: [Create your first connection](../tutorials/first-connection.md).

## The connection dialog
"New connection" (`Ctrl+Alt+N`) opens the dialog. It only offers what the drivers declare: the engines, TLS modes, authentication kinds and driver properties come from each driver's capabilities, and what a driver does not support is hidden or disabled, never faked ([invariant I8](../../CONTEXT.md#invariants-non-negotiable)).

- **Endpoint**: host, port (empty means the engine's default) and database, or the absolute path of a SQLite file. "URL" fills the fields from a pasted URL and is not kept; a URL that carries a password is refused.
- **Password**: "Save in vault" / "Save in keyring" / "Ask every time" ([First start and secrets](first-start-and-secrets.md#where-passwords-go)).
- **SCRAM only** (PostgreSQL, TimescaleDB, YugabyteDB): refuses md5, cleartext and servers that skip authentication, so a server cannot downgrade the password exchange. It does not stop a man in the middle that relays the exchange when TLS is below "Verify CA"; the dialog warns about that combination.
- **Driver properties**: only a short allowlist per engine (for example `search_path`, `statement_timeout` on PostgreSQL; `time_zone` on MySQL; `busy_timeout` on SQLite). Properties can never change TLS, credentials, read-only or other safety settings ([docs/08](../08-engine-capabilities.md#capability-model)).
- **Folder**: a `/`-separated path such as `team/sales`.
- **"Test connection"** runs the real connect flow and closes the session; **"Save"** writes the connection to `connections.json`, without any password.

Edits to an open connection apply at the next connect.

## TLS
TLS is verified by default ([invariant I5](../../CONTEXT.md#invariants-non-negotiable)). The modes the driver declares are listed strongest first:

| Mode | What is checked |
|---|---|
| "Verify CA and host name" (default) | The certificate chains to a trusted CA and names the host you typed. A "CA file", when given, replaces the system trust store |
| "Verify CA" | The certificate chains to the CA file you give (required); the host name is not checked |
| "Encrypt only" | Nothing: encrypted, but a man in the middle can read and change the traffic |
| "Disabled" | No TLS: traffic, credentials included, is readable on the network |

Each weaker mode shows a warning in the dialog and among the session's security warnings. A server that refuses TLS fails the connect: there is never a fallback to plain text. Client certificates (an unencrypted PEM key) are supported on PostgreSQL, TimescaleDB, YugabyteDB, MySQL and MariaDB. QuestDB and TiDB offer "Disabled" only ([below](#engine-conditions)); SQLite has no network.

## SSH tunnels
"Use an SSH tunnel" adds an SSH server, then "Add jump host" adds hops in front of it, each with its own authentication ("SSH agent", "Password", "Key file") and its own "Host key policy":
- **"Strict (confirm unknown keys)"** (default): an unknown key is shown with its fingerprint for you to confirm ("Trust this key and connect");
- **"Accept new keys"**: unknown keys are recorded without asking; known keys are still verified;
- **"Do not verify"**: anyone on the path can impersonate the SSH server; the dialog says so.

A host whose key **changed** is refused, with no way around it from that dialog. Trusted keys go to NineSQL's own `known_hosts`; `~/.ssh/known_hosts` is read, never written. Key files must be **Ed25519 or ECDSA**: RSA key files are refused for now (use the SSH agent for an RSA key). A tunnel protects the way to the SSH server only: when the database is elsewhere and TLS is "Disabled", the dialog warns that credentials cross the last leg in clear.

## Connection types
"Connection type" offers **Development**, **Test** and **Production** (and types you define or import), each with a color. A type carries rules:
- **Production**: destructive statements, init SQL and saves of edited rows need confirmation, new sessions start in **manual commit**, statement text is never logged nor kept in the history, and read-only comes from the type ("Read-write" is not available for a production type);
- a type can also ask for confirmation of **every** write, or turn off statement logging.

The type color shows as a stripe in the navigator, a badge in the status bar and, for production, a frame around the editor area while that connection is active ([Safety model](safety-model.md#production-connections)).

## Read-only connections
"Read-only" (or a type that is read-only) makes the **database engine** refuse writes for the whole session, not only the interface ([invariant I3](../../CONTEXT.md#invariants-non-negotiable)). If the engine does not confirm read-only, the session is closed. How each engine does it, and the engines where read-only depends on the server: [Safety model](safety-model.md#read-only-is-enforced-by-the-database).

A SQLite file inside a DBeaver directory always opens read-only, with a "Read-only: DBeaver file" badge.

## Manage connections
| Action | Keys |
|---|---|
| "Connect" the selected connection | `Ctrl+K` then `Ctrl+O` |
| "Disconnect" | `Ctrl+K` then `Ctrl+X` |
| "Edit connection" | `F4` |
| "Duplicate connection" (saved passwords are not copied) | `Ctrl+K` then `Ctrl+D` |
| "Delete connection" (asks first; deletes its saved secrets and closes its sessions) | `Ctrl+K` then `Delete` |
| "Test connection" | `Ctrl+Alt+T` |
| "Show connection problems" | `Ctrl+K` then `Ctrl+L` |

Connecting may first ask for the vault's master password, "Ask every time" passwords, an unknown SSH host key and, for imported init SQL, a confirmation. A session with weakened settings shows a "Security warnings" notification that stays until you close it. Deleting or disconnecting a connection with an open transaction asks whether to commit or roll back (always for production; for other types a preference can turn it off, [SQL editor](sql-editor.md#preferences)).

## Engine conditions
| Engine | TLS | Read-only | Notes |
|---|---|---|---|
| PostgreSQL | All four modes | Engine-side session setting, checked after every statement | One database per connection (no database level in the navigator) |
| TimescaleDB | All four modes | As PostgreSQL | Needs the `timescaledb` extension in the connection's database; hypertables and continuous aggregates are browsed, chunks hidden |
| YugabyteDB (YSQL) | All four modes | As PostgreSQL | "Read committed" offered only when the server really runs it |
| QuestDB | "Disabled" only | Only as QuestDB's read-only user or on a read-only server | Cleartext password: sent only through an SSH tunnel whose hops all use the strict host-key policy; no transactions; no row editing; no driver properties |
| MySQL, MariaDB | All four modes | Engine-side session setting, checked after every statement | — |
| TiDB | "Disabled" only (every TiDB connection is unencrypted for now) | Only on a read-only TiDB instance, for an account that mode covers (not `root`) | Use an SSH tunnel for a remote TiDB; exports run without an engine-side read-only transaction |
| SQLite | — (local file) | File opened read-only, writes denied by an authorizer | The file must exist and the path be absolute; no user or password |

At connect NineSQL checks that the server is the engine you chose ("this server reports version …, which is …, not …; choose the matching engine"). The details per engine, including the exact read-only messages of QuestDB and TiDB: [Create your first connection, section 8](../tutorials/first-connection.md#8-timescaledb-yugabytedb-questdb-and-tidb) and [docs/08](../08-engine-capabilities.md).

## Not built yet
- Proxies (SOCKS5, HTTP CONNECT), network profiles, Kerberos / GSSAPI, encrypted client keys, RSA SSH key files.
- TLS for TiDB, SCRAM channel binding (`SCRAM-SHA-256-PLUS`).
- Applying a connection's default schema or catalog at connect (stored by the DBeaver import, not used yet).
- A reconnect offered when a session is lost: connect again by hand.
- Drag-and-drop between folders (folders are a typed path).
