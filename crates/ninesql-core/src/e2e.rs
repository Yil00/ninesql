//! Test-build guards of the end-to-end scenario runner (feature `e2e-scenarios`, off by
//! default; [ADR-0029](../../../docs/adr/0029-automated-ui-end-to-end-tests-and-application-logging.md)
//! §2.5). Never part of a release build: the feature is not in `default`, and the binary's own
//! guards (ADR-0029 §2.2) refuse a package built with it.
//!
//! The UI runner calls **one** function at startup, before [`crate::CoreServices::start`]:
//!
//! ```no_run
//! # fn run(run_dir: &std::path::Path) -> Result<(), ninesql_core::e2e::GuardError> {
//! // `run_dir`: the run directory that holds HOME, the XDG directories and `--e2e-out`.
//! let guards = ninesql_core::e2e::install(run_dir)?;
//! assert_eq!(guards.run_dir(), run_dir);
//! # Ok(()) }
//! ```
//!
//! From then on, in this process, the core enforces:
//! - **Connections to loopback only (I2).** [`crate::SessionManager::open`] resolves every
//!   network endpoint of a connection before it reads a secret: the database host and every SSH
//!   jump host and SSH server, and the tunnel's target. Each must resolve to loopback addresses
//!   only (`127.0.0.0/8`, `::1`, `::ffff:127.0.0.0/104`), otherwise the open fails with
//!   [`crate::CoreError::TestBuild`]. The address checked is the address connected to (no
//!   second resolution a DNS rebinding could answer differently): the driver gets the IP
//!   literal as its endpoint, with the host name kept as the TLS server name; each SSH hop gets
//!   it as its [`ninesql_ssh::HopSpec::dial_address`] (its `known_hosts` entry stays by name);
//!   the tunnel's target is forwarded to the IP literal. A tunnel target is resolved **here**,
//!   not by the SSH server, so a name only the server knows (a container name) is refused.
//!   Database files must lie inside the run directory. As a second layer, every driver
//!   registered in a [`crate::DriverRegistry`] is wrapped: its `connect` refuses a TCP endpoint
//!   that is not a loopback IP literal and a file outside the run directory, whoever calls it.
//! - **Files inside the run directory (I4).** Export targets
//!   ([`crate::TransferService::check_export_path`]), CSV import sources, the target of
//!   "Export connections to DBeaver…" ([`crate::CoreServices::export_dbeaver`]) and the folder of
//!   a DBeaver import must lie inside the run directory; their directory is canonicalised and
//!   must be named without a symbolic link, `.` or `..`.
//! - **The vault, never the keyring (I1).** [`install`] refuses a build with the
//!   `secret-service` feature (a runtime refusal, not `compile_error!`, because the CI clippy
//!   job builds with `--all-features`), [`crate::CoreServices::start`] refuses to start without
//!   [`install`], never probes the Secret Service, refuses NineSQL directories (and
//!   `known_hosts` files) outside the run directory, and fails when the encrypted vault cannot
//!   be opened, so new secrets always go to the vault in the run directory. The runner creates
//!   that vault with a random master password per run (ADR-0029 §2.5).
//!
//! Fail closed: in a build with the feature, a connect, a transfer or a start before
//! [`install`] is refused ([`GuardError::NotInstalled`]).
//!
//! What this module does not check (the runner does, ADR-0029 §2.4): that `HOME` and the five
//! XDG variables point inside the run directory, that its NineSQL directories are empty at
//! start, and that it is not the user's account home. Files that are only read by a connect (a
//! TLS CA file, a client certificate, an SSH key) are not confined.

use std::net::IpAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use ninesql_storage::Paths;

use crate::connect::Target;
use crate::connection::Connection;
use crate::session::KnownHostsFiles;

