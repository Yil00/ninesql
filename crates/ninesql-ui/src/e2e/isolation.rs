//! Isolation from the user's data (ADR-0029 §2.4), checked before GPUI starts and before
//! anything is read or written: `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME` and
//! `XDG_CACHE_HOME` are set, absolute and inside one run directory that also holds the
//! `--e2e-out` directory; the run directory is a real directory owned by the user with mode
//! `0700`, under the system temporary directory or a `target` directory, not the account's
//! home directory (read from the password database, not from `$HOME`) nor a default XDG
//! location under it; the NineSQL subdirectories are empty. The run directory is also checked by
//! the core's guards (`ninesql_core::e2e::install`). Any failure stops the run with status 2
//! and a fixed message.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The five variables a run must set.
pub const VARIABLES: [&str; 5] = [
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
];

/// What the checks found: the run directory and the canonical output directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Isolated {
    /// The run directory (canonical): the deepest common ancestor of the five directories and
    /// the output directory.
    pub run_dir: PathBuf,
    /// `--e2e-out`, canonical.
    pub out_dir: PathBuf,
    /// `$HOME`, canonical.
    pub home: PathBuf,
}

/// The account's home directory from the password database (`getpwuid_r`), never `$HOME`.
#[must_use]
pub fn account_home() -> Option<PathBuf> {
    nix::unistd::User::from_uid(nix::unistd::Uid::effective())
        .ok()
        .flatten()
        .map(|u| u.dir)
}

fn deepest_common_ancestor(paths: &[PathBuf]) -> Option<PathBuf> {
    let first = paths.first()?;
    let mut common = first.clone();
    for p in &paths[1..] {
        while !p.starts_with(&common) {
            if !common.pop() {
                return None;
            }
        }
    }
    Some(common)
}

