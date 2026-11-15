//! Reading a whole import source: every project of the workspaces found by one detection
//! ([`crate::detect_workspaces`]) or one picked folder ([`crate::open_folder`]), under
//! whole-source limits, read once per folder (device and inode), and cancellable (P2-D review, M1).
//!
//! The per-file and per-project limits of [`crate::import_project`] still apply to each project;
//! the limits here bound the sum, so a crafted folder (one large project plus hundreds of
//! symbolic links to it, or hundreds of distinct projects) cannot make one import read or map an
//! unbounded amount of data (I6).
//!
//! # Rules
//! - **Read once**: a workspace that is the same directory (device and inode) as one already
//!   seen in the same call (the native, Flatpak and Snap locations can be links to one folder)
//!   is skipped; a project whose `.dbeaver` directory is the same directory as one already seen
//!   is skipped (a symbolic link to a project folder, a `..` path, a linked `.dbeaver`).
//!   Detection already skips such folders inside one workspace
//!   ([`crate::Workspace::duplicate_projects`]). Each skip is an [`IssueKind::Duplicate`] entry
//!   of [`SourceImport::issues`]; a duplicate of a project that was not read because of a limit
//!   says so.
//! - **Cut listings**: a workspace whose listing was cut ([`crate::Workspace::listing_truncated`])
//!   gets an [`IssueKind::Truncated`] entry.
//! - **Whole-source limits** ([`SourceLimits`]): projects read, connection entries, bytes read
//!   (data-sources and credentials files together) and report entries. Past a limit, nothing
//!   more is read: the project being read is cut short with an [`IssueKind::Truncated`] entry in
//!   its own issues, the projects not read are listed in an [`IssueKind::Truncated`] entry of
//!   [`SourceImport::issues`], and [`SourceImport::truncated`] is set (it is set whenever any
//!   [`IssueKind::Truncated`] entry exists, at any level). Never partial without a report.
//! - **Report entries**: duplicate entries count against [`SourceLimits::max_issues`] (past it,
//!   one last entry counts them); the few limit entries are always kept.
//! - **Bytes** are charged whether a read succeeds or fails (bytes read before an error, a file
//!   that grew, the full limit for a pseudo-file reporting a size of 0); files under `/proc`,
//!   `/sys` and `/dev` are never read.
//! - **Cancellation**: the [`CancelFlag`] is checked before each project, before each file
//!   read, after parsing, and every [`CANCEL_CHECK_INTERVAL`] connections while mapping. Once it
//!   is set, the call returns [`DbeaverError::Cancelled`] and drops everything it read (secrets
//!   included, zeroized on drop). Parsing one file is not interrupted; it is bounded by the
//!   per-file size limits.
//! - **I4**: only read-only opens, `stat`, `readdir` and `readlink`, as everywhere in this crate.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::detect::{Project, Workspace};
use crate::error::DbeaverError;
use crate::fsread::{self, FileId};
use crate::mapping::{self, ProjectImport, ReadBudget};
use crate::report::{ImportIssue, IssueKind, MAX_IDENT, Severity, ident, shown_path};

/// Most distinct projects read in one import source.
pub const MAX_SOURCE_PROJECTS: usize = 256;
/// Most connection entries (imported, disabled or failed) in one import source.
pub const MAX_SOURCE_CONNECTIONS: usize = 10_000;
/// Most bytes of DBeaver files (data-sources and credentials files together) read in one import
/// source.
pub const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
/// Most report entries (source, project and connection entries together) in one import source.
pub const MAX_SOURCE_ISSUES: usize = 50_000;
/// Connections mapped between two checks of the [`CancelFlag`].
pub const CANCEL_CHECK_INTERVAL: usize = 64;

/// Most project names listed in the "not read" entry; the others are counted.
const MAX_NOT_READ_LISTED: usize = 20;

/// Whole-source limits. [`SourceLimits::default`] is the constants of this module; a caller
/// may lower a limit (tests, a smaller budget) but never raise one: values above the constants
/// are clamped to them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SourceLimits {
    /// Most distinct projects read ([`MAX_SOURCE_PROJECTS`]).
    pub max_projects: usize,
    /// Most connection entries ([`MAX_SOURCE_CONNECTIONS`]).
    pub max_connections: usize,
    /// Most bytes read ([`MAX_SOURCE_BYTES`]).
    pub max_bytes: u64,
    /// Most report entries ([`MAX_SOURCE_ISSUES`]).
    pub max_issues: usize,
}