/// Longest wait for the resolution of one host name.
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a test-build guard refused. Messages name the host or what was refused, never a secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GuardError {
    /// The binary was built with the `secret-service` feature, which scenario runs refuse.
    #[error(
        "this test build has the `secret-service` feature; scenario runs refuse it so the keyring is never touched"
    )]
    SecretServiceCompiledIn,
    /// [`install`] was not called in this process.
    #[error("the test-build guards are not installed; the scenario runner must install them first")]
    NotInstalled,
    /// [`install`] was called before with another run directory.
    #[error("the test-build guards are already installed for another run directory")]
    AlreadyInstalled,
    /// The run directory is not usable.
    #[error("the run directory {0}")]
    RunDirectory(&'static str),
    /// A host name could not be resolved (or not in time).
    #[error("test build: `{host}` cannot be resolved; only loopback endpoints are allowed")]
    Unresolved {
        /// The host name.
        host: String,
    },
    /// A host is, or resolves to, an address that is not loopback.
    #[error("test build: `{host}` is not a loopback endpoint; only loopback endpoints are allowed")]
    NotLoopback {
        /// The host name or address.
        host: String,
    },
    /// A file lies outside the run directory.
    #[error("test build: the {what} must lie inside the run directory")]
    OutsideRunDirectory {
        /// What was refused (`export file`, `database file`…).
        what: &'static str,
    },
    /// A path is relative, or names a symbolic link, `.` or `..`.
    #[error("test build: the {what} must be an absolute path with no symbolic link, `.` or `..`")]
    NotCanonical {
        /// What was refused.
        what: &'static str,
    },
    /// The encrypted vault cannot be opened, and a test build uses no other store.
    #[error("test build: the encrypted vault is not available; scenario runs use no other store")]
    NoVault,
}

impl GuardError {
    /// A short, stable name of the error kind, safe to log at any level (no host, no path).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SecretServiceCompiledIn => "test-build-secret-service",
            Self::NotInstalled => "test-build-not-installed",
            Self::AlreadyInstalled => "test-build-already-installed",
            Self::RunDirectory(_) => "test-build-run-directory",
            Self::Unresolved { .. } => "test-build-unresolved",
            Self::NotLoopback { .. } => "test-build-not-loopback",
            Self::OutsideRunDirectory { .. } => "test-build-outside-run-directory",
            Self::NotCanonical { .. } => "test-build-not-canonical",
            Self::NoVault => "test-build-no-vault",
        }
    }
}

/// The guards of one scenario run: its run directory (canonical). Built by [`install`];
/// [`TestBuildGuards::new`] builds one without installing it (tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestBuildGuards {
    run_dir: PathBuf,
}

static GUARDS: OnceLock<TestBuildGuards> = OnceLock::new();

/// Installs the guards for this process, once: the call the UI runner makes at startup, before
/// [`crate::CoreServices::start`]. Calling it again with the same directory returns the guards
/// already installed. **Blocking** (a few `stat` calls).
///
/// # Errors
/// [`GuardError::SecretServiceCompiledIn`] in a build with the `secret-service` feature;
/// [`GuardError::RunDirectory`] (see [`TestBuildGuards::new`]);
/// [`GuardError::AlreadyInstalled`] for another directory.
pub fn install(run_dir: impl AsRef<Path>) -> Result<&'static TestBuildGuards, GuardError> {
    refuse_secret_service()?;
    let guards = TestBuildGuards::new(run_dir.as_ref())?;
    let installed = GUARDS.get_or_init(|| guards.clone());
    if *installed == guards {
        tracing::info!("test-build guards installed");
        Ok(installed)
    } else {
        Err(GuardError::AlreadyInstalled)
    }
}

/// The guards installed by [`install`].
///
/// # Errors
/// [`GuardError::NotInstalled`].
pub fn installed() -> Result<&'static TestBuildGuards, GuardError> {
    GUARDS.get().ok_or(GuardError::NotInstalled)
}

/// [`GuardError::SecretServiceCompiledIn`] when this build has the `secret-service` feature.
///
/// # Errors
/// As above.
pub fn refuse_secret_service() -> Result<(), GuardError> {
    if cfg!(feature = "secret-service") {
        Err(GuardError::SecretServiceCompiledIn)
    } else {
        Ok(())
    }
}

