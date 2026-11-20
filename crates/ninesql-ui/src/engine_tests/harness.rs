//! The harness of the engine tests: a window on the shipped drivers with an in-memory secret
//! store and temporary XDG directories, the dev engines of `dev/engines.json` (skipped when
//! unreachable unless `NINESQL_REQUIRE_ENGINES` lists them), a separate driver session for setup
//! and read-backs, scratch tables named per test and per run, and the I1 scan.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{Entity, TestAppContext, VisualTestContext};
use gpui_kit::Action;
use ninesql_core::driver_api::dml::cell_value;
use ninesql_core::driver_api::{
    self as api, ConnectParams, Driver, DriverError, ExecOptions, Param, ResultItem, Session,
    Statement,
};
use ninesql_core::secrets::{MemoryStore, SecretStores, StoreKind};
use ninesql_core::storage::{Paths, ProjectName, XdgDirs};
use ninesql_core::{
    ConnectionId, ConnectionTypeId, KnownHostsFiles, ManagerConfig, SessionManager, TlsMode,
    Workspace as CoreWorkspace,
};

use crate::connections::dialog::TestState;
use crate::connections::form::{AuthChoice, SecretMode};
use crate::connections::hub::{ConnectionHub, HubConfig, install, registry_with};
use crate::editor_tab::SqlEditorTab;
use crate::navigator::Navigator;
use crate::settings::{self, SettingsState};
use crate::workspace::Workspace;

/// How long one step may take against a real engine (connects, metadata loads, saves).
const STEP_TIMEOUT: Duration = Duration::from_secs(60);

/// The engines these tests run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Engine {
    Postgresql,
    Mysql,
    Mariadb,
    Sqlite,
}

impl Engine {
    /// The engine id of `dev/engines.json` and of the drivers.
    pub(super) fn id(self) -> &'static str {
        match self {
            Self::Postgresql => "postgresql",
            Self::Mysql => "mysql",
            Self::Mariadb => "mariadb",
            Self::Sqlite => "sqlite",
        }
    }

    /// The schema (or database) the "nine" tables are in, as the navigator names it.
    pub(super) fn schema(self) -> &'static str {
        match self {
            Self::Postgresql => "public",
            Self::Mysql | Self::Mariadb => "nine",
            Self::Sqlite => "main",
        }
    }

    fn driver(self) -> Box<dyn Driver> {
        match self {
            Self::Postgresql => Box::new(ninesql_driver_pgwire::PgWireDriver::new()),
            Self::Mysql | Self::Mariadb => {
                Box::new(ninesql_driver_mysqlwire::MySqlWireDriver::new())
            }
            Self::Sqlite => Box::new(ninesql_driver_sqlite::SqliteDriver::new()),
        }
    }

    /// `CREATE TABLE` of a scratch table: `id` primary key, `label` text.
    fn create_table(self, table: &str) -> String {
        match self {
            Self::Postgresql => {
                format!(
                    "CREATE TABLE {table} (id integer PRIMARY KEY, label varchar(100) NOT NULL)"
                )
            }
            Self::Mysql | Self::Mariadb => format!(
                "CREATE TABLE {table} (id INT PRIMARY KEY, label VARCHAR(100) NOT NULL) ENGINE=InnoDB"
            ),
            Self::Sqlite => {
                format!("CREATE TABLE {table} (id INTEGER PRIMARY KEY, label TEXT NOT NULL)")
            }
        }
    }

    /// Fills a scratch table with rows `1..=rows` (`label` = `row <id>`), at most 10 000.
    fn fill_table(self, table: &str, rows: u32) -> String {
        let digits = "(SELECT 0 AS d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 \
                      UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 \
                      UNION ALL SELECT 8 UNION ALL SELECT 9)";
        let label = match self {
            Self::Mysql | Self::Mariadb => "CONCAT('row ', n)",
            Self::Postgresql | Self::Sqlite => "'row ' || n",
        };
        format!(
            "INSERT INTO {table} (id, label) SELECT n, {label} FROM (SELECT a.d + 10 * b.d + \
             100 * c.d + 1000 * e.d + 1 AS n FROM {digits} a CROSS JOIN {digits} b CROSS JOIN \
             {digits} c CROSS JOIN {digits} e) s WHERE n <= {rows}"
        )
    }
}

