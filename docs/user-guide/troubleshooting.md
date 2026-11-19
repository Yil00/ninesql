# Troubleshooting

Two actions list what NineSQL noticed in the background:
- "Show settings problems" (`Ctrl+K` then `Ctrl+E`): invalid values and key bindings in `settings.toml` (the defaults are used instead).
- "Show connection problems" (`Ctrl+K` then `Ctrl+L`): what was fixed or refused when loading `connections.json`, saved connections their driver refuses, whether sleep and screen lock are watched, and a workspace or runtime that could not start.

NineSQL writes no log file of its own.

## Starting NineSQL
| Symptom | What to do |
|---|---|
| "another NineSQL process is using this workspace (… is locked); close it before starting a new one" | Only one NineSQL process can use the workspace. Use the running one ("New window", `Ctrl+Shift+N`), or quit it first |
| "file locking is not available on the file system holding …" | The data directory is on a network or FUSE file system without locks. Move `XDG_DATA_HOME` to a local file system |
| The window does not open, or renders very slowly | NineSQL needs a GPU with Vulkan 1.2 drivers and a Wayland or X11 session; software Vulkan (`lavapipe`) is slow ([Install](install.md#system-requirements)) |
| A key binding or a theme setting is ignored | Run "Show settings problems": an invalid entry keeps its default |

## Secrets
| Symptom | What to do |
|---|---|
| NineSQL asks for the master password at every start | Expected: the vault is locked every time NineSQL starts, and after sleep, screen lock, 15 minutes without use or "Lock secrets now" |
| "Wrong master password." then "Too many wrong attempts: try again in N s." | After three wrong attempts NineSQL waits between attempts (up to 60 s) |
| You forgot the master password | It cannot be recovered, and the passwords saved in the vault are lost; the connections themselves stay. To start over, quit NineSQL, move `~/.local/share/ninesql/vault.bin` away, then edit each connection ("Edit connection", `F4`) and save its password again: the first save creates a new vault |
| "The master password must be at least 12 characters long." | A new master password needs at least 12 characters, not only spaces |
| "Save in keyring" is not offered | The keyring backend is off in default builds; passwords go to the vault ([First start and secrets](first-start-and-secrets.md#where-passwords-go)) |
| "Sleep and screen lock are not watched…" | No systemd-logind on the system bus (macOS, a Flatpak without the logind permission, some containers). Use "Lock secrets now" (`Ctrl+Alt+L`) before leaving the computer |

## Connecting
| Symptom | What to do |
|---|---|
| A TLS error with "Verify CA and host name" | The server's certificate is not signed by a CA your system trusts, or does not name the host you typed. Give the server's CA in "CA file" and connect with the host name in the certificate. Lowering the mode to "Encrypt only" removes the protection against a man in the middle |
| "Verify CA" refuses to save | That mode needs a CA file: it is the only trusted authority |
| "this server reports version …, which is …, not …; choose the matching engine" | The engine in the dialog does not match the server (for example MariaDB chosen for a MySQL server). Pick the right engine. For TimescaleDB, the `timescaledb` extension must exist in the connection's database |
| "Unknown SSH host key" | Compare the fingerprint with the one the server's administrator gives you before pressing "Trust this key and connect" |
| A changed SSH host key is refused | Expected: the key differs from the one you trusted, which can mean a man in the middle. If the server really changed its key, remove the old entry from `~/.local/share/ninesql/known_hosts` (and from `~/.ssh/known_hosts` if it is there) after checking the new fingerprint with the administrator |
| "RSA key files are not supported (ADR-0016) …" | Use an Ed25519 or ECDSA key, or load the RSA key into `ssh-agent` and choose "SSH agent" |
| QuestDB: "the server asks for a cleartext password over a connection that is not protected …" | QuestDB only takes a password in clear and offers no TLS: connect through an SSH tunnel whose hops all use "Strict (confirm unknown keys)" |
| QuestDB or TiDB: a read-only connection is refused | These engines have no session read-only mode. Use QuestDB's read-only user, or a read-only TiDB instance with an account that mode covers (not `root`); or use an account with `SELECT` privileges only, on a connection that is not read-only ([Connections](connections.md#engine-conditions)) |
| MySQL 8: the first connection after a server restart fails over plain TCP | `caching_sha2_password` needs the full password exchange then, which NineSQL sends only over verified TLS or a strict SSH tunnel. Connect once with TLS "Verify CA and host name" (or through a tunnel) |
| A DBeaver connection was imported disabled | "Enable connection" (`Ctrl+K` then `Ctrl+Shift+E`) says why and what to fix ([Import from DBeaver](import-from-dbeaver.md#connections-imported-disabled)) |
| A SQLite file opens read-only with "Read-only: DBeaver file" | The file is inside a DBeaver directory, which NineSQL never modifies. Copy it elsewhere to edit it |
| A session ended on its own | The server or the network closed it. NineSQL does not reconnect by itself yet: run "Connect" again |

## Running SQL
| Symptom | What to do |
|---|---|
| "The session is busy with another statement or a paused result: …" | A result is still waiting to be fetched on that session. Fetch it to its end, cancel it ("Cancel execution", `Ctrl+.`), or open another editor on a second session |
| "Result closed after … idle: …" | NineSQL closes a result left waiting (240 s by default, `paused_result_idle_secs`) before the server drops the session. The loaded rows stay; run the statement again to read more |
| "Truncated: memory budget reached. …" | The run reached its memory budget (256 MiB by default, `result_memory_mib`). Narrow the query, or raise the budget in `settings.toml` |
| "Line N: …. Nothing was run." | The script holds a piece the engine's client would not send (a psql meta-command, `COPY … FROM STDIN`, an invalid `DELIMITER`). Remove it |
| "Bind parameters need a single statement: …" | Run statements with placeholders one at a time |
| A harmless statement asks for confirmation on production | The classifier errs on the side of asking ([Safety model](safety-model.md#the-limits-of-the-statement-checks)) |
| "Waiting for the previous statement to stop…" | The server takes a moment to confirm the cancel; the next run starts when it does |

## Editing, export and import
| Symptom | What to do |
|---|---|
| "Read-only: open the table from the navigator (Open table data) to edit its rows." | Only results of "Open table data" are editable |
| "Read-only: the table has no primary key or unique key that identifies rows …" | Editing needs a key; add one, or edit with SQL |
| "Not saved yet: the table's result is still open …" | Press "Stop fetching" (or "Cancel") in the toolbar, then save again: the loaded rows and your changes are kept |
| "Not saved: change N of the save: …; nothing was saved" | A row changed or disappeared since you read it. "Refresh table data" (`Alt+T` then `R`) and redo the change |
| An export is refused for its folder | DBeaver workspaces, NineSQL's own folders and `~/.ssh` are refused; choose another folder |
| A TiDB export is refused on production | TiDB has no read-only transaction for the export: export from a read-only TiDB connection |
| "All or nothing" is disabled in the CSV import | The table's storage cannot roll back (MyISAM, MEMORY, Aria…) or could not be read. Rows committed before an error stay with "Commit every 1000 rows" |
| A script does not open | Scripts over 8 MiB, not in UTF-8, outside `scripts/` or reached through a symbolic link are refused |
| A script was not reopened at start | It was deleted, moved or changed since NineSQL quit: open it with "Open script…" (`Ctrl+O`) |

## Reporting a problem
Open an issue on the project's GitHub page with the NineSQL commit you built, your distribution, the engine and its version, and the exact message. Never paste a password, a connection URL with credentials, or real data. Security problems go through [SECURITY.md](../../SECURITY.md), not a public issue.