/// Whether `ip` is a loopback address: `127.0.0.0/8`, `::1`, or an IPv4-mapped `127.x.y.z`.
/// The unspecified address (`0.0.0.0`, `::`) is not, though Linux routes it locally.
#[must_use]
pub fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// `path` without symbolic links, `.` or `..`: `canonicalize` gives it back unchanged.
fn is_canonical(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
        && std::fs::canonicalize(path).is_ok_and(|c| c == path)
}

impl TestBuildGuards {
    /// Guards for `run_dir`, not installed. The directory must exist and be absolute, named
    /// without a symbolic link, `.` or `..`, a real directory owned by the current user with no
    /// group or other permission (`0700`), and lie strictly under the system temporary
    /// directory or under a directory named `target` (the repository's build directory), so a
    /// mistaken run directory such as the home directory is refused. **Blocking.**
    ///
    /// # Errors
    /// [`GuardError::RunDirectory`].
    pub fn new(run_dir: &Path) -> Result<Self, GuardError> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if !is_canonical(run_dir) {
            return Err(GuardError::RunDirectory(
                "must be an existing absolute path with no symbolic link, `.` or `..`",
            ));
        }
        let meta = std::fs::symlink_metadata(run_dir)
            .map_err(|_| GuardError::RunDirectory("cannot be read"))?;
        if !meta.is_dir() {
            return Err(GuardError::RunDirectory("is not a directory"));
        }
        if meta.uid() != ninesql_storage::fs::current_uid()
            || meta.permissions().mode() & 0o077 != 0
        {
            return Err(GuardError::RunDirectory(
                "must be owned by the current user with mode 0700",
            ));
        }
        let under_temp = std::fs::canonicalize(std::env::temp_dir())
            .is_ok_and(|tmp| run_dir != tmp && run_dir.starts_with(&tmp));
        let under_target = run_dir
            .ancestors()
            .skip(1)
            .any(|a| a.file_name().is_some_and(|n| n == "target"));
        if !under_temp && !under_target {
            return Err(GuardError::RunDirectory(
                "must lie under the system temporary directory or a `target` directory",
            ));
        }
        Ok(Self {
            run_dir: run_dir.to_path_buf(),
        })
    }

    /// The run directory (canonical).
    #[must_use]
    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// Checks a file NineSQL is about to create or replace (an export, a DBeaver export file
    /// or project folder): absolute, its directory named without a symbolic link, `.` or `..`
    /// and inside the run directory, the path itself not a symbolic link. **Blocking.**
    ///
    /// # Errors
    /// [`GuardError::NotCanonical`], [`GuardError::OutsideRunDirectory`].
    pub fn check_new_file(&self, path: &Path, what: &'static str) -> Result<(), GuardError> {
        let not_canonical = GuardError::NotCanonical { what };
        let (Some(parent), Some(Component::Normal(_))) =
            (path.parent(), path.components().next_back())
        else {
            return Err(not_canonical);
        };
        if !is_canonical(parent) {
            return Err(not_canonical);
        }
        if !parent.starts_with(&self.run_dir) {
            return Err(GuardError::OutsideRunDirectory { what });
        }
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(not_canonical);
        }
        Ok(())
    }

    /// Checks a file NineSQL reads (a CSV import source) or a folder (a DBeaver import source):
    /// it exists, is named without a symbolic link, `.` or `..`, and lies inside the run
    /// directory. **Blocking.**
    ///
    /// # Errors
    /// [`GuardError::NotCanonical`], [`GuardError::OutsideRunDirectory`].
    pub fn check_existing(&self, path: &Path, what: &'static str) -> Result<(), GuardError> {
        if !is_canonical(path) {
            return Err(GuardError::NotCanonical { what });
        }
        if !path.starts_with(&self.run_dir) {
            return Err(GuardError::OutsideRunDirectory { what });
        }
        Ok(())
    }

    /// Checks a database file a driver opens: [`Self::check_existing`] when it exists,
    /// [`Self::check_new_file`] when the driver may create it. **Blocking.**
    ///
    /// # Errors
    /// As those two.
    pub fn check_database_file(&self, path: &Path) -> Result<(), GuardError> {
        const WHAT: &str = "database file";
        match std::fs::symlink_metadata(path) {
            Ok(_) => self.check_existing(path, WHAT),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.check_new_file(path, WHAT),
            Err(_) => Err(GuardError::NotCanonical { what: WHAT }),
        }
    }

    /// Checks the NineSQL directories and the `known_hosts` files of a start: each absolute,
    /// with no `.` or `..`, inside the run directory (compared by name: the directories may not
    /// exist yet; [`Self::check_paths_resolved`] compares them resolved once they do).
    ///
    /// # Errors
    /// [`GuardError::NotCanonical`], [`GuardError::OutsideRunDirectory`].
    pub fn check_paths(
        &self,
        paths: &Paths,
        known_hosts: &KnownHostsFiles,
    ) -> Result<(), GuardError> {
        let named = [
            (paths.config_dir(), "configuration directory"),
            (paths.data_dir(), "data directory"),
            (paths.state_dir(), "state directory"),
            (known_hosts.own.as_path(), "known_hosts file"),
        ];
        let user = known_hosts
            .user
            .as_deref()
            .map(|p| (p, "user known_hosts file"));
        for (path, what) in named.into_iter().chain(user) {
            let plain = path.is_absolute()
                && path
                    .components()
                    .all(|c| matches!(c, Component::RootDir | Component::Normal(_)));
            if !plain {
                return Err(GuardError::NotCanonical { what });
            }
            if !path.starts_with(&self.run_dir) {
                return Err(GuardError::OutsideRunDirectory { what });
            }
        }
        Ok(())
    }

    /// The NineSQL directories resolved (symbolic links followed) still lie inside the run
    /// directory. Call it once they exist ([`crate::Workspace::open`] creates them).
    /// **Blocking.**
    ///
    /// # Errors
    /// [`GuardError::OutsideRunDirectory`] (a directory that cannot be resolved included).
    pub fn check_paths_resolved(&self, paths: &Paths) -> Result<(), GuardError> {
        for (path, what) in [
            (paths.config_dir(), "configuration directory"),
            (paths.data_dir(), "data directory"),
            (paths.state_dir(), "state directory"),
        ] {
            if !std::fs::canonicalize(path).is_ok_and(|c| c.starts_with(&self.run_dir)) {
                return Err(GuardError::OutsideRunDirectory { what });
            }
        }
        Ok(())
    }

    /// Resolves `host` and checks that every address is loopback; returns the address to
    /// connect to (the first IPv4 one, else the first). An IP literal is checked as it is.
    ///
    /// # Errors
    /// [`GuardError::Unresolved`] (no address, a failure or [`RESOLVE_TIMEOUT`]),
    /// [`GuardError::NotLoopback`].
    pub async fn resolve_loopback(&self, host: &str, port: u16) -> Result<IpAddr, GuardError> {
        if let Ok(ip) = host.parse::<IpAddr>() {
            return if is_loopback(ip) {
                Ok(ip)
            } else {
                Err(GuardError::NotLoopback {
                    host: host.to_owned(),
                })
            };
        }
        let unresolved = || GuardError::Unresolved {
            host: host.to_owned(),
        };
        let found: Vec<IpAddr> =
            tokio::time::timeout(RESOLVE_TIMEOUT, tokio::net::lookup_host((host, port)))
                .await
                .map_err(|_| unresolved())?
                .map_err(|_| unresolved())?
                .map(|a| a.ip())
                .collect();
        if found.is_empty() {
            return Err(unresolved());
        }
        if !found.iter().copied().all(is_loopback) {
            return Err(GuardError::NotLoopback {
                host: host.to_owned(),
            });
        }
        Ok(found
            .iter()
            .copied()
            .find(IpAddr::is_ipv4)
            .unwrap_or(found[0]))
    }
}

