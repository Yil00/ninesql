# Import from DBeaver

This tutorial imports the connections of a DBeaver workspace into NineSQL (P2-B, P2-C; feature F-CON-14). You see everything the import will do before anything is saved, the passwords DBeaver saved move to the vault or the keyring, and DBeaver's files are only read: NineSQL never changes, moves or deletes them ([invariant I4](../../CONTEXT.md#invariants-non-negotiable)).

The screens were written without a display (cloud sessions); their visual pass is part of the MVP-end tests (P6-F). The labels below are the exact texts of the code.

## Before you start
- NineSQL builds and runs ([Build and run](build-and-run.md)).
- A DBeaver workspace on this computer. NineSQL looks in the three usual places: the `.deb` / tarball install (`~/.local/share/DBeaverData/workspace6`), Flatpak (`~/.var/app/io.dbeaver.DBeaverCommunity/data/DBeaverData/workspace6`) and Snap (`~/snap/dbeaver-ce/current/.local/share/DBeaverData/workspace6`). Anywhere else, you choose the folder.
- If DBeaver saved passwords, read [Secrets](secrets.md) first: by default they move to NineSQL's encrypted vault, which you create with a master password the first time.

## 1. Start the import
Three ways:
- **First run.** When the project has no connection and a DBeaver workspace is found in one of the usual places, a box above the editor asks "Import your DBeaver connections?" with the workspace it found. Press "Import from DBeaver". "Choose folder…" picks another folder; "Not now" hides the box in this window; "Don't show again" hides it for good (it writes `first_run_prompt = false` under `[import]` in `settings.toml`, see [Build and run](build-and-run.md#settings-and-key-bindings)).
- **Any time.** "Import from DBeaver" (`Ctrl+K` then `Ctrl+I`) previews the workspaces of the usual places. When none is found, the screen says "No DBeaver workspace was found in the known locations (native, Flatpak, Snap). Choose its folder, or type its path."
- **Another folder.** "Import from DBeaver: choose folder" (`Ctrl+K` then `Ctrl+Shift+I`) opens the desktop's folder picker. You can also type the path in the field of the import screen and press "Preview". Pick a DBeaver workspace (for example a `workspace6` folder) or a single project folder.

All three are in the command palette (`Ctrl+Shift+P`, type "dbeaver").

Finding the workspace only lists folders. Reading it (and decrypting DBeaver's saved passwords in memory) happens when the preview runs, off the interface thread. While it runs, the screen says "Reading the DBeaver workspace…". "Cancel" stops the read (between two files, or every 64 connections) and the screen says "Import preview cancelled: nothing was imported." Nothing is written either way. A read stuck on a network or FUSE file that does not answer cannot be interrupted until that file answers.

The read is bounded for one source (every workspace found, or the folder you picked): at most 256 projects, 10 000 connections, 64 MiB read and 50 000 report entries, and looking for workspaces lists at most 2 048 folders and 200 000 entries. A project reached twice (for example through a symbolic link) is read once. Files under `/proc`, `/sys` and `/dev` are never read. When a folder holds so many entries that no project could be found before the limit, the screen says "Cannot preview: the folder holds too many entries to look for DBeaver projects in it; pick the DBeaver workspace folder or a project folder directly."

## 2. Read the preview
The "Import from DBeaver" screen lists, from top to bottom:
- **"Incomplete preview"** (only when a limit was reached): "Incomplete preview: a limit was reached while reading the DBeaver workspace, so some projects or connections are not listed and will not be imported. Pick a smaller folder (one project) to import the rest."
- **"Source issues"** (only when there are some): the limits reached and the folders read once because they were reached twice, one line each.
- **"Source"**: the workspaces read, with how DBeaver was installed and its version.
- **"Saved passwords"**: how many secrets DBeaver saved and where they go ("… saved secret(s) move to the encrypted vault. Values are never shown."). Untick "Import saved passwords" to leave them behind: NineSQL then asks for them at each connect. The screen never shows a password, not even masked: only counts and fields such as "Password (database)" or "SSH key passphrase (SSH server)".
- **"Connection types"**: types the import creates, and existing types it makes stricter. An import never weakens a type. When a built-in type such as Test or Production gains rules (for example "asks before destructive statements"), the line is highlighted and says how many of your saved connections it changes: "The built-in type "Test" becomes stricter: asks before destructive statements. This changes 2 saved connection(s) of this type".
- **One block per DBeaver project**: what happened to its saved-passwords file, empty folders (listed, not created), then one row per connection.

Each connection row shows its name, a badge and its engine, type and folder:

| Badge | Meaning |
|---|---|
| "new" | Created as a new NineSQL connection |
| "update" | Updates the connection imported before from the same DBeaver connection, project and workspace (re-import does not duplicate) |
| "imported disabled" | Saved but disabled; the row says why, for example "Disabled: no driver: …" (Oracle and other engines without a NineSQL driver yet) or "route not reproduced" (a DBeaver network profile, a proxy or another SSH forwarding target) |
| "not imported" | Not saved; the row says why |

Under it, as they apply:
- "Saved secrets to move: Password (database)…"
- "1 init SQL statement(s): will ask for confirmation on first connect." Imported init SQL always asks once, whatever the connection type ([ADR-0022](../adr/0022-pre-phase-2-hardening-decisions.md)).
- For a new connection over the network, its TLS mode ("TLS Verify CA and host name", with the CA file if any), in the warning colour when it is weaker than "Verify CA and host name".
- For an update, what changes ("Endpoint: old → new", "User name", "SSH route", "Connection type", "Driver properties changed: …", "Init SQL"), the saved secrets dropped because what they authenticate against changed ("Saved secrets dropped (asked at connect): database. …"), and TLS.
- The report entries of the connection (notes and warnings from the import).

**TLS on re-import.** When DBeaver's TLS setting is weaker than, or differs from, the one saved in NineSQL, the row says "TLS: the saved setting (…) is kept; DBeaver has …". The saved setting stays unless you tick "Accept the imported TLS settings". A warning shows under the box, before and after you tick it: a weaker mode lets a man in the middle read or change the traffic, and a different CA file lets whoever holds that CA impersonate the server ([invariant I5](../../CONTEXT.md#invariants-non-negotiable)). A new preview (for example after the source changed) unticks it again.

**Leave a connection out** with its checkbox. "Update connections imported before" (shown when some rows are updates) leaves every update out at once.

**Test before importing.** "Test" (on rows imported enabled) opens the connection as it would be saved, then closes it. For an update whose TLS setting is kept, Test uses the kept, saved TLS setting, even when "Accept the imported TLS settings" is ticked: it never tries the weaker one before you import it. It always asks for the credentials: a test never uses a password from DBeaver or one saved in NineSQL for settings you have not accepted yet. Init SQL asks for confirmation as it will after the import. The result shows on the row ("Test succeeded: …", "Test failed: …").

## 3. Import
Press "Import N connection(s)". NineSQL reads the DBeaver workspace again and writes only if it still gives the same preview. What can happen:
- **The vault is not created yet**: the "Create the vault" prompt opens (master password of at least 12 characters, typed twice). The import then runs with the same preview. Cancel it and nothing is written: "Not imported: the vault is needed for the saved passwords. Unlock it, or turn off "Import saved passwords"."
- **The vault is locked**: the "Unlock the vault" prompt opens, then the import runs again.
- **The DBeaver workspace or your saved connections changed since the preview**: nothing is written, the preview is computed again and the screen says "The DBeaver workspace or the saved connections changed since the preview: it was computed again. Review it, then import again."

Then the screen shows "Imported": how many connections were created, updated, disabled, left out and not imported, how many saved passwords moved, how many connection types changed, and "Report saved to …": the report in your project, `imports/dbeaver-<date>.json`. It holds names, statuses, reasons and counts, never a password. If the report cannot be written (for example a full disk or a permission problem in the project folder), the import is still applied and the screen says "Import report not saved: <reason>. The connections were imported." instead, in the warning colour.

If DBeaver had saved passwords, the screen also advises: "DBeaver still keeps the passwords it saved, in a file that is only obfuscated. Once you have switched to NineSQL, consider deleting them in DBeaver …". NineSQL never does it for you ([ADR-0006](../adr/0006-dbeaver-interoperability.md) §4); the report says so too.

The navigator shows the imported connections in their DBeaver folders.

## 4. Connections imported disabled
In the navigator, a disabled connection shows "disabled: no driver", "disabled: route not reproduced" or "disabled: settings refused by the driver"; hover it for the full reason. "Connect" on it says why and does not start.

To enable it, select it and use "Enable connection" (`Ctrl+K` then `Ctrl+Shift+E`). The dialog shows the reason, and NineSQL checks that its cause is gone:
- **No driver / settings refused**: "Enable" asks the core; if the driver still refuses (or there is none in this build), the dialog says "Not enabled: …" and the connection stays disabled. Edit it (`F4`) first if its settings need a change.
- **Route not reproduced**: DBeaver reached the server through something the import could not reproduce. Connecting without it would reach the server another way. Edit the connection (`F4`) to set up the SSH tunnel, then tick "I set up the route again in NineSQL (SSH tunnel or network) and checked it" and press "Enable". Without the tick, "Enable" stays disabled.

## 5. Import again later
Run the import again whenever DBeaver changes: connections imported before are updated in place (same NineSQL connection), new ones are created. NineSQL keeps your own changes where it matters: the name, folder and description you gave, a stricter read-only or manual-commit setting, a stricter TLS mode (unless you accept the change), a stricter type, and a connection you disabled yourself.

## What is not imported (yet)
- DBeaver SQL scripts (`Scripts/`) are not copied.
- Settings with no NineSQL equivalent are listed in the report entries of each connection ([docs/06](../06-dbeaver-interop.md)).

## Next
- [Create your first connection](first-connection.md) explains each field of the connection dialog, useful to fix an imported connection.
- [Secrets](secrets.md): where the moved passwords live, and how to lock them.
- [Export and import data](export-import.md#7-export-connections-to-dbeaver): the other direction, NineSQL connections written to a new DBeaver file without passwords. Since phase 5, when a DBeaver connection sets its TLS mode both in its SSL handler and in an `sslMode` driver property, the import keeps the stricter of the two.
