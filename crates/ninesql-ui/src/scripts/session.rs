//! Session restore (F-WS-06): `$XDG_STATE_HOME/ninesql/session.json`, written when NineSQL
//! quits and read at the next start.
//!
//! - **References only (I1, ADR-0025 §4).** For each window: the saved scripts open in it
//!   (paths relative to the project's `scripts/`), the connection each tab was using, cursor
//!   and scroll positions, the split layout and whether the navigator was shown. Never editor
//!   text: an unsaved tab (a scratch buffer, or the unsaved edits of a script) could hold a
//!   password typed in a statement, so unsaved tabs are not restored and scripts reopen as
//!   saved on disk. Object editors are not restored either.
//! - **Checked on restore.** Each script is read through the [`ScriptStore`] rules (inside
//!   `scripts/`, no symbolic link, not in a DBeaver workspace, size cap, UTF-8) and compared
//!   with the size and modification time recorded at quit: a missing or changed file is
//!   skipped with a notice. Restored tabs are bounded ([`MAX_RESTORED_WINDOWS`],
//!   [`MAX_RESTORED_TABS`], [`MAX_RESTORED_BYTES`]).
//! - **No connection is opened (I3).** A restored tab shows its connection as "not connected";
//!   connecting goes through the normal flow (secrets, confirmations).
//! - **Versioned.** `schema_version` 1 through `ninesql-storage`'s documents: a file written
//!   by a newer NineSQL is refused and left untouched (and not overwritten at quit), as is an
//!   embedded editor session of a newer schema.
//! - **Blocking** functions here run on a background thread (I6).

use std::path::Path;

use ninesql_core::ConnectionId;
use ninesql_core::storage::StorageError;
use ninesql_core::storage::document::{self, Document, Format};
use ninesql_core::storage::fs::ensure_private_dir;
use ninesql_editor::script::{EditorSession, SessionTab};
use serde::{Deserialize, Serialize};

use super::store::{LoadedScript, ScriptFileError, ScriptStore};
use crate::layout::LayoutSnapshot;

/// Most windows restored.
pub const MAX_RESTORED_WINDOWS: usize = 8;
/// Most tabs restored per window.
pub const MAX_RESTORED_TABS: usize = 64;
/// Most script bytes read for one restore (every window together).
pub const MAX_RESTORED_BYTES: u64 = 64 * 1024 * 1024;

/// The session file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionFile {
    /// The project the scripts belong to (its name); another project's session is not
    /// restored.
    pub project: String,
    /// Windows, in the order they were opened.
    #[serde(default)]
    pub windows: Vec<WindowSession>,
}

impl Document for SessionFile {
    const NAME: &'static str = "session";
    const FORMAT: Format = Format::Json;
    const SCHEMA_VERSION: u32 = 1;
}

fn shown() -> bool {
    true
}

/// One window of the session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowSession {
    /// The saved scripts open in the window, with their connection and positions.
    pub editor: EditorSession,
    /// The split layout; its tab indices point into `editor.tabs`.
    pub layout: LayoutSnapshot,
    /// Whether the navigator dock was shown.
    #[serde(default = "shown")]
    pub navigator_visible: bool,
}

/// One tab to reopen, with its script read.
#[derive(Debug)]
pub struct RestoredTab {
    /// Index in the window's `editor.tabs` (what the layout points at).
    pub index: usize,
    /// The script.
    pub script: LoadedScript,
    /// The connection the tab was using (shown "not connected").
    pub connection: Option<ConnectionId>,
    /// 0-based caret line and column.
    pub cursor: (usize, usize),
    /// 0-based first visible line.
    pub scroll_line: usize,
}

/// One window to restore.
#[derive(Debug)]
pub struct WindowPlan {
    /// Tabs whose script could be read.
    pub tabs: Vec<RestoredTab>,
    /// The layout.
    pub layout: LayoutSnapshot,
    /// The navigator dock.
    pub navigator_visible: bool,
}

/// What the start-up restore does.
#[derive(Debug, Default)]
pub struct RestorePlan {
    /// Windows with at least one tab to reopen.
    pub windows: Vec<WindowPlan>,
    /// Scripts skipped and other problems, for one notice.
    pub notices: Vec<String>,
    /// The session file must not be overwritten at quit (written by a newer NineSQL).
    pub keep_file: bool,
}