impl Default for SourceLimits {
    fn default() -> Self {
        Self {
            max_projects: MAX_SOURCE_PROJECTS,
            max_connections: MAX_SOURCE_CONNECTIONS,
            max_bytes: MAX_SOURCE_BYTES,
            max_issues: MAX_SOURCE_ISSUES,
        }
    }
}

impl SourceLimits {
    /// The same limits with at most `n` projects.
    #[must_use]
    pub const fn with_max_projects(mut self, n: usize) -> Self {
        self.max_projects = n;
        self
    }

    /// The same limits with at most `n` connection entries.
    #[must_use]
    pub const fn with_max_connections(mut self, n: usize) -> Self {
        self.max_connections = n;
        self
    }

    /// The same limits with at most `n` bytes read.
    #[must_use]
    pub const fn with_max_bytes(mut self, n: u64) -> Self {
        self.max_bytes = n;
        self
    }

    /// The same limits with at most `n` report entries.
    #[must_use]
    pub const fn with_max_issues(mut self, n: usize) -> Self {
        self.max_issues = n;
        self
    }

    fn clamped(self) -> Self {
        Self {
            max_projects: self.max_projects.min(MAX_SOURCE_PROJECTS),
            max_connections: self.max_connections.min(MAX_SOURCE_CONNECTIONS),
            max_bytes: self.max_bytes.min(MAX_SOURCE_BYTES),
            max_issues: self.max_issues.min(MAX_SOURCE_ISSUES),
        }
    }
}

/// A cooperative cancel signal for a long read. Cloning shares the flag: keep one clone, hand
/// another to the reading thread, and call [`CancelFlag::cancel`] to stop it.
#[derive(Debug, Clone, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    /// A flag that is not set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Wraps an existing shared flag (for a caller that already has one).
    #[must_use]
    pub fn from_shared(flag: Arc<AtomicBool>) -> Self {
        Self(flag)
    }

    /// Asks every read holding a clone of this flag to stop at its next check.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether [`CancelFlag::cancel`] was called.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// One project read by [`import_source`].
#[derive(Debug)]
#[must_use = "the import result carries secrets to move and issues to show"]
pub struct SourceProject {
    /// The project as found by detection (the first occurrence of its folder).
    pub project: Project,
    /// What reading it produced, as [`crate::import_project`] returns it.
    pub import: ProjectImport,
}

/// Every project of an import source, read once each, under the whole-source limits.
#[derive(Debug)]
#[must_use = "the import result carries secrets to move and issues to show"]
pub struct SourceImport {
    /// The workspaces kept (duplicates removed), each with only the projects that were read.
    pub workspaces: Vec<Workspace>,
    /// The projects read, in detection order.
    pub projects: Vec<SourceProject>,
    /// Source-level report entries: duplicates skipped ([`IssueKind::Duplicate`]) and limits
    /// reached ([`IssueKind::Truncated`]). Show them with the import's own issues.
    pub issues: Vec<ImportIssue>,
    /// A whole-source limit was reached (or a folder listing was cut): something was not read
    /// or not imported, and [`SourceImport::issues`] or a project's issues say what.
    pub truncated: bool,
}

/// Reads every project of `workspaces` under the default [`SourceLimits`].
///
/// # Errors
/// Only [`DbeaverError::Cancelled`], when `cancel` is set during the read. Every other problem
/// is a report entry of the result.
pub fn import_source(
    workspaces: Vec<Workspace>,
    cancel: &CancelFlag,
) -> Result<SourceImport, DbeaverError> {
    import_source_with_limits(workspaces, SourceLimits::default(), cancel)
}