/// The addresses a connection's endpoints resolved to, which the connect flow uses instead of
/// the names ([`check_connection`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Pins {
    /// The database host (the tunnel's target when there is a tunnel).
    target: Option<IpAddr>,
    /// The SSH jump hosts, in order.
    jump_hosts: Vec<IpAddr>,
    /// The SSH server.
    server: Option<IpAddr>,
}

/// Checks every endpoint of `connection` against the installed guards and returns the
/// addresses to connect to. Called by the connect flow before any secret is read.
///
/// # Errors
/// [`GuardError`] (as [`crate::CoreError::TestBuild`]).
pub(crate) async fn check_connection(
    connection: &Connection,
    target: &Target,
) -> Result<Pins, GuardError> {
    let guards = installed()?;
    let mut pins = Pins::default();
    match target {
        Target::Tcp { host, port, .. } => {
            pins.target = Some(guards.resolve_loopback(host, *port).await?);
        }
        Target::File(path) => {
            let path = path.clone();
            tokio::task::spawn_blocking(move || guards.check_database_file(&path))
                .await
                .map_err(|_| GuardError::NotCanonical {
                    what: "database file",
                })??;
        }
    }
    if let Some(ssh) = &connection.ssh {
        for hop in &ssh.jump_hosts {
            pins.jump_hosts
                .push(guards.resolve_loopback(&hop.host, hop.port).await?);
        }
        pins.server = Some(
            guards
                .resolve_loopback(&ssh.server.host, ssh.server.port)
                .await?,
        );
    }
    Ok(pins)
}