/// Runs the checks with `lookup` as the environment, `account_home` from the password database
/// and `temp_dir` the system temporary directory.
///
/// # Errors
/// A fixed message (no path, no value) naming the rule that failed.
pub fn check(
    lookup: impl Fn(&str) -> Option<OsString>,
    out_dir: &Path,
    account_home: Option<&Path>,
    temp_dir: &Path,
) -> Result<Isolated, &'static str> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let mut dirs = Vec::new();
    for var in VARIABLES {
        let value = lookup(var).filter(|v| !v.is_empty()).ok_or(
            "HOME, XDG_CONFIG_HOME, XDG_DATA_HOME, XDG_STATE_HOME and XDG_CACHE_HOME must all be set",
        )?;
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            return Err("HOME and the XDG directories must be absolute paths");
        }
        let canonical =
            std::fs::canonicalize(&path).map_err(|_| "HOME and the XDG directories must exist")?;
        if !canonical.is_dir() {
            return Err("HOME and the XDG directories must be directories");
        }
        dirs.push(canonical);
    }
    let home = dirs[0].clone();
    if !out_dir.is_absolute() {
        return Err("--e2e-out must be an absolute path");
    }
    let out = std::fs::canonicalize(out_dir).map_err(|_| "--e2e-out must exist")?;
    if !out.is_dir() {
        return Err("--e2e-out must be a directory");
    }
    let mut all = dirs.clone();
    all.push(out.clone());
    let run_dir = deepest_common_ancestor(&all)
        .ok_or("HOME, the XDG directories and --e2e-out must share one run directory")?;
    if all.contains(&run_dir) {
        return Err(
            "HOME, the XDG directories and --e2e-out must be distinct subdirectories of one run directory",
        );
    }
    let meta =
        std::fs::symlink_metadata(&run_dir).map_err(|_| "the run directory cannot be read")?;
    if !meta.is_dir()
        || meta.uid() != ninesql_core::storage::fs::current_uid()
        || meta.permissions().mode() & 0o077 != 0
    {
        return Err("the run directory must be owned by the current user with mode 0700");
    }
    let temp = std::fs::canonicalize(temp_dir).unwrap_or_else(|_| temp_dir.to_path_buf());
    let under_temp = run_dir != temp && run_dir.starts_with(&temp);
    let under_target = run_dir
        .ancestors()
        .skip(1)
        .any(|a| a.file_name().is_some_and(|n| n == "target"));
    if !under_temp && !under_target {
        return Err(
            "the run directory must lie under the system temporary directory or a target directory",
        );
    }
    let Some(account) = account_home else {
        return Err("the account's home directory cannot be read from the password database");
    };
    let account = std::fs::canonicalize(account).unwrap_or_else(|_| account.to_path_buf());
    let defaults = [
        account.clone(),
        account.join(".config"),
        account.join(".local/share"),
        account.join(".local/state"),
        account.join(".cache"),
    ];
    for d in &defaults {
        let d = std::fs::canonicalize(d).unwrap_or_else(|_| d.clone());
        if run_dir.starts_with(&d) && d != account {
            return Err("the run directory must not lie in a default XDG directory of the account");
        }
        if d.starts_with(&run_dir) {
            return Err("the run directory must not be or contain the account's home directory");
        }
        if dirs.contains(&d) {
            return Err("HOME and the XDG directories must not be the account's own directories");
        }
    }
    // The NineSQL directories must be empty (or absent) at start.
    for d in &dirs[1..] {
        let app = d.join(ninesql_core::storage::paths::APP_DIR);
        match std::fs::read_dir(&app) {
            Ok(mut entries) => {
                if entries.next().is_some() {
                    return Err("the NineSQL directories of the run must be empty at start");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("the NineSQL directories of the run cannot be read"),
        }
    }
    Ok(Isolated {
        run_dir,
        out_dir: out,
        home,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    struct Run {
        _tmp: tempfile::TempDir,
        dir: PathBuf,
        env: HashMap<&'static str, PathBuf>,
    }

    fn run() -> Run {
        let tmp = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(tmp.path()).unwrap().join("run");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut env = HashMap::new();
        for (var, sub) in VARIABLES
            .iter()
            .zip(["home", "config", "data", "state", "cache"])
        {
            std::fs::create_dir(dir.join(sub)).unwrap();
            env.insert(*var, dir.join(sub));
        }
        std::fs::create_dir(dir.join("out")).unwrap();
        Run {
            _tmp: tmp,
            dir,
            env,
        }
    }

    fn check_run(r: &Run, account: &Path) -> Result<Isolated, &'static str> {
        let env = r.env.clone();
        check(
            move |k| env.get(k).map(|p| p.clone().into_os_string()),
            &r.dir.join("out"),
            Some(account),
            &std::env::temp_dir(),
        )
    }

    #[test]
    fn a_fresh_private_run_directory_passes() {
        let r = run();
        let i = check_run(&r, Path::new("/home/someone-else")).unwrap();
        assert_eq!(i.run_dir, r.dir);
        assert_eq!(i.home, r.dir.join("home"));
        assert!(account_home().is_some(), "the password database answers");
    }

    #[test]
    fn missing_relative_or_scattered_variables_are_refused() {
        let r = run();
        let mut env = r.env.clone();
        env.remove("XDG_CACHE_HOME");
        let e = check(
            |k| env.get(k).map(|p| p.clone().into_os_string()),
            &r.dir.join("out"),
            Some(Path::new("/nonexistent")),
            &std::env::temp_dir(),
        );
        assert!(e.unwrap_err().contains("must all be set"));

        let mut env = r.env.clone();
        env.insert("XDG_DATA_HOME", PathBuf::from("data"));
        let e = check(
            |k| env.get(k).map(|p| p.clone().into_os_string()),
            &r.dir.join("out"),
            Some(Path::new("/nonexistent")),
            &std::env::temp_dir(),
        );
        assert!(e.unwrap_err().contains("absolute"));

        // HOME elsewhere: the common ancestor is the temp dir itself, not a private run dir.
        let other = run();
        let mut env = r.env.clone();
        env.insert("HOME", other.dir.join("home"));
        assert!(
            check(
                |k| env.get(k).map(|p| p.clone().into_os_string()),
                &r.dir.join("out"),
                Some(Path::new("/nonexistent")),
                &std::env::temp_dir(),
            )
            .is_err()
        );
    }

    #[test]
    fn group_readable_run_directory_is_refused() {
        let r = run();
        std::fs::set_permissions(&r.dir, std::fs::Permissions::from_mode(0o750)).unwrap();
        assert!(
            check_run(&r, Path::new("/nonexistent"))
                .unwrap_err()
                .contains("0700")
        );
    }

    #[test]
    fn the_account_home_is_refused() {
        let r = run();
        // The run directory is the account's home, or inside its ~/.cache.
        assert!(check_run(&r, &r.dir).is_err());
        // A run directory below the account's home (a repository's `target/`) is allowed.
        check_run(&r, r.dir.parent().unwrap()).unwrap();
        let fake_account = r.dir.join("acct");
        std::fs::create_dir(&fake_account).unwrap();
        assert!(
            check_run(&r, &fake_account).is_err(),
            "contains the account home"
        );
    }

    #[test]
    fn non_empty_ninesql_directories_are_refused() {
        let r = run();
        let app = r.dir.join("data").join("ninesql");
        std::fs::create_dir(&app).unwrap();
        check_run(&r, Path::new("/nonexistent")).unwrap();
        std::fs::write(app.join("connections.json"), b"{}").unwrap();
        assert!(
            check_run(&r, Path::new("/nonexistent"))
                .unwrap_err()
                .contains("empty")
        );
    }

    #[test]
    fn outside_temp_and_target_is_refused() {
        let r = run();
        let env = r.env.clone();
        // A temp dir elsewhere: the run directory is not under it, nor under `target`.
        let e = check(
            |k| env.get(k).map(|p| p.clone().into_os_string()),
            &r.dir.join("out"),
            Some(Path::new("/nonexistent")),
            Path::new("/var/empty-temp-for-test"),
        );
        assert!(e.unwrap_err().contains("temporary directory"));
    }
}
