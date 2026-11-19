# Import from DBeaver

NineSQL can bring over the connections of a DBeaver workspace: their folders, connection types, SSH tunnels, TLS settings, driver properties and saved passwords. You see everything the import will do before anything is saved, and DBeaver's files are only read: NineSQL never changes, moves or deletes them ([invariant I4](../../CONTEXT.md#invariants-non-negotiable)).

Step by step, with every screen: [Import from DBeaver](../tutorials/import-from-dbeaver.md).

## Start
- **First start**: when the project is empty and a DBeaver workspace is found, NineSQL offers "Import your DBeaver connections?".
- **Any time**: "Import from DBeaver" (`Ctrl+K` then `Ctrl+I`) looks in the three usual places: the `.deb` / tarball install, the Flatpak and the Snap.
- **Another folder**: "Import from DBeaver: choose folder" (`Ctrl+K` then `Ctrl+Shift+I`) takes a workspace folder (such as `workspace6`) or a single project folder.
- **In the Flatpak**: only the native location (`~/.local/share/DBeaverData`, the `.deb` / tarball install) is visible to the sandbox, read-only, so only it is found automatically. A DBeaver Flatpak (`~/.var/app/io.dbeaver.DBeaverCommunity/…`) or Snap (`~/snap/dbeaver-ce/…`) workspace, or any other folder, needs "Import from DBeaver: choose folder", which goes through the folder picker.

## The preview
The preview lists, before anything is written:
- each connection as "new", "update" (imported before from the same DBeaver connection, project and workspace), "imported disabled" (with the reason) or "not imported" (with the reason);
- the saved passwords to move, as counts and field names only; untick "Import saved passwords" to leave them behind and be asked at each connect instead;
- connection types created, or existing types made **stricter** (an import never weakens a type);
- init SQL, which NineSQL will ask you to confirm on the first connect, whatever the connection type;
- for an update, what changes, and TLS: a weaker or different TLS setting from DBeaver is not applied unless you tick "Accept the imported TLS settings" for that connection.

"Test" on a row opens the connection as it would be saved, asking for credentials, then closes it. "Cancel" stops a long read; nothing is written.

## Import
"Import N connection(s)" reads the workspace again and writes only if it still gives the same preview. Saved passwords move to the vault (or the keyring, where enabled); the first time, NineSQL asks you to create the vault ([First start and secrets](first-start-and-secrets.md#the-vault)). A report is saved in your project (`imports/dbeaver-<date>.json`) with names, statuses and reasons, never a password.

DBeaver still keeps the passwords it saved, in a file that is only obfuscated. Once you have switched, consider deleting them in DBeaver; NineSQL never does it for you.

## Connections imported disabled
A connection is imported disabled, never silently changed, when NineSQL cannot reproduce it safely:
- **no driver**: an engine NineSQL does not support yet (Oracle, SQL Server…);
- **route not reproduced**: DBeaver reached the server through a network profile, a proxy or another SSH forwarding target; connecting without it would reach the server another way;
- **settings refused by the driver**.

"Enable connection" (`Ctrl+K` then `Ctrl+Shift+E`) checks that the cause is gone. For a route, set up the SSH tunnel first ("Edit connection", `F4`), then confirm "I set up the route again in NineSQL (SSH tunnel or network) and checked it".

## Import again
Run the import whenever DBeaver changes. Connections imported before are updated in place, new ones are created. NineSQL keeps your own changes where it matters: the name, folder and description you gave, a stricter read-only, manual-commit or TLS setting, a stricter type, and a connection you disabled yourself. A saved password is kept only while what it authenticates against (engine, host, port, user, TLS, SSH route and key file) is unchanged.

## What is not imported
- DBeaver SQL scripts (`Scripts/`) are not copied; empty DBeaver folders are listed, not created.
- DBeaver settings with no NineSQL equivalent are listed in the report. Driver properties the NineSQL driver does not accept are dropped with a report entry (all of them for QuestDB).
- A connection's default schema and catalog are stored but not applied when connecting yet.
- RSA SSH key files are flagged: NineSQL accepts Ed25519 and ECDSA keys, or the SSH agent.
- One source is read within limits (256 projects, 10 000 connections, 64 MiB); past a limit the preview says "Incomplete preview" and you import the rest by picking a smaller folder.

The rules in detail: [docs/06](../06-dbeaver-interop.md).

## The other direction
"Export connections to DBeaver…" writes chosen connections to a **new** DBeaver file or project folder, without any password: see [Results, editing and transfer](results-editing-transfer.md#export-connections-to-dbeaver).