impl Pins {
    /// Points each hop of `spec` at its checked address and its target at the checked IP
    /// literal (resolved here, never by the SSH server).
    pub(crate) fn apply_to_tunnel(&self, spec: &mut ninesql_ssh::TunnelSpec) {
        for (hop, ip) in spec.jump_hosts.iter_mut().zip(&self.jump_hosts) {
            hop.dial_address = Some(*ip);
        }
        spec.server.dial_address = self.server;
        if let Some(ip) = self.target {
            spec.target_host = ip.to_string();
        }
    }

    /// Without a tunnel: the driver connects to the checked IP literal, and verifies TLS
    /// against the host name it was given (unless the core already set a server name).
    pub(crate) fn apply_to_params(&self, params: &mut ninesql_driver_api::ConnectParams) {
        let Some(ip) = self.target else {
            return;
        };
        if params.tunnel.is_some() {
            return;
        }
        if let ninesql_driver_api::Endpoint::Tcp { host, .. } = &mut params.endpoint {
            let name = std::mem::replace(host, ip.to_string());
            if params.tls.server_name.is_none() && name.parse::<IpAddr>().is_err() {
                params.tls.server_name = Some(name);
            }
        }
    }
}

/// The second layer: a driver wrapped by [`crate::DriverRegistry::register`] in a test build.
/// Its `connect` refuses, whoever calls it, a TCP endpoint that is not a loopback IP literal
/// (the connect flow passes the checked literal, [`Pins::apply_to_params`]; a tunnel's local
/// end is `127.0.0.1`) and a database file outside the run directory.
pub(crate) struct GuardedDriver(pub(crate) std::sync::Arc<dyn ninesql_driver_api::Driver>);

/// Checks the endpoint a driver is asked to connect to.
pub(crate) fn check_driver_endpoint(
    endpoint: &ninesql_driver_api::Endpoint,
) -> Result<(), GuardError> {
    let guards = installed()?;
    match endpoint {
        ninesql_driver_api::Endpoint::Tcp { host, .. } => match host.parse::<IpAddr>() {
            Ok(ip) if is_loopback(ip) => Ok(()),
            _ => Err(GuardError::NotLoopback { host: host.clone() }),
        },
        ninesql_driver_api::Endpoint::File { path, .. } => guards.check_database_file(path),
        _ => Ok(()),
    }
}

