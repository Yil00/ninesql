# Create your first connection

This tutorial creates, tests and opens a connection to PostgreSQL, MySQL, MariaDB or SQLite from the connection dialog (P1-C). It uses the [dev databases](dev-databases.md); replace the values with your own server's.

What you can do once connected: see the session in the navigator and the status bar, with its type and read-only badges and any security warning. Running SQL and browsing tables come with the SQL editor and results grid (phase 3) and the navigator tree (phase 4); they are not built yet.

## Before you start
- NineSQL builds and runs ([Build and run](build-and-run.md)).
- For PostgreSQL, MySQL or MariaDB: `make dev-up` ([Start the dev databases](dev-databases.md)).

## 1. Open the dialog
Press `Ctrl+Alt+N` ("New connection"), or open the command palette (`Ctrl+Shift+P`) and type "connection". With no connection saved yet, the navigator also says: No connections yet. Use "New connection" (Ctrl+Alt+N) or "Import from DBeaver" (Ctrl+K Ctrl+I). To bring your DBeaver connections over instead, see [Import from DBeaver](import-from-dbeaver.md).

The dialog only offers what the drivers of your build declare: engines, TLS modes, authentication kinds and driver properties come from each driver's capabilities, and what a driver does not support is hidden or disabled, never faked ([invariant I8](../../CONTEXT.md#invariants-non-negotiable)).

## 2. Fill in the connection
From top to bottom:

| Field | PostgreSQL (dev) | MySQL / MariaDB (dev) | SQLite |
|---|---|---|---|
| **Name** (shown in the navigator) | `nine-pg` | `nine-mysql` | `nine-sqlite` |
| **Engine** | PostgreSQL | MySQL or MariaDB | SQLite |
| **Endpoint** | "Host and port": `127.0.0.1`, `15432`, database `nine` | `127.0.0.1`, `13306` (MariaDB `13307`), database `nine` | "Database file": the absolute path, e.g. `/home/you/nine.db` |
| **Authentication** | "User and password": `nine` | "User and password": `nine` | "No authentication" |