/// A dev engine of `dev/engines.json`.
#[derive(Debug, Clone)]
pub(super) struct Dev {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
}

/// Every distinct password of `dev/engines.json` the I1 scan looks for: its `"password"`
/// fields and the `password: <value>` of its notes (root and SYSDBA passwords), as
/// `scripts/e2e-local.sh` redacts them. Values under 4 characters are skipped (too short to
/// scan for without false positives), as in the script.
pub(super) fn dev_passwords() -> Vec<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dev/engines.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{e}"));
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}"));
    let passwords = passwords_in(&doc);
    assert!(
        !passwords.is_empty(),
        "no password found in dev/engines.json"
    );
    passwords
}

/// The distinct passwords of a parsed `dev/engines.json`, longest first.
fn passwords_in(doc: &serde_json::Value) -> Vec<String> {
    fn walk(v: &serde_json::Value, found: &mut std::collections::BTreeSet<String>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, x) in map {
                    if k == "password"
                        && let Some(p) = x.as_str()
                        && !p.is_empty()
                    {
                        found.insert(p.to_owned());
                    }
                    walk(x, found);
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|x| walk(x, found)),
            serde_json::Value::String(s) => found.extend(note_passwords(s)),
            _ => {}
        }
    }
    let mut found = std::collections::BTreeSet::new();
    walk(doc, &mut found);
    let mut out: Vec<String> = found.into_iter().filter(|w| w.len() >= 4).collect();
    out.sort_by_key(|w| std::cmp::Reverse(w.len()));
    out
}

/// The `<value>` of each `password: <value>` in a note (the script's
/// `password:\s*([^\s,;]+)`, a trailing `.` dropped).
fn note_passwords(text: &str) -> Vec<String> {
    const KEY: &str = "password:";
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(KEY) {
        rest = rest[at + KEY.len()..].trim_start();
        let end = rest
            .find(|c: char| c.is_whitespace() || c == ',' || c == ';')
            .unwrap_or(rest.len());
        let value = rest[..end].trim_end_matches('.');
        if !value.is_empty() {
            out.push(value.to_owned());
        }
        rest = &rest[end..];
    }
    out
}

#[test]
fn the_i1_scan_looks_for_every_dev_password() {
    let doc = serde_json::json!({
        "engines": [
            { "id": "a", "password": "first_pw" },
            { "id": "b", "password": "first_pw", "notes": "root password: second_pw (set by x)" },
            { "id": "c", "password": null, "notes": "SYSDBA password: third_pw. Database: path" },
            { "id": "d", "password": "", "notes": "password:fourth_pw, password: abc" }
        ]
    });
    assert_eq!(
        passwords_in(&doc),
        ["fourth_pw", "second_pw", "first_pw", "third_pw"]
    );
    // The real file: the shared dev password and the root password of its notes.
    let real = dev_passwords();
    assert!(real.iter().any(|p| p == &dev_entry("postgresql").password));
    assert!(real.len() >= 2, "{} passwords", real.len());
}

fn required(engine: &str) -> bool {
    std::env::var("NINESQL_REQUIRE_ENGINES")
        .unwrap_or_default()
        .split([',', ' '])
        .any(|e| e.trim() == engine || e.trim() == "all")
}

fn dev_entry(id: &str) -> Dev {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dev/engines.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{e}"));
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}"));
    let Some(e) = doc["engines"]
        .as_array()
        .and_then(|a| a.iter().find(|e| e["id"] == id))
    else {
        panic!("{id} is not in dev/engines.json");
    };
    let text = |k: &str| e[k].as_str().unwrap_or_default().to_owned();
    Dev {
        host: text("host"),
        port: e["port"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .unwrap_or_else(|| panic!("port of {id}")),
        user: text("user"),
        password: text("password"),
        database: text("database"),
    }
}