#[async_trait::async_trait]
impl ninesql_driver_api::Driver for GuardedDriver {
    fn family(&self) -> &'static str {
        self.0.family()
    }

    fn engines(&self) -> &[ninesql_driver_api::EngineId] {
        self.0.engines()
    }

    fn capabilities(
        &self,
        engine: &ninesql_driver_api::EngineId,
        server_version: Option<&str>,
    ) -> ninesql_driver_api::DriverResult<ninesql_driver_api::Capabilities> {
        self.0.capabilities(engine, server_version)
    }

    fn dialect(
        &self,
        engine: &ninesql_driver_api::EngineId,
    ) -> ninesql_driver_api::DriverResult<std::sync::Arc<dyn ninesql_driver_api::Dialect>> {
        self.0.dialect(engine)
    }

    async fn connect(
        &self,
        params: ninesql_driver_api::ConnectParams,
    ) -> ninesql_driver_api::DriverResult<Box<dyn ninesql_driver_api::Session>> {
        // File checks are a few `stat` calls; the runtime's blocking pool is not needed for
        // them more than for the drivers' own path checks.
        if let Err(e) = check_driver_endpoint(&params.endpoint) {
            return Err(ninesql_driver_api::DriverError::InvalidParams(
                e.to_string(),
            ));
        }
        self.0.connect(params).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A run directory under the system temporary directory, `0700`, canonical.
    fn run_dir() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let canonical = std::fs::canonicalize(tmp.path()).unwrap();
        (tmp, canonical)
    }

    #[test]
    fn loopback_addresses_only() {
        for ip in [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::new(127, 3, 2, 1)),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V6(Ipv4Addr::new(127, 0, 0, 1).to_ipv6_mapped()),
        ] {
            assert!(is_loopback(ip), "{ip}");
        }
        for ip in [
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
            IpAddr::V6(Ipv4Addr::new(10, 0, 0, 1).to_ipv6_mapped()),
            "fe80::1".parse().unwrap(),
        ] {
            assert!(!is_loopback(ip), "{ip}");
        }
    }

    #[test]
    fn the_run_directory_must_be_private_canonical_and_under_temp_or_target() {
        let (_tmp, dir) = run_dir();
        let g = TestBuildGuards::new(&dir).unwrap();
        assert_eq!(g.run_dir(), dir);

        // Group / other access.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750)).unwrap();
        assert!(matches!(
            TestBuildGuards::new(&dir),
            Err(GuardError::RunDirectory(_))
        ));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Relative, `..`, a symbolic link, a file, missing.
        assert!(TestBuildGuards::new(Path::new("run")).is_err());
        let sub = dir.join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(TestBuildGuards::new(&sub.join("..").join("sub")).is_err());
        let link = dir.join("link");
        std::os::unix::fs::symlink(&sub, &link).unwrap();
        assert!(TestBuildGuards::new(&link).is_err());
        let file = dir.join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(TestBuildGuards::new(&file).is_err());
        assert!(TestBuildGuards::new(&dir.join("missing")).is_err());
        // Not under the temporary directory nor a `target` directory.
        assert!(TestBuildGuards::new(Path::new("/")).is_err());
        let tmp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        assert!(TestBuildGuards::new(&tmp).is_err(), "the temp dir itself");
    }

    #[test]
    fn new_files_must_be_inside_the_run_directory_without_links() {
        let (_tmp, dir) = run_dir();
        let g = TestBuildGuards::new(&dir).unwrap();
        let out = dir.join("out");
        std::fs::create_dir(&out).unwrap();
        g.check_new_file(&out.join("rows.csv"), "export file")
            .unwrap();
        // An existing regular file may be replaced (the transfer rules decide).
        std::fs::write(out.join("old.csv"), b"").unwrap();
        g.check_new_file(&out.join("old.csv"), "export file")
            .unwrap();

        let (_other_tmp, other) = run_dir();
        assert_eq!(
            g.check_new_file(&other.join("rows.csv"), "export file"),
            Err(GuardError::OutsideRunDirectory {
                what: "export file"
            })
        );
        // A link to a folder outside, a link as the file, `..`, relative, no file name.
        std::os::unix::fs::symlink(&other, dir.join("away")).unwrap();
        assert!(matches!(
            g.check_new_file(&dir.join("away").join("rows.csv"), "export file"),
            Err(GuardError::NotCanonical { .. })
        ));
        std::os::unix::fs::symlink(other.join("x.csv"), out.join("link.csv")).unwrap();
        assert!(matches!(
            g.check_new_file(&out.join("link.csv"), "export file"),
            Err(GuardError::NotCanonical { .. })
        ));
        assert!(
            g.check_new_file(&out.join("..").join("..").join("x.csv"), "export file")
                .is_err()
        );
        assert!(g.check_new_file(&out.join(".."), "export file").is_err());
        assert!(
            g.check_new_file(Path::new("rows.csv"), "export file")
                .is_err()
        );
        assert!(g.check_new_file(Path::new("/"), "export file").is_err());
        // The run directory's parent.
        assert!(
            g.check_new_file(&dir.parent().unwrap().join("x.csv"), "export file")
                .is_err()
        );
    }

    #[test]
    fn existing_files_and_database_files_must_be_inside_without_links() {
        let (_tmp, dir) = run_dir();
        let g = TestBuildGuards::new(&dir).unwrap();
        let csv = dir.join("in.csv");
        std::fs::write(&csv, b"a\n1\n").unwrap();
        g.check_existing(&csv, "import file").unwrap();
        let link = dir.join("link.csv");
        std::os::unix::fs::symlink(&csv, &link).unwrap();
        assert!(matches!(
            g.check_existing(&link, "import file"),
            Err(GuardError::NotCanonical { .. })
        ));
        let (_other_tmp, other) = run_dir();
        let outside = other.join("in.csv");
        std::fs::write(&outside, b"").unwrap();
        assert!(matches!(
            g.check_existing(&outside, "import file"),
            Err(GuardError::OutsideRunDirectory { .. })
        ));
        assert!(
            g.check_existing(&dir.join("missing.csv"), "import file")
                .is_err()
        );

        // Database files: existing or to be created, inside only.
        let db = dir.join("nine.sqlite");
        g.check_database_file(&db).unwrap();
        std::fs::write(&db, b"").unwrap();
        g.check_database_file(&db).unwrap();
        assert!(g.check_database_file(&other.join("nine.sqlite")).is_err());
        assert!(g.check_database_file(&link).is_err());
    }

    #[test]
    fn paths_must_lie_inside_the_run_directory() {
        let (_tmp, dir) = run_dir();
        let g = TestBuildGuards::new(&dir).unwrap();
        let xdg = |root: &Path| {
            Paths::new(&ninesql_storage::XdgDirs {
                config_home: root.join("config"),
                data_home: root.join("data"),
                state_home: root.join("state"),
            })
        };
        let inside = xdg(&dir);
        let kh = KnownHostsFiles {
            own: inside.data_dir().join("known_hosts"),
            user: Some(dir.join("home/.ssh/known_hosts")),
        };
        g.check_paths(&inside, &kh).unwrap();
        let (_other_tmp, other) = run_dir();
        assert!(matches!(
            g.check_paths(&xdg(&other), &kh),
            Err(GuardError::OutsideRunDirectory { .. })
        ));
        let user_ssh = KnownHostsFiles {
            own: kh.own.clone(),
            user: Some(other.join(".ssh/known_hosts")),
        };
        assert!(g.check_paths(&inside, &user_ssh).is_err());
        assert!(matches!(
            g.check_paths(&xdg(&dir.join("..").join("x")), &kh),
            Err(GuardError::NotCanonical { .. })
        ));
        // Resolved: a directory that is a link to the outside is refused once it exists.
        for d in [inside.config_dir(), inside.data_dir()] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::create_dir_all(dir.join("state")).unwrap();
        std::os::unix::fs::symlink(&other, inside.state_dir()).unwrap();
        assert!(g.check_paths_resolved(&inside).is_err());
        std::fs::remove_file(inside.state_dir()).unwrap();
        std::fs::create_dir(inside.state_dir()).unwrap();
        g.check_paths_resolved(&inside).unwrap();
    }

    #[test]
    fn hosts_must_resolve_to_loopback_only() {
        let (_tmp, dir) = run_dir();
        let g = TestBuildGuards::new(&dir).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            assert_eq!(
                g.resolve_loopback("127.0.0.1", 5432).await,
                Ok(IpAddr::V4(Ipv4Addr::LOCALHOST))
            );
            assert_eq!(
                g.resolve_loopback("::1", 5432).await,
                Ok(IpAddr::V6(Ipv6Addr::LOCALHOST))
            );
            let localhost = g.resolve_loopback("localhost", 5432).await.unwrap();
            assert!(is_loopback(localhost));
            for host in ["10.1.2.3", "0.0.0.0", "::", "192.0.2.1", "::ffff:10.0.0.1"] {
                assert!(
                    matches!(
                        g.resolve_loopback(host, 5432).await,
                        Err(GuardError::NotLoopback { .. })
                    ),
                    "{host}"
                );
            }
            // `.invalid` never resolves (RFC 6761); a bracketed literal is not a host name.
            for host in ["db.invalid", "[::1]", ""] {
                assert!(
                    matches!(
                        g.resolve_loopback(host, 5432).await,
                        Err(GuardError::Unresolved { .. })
                    ),
                    "{host}"
                );
            }
        });
    }

    #[test]
    fn pins_replace_names_with_checked_addresses() {
        use ninesql_driver_api as api;
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let pins = Pins {
            target: Some(ip),
            jump_hosts: vec![IpAddr::V6(Ipv6Addr::LOCALHOST)],
            server: Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2))),
        };
        // Direct: the literal is the endpoint, the name the TLS server name.
        let mut params = api::ConnectParams::new(
            api::EngineId::new("postgresql"),
            api::Endpoint::tcp("localhost", 15432),
        );
        pins.apply_to_params(&mut params);
        assert_eq!(params.endpoint, api::Endpoint::tcp("127.0.0.1", 15432));
        assert_eq!(params.tls.server_name.as_deref(), Some("localhost"));
        // An IP literal stays, with no server name.
        let mut literal = api::ConnectParams::new(
            api::EngineId::new("postgresql"),
            api::Endpoint::tcp("127.0.0.1", 15432),
        );
        pins.apply_to_params(&mut literal);
        assert_eq!(literal.tls.server_name, None);

        // Tunnel: every hop dials its address, the target is the literal.
        let auth = || ninesql_ssh::HopAuth::Agent;
        let mut spec = ninesql_ssh::TunnelSpec::new(
            ninesql_ssh::HopSpec::new("ssh.localhost", "u", auth()),
            "db.localhost",
            5432,
        )
        .via(ninesql_ssh::HopSpec::new("jump.localhost", "u", auth()));
        pins.apply_to_tunnel(&mut spec);
        assert_eq!(spec.jump_hosts[0].dial_address, Some(pins.jump_hosts[0]));
        assert_eq!(
            spec.jump_hosts[0].host, "jump.localhost",
            "known_hosts by name"
        );
        assert_eq!(spec.server.dial_address, pins.server);
        assert_eq!(spec.target_host, "127.0.0.1");
    }

    #[test]
    fn error_kinds_carry_no_host_or_path() {
        let e = GuardError::NotLoopback {
            host: "db.example.test".into(),
        };
        assert!(e.to_string().contains("db.example.test"), "the UI names it");
        assert_eq!(e.kind(), "test-build-not-loopback");
    }
}