Notes:
- **Port**: left empty, the engine's default port is used (the placeholder shows it, "Port (default 5432)").
- **URL instead of fields**: choose "URL", paste a URL and press "Fill fields". The URL only fills the fields and is not kept. A URL that carries a password or any credential-like parameter is refused.
- **SQLite**: the file must exist (NineSQL does not create it) and the path must be absolute. A file that looks like it is inside a DBeaver directory is opened read-only: NineSQL never modifies DBeaver files ([invariant I4](../../CONTEXT.md#invariants-non-negotiable)), and the session shows a "Read-only: DBeaver file" badge. SQLite has no TLS, SSH or password.

### The password
Under the password field, choose how it is kept:
- **"Save in vault"** (or **"Save in keyring"** where a Secret Service backend is enabled): saved in a secret store, never in a file. The first time you save to the vault, NineSQL asks you to create it with a master password; see [Secrets](secrets.md).
- **"Ask every time"**: nothing is saved. At each connect, a "Credentials" dialog asks for it: "Kept in memory for this connection only; never written to disk."

The dialog never shows a saved password, not even masked: it says "Saved in vault", "Saved in keyring", "Not saved" or "Asked at every connect". Password fields have no reveal button and cannot be copied.

### SCRAM only (PostgreSQL)
With "User and password" on PostgreSQL, a "SCRAM only" toggle appears: "Refuse md5, cleartext and servers that skip authentication." It stops a server, or someone in the middle, from downgrading the password exchange. It does **not** stop a man in the middle that relays the exchange when TLS is below "Verify CA": the dialog warns about that combination. MySQL and MariaDB do not offer it (no MySQL authentication exchange gives that proof, [docs/05](../05-security.md#scram-only-authentication)).

## 3. TLS
The TLS row lists the modes the driver declares, strongest first. PostgreSQL, MySQL and MariaDB offer all four; the default is the strongest:

| Mode | What is checked | Warning shown |
|---|---|---|
| **"Verify CA and host name"** (default) | The certificate chains to a trusted CA **and** names the host you typed | none |
| **"Verify CA"** | The certificate chains to the CA file you give; the host name is not checked | "Verify CA: the host name is not checked; any server holding a certificate from this CA is accepted." |
| **"Encrypt only"** | Nothing: the traffic is encrypted, the server is not verified | "Encrypt only: the server certificate is not verified, so a man in the middle can read and change the traffic." |
| **"Disabled"** | No TLS | "TLS is disabled: the traffic, including credentials, is readable on the network." |

**CA file.** With the two verifying modes, a "CA file" field appears. With "Verify CA and host name" it is optional and *replaces* the system trust store. With "Verify CA" it is required: the file you give is the only trusted authority ([ADR-0019](../adr/0019-tls-ca-file-semantics.md)); without it the dialog shows an error and does not save.

**With the dev databases**, the servers use a throwaway CA your system does not trust. Copy it to a file as shown in [Start the dev databases](dev-databases.md#tls-on-the-dev-engines), keep "Verify CA and host name" and put that file in "CA file". The host must be `127.0.0.1` or `localhost`, the names in the dev certificate.

**Client certificates.** PostgreSQL, MySQL and MariaDB accept a "Client certificate (optional)" and its "Client key". Encrypted client keys are not supported by these drivers.

## 4. SSH tunnel (optional)
For a server you reach through SSH. The dev environment has no SSH server, so this part needs your own.

1. Press "Use an SSH tunnel" and fill the "SSH server" block: host, port (22 if empty), user. The same button then reads "Add jump host".
2. Choose its authentication: "SSH agent", "Password" or "Key file". Key files must be **Ed25519 or ECDSA**: "RSA key files are not supported: use an Ed25519 or ECDSA key, or ssh-agent (ADR-0016)." Tick "Key has a passphrase" if it has one; the passphrase is kept like a password (saved in a store, or asked every time).
3. "Add jump host" adds a hop; the hops in front of the SSH server are shown as "Jump host 1", "Jump host 2"…, each with its own authentication.
4. Each hop has a "Host key policy":
   - **"Strict (confirm unknown keys)"** (default): an unknown key is shown to you for confirmation;
   - **"Accept new keys"**: unknown keys are recorded without asking (trust on first use). The dialog warns that a man in the middle on the first connection would go unnoticed; known keys are still verified and a changed key still blocks;
   - **"Do not verify"**: the dialog warns that anyone on the path can impersonate this SSH server and capture the tunnel and its credentials.

When the database is not on the SSH server itself and TLS is "Disabled", the dialog warns that the tunnel only protects the way to the SSH server: from there, the credentials cross the network in clear. Enable TLS, or use host `localhost` when the database runs on the SSH server. TLS still checks the real database host name through the tunnel.

**First connection through a hop with the strict policy**: an "Unknown SSH host key" dialog shows the hop, the key type and its fingerprint: "Compare the fingerprint with the one the server administrator gives you. No credential was sent to this host." Press "Trust this key and connect" only if it matches. The key is then saved in NineSQL's own `known_hosts` (`~/.ssh/known_hosts` is read, never written). A host whose key **changed** is refused, with no way to connect anyway from that dialog ([ADR-0011](../adr/0011-tls-and-ssh-defaults.md)).

## 5. Connection type and read-only
"Connection type" offers the built-in **Development**, **Test** and **Production** types (and any you define), each with its color:
- **Production**: "Production: destructive statements and init SQL need confirmation, and new sessions start in manual commit." Statement text is never logged for production connections.
- The read-only choice is "Type default", "Read-only" or "Read-write". "Read-write is not available for a production type: its read-only setting comes from the type."

Read-only is enforced by the database engine for the whole session, not only by the interface ([invariant I3](../../CONTEXT.md#invariants-non-negotiable), [docs/05](../05-security.md#read-only-sessions)): if the engine does not confirm read-only, the session is closed.

Optionally, put the connection in a **Folder**: a `/`-separated path such as `team/sales`.

## 6. Test, then save
Press **"Test connection"**: it runs the real connect flow with what the dialog holds, then closes the session. The dialog shows "Testing…", then "Connected to <server version>" (with the security warnings, if any) or "Failed: <reason>". Fix the settings until it connects, then press **"Save"**. A notification confirms: Saved "nine-pg".

The connection is written to `connections.json` in the workspace, without any password ([where files are](build-and-run.md#where-ninesql-keeps-its-files)).

## 7. Connect
Select the connection in the navigator (`Ctrl+K` then `Ctrl+N` moves the focus there) and run **"Connect"** (`Ctrl+K` then `Ctrl+O`).
- NineSQL may ask first: the vault's master password if it is locked ([Secrets](secrets.md)), "Ask every time" passwords, an unknown SSH host key.
- On success: "Connected to "nine-pg"." If the session has weakened settings, a "Security warnings" notification lists them; it stays until you close it.
- The status bar always shows the connection type, and a read-only badge when the session is read-only. For a **production** connection, a frame in the type color surrounds the editor area of the window while that connection is the window's active one. The navigator shows each connection's type color stripe and badges. These come from the open session, so an edit you save applies at the next connect, and the dialog says so ("This connection is open: its sessions keep their current settings until you reconnect.").

To run statements on the connection, open an SQL editor (`Ctrl+T`): see [Run SQL and read results](run-sql.md). On a production connection, destructive statements open the "Run on production?" confirmation first.

## Manage connections
| Action | Keys |
|---|---|
| Edit the selected connection | `F4` |
| Duplicate (saved passwords are not copied) | `Ctrl+K` then `Ctrl+D` |
| Delete (asks first; deletes its saved secrets and closes its sessions) | `Ctrl+K` then `Delete` |
| Test the selected connection | `Ctrl+Alt+T` |
| Disconnect | `Ctrl+K` then `Ctrl+X` |
| Show connection problems | `Ctrl+K` then `Ctrl+L` |

"Show connection problems" lists what NineSQL found when loading `connections.json` (for example a key that looked like a secret and was removed), saved connections that their driver refuses, and whether sleep and screen lock are watched.

## Next
- [Run SQL and read results](run-sql.md).
- [Secrets: keyring, vault and locking](secrets.md).
- Per-engine details (TLS, read-only mechanism, properties): [docs/08](../08-engine-capabilities.md).