/// Reads every project of `workspaces` under `limits` (clamped to the constants of this module).
/// See the [module documentation](self) for the rules.
///
/// # Errors
/// Only [`DbeaverError::Cancelled`], when `cancel` is set during the read.
pub fn import_source_with_limits(
    workspaces: Vec<Workspace>,
    limits: SourceLimits,
    cancel: &CancelFlag,
) -> Result<SourceImport, DbeaverError> {
    let limits = limits.clamped();
    let mut budget = ReadBudget {
        bytes: limits.max_bytes,
        connections: limits.max_connections,
        issues: limits.max_issues,
        source: Some(limits),
        cancel: Some(cancel),
        source_limit_hit: false,
    };
    let mut report = SourceReport::default();
    let mut seen_workspaces: BTreeMap<Key, PathBuf> = BTreeMap::new();
    // First folder of each project key, and whether it was read.
    let mut seen_projects: BTreeMap<Key, (PathBuf, bool)> = BTreeMap::new();
    let mut kept_workspaces = Vec::new();
    let mut projects: Vec<SourceProject> = Vec::new();
    let mut not_read: Vec<String> = Vec::new();
    let mut stop_reason: Option<String> = None;

    for mut workspace in workspaces {
        budget.check_cancel()?;
        let key = key_of(&workspace.path);
        if let Some(first) = seen_workspaces.get(&key) {
            report.counted(
                &mut budget,
                ImportIssue::new(
                    Severity::Info,
                    IssueKind::Duplicate,
                    None,
                    format!(
                        "the workspace {} is the same folder as {}: read once",
                        shown_path(&workspace.path),
                        shown_path(first)
                    ),
                ),
            );
            continue;
        }
        seen_workspaces.insert(key, workspace.path.clone());
        if workspace.listing_truncated {
            report.always(ImportIssue::new(
                Severity::Warning,
                IssueKind::Truncated,
                None,
                format!(
                    "the listing of {} was cut (more than {MAX_SOURCE_PROJECTS} project folders,                      or a directory or detection limit was reached): the projects past the cut                      are not read",
                    shown_path(&workspace.path)
                ),
            ));
        }
        let found = std::mem::take(&mut workspace.projects);
        for project in found {
            budget.check_cancel()?;
            let key = project_key(&project);
            if let Some((first, read)) = seen_projects.get(&key) {
                let issue = duplicate_issue(&project.path, first, *read);
                report.counted(&mut budget, issue);
                continue;
            }
            if let Some(reason) = limit_reached(&limits, &budget, projects.len()) {
                stop_reason.get_or_insert(reason);
                not_read.push(ident(&project.name, MAX_IDENT));
                seen_projects.insert(key, (project.path.clone(), false));
                continue;
            }
            seen_projects.insert(key, (project.path.clone(), true));
            let import = mapping::import_project_limited(&project, &mut budget)?;
            workspace.projects.push(project.clone());
            projects.push(SourceProject { project, import });
        }
        // After the projects, so each duplicate says whether its project was read.
        for dup in std::mem::take(&mut workspace.duplicate_projects) {
            let read = seen_projects
                .get(&project_key_of_path(&dup.same_as))
                .is_some_and(|(_, read)| *read);
            report.counted(&mut budget, duplicate_issue(&dup.path, &dup.same_as, read));
        }
        if workspace.more_duplicates > 0 {
            report.counted(
                &mut budget,
                ImportIssue::new(
                    Severity::Warning,
                    IssueKind::Duplicate,
                    None,
                    format!(
                        "{} more project folders of {} resolve to folders already listed: read \
                         once",
                        workspace.more_duplicates,
                        shown_path(&workspace.path)
                    ),
                ),
            );
            workspace.more_duplicates = 0;
        }
        kept_workspaces.push(workspace);
    }

    if let Some(reason) = stop_reason {
        let mut names = not_read
            .iter()
            .take(MAX_NOT_READ_LISTED)
            .map(|n| format!("`{n}`"))
            .collect::<Vec<_>>()
            .join(", ");
        if not_read.len() > MAX_NOT_READ_LISTED {
            names.push_str(&format!(
                " and {} more",
                not_read.len() - MAX_NOT_READ_LISTED
            ));
        }
        report.always(ImportIssue::new(
            Severity::Warning,
            IssueKind::Truncated,
            None,
            format!("{reason}: {} project(s) not read: {names}", not_read.len()),
        ));
    }
    let issues = report.finish(&limits);
    budget.check_cancel()?;
    // Any cut at any level: source entries, project entries, connection entries.
    let truncated = budget.source_limit_hit
        || issues.iter().any(is_truncation)
        || projects.iter().any(|p| {
            p.import.issues.iter().any(is_truncation)
                || p.import
                    .connections
                    .iter()
                    .any(|c| c.issues.iter().any(is_truncation))
        });
    tracing::debug!(
        projects = projects.len(),
        source_issues = issues.len(),
        truncated,
        "DBeaver import source read"
    );
    Ok(SourceImport {
        workspaces: kept_workspaces,
        projects,
        issues,
        truncated,
    })
}

fn is_truncation(issue: &ImportIssue) -> bool {
    issue.kind == IssueKind::Truncated
}