/// Where the test connects: a dev engine, or a SQLite file of its own. `None` (with a line on
/// stderr) when the engine is unreachable and not required.
pub(super) fn target(engine: Engine, dir: &Path) -> Option<Target> {
    if engine == Engine::Sqlite {
        return Some(Target {
            engine,
            dev: None,
            file: Some(nine_sqlite(dir)),
        });
    }
    let dev = dev_entry(engine.id());
    let addr = format!("{}:{}", dev.host, dev.port);
    let reachable = addr
        .parse()
        .ok()
        .and_then(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_secs(2)).ok())
        .is_some();
    if !reachable {
        assert!(
            !required(engine.id()),
            "{} is required but unreachable at {addr}",
            engine.id()
        );
        #[allow(clippy::print_stderr)] // Test output: why the test did nothing.
        {
            eprintln!(
                "skipped: {} unreachable at {addr} (python3 dev/ready.py up core)",
                engine.id()
            );
        }
        return None;
    }
    Some(Target {
        engine,
        dev: Some(dev),
        file: None,
    })
}

/// The "nine" dataset in a new SQLite file under `dir` (`dev/seed/sqlite/nine.sql`, with
/// `big_events` cut to 1 000 rows: these tests do not need a million rows there).
fn nine_sqlite(dir: &Path) -> PathBuf {
    let seed = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dev/seed/sqlite/nine.sql");
    let seed = std::fs::read_to_string(seed).unwrap_or_else(|e| panic!("{e}"));
    let seed = seed.replace("i < 1000000", "i < 1000");
    let file = dir.join("nine.db");
    let c = rusqlite::Connection::open(&file).unwrap_or_else(|e| panic!("{e}"));
    c.execute_batch(&seed).unwrap_or_else(|e| panic!("{e}"));
    drop(c);
    file
}

/// A dev engine or a SQLite file.
#[derive(Debug, Clone)]
pub(super) struct Target {
    pub engine: Engine,
    pub dev: Option<Dev>,
    pub file: Option<PathBuf>,
}

impl Target {
    fn params(&self) -> ConnectParams {
        let engine = api::EngineId::new(self.engine.id());
        if let Some(file) = &self.file {
            return ConnectParams::new(engine, api::Endpoint::file(file));
        }
        let Some(dev) = &self.dev else {
            panic!("a dev engine");
        };
        let mut p = ConnectParams::new(engine, api::Endpoint::tcp(&dev.host, dev.port));
        p.auth = api::Auth::password(&dev.user, Some(api::Secret::new(dev.password.clone())));
        p.database = Some(dev.database.clone());
        p.tls.mode = api::TlsMode::Disable;
        p.application_name = "ninesql-ui-engine-tests".into();
        p.connect_timeout = Duration::from_secs(30);
        p
    }
}

/// A scratch table name unique per test and per run (tests run in parallel, and so may two
/// runs on one machine).
pub(super) fn scratch_name(what: &str) -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("ui_e2e_{what}_{}", &id[..12])
}

// ---------------------------------------------------------------------------------------------
// A separate driver session (setup, read-backs, cleanup), never the UI's.

/// A driver session of its own, on its own Tokio runtime: what the test reads back does not go
/// through the UI or the core.
pub(super) struct Admin {
    rt: tokio::runtime::Runtime,
    session: Option<Box<dyn Session>>,
    engine: Engine,
    dropped: Vec<String>,
}