/// Reads the session file and every script it names. **Blocking.**
#[must_use]
pub fn read_plan(path: &Path, store: &ScriptStore, project: &str) -> RestorePlan {
    let mut plan = RestorePlan::default();
    let file = match document::load::<SessionFile>(path) {
        Ok(Some(loaded)) => loaded.document,
        Ok(None) => return plan,
        Err(StorageError::NewerVersion { .. }) => {
            plan.keep_file = true;
            plan.notices.push(
                "The previous session was saved by a newer NineSQL: it was not restored, and it is kept as it is."
                    .to_owned(),
            );
            return plan;
        }
        Err(e) => {
            plan.notices
                .push(format!("The previous session could not be read: {e}"));
            return plan;
        }
    };
    if file.windows.iter().any(|w| w.editor.is_newer()) {
        plan.keep_file = true;
        plan.notices.push(
            "The previous session was saved by a newer NineSQL: it was not restored, and it is kept as it is."
                .to_owned(),
        );
        return plan;
    }
    if file.project != project {
        return plan;
    }
    let mut budget = MAX_RESTORED_BYTES;
    for window in file.windows.into_iter().take(MAX_RESTORED_WINDOWS) {
        let mut tabs = Vec::new();
        for (index, tab) in window.editor.tabs.iter().enumerate() {
            if tabs.len() >= MAX_RESTORED_TABS {
                plan.notices.push(format!(
                    "Only the first {MAX_RESTORED_TABS} scripts of a window were reopened."
                ));
                break;
            }
            match read_tab(store, tab, &mut budget) {
                Ok(script) => tabs.push(RestoredTab {
                    index,
                    script,
                    connection: tab.connection,
                    cursor: (tab.cursor_line, tab.cursor_column),
                    scroll_line: tab.scroll_line,
                }),
                Err(why) => plan.notices.push(why),
            }
        }
        if !tabs.is_empty() {
            plan.windows.push(WindowPlan {
                tabs,
                layout: window.layout,
                navigator_visible: window.navigator_visible,
            });
        }
    }
    plan
}

fn read_tab(
    store: &ScriptStore,
    tab: &SessionTab,
    budget: &mut u64,
) -> Result<LoadedScript, String> {
    let name = crate::browse::display_name(&tab.script.as_path().to_string_lossy());
    let skipped = |why: &str| format!("\"{name}\" was not reopened: {why}");
    let now = match store.stamp(&tab.script) {
        Ok(Some(stamp)) => stamp,
        Ok(None) => return Err(skipped("the file is missing.")),
        Err(e) => return Err(skipped(&e.to_string())),
    };
    if tab.stamp.is_some_and(|recorded| recorded != now) {
        return Err(skipped(
            "the file changed since NineSQL quit. Open it with \"Open script…\".",
        ));
    }
    if now.len > *budget {
        return Err(skipped("too much script text to reopen at once."));
    }
    let script = match store.read(&tab.script) {
        Ok(s) => s,
        Err(ScriptFileError::NotFound(_)) => return Err(skipped("the file is missing.")),
        Err(e) => return Err(skipped(&e.to_string())),
    };
    if tab.stamp.is_some_and(|recorded| recorded != script.stamp) {
        return Err(skipped(
            "the file changed since NineSQL quit. Open it with \"Open script…\".",
        ));
    }
    *budget = budget.saturating_sub(script.stamp.len);
    Ok(script)
}