/// Source-level report entries. Duplicate entries draw on the report-entry budget (past it they
/// are counted in one last entry); limit entries are always kept (a few per source).
#[derive(Default)]
struct SourceReport {
    issues: Vec<ImportIssue>,
    dropped: usize,
}

impl SourceReport {
    fn counted(&mut self, budget: &mut ReadBudget<'_>, issue: ImportIssue) {
        if budget.issues == 0 {
            self.dropped += 1;
        } else {
            budget.issues -= 1;
            self.issues.push(issue);
        }
    }

    fn always(&mut self, issue: ImportIssue) {
        self.issues.push(issue);
    }

    fn finish(mut self, limits: &SourceLimits) -> Vec<ImportIssue> {
        if self.dropped > 0 {
            self.issues.push(ImportIssue::new(
                Severity::Warning,
                IssueKind::Truncated,
                None,
                format!(
                    "the import limit of {} report entries for the whole source was reached: {} \
                     more entries are not shown",
                    limits.max_issues, self.dropped
                ),
            ));
        }
        self.issues
    }
}

fn duplicate_issue(path: &Path, first: &Path, first_read: bool) -> ImportIssue {
    let message = if first_read {
        format!(
            "the project at {} resolves to the same folder as the project at {}: read once",
            shown_path(path),
            shown_path(first)
        )
    } else {
        format!(
            "the project at {} resolves to the same folder as the project at {}, which was not \
             read because an import limit was reached",
            shown_path(path),
            shown_path(first)
        )
    };
    ImportIssue::new(Severity::Warning, IssueKind::Duplicate, None, message)
}

/// Reads one project like [`crate::import_project`] (per-project limits only), checking
/// `cancel` between files and every [`CANCEL_CHECK_INTERVAL`] connections.
///
/// # Errors
/// Only [`DbeaverError::Cancelled`].
pub fn import_project_with(
    project: &Project,
    cancel: &CancelFlag,
) -> Result<ProjectImport, DbeaverError> {
    let mut budget = ReadBudget::per_project(Some(cancel));
    mapping::import_project_limited(project, &mut budget)
}

/// Why no further project is read, if a whole-source limit is reached.
fn limit_reached(limits: &SourceLimits, budget: &ReadBudget<'_>, read: usize) -> Option<String> {
    if read >= limits.max_projects {
        Some(format!(
            "the import limit of {} projects for the whole source was reached",
            limits.max_projects
        ))
    } else if budget.connections == 0 {
        Some(format!(
            "the import limit of {} connections for the whole source was reached",
            limits.max_connections
        ))
    } else if budget.bytes == 0 {
        Some(format!(
            "the import limit of {} bytes read for the whole source was reached",
            limits.max_bytes
        ))
    } else if budget.issues == 0 {
        Some(format!(
            "the import limit of {} report entries for the whole source was reached",
            limits.max_issues
        ))
    } else {
        None
    }
}

/// Identity of a folder: device and inode, else its canonical path, else the path as given.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Id(FileId),
    Path(PathBuf),
}

fn key_of(path: &Path) -> Key {
    fsread::file_id(path).map_or_else(
        || Key::Path(fsread::canonicalize(path).unwrap_or_else(|| path.to_path_buf())),
        Key::Id,
    )
}

/// The folder a project is read from: its `.dbeaver` directory (device and inode), else its
/// own folder.
fn project_key(project: &Project) -> Key {
    project_key_of_path(&project.path)
}

fn project_key_of_path(path: &Path) -> Key {
    fsread::file_id(&path.join(".dbeaver")).map_or_else(|| key_of(path), Key::Id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_clamped_never_raised() {
        let c = SourceLimits::default()
            .with_max_projects(usize::MAX)
            .with_max_connections(3)
            .with_max_bytes(u64::MAX)
            .with_max_issues(usize::MAX)
            .clamped();
        assert_eq!(c.max_projects, MAX_SOURCE_PROJECTS);
        assert_eq!(c.max_connections, 3);
        assert_eq!(c.max_bytes, MAX_SOURCE_BYTES);
        assert_eq!(c.max_issues, MAX_SOURCE_ISSUES);
    }

    #[test]
    fn cancel_flag_is_shared_by_clones() {
        let a = CancelFlag::new();
        let b = a.clone();
        assert!(!b.is_cancelled());
        a.cancel();
        assert!(b.is_cancelled());
        let shared = Arc::new(AtomicBool::new(true));
        assert!(CancelFlag::from_shared(shared).is_cancelled());
    }
}