impl Admin {
    pub(super) fn connect(target: &Target) -> Self {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|e| panic!("{e}"));
        let driver = target.engine.driver();
        let params = target.params();
        let session = rt
            .block_on(driver.connect(params))
            .unwrap_or_else(|e| panic!("admin connect: {e}"));
        let mut admin = Self {
            rt,
            session: Some(session),
            engine: target.engine,
            dropped: Vec::new(),
        };
        // A cleanup never waits long on a lock a broken test left behind.
        match target.engine {
            Engine::Postgresql => admin.ok("SET lock_timeout = '10s'"),
            Engine::Mysql | Engine::Mariadb => admin.ok("SET SESSION lock_wait_timeout = 10"),
            Engine::Sqlite => {}
        }
        admin
    }

    fn rows(&mut self, sql: &str) -> Result<Vec<api::arrow_array::RecordBatch>, DriverError> {
        let mut session = self
            .session
            .take()
            .unwrap_or_else(|| panic!("admin session in use"));
        let sql = sql.to_owned();
        let out = self.rt.block_on(async {
            let mut stream = session
                .execute(Statement::sql(sql), ExecOptions::default())
                .await?;
            let mut batches = Vec::new();
            while let Some(item) = stream.next().await {
                if let ResultItem::Rows(b) = item? {
                    batches.push(b);
                }
            }
            Ok(batches)
        });
        self.session = Some(session);
        out
    }

    /// Runs `sql`, which must succeed.
    pub(super) fn ok(&mut self, sql: &str) {
        if let Err(e) = self.rows(sql) {
            panic!("{sql}: {e}");
        }
    }

    /// The first column of the first row, if any.
    pub(super) fn scalar(&mut self, sql: &str) -> Option<Param> {
        let batches = self.rows(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let b = batches.into_iter().find(|b| b.num_rows() > 0)?;
        Some(cell_value(&b, 0, 0).unwrap_or_else(|e| panic!("{e}")))
    }

    /// `SELECT count(*)` of `table`.
    pub(super) fn count(&mut self, table: &str) -> i64 {
        match self.scalar(&format!("SELECT count(*) FROM {table}")) {
            Some(Param::Int(n)) => n,
            Some(Param::Decimal(t) | Param::Text(t)) => t.parse().unwrap_or(-1),
            other => panic!("count of {table}: {other:?}"),
        }
    }

    /// The text of `label` in the row `id` of `table`.
    pub(super) fn label(&mut self, table: &str, id: &str) -> Option<String> {
        match self.scalar(&format!("SELECT label FROM {table} WHERE id = {id}")) {
            Some(Param::Text(t)) => Some(t),
            Some(other) => panic!("label: {other:?}"),
            None => None,
        }
    }

    /// Creates a scratch table with `rows` rows (dropped when this session is dropped).
    pub(super) fn scratch_table(&mut self, what: &str, rows: u32) -> String {
        let table = scratch_name(what);
        let create = self.engine.create_table(&table);
        self.ok(&create);
        self.dropped.push(table.clone());
        if rows > 0 {
            let fill = self.engine.fill_table(&table, rows);
            self.ok(&fill);
        }
        table
    }
}