/// Writes the session file: each tab's script is stamped (size, modification time) so a file
/// changed before the next start is skipped. **Blocking.**
///
/// # Errors
/// A user-presentable reason; the previous file is left as it was.
pub fn write(
    path: &Path,
    store: Option<&ScriptStore>,
    mut file: SessionFile,
) -> Result<(), String> {
    for window in &mut file.windows {
        for tab in &mut window.editor.tabs {
            tab.stamp = store.and_then(|s| s.stamp(&tab.script).ok().flatten());
        }
    }
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir).map_err(|e| e.to_string())?;
    }
    // A file written by a newer NineSQL is never overwritten (ADR-0008 §2).
    if let Err(StorageError::NewerVersion { .. }) = document::load::<SessionFile>(path) {
        return Err("the session file was written by a newer NineSQL: kept as it is".to_owned());
    }
    document::save(path, &file).map_err(|e| e.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use std::fs;

    use ninesql_core::DbeaverRoots;
    use ninesql_core::storage::{Paths, ProjectName, XdgDirs};
    use ninesql_editor::script::ScriptPath;

    use super::*;

    fn paths(base: &Path) -> Paths {
        let base = base.to_path_buf();
        let xdg = XdgDirs::from_lookup(|k| {
            let dir = match k {
                "XDG_CONFIG_HOME" => "config",
                "XDG_DATA_HOME" => "data",
                "XDG_STATE_HOME" => "state",
                "HOME" => "home",
                _ => return None,
            };
            Some(base.join(dir).into_os_string())
        })
        .unwrap();
        Paths::new(&xdg)
    }

    fn tab(script: &str, connection: Option<ConnectionId>) -> SessionTab {
        SessionTab {
            script: ScriptPath::new(script).unwrap(),
            connection,
            cursor_line: 2,
            cursor_column: 4,
            scroll_line: 1,
            stamp: None,
        }
    }

    fn session(tabs: Vec<SessionTab>) -> SessionFile {
        let n = tabs.len();
        SessionFile {
            project: "General".to_owned(),
            windows: vec![WindowSession {
                editor: EditorSession::new(tabs, Some(0)),
                layout: LayoutSnapshot::Pane {
                    tabs: (0..n).collect(),
                    active: 0,
                    focused: true,
                },
                navigator_visible: false,
            }],
        }
    }

    #[test]
    fn write_then_read_skips_missing_and_changed_scripts() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let store = ScriptStore::new(
            &p.project(&ProjectName::default_project()),
            DbeaverRoots::none(),
        );
        let c = ConnectionId::new_random();
        for name in ["a.sql", "dir/b.sql", "c.sql"] {
            store
                .write(&ScriptPath::new(name).unwrap(), "SELECT 1;", None, true)
                .unwrap();
        }
        let file = session(vec![
            tab("a.sql", Some(c)),
            tab("dir/b.sql", None),
            tab("c.sql", None),
            tab("gone.sql", None),
        ]);
        write(&p.session_file(), Some(&store), file).unwrap();
        let mode = fs::metadata(p.session_file()).unwrap();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777,
            0o600
        );
        // c.sql changes after quit (another size).
        store
            .write(
                &ScriptPath::new("c.sql").unwrap(),
                "SELECT 22;",
                None,
                false,
            )
            .unwrap();
        let plan = read_plan(&p.session_file(), &store, "General");
        assert_eq!(plan.windows.len(), 1);
        let w = &plan.windows[0];
        let names: Vec<_> = w.tabs.iter().map(|t| t.script.path.file_name()).collect();
        assert_eq!(names, ["a.sql", "b.sql"]);
        assert_eq!(w.tabs[0].connection, Some(c));
        assert_eq!(w.tabs[0].cursor, (2, 4));
        assert_eq!(w.tabs[0].scroll_line, 1);
        assert!(!w.navigator_visible);
        assert_eq!(plan.notices.len(), 2, "{:?}", plan.notices);
        assert!(
            plan.notices
                .iter()
                .any(|n| n.contains("c.sql") && n.contains("changed"))
        );
        assert!(
            plan.notices
                .iter()
                .any(|n| n.contains("gone.sql") && n.contains("missing"))
        );
        // Another project's session is not restored.
        assert!(
            read_plan(&p.session_file(), &store, "Other")
                .windows
                .is_empty()
        );
    }

    #[test]
    fn newer_session_files_are_refused_and_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let store = ScriptStore::new(
            &p.project(&ProjectName::default_project()),
            DbeaverRoots::none(),
        );
        fs::create_dir_all(p.state_dir()).unwrap();
        let newer = r#"{"schema_version": 2, "project": "General", "windows": []}"#;
        fs::write(p.session_file(), newer).unwrap();
        let plan = read_plan(&p.session_file(), &store, "General");
        assert!(plan.keep_file);
        assert!(plan.windows.is_empty());
        assert!(write(&p.session_file(), Some(&store), session(Vec::new())).is_err());
        assert_eq!(fs::read_to_string(p.session_file()).unwrap(), newer);
        // A newer embedded editor session is refused too.
        let nested = r#"{"schema_version": 1, "project": "General", "windows": [
            {"editor": {"schema_version": 7, "tabs": []}, "layout": {"kind": "pane", "tabs": []}}]}"#;
        fs::write(p.session_file(), nested).unwrap();
        let plan = read_plan(&p.session_file(), &store, "General");
        assert!(plan.keep_file);
        // An invalid file is reported, not restored.
        fs::write(p.session_file(), "{not json").unwrap();
        let plan = read_plan(&p.session_file(), &store, "General");
        assert!(!plan.keep_file);
        assert_eq!(plan.notices.len(), 1);
    }
}