impl Drop for Admin {
    fn drop(&mut self) {
        for table in std::mem::take(&mut self.dropped) {
            if let Err(e) = self.rows(&format!("DROP TABLE IF EXISTS {table}")) {
                #[allow(clippy::print_stderr)] // Test output: a leftover table to remove by hand.
                {
                    eprintln!("could not drop the scratch table {table}: {e}");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The application.

/// A window on the shipped drivers with temporary XDG directories and an in-memory secret store.
pub(super) struct App {
    pub target: Target,
    pub hub: Entity<ConnectionHub>,
    pub ws: Entity<Workspace>,
    pub nav: Entity<Navigator>,
    pub vcx: VisualTestContext,
    pub root: PathBuf,
    /// "Load more" rows [`Self::reveal`] activated so far.
    pub load_more_clicks: usize,
}

fn xdg(base: &Path) -> XdgDirs {
    let base = base.to_path_buf();
    XdgDirs::from_lookup(|k| {
        let dir = match k {
            "XDG_CONFIG_HOME" => "config",
            "XDG_DATA_HOME" => "data",
            "XDG_STATE_HOME" => "state",
            "XDG_CACHE_HOME" => "cache",
            "HOME" => "home",
            _ => return None,
        };
        Some(base.join(dir).into_os_string())
    })
    .unwrap_or_else(|e| panic!("{e}"))
}

/// Runs the foreground until `done`, letting the driver runtime's threads work in between;
/// panics with `what` after [`STEP_TIMEOUT`].
pub(super) fn wait(
    vcx: &mut VisualTestContext,
    what: &str,
    mut done: impl FnMut(&mut VisualTestContext) -> bool,
) {
    vcx.executor().allow_parking();
    let deadline = Instant::now() + STEP_TIMEOUT;
    loop {
        vcx.run_until_parked();
        if done(vcx) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

impl App {
    /// Starts the application for `target`, its storage under `root`.
    pub(super) fn start(cx: &mut TestAppContext, target: Target, root: &Path) -> Self {
        Self::start_with(cx, target, root, ninesql_core::MetadataConfig::default())
    }

    /// [`Self::start`] with the metadata service's limits of the caller's (smaller pages).
    pub(super) fn start_with(
        cx: &mut TestAppContext,
        target: Target,
        root: &Path,
        metadata: ninesql_core::MetadataConfig,
    ) -> Self {
        let paths = Paths::new(&xdg(root));
        let store = Arc::new(MemoryStore::new(StoreKind::Vault));
        let mut stores = SecretStores::new();
        stores.register(store);
        let registry = registry_with(vec![
            Arc::new(ninesql_driver_pgwire::PgWireDriver::new()),
            Arc::new(ninesql_driver_mysqlwire::MySqlWireDriver::new()),
            Arc::new(ninesql_driver_sqlite::SqliteDriver::new()),
        ]);
        let mut manager = ManagerConfig::with_known_hosts(KnownHostsFiles {
            own: paths.known_hosts_file(),
            user: None,
        });
        manager.workers = 2;
        let manager = SessionManager::new(manager, registry.clone(), stores.clone())
            .unwrap_or_else(|e| panic!("{e}"));
        let workspace = CoreWorkspace::open(paths.clone(), ProjectName::default_project())
            .unwrap_or_else(|e| panic!("{e}"));
        let mut config = HubConfig::empty();
        config.registry = registry;
        config.transfer = Some(ninesql_core::TransferService::new(manager.clone(), &paths));
        config.manager = Some(manager);
        config.workspace = Some(workspace);
        config.stores = stores;
        config.metadata = metadata;
        let (settings, diagnostics) = settings::parse("");
        let hub = cx.update(|cx| {
            crate::app::init(
                SettingsState {
                    path: None,
                    settings,
                    diagnostics,
                    save_blocked: None,
                },
                cx,
            );
            install(config, cx)
        });
        let Some((window, ws)) = cx.update(crate::app::open_window) else {
            panic!("the test platform opens windows");
        };
        let vcx = VisualTestContext::from_window(window, cx);
        vcx.executor().allow_parking();
        vcx.run_until_parked();
        let nav = ws.read_with(&vcx, |ws, _| ws.navigator().clone());
        Self {
            target,
            hub,
            ws,
            nav,
            vcx,
            root: root.to_path_buf(),
            load_more_clicks: 0,
        }
    }

    /// Dispatches a registered action as the palette does.
    pub(super) fn perform(&mut self, action: &dyn Action) {
        let action = action.boxed_clone();
        self.ws.update_in(&mut self.vcx, |ws, window, cx| {
            assert!(
                ws.perform(action.as_ref(), window, cx),
                "{} is handled",
                action.name()
            );
        });
        self.vcx.run_until_parked();
    }

    /// "New connection" (`ctrl-alt-n`), the dialog filled for the target, "Test connection",
    /// then "Save". Returns the saved connection.
    pub(super) fn create_connection(
        &mut self,
        name: &str,
        ty: ConnectionTypeId,
        read_only: bool,
    ) -> ConnectionId {
        self.perform(&crate::actions::NewConnection);
        let Some(dialog) = self
            .ws
            .read_with(&self.vcx, |ws, _| ws.connection_dialog().cloned())
        else {
            panic!("the connection dialog");
        };
        let engine = self.target.engine.id();
        let offered = self
            .hub
            .read_with(&self.vcx, |h, _| h.offer(engine).cloned());
        let Some(offer) = offered else {
            panic!("{engine} is offered");
        };
        dialog.update(&mut self.vcx, |d, cx| d.set_engine(engine, cx));
        let mut fields: Vec<(&str, String)> = vec![("name", name.to_owned())];
        if let Some(file) = &self.target.file {
            fields.push(("file", file.to_string_lossy().into_owned()));
        }
        let password = self.target.dev.as_ref().map(|d| d.password.clone());
        if let Some(dev) = &self.target.dev {
            fields.push(("host", dev.host.clone()));
            fields.push(("port", dev.port.to_string()));
            fields.push(("database", dev.database.clone()));
            fields.push(("username", dev.user.clone()));
        }
        for (field, value) in fields {
            let input = dialog.read_with(&self.vcx, |d, _| d.input(field).cloned());
            let Some(input) = input else {
                panic!("no input {field}");
            };
            self.vcx
                .update(|window, cx| input.update(cx, |s, cx| s.set_value(value, window, cx)));
        }
        let auth = if password.is_some() {
            AuthChoice::Password
        } else {
            AuthChoice::None
        };
        dialog.update_in(&mut self.vcx, |d, window, cx| d.set_auth(auth, window, cx));
        dialog.update(&mut self.vcx, |d, _| {
            let f = d.form_mut();
            f.connection_type = Some(ty);
            f.read_only = Some(read_only);
            if offer.offers_tls() {
                // The dev engines' TLS uses a throwaway CA; plaintext is the visible,
                // per-connection choice (I5), on loopback only.
                f.tls_mode = TlsMode::Disable;
            }
            if password.is_some() {
                f.auth_secret.mode = SecretMode::Save;
            }
        });
        let type_secret = |app: &mut Self| {
            if let Some(password) = &password {
                let input = dialog.read_with(&app.vcx, |d, _| d.input("secret").cloned());
                let Some(input) = input else {
                    panic!("the password input");
                };
                let password = password.clone();
                app.vcx.update(|window, cx| {
                    input.update(cx, |s, cx| s.set_value(password, window, cx));
                });
            }
        };
        type_secret(self);
        // "Test connection" opens and closes a session with what the dialog holds.
        dialog.update_in(&mut self.vcx, |d, window, cx| d.test_connection(window, cx));
        assert_eq!(
            dialog.read_with(&self.vcx, |d, _| d.error().map(str::to_owned)),
            None
        );
        let d2 = dialog.clone();
        wait(&mut self.vcx, "Test connection", |vcx| {
            d2.read_with(vcx, |d, _| matches!(d.test_state(), TestState::Done(_)))
        });
        let tested = dialog.read_with(&self.vcx, |d, _| d.test_state().clone());
        assert!(
            matches!(
                tested,
                TestState::Done(crate::connections::connect_flow::FlowResult::TestOk { .. })
            ),
            "{tested:?}"
        );
        type_secret(self);
        dialog.update_in(&mut self.vcx, |d, window, cx| d.save(window, cx));
        let hub = self.hub.clone();
        let wanted = name.to_owned();
        wait(&mut self.vcx, "the connection is saved", |vcx| {
            hub.read_with(vcx, |h, _| h.connections().iter().any(|c| c.name == wanted))
        });
        assert_eq!(
            dialog.read_with(&self.vcx, |d, _| d.error().map(str::to_owned)),
            None
        );
        let id = self.hub.read_with(&self.vcx, |h, _| {
            h.connections()
                .iter()
                .find(|c| c.name == name)
                .map(|c| c.id)
        });
        id.unwrap_or_else(|| panic!("saved"))
    }

    /// Selects the connection in the navigator and runs "Connect" on it; waits until the
    /// active editor runs on the new session.
    pub(super) fn connect(&mut self, id: ConnectionId) -> Entity<SqlEditorTab> {
        let tab = self.connect_unsettled(id);
        self.settle(id, &tab);
        tab
    }

    /// [`Self::connect`] without waiting for the editor's completion warm-up.
    pub(super) fn connect_unsettled(&mut self, id: ConnectionId) -> Entity<SqlEditorTab> {
        self.nav
            .update(&mut self.vcx, |n, cx| n.select_connection(id, cx));
        self.perform(&crate::actions::ConnectConnection);
        let hub = self.hub.clone();
        wait(&mut self.vcx, "the session opens", |vcx| {
            hub.read_with(vcx, |h, _| h.is_open(id))
        });
        let tab = self
            .ws
            .read_with(&self.vcx, |ws, _| ws.active_editor().cloned());
        let Some(tab) = tab else {
            panic!("an editor tab");
        };
        let t = tab.clone();
        wait(&mut self.vcx, "the editor takes the session", |vcx| {
            t.read_with(vcx, |t, _| t.connection() == Some(id))
        });
        tab
    }

    /// Waits until the editor's completion warm-up after the connect has ended (its default
    /// schema is known and no metadata load is in flight), as a user who looks at the window
    /// before clicking would; the steps then start from a quiet session. Browsing without it
    /// is covered by `navigator_right_after_connect_loads_without_a_busy_session`.
    pub(super) fn settle(&mut self, id: ConnectionId, tab: &Entity<SqlEditorTab>) {
        let t = tab.clone();
        wait(&mut self.vcx, "the editor's default schema", |vcx| {
            t.read_with(vcx, |t, _| t.default_schema().is_some())
        });
        let metadata = self.hub.read_with(&self.vcx, |h, _| h.metadata());
        let Some(metadata) = metadata else {
            panic!("a metadata service");
        };
        let mut quiet = 0;
        wait(&mut self.vcx, "no metadata load in flight", |_| {
            if metadata.cache_stats(id).is_none_or(|s| s.in_flight == 0) {
                quiet += 1;
            } else {
                quiet = 0;
            }
            quiet >= 10
        });
    }

    /// The navigator's row labels.
    pub(super) fn rows(&self) -> Vec<String> {
        self.nav
            .read_with(&self.vcx, |n, cx| n.rows(cx))
            .into_iter()
            .map(|(_, l)| l)
            .collect()
    }

    /// Expands the navigator along `labels` (each looked for below the previous one; a row
    /// already expanded is left so) and selects the last one. Returns its index. A label that is
    /// not among the loaded children of its parent is looked for behind that parent's "Load
    /// more" row, which is activated as a user would ([`Self::load_more_clicks`] counts them).
    pub(super) fn reveal(&mut self, labels: &[&str]) -> usize {
        let mut after = 0;
        let mut parent_depth: Option<usize> = None;
        let mut found = 0;
        for (i, label) in labels.iter().enumerate() {
            let nav = self.nav.clone();
            let label = (*label).to_owned();
            let started = Instant::now();
            let at = loop {
                self.vcx.run_until_parked();
                // The rows below the parent (its subtree), from `after` on.
                let (hit, load_more) = nav.read_with(&self.vcx, |n, cx| {
                    let rows = n.rows(cx);
                    let below = rows
                        .iter()
                        .enumerate()
                        .skip(after)
                        .take_while(|(_, (d, _))| parent_depth.is_none_or(|p| *d > p));
                    let mut hit = None;
                    let mut load_more = None;
                    for (ix, (d, l)) in below {
                        if *l == label && hit.is_none() {
                            hit = Some(ix);
                        }
                        if l == "Load more"
                            && load_more.is_none()
                            && parent_depth.is_none_or(|p| *d <= p + 2)
                        {
                            load_more = Some(ix);
                        }
                    }
                    (hit, load_more)
                });
                if let Some(ix) = hit {
                    break ix;
                }
                if let Some(ix) = load_more {
                    nav.update(&mut self.vcx, |n, cx| n.activate(ix, cx));
                    self.load_more_clicks += 1;
                }
                assert!(
                    started.elapsed() < STEP_TIMEOUT,
                    "timed out: navigator row {label:?} in {:?}",
                    self.rows()
                );
                std::thread::sleep(Duration::from_millis(10));
            };
            found = at;
            after = at + 1;
            parent_depth =
                Some(nav.read_with(&self.vcx, |n, cx| n.rows(cx).get(at).map_or(0, |(d, _)| *d)));
            if i + 1 < labels.len() {
                let expand = nav.read_with(&self.vcx, |n, _| {
                    n.tree().can_expand(at) && !n.tree().is_expanded(at)
                });
                if expand {
                    nav.update(&mut self.vcx, |n, cx| n.expand(at, cx));
                }
            }
        }
        self.nav
            .update(&mut self.vcx, |n, cx| n.select_index(Some(found), cx));
        found
    }

    /// Closes the connection's sessions and waits until they are gone.
    pub(super) fn disconnect(&mut self, id: ConnectionId) {
        self.hub.update(&mut self.vcx, |h, cx| h.disconnect(id, cx));
        let hub = self.hub.clone();
        wait(&mut self.vcx, "the sessions close", |vcx| {
            hub.read_with(vcx, |h, _| !h.is_open(id))
        });
    }

    /// I1: no file under the test's directories holds any password of `dev/engines.json`.
    pub(super) fn assert_no_password_on_disk(&self) {
        let passwords = dev_passwords();
        let mut seen = 0;
        for file in walk(&self.root) {
            seen += 1;
            let bytes = std::fs::read(&file).unwrap_or_default();
            for (n, password) in passwords.iter().enumerate() {
                // The value itself is never printed, only its rank in the list.
                assert!(
                    !bytes
                        .windows(password.len())
                        .any(|w| w == password.as_bytes()),
                    "I1: dev password #{n} of dev/engines.json is in {}",
                    file.display()
                );
            }
        }
        assert!(seen > 0, "the scan found the workspace files");
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}
