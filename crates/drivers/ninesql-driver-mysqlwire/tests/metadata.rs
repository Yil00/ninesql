//! Navigator metadata and DDL generation (P4-A, F-NAV-01, F-NAV-02, F-NAV-07) against the
//! `mysql` and `mariadb` services of `dev/compose.yaml`. Every test runs on each engine that
//! is reachable.
//!
//! An engine is skipped when unreachable, unless `NINESQL_REQUIRE_ENGINES` lists it. Start them
//! with `python3 dev/ready.py up core`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::path::PathBuf;
use std::time::Duration;

use ninesql_driver_api::{
    Auth, ConnectParams, ConstraintKind, Driver, DriverError, Endpoint, EngineId, ExecOptions,
    Metadata, ObjectDetails, ObjectInfo, ObjectKind, ObjectPath, PageRequest, ReferentialAction,
    RoutineKind, Secret, Session, Statement, TlsMode, conformance,
};
use ninesql_driver_mysqlwire::MySqlWireDriver;
use tokio::sync::Mutex;

/// The tests share the `ninesql_meta_*` objects: one at a time.
static SERIAL: Mutex<()> = Mutex::const_new(());

struct Dev {
    engine: EngineId,
    host: String,
    port: u16,
    user: String,
    password: String,
    database: String,
}

fn dev_of(id: &str, engine: EngineId) -> Option<Dev> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../dev/engines.json");
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let e = doc["engines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == id)
        .unwrap();
    let dev = Dev {
        engine,
        host: e["host"].as_str().unwrap().to_owned(),
        port: u16::try_from(e["port"].as_u64().unwrap()).unwrap(),
        user: e["user"].as_str().unwrap().to_owned(),
        password: e["password"].as_str().unwrap().to_owned(),
        database: e["database"].as_str().unwrap().to_owned(),
    };
    let addr = format!("{}:{}", dev.host, dev.port);
    let reachable = addr
        .parse()
        .ok()
        .and_then(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_secs(2)).ok())
        .is_some();
    if reachable {
        return Some(dev);
    }
    let required = std::env::var("NINESQL_REQUIRE_ENGINES")
        .unwrap_or_default()
        .split(',')
        .any(|s| s.trim() == id);
    assert!(!required, "{id} is required but unreachable at {addr}");
    eprintln!("skipped: {id} unreachable at {addr} (python3 dev/ready.py up core)");
    None
}

fn devs() -> Vec<Dev> {
    [
        dev_of("mysql", EngineId::MYSQL),
        dev_of("mariadb", EngineId::MARIADB),
    ]
    .into_iter()
    .flatten()
    .collect()
}

async fn connect(dev: &Dev, read_only: bool) -> Box<dyn Session> {
    let mut p = ConnectParams::new(dev.engine.clone(), Endpoint::tcp(&dev.host, dev.port));
    p.auth = Auth::password(&dev.user, Some(Secret::new(dev.password.clone())));
    p.database = Some(dev.database.clone());
    p.tls.mode = TlsMode::Disable;
    p.application_name = "ninesql-mysqlwire-metadata-tests".into();
    p.read_only = read_only;
    match MySqlWireDriver::new().connect(p).await {
        Ok(s) => s,
        Err(e) => panic!("{}: connect failed: {e}", dev.engine),
    }
}

async fn exec(s: &mut dyn Session, sql: &str) -> Result<(), DriverError> {
    let mut stream = s
        .execute(Statement::sql(sql), ExecOptions::default())
        .await?;
    while let Some(item) = stream.next().await {
        item?;
    }
    Ok(())
}

async fn exec_all(s: &mut dyn Session, engine: &EngineId, sqls: &[&str]) {
    for sql in sqls {
        if let Err(e) = exec(s, sql).await {
            panic!("{engine}: {sql:?} failed: {e}");
        }
    }
}

const WEIRD: &str = "ninesql_meta_Weird `Name` é";

const DROP: &[&str] = &[
    "DROP VIEW IF EXISTS ninesql_meta_v",
    "DROP TABLE IF EXISTS `ninesql_meta_Weird ``Name`` é`",
    "DROP TABLE IF EXISTS ninesql_meta_events",
    "DROP TABLE IF EXISTS ninesql_meta_parent",
    "DROP PROCEDURE IF EXISTS ninesql_meta_same",
    "DROP FUNCTION IF EXISTS ninesql_meta_same",
    "DROP FUNCTION IF EXISTS ninesql_meta_f",
    "DROP PROCEDURE IF EXISTS ninesql_meta_p",
    "DROP EVENT IF EXISTS ninesql_meta_ev",
];

const CREATE: &[&str] = &[
    "CREATE TABLE ninesql_meta_parent (id INT PRIMARY KEY, code VARCHAR(20) NOT NULL, \
     CONSTRAINT ninesql_meta_code UNIQUE (code)) COMMENT 'parent table'",
    "CREATE TABLE `ninesql_meta_Weird ``Name`` é` ( \
       `select` INT NOT NULL AUTO_INCREMENT, \
       parent_id INT NOT NULL, \
       note VARCHAR(50) COMMENT 'a note', \
       PRIMARY KEY (`select`), \
       KEY ninesql_meta_note_idx (note, parent_id), \
       CONSTRAINT ninesql_meta_chk CHECK (note IS NULL OR CHAR_LENGTH(note) < 40), \
       CONSTRAINT ninesql_meta_fk FOREIGN KEY (parent_id) REFERENCES ninesql_meta_parent (id) \
         ON DELETE CASCADE ON UPDATE RESTRICT)",
    "CREATE TABLE ninesql_meta_events (id INT NOT NULL, y INT NOT NULL, PRIMARY KEY (id, y)) \
     PARTITION BY RANGE (y) (PARTITION p2025 VALUES LESS THAN (2026), \
     PARTITION p2026 VALUES LESS THAN (2027))",
    "CREATE VIEW ninesql_meta_v AS SELECT id, code FROM ninesql_meta_parent",
    "CREATE TRIGGER ninesql_meta_trg BEFORE INSERT ON ninesql_meta_parent FOR EACH ROW \
     SET NEW.code = UPPER(NEW.code)",
    "CREATE FUNCTION ninesql_meta_same(a INT) RETURNS INT DETERMINISTIC RETURN a + 1",
    "CREATE PROCEDURE ninesql_meta_same() SELECT 1",
    "CREATE FUNCTION ninesql_meta_f(a INT) RETURNS INT DETERMINISTIC COMMENT 'adds' RETURN a + 2",
    "CREATE PROCEDURE ninesql_meta_p(IN n INT) BEGIN SELECT n; SELECT n + 1; END",
    "CREATE EVENT ninesql_meta_ev ON SCHEDULE EVERY 1 DAY STARTS '2030-01-01 00:00:00' \
     DISABLE COMMENT 'nightly' DO DELETE FROM ninesql_meta_parent WHERE id < 0",
];

async fn fixture(dev: &Dev, s: &mut dyn Session) {
    exec_all(s, &dev.engine, DROP).await;
    if dev.engine == EngineId::MARIADB {
        exec_all(
            s,
            &dev.engine,
            &["DROP SEQUENCE IF EXISTS ninesql_meta_seq"],
        )
        .await;
    }
    exec_all(s, &dev.engine, CREATE).await;
    if dev.engine == EngineId::MARIADB {
        exec_all(
            s,
            &dev.engine,
            &["CREATE SEQUENCE ninesql_meta_seq START WITH 10 INCREMENT BY 5"],
        )
        .await;
    } else {
        // Functional key part (MySQL 8.0.13+).
        exec_all(
            s,
            &dev.engine,
            &["CREATE INDEX ninesql_meta_expr_idx ON `ninesql_meta_Weird ``Name`` é` ((LOWER(note)))"],
        )
        .await;
    }
}

async fn all(
    m: &dyn Metadata,
    parent: &ObjectPath,
    kind: ObjectKind,
    filter: &str,
) -> Vec<ObjectInfo> {
    let mut out = Vec::new();
    let mut cursor = None;
    loop {
        let page = m
            .list(
                parent,
                kind,
                PageRequest::first()
                    .with_limit(2)
                    .with_cursor(cursor)
                    .with_filter((!filter.is_empty()).then(|| filter.to_owned())),
            )
            .await
            .unwrap_or_else(|e| panic!("{kind:?} under {parent:?}: {e}"));
        out.extend(page.items);
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => return out,
        }
    }
}

fn names(items: &[ObjectInfo]) -> Vec<&str> {
    items.iter().map(ObjectInfo::name).collect()
}

fn one<'a>(items: &'a [ObjectInfo], name: &str) -> &'a ObjectInfo {
    items
        .iter()
        .find(|o| o.name() == name)
        .unwrap_or_else(|| panic!("{name} not in {items:?}"))
}

/// Phase 4 exit criterion: the "nine" dataset (with this file's fixture, which adds triggers,
/// routines, events, partitions and, on MariaDB, a sequence) browses fully on a read-only
/// session, every node and every declared DDL (`conformance::assert_read_only_browse`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nine_dataset_browses_fully_on_a_read_only_session() {
    let _serial = SERIAL.lock().await;
    for dev in devs() {
        let e = &dev.engine;
        let mut admin = connect(&dev, false).await;
        fixture(&dev, admin.as_mut()).await;
        let mut s = connect(&dev, true).await;
        let schema = ObjectPath::root().child(ObjectKind::Schema, &dev.database);
        let r = conformance::assert_read_only_browse(s.as_ref(), schema, &|_| false).await;
        assert!(r.count(ObjectKind::Table) >= 9, "{e}: {r:?}");
        assert!(r.count(ObjectKind::View) >= 2, "{e}: {r:?}");
        assert!(r.count(ObjectKind::Column) >= 30, "{e}: {r:?}");
        assert!(r.count(ObjectKind::ForeignKey) >= 3, "{e}: {r:?}");
        assert!(r.count(ObjectKind::Index) >= 4, "{e}: {r:?}");
        assert!(r.count(ObjectKind::Constraint) >= 6, "{e}: {r:?}");
        assert_eq!(r.count(ObjectKind::Partition), 2, "{e}: {r:?}");
        assert!(r.count(ObjectKind::Trigger) >= 1, "{e}: {r:?}");
        assert!(r.count(ObjectKind::Routine) >= 4, "{e}: {r:?}");
        assert!(r.count(ObjectKind::Event) >= 1, "{e}: {r:?}");
        if *e == EngineId::MARIADB {
            assert!(r.count(ObjectKind::Sequence) >= 1, "{e}: {r:?}");
        }
        // Writes are still refused after the browse.
        assert!(matches!(
            exec(s.as_mut(), "CREATE TABLE ninesql_p4_browse (a INT)").await,
            Err(DriverError::ReadOnly(_))
        ));
        s.close().await.unwrap();
        admin.close().await.unwrap();
    }
}

/// Every declared kind lists the expected objects, on a read-only session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_declared_kind_lists_its_objects() {
    let _serial = SERIAL.lock().await;
    for dev in devs() {
        let e = &dev.engine;
        let mut admin = connect(&dev, false).await;
        fixture(&dev, admin.as_mut()).await;
        let s = connect(&dev, true).await;
        assert!(s.is_read_only());
        let caps = s.capabilities().metadata.clone();
        let m = s.metadata();
        for parent in [
            None,
            Some(ObjectKind::Schema),
            Some(ObjectKind::Table),
            Some(ObjectKind::View),
        ] {
            for k in m.child_kinds(parent) {
                assert!(
                    caps.kinds.contains(&k),
                    "{e}: {k:?} listed but not declared"
                );
            }
        }
        let sch = ObjectPath::root().child(ObjectKind::Schema, &dev.database);
        let tables = all(m.as_ref(), &sch, ObjectKind::Table, "ninesql_meta_").await;
        assert_eq!(
            names(&tables),
            vec![WEIRD, "ninesql_meta_events", "ninesql_meta_parent"],
            "{e}"
        );
        assert_eq!(
            one(&tables, "ninesql_meta_parent").comment.as_deref(),
            Some("parent table")
        );
        match &one(&tables, "ninesql_meta_events").details {
            ObjectDetails::Table(t) => {
                assert_eq!(t.flavour.as_deref(), Some("partitioned"), "{e}");
                assert_eq!(t.partition_key.as_deref(), Some("RANGE (`y`)"), "{e}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            names(&all(m.as_ref(), &sch, ObjectKind::View, "ninesql_meta_").await),
            vec!["ninesql_meta_v"]
        );
        let routines = all(m.as_ref(), &sch, ObjectKind::Routine, "ninesql_meta_").await;
        assert_eq!(
            names(&routines),
            vec![
                "ninesql_meta_f",
                "ninesql_meta_p",
                "ninesql_meta_same",
                "ninesql_meta_same"
            ],
            "{e}"
        );
        match &one(&routines, "ninesql_meta_f").details {
            ObjectDetails::Routine(r) => {
                assert_eq!(r.kind, RoutineKind::Function);
                // `int` on MySQL, `int(11)` on MariaDB.
                assert!(
                    r.return_type.as_deref().unwrap().starts_with("int"),
                    "{e}: {r:?}"
                );
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            one(&routines, "ninesql_meta_f").comment.as_deref(),
            Some("adds")
        );
        let events = all(m.as_ref(), &sch, ObjectKind::Event, "ninesql_meta_").await;
        assert_eq!(names(&events), vec!["ninesql_meta_ev"]);
        assert_eq!(events[0].comment.as_deref(), Some("nightly"));
        if *e == EngineId::MARIADB {
            assert_eq!(
                names(&all(m.as_ref(), &sch, ObjectKind::Sequence, "ninesql_meta_").await),
                vec!["ninesql_meta_seq"]
            );
            // Sequences are not tables.
            assert!(!names(&tables).contains(&"ninesql_meta_seq"));
        } else {
            assert!(matches!(
                m.list(&sch, ObjectKind::Sequence, PageRequest::first())
                    .await,
                Err(DriverError::Unsupported(_))
            ));
        }

        let weird = sch.child(ObjectKind::Table, WEIRD);
        let cols = all(m.as_ref(), &weird, ObjectKind::Column, "").await;
        assert_eq!(names(&cols), vec!["select", "parent_id", "note"]);
        assert_eq!(one(&cols, "note").comment.as_deref(), Some("a note"));
        match &one(&cols, "select").details {
            ObjectDetails::Column(c) => assert!(c.primary_key && c.auto_generated && !c.nullable),
            other => panic!("{other:?}"),
        }
        let cons = all(m.as_ref(), &weird, ObjectKind::Constraint, "").await;
        assert_eq!(names(&cons), vec!["PRIMARY", "ninesql_meta_chk"], "{e}");
        match &one(&cons, "PRIMARY").details {
            ObjectDetails::Constraint(c) => {
                assert_eq!(c.kind, ConstraintKind::PrimaryKey);
                assert_eq!(c.columns, vec!["select"]);
            }
            other => panic!("{other:?}"),
        }
        match &one(&cons, "ninesql_meta_chk").details {
            ObjectDetails::Constraint(c) => {
                assert_eq!(c.kind, ConstraintKind::Check);
                assert!(
                    c.definition
                        .as_deref()
                        .unwrap()
                        .to_lowercase()
                        .contains("char_length"),
                    "{e}: {c:?}"
                );
            }
            other => panic!("{other:?}"),
        }
        let parent = sch.child(ObjectKind::Table, "ninesql_meta_parent");
        let pcons = all(m.as_ref(), &parent, ObjectKind::Constraint, "").await;
        match &one(&pcons, "ninesql_meta_code").details {
            ObjectDetails::Constraint(c) => {
                assert_eq!(c.kind, ConstraintKind::Unique);
                assert_eq!(c.columns, vec!["code"]);
            }
            other => panic!("{other:?}"),
        }
        let fks = all(m.as_ref(), &weird, ObjectKind::ForeignKey, "").await;
        assert_eq!(names(&fks), vec!["ninesql_meta_fk"]);
        match &fks[0].details {
            ObjectDetails::ForeignKey(f) => {
                assert_eq!(f.columns, vec!["parent_id"]);
                assert_eq!(f.referenced_table, parent);
                assert_eq!(f.referenced_columns, vec!["id"]);
                assert_eq!(f.on_delete, Some(ReferentialAction::Cascade));
                assert_eq!(f.on_update, Some(ReferentialAction::Restrict));
            }
            other => panic!("{other:?}"),
        }
        let idx = all(m.as_ref(), &weird, ObjectKind::Index, "").await;
        match &one(&idx, "ninesql_meta_note_idx").details {
            ObjectDetails::Index(i) => {
                assert!(!i.unique && !i.primary);
                assert_eq!(i.columns, vec!["note", "parent_id"]);
                assert_eq!(i.method.as_deref(), Some("BTREE"));
            }
            other => panic!("{other:?}"),
        }
        match &one(&idx, "PRIMARY").details {
            ObjectDetails::Index(i) => assert!(i.primary && i.unique),
            other => panic!("{other:?}"),
        }
        if *e == EngineId::MYSQL {
            match &one(&idx, "ninesql_meta_expr_idx").details {
                ObjectDetails::Index(i) => {
                    assert_eq!(i.columns.len(), 1);
                    assert!(i.columns[0].to_lowercase().contains("lower"), "{i:?}");
                }
                other => panic!("{other:?}"),
            }
        }
        let trg = all(m.as_ref(), &parent, ObjectKind::Trigger, "").await;
        match &one(&trg, "ninesql_meta_trg").details {
            ObjectDetails::Trigger(t) => {
                assert_eq!(t.timing.as_deref(), Some("BEFORE"));
                assert_eq!(t.events, vec!["INSERT"]);
                assert!(t.for_each_row && t.enabled);
            }
            other => panic!("{other:?}"),
        }
        let parts = all(
            m.as_ref(),
            &sch.child(ObjectKind::Table, "ninesql_meta_events"),
            ObjectKind::Partition,
            "",
        )
        .await;
        assert_eq!(names(&parts), vec!["p2025", "p2026"]);
        match &parts[1].details {
            ObjectDetails::Partition(p) => {
                assert_eq!(p.method.as_deref(), Some("RANGE"));
                assert_eq!(p.bound.as_deref(), Some("VALUES LESS THAN (2027)"));
                assert!(!p.partitioned);
            }
            other => panic!("{other:?}"),
        }
        // The "nine" dataset browses fully.
        let nine = all(m.as_ref(), &sch, ObjectKind::Table, "").await;
        for t in [
            "all_types",
            "big_events",
            "customers",
            "nine_meta",
            "orders",
            "products",
        ] {
            assert!(names(&nine).contains(&t), "{e}: {t} missing");
        }
        let orders = sch.child(ObjectKind::Table, "orders");
        let ofks = all(m.as_ref(), &orders, ObjectKind::ForeignKey, "").await;
        assert_eq!(
            names(&ofks),
            vec!["orders_customer_fk", "orders_product_fk"]
        );
        match &ofks[0].details {
            ObjectDetails::ForeignKey(f) => {
                assert_eq!(f.columns, vec!["customer_id"]);
                assert_eq!(
                    f.referenced_table,
                    sch.child(ObjectKind::Table, "customers")
                );
                assert_eq!(f.referenced_columns, vec!["id"]);
            }
            other => panic!("{other:?}"),
        }
        assert!(
            names(&all(m.as_ref(), &orders, ObjectKind::Index, "").await)
                .contains(&"orders_customer_idx")
        );
        assert!(
            names(&all(m.as_ref(), &sch, ObjectKind::View, "").await)
                .contains(&"customer_order_totals")
        );
        // Wrong parent kinds and missing tables.
        assert!(matches!(
            m.list(&sch, ObjectKind::Index, PageRequest::first()).await,
            Err(DriverError::Unsupported(_))
        ));
        assert!(
            m.list(
                &sch.child(ObjectKind::Table, "nope"),
                ObjectKind::Index,
                PageRequest::first()
            )
            .await
            .unwrap()
            .items
            .is_empty()
        );
        assert!(s.is_read_only());
        s.close().await.unwrap();
        exec_all(admin.as_mut(), e, DROP).await;
        admin.close().await.unwrap();
    }
}

/// `SHOW CREATE` output re-creates every object; regenerated DDL is identical.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_ddl_round_trips() {
    let _serial = SERIAL.lock().await;
    for dev in devs() {
        let e = &dev.engine;
        let mut admin = connect(&dev, false).await;
        fixture(&dev, admin.as_mut()).await;
        let s = connect(&dev, true).await;
        let m = s.metadata();
        let sch = ObjectPath::root().child(ObjectKind::Schema, &dev.database);
        let parent = sch.child(ObjectKind::Table, "ninesql_meta_parent");
        let mut objects = vec![
            parent.clone(),
            sch.child(ObjectKind::Table, WEIRD),
            sch.child(ObjectKind::Table, "ninesql_meta_events"),
            sch.child(ObjectKind::View, "ninesql_meta_v"),
            parent.child(ObjectKind::Trigger, "ninesql_meta_trg"),
            sch.child(ObjectKind::Routine, "ninesql_meta_f"),
            sch.child(ObjectKind::Routine, "ninesql_meta_p"),
            sch.child(ObjectKind::Event, "ninesql_meta_ev"),
        ];
        if *e == EngineId::MARIADB {
            objects.push(sch.child(ObjectKind::Sequence, "ninesql_meta_seq"));
        }
        let mut ddls = Vec::new();
        for o in &objects {
            ddls.push(
                m.ddl(o)
                    .await
                    .unwrap_or_else(|err| panic!("{e}: {o:?}: {err}")),
            );
        }
        assert!(
            ddls[1].starts_with("CREATE TABLE `ninesql_meta_Weird ``Name`` é` ("),
            "{e}: {}",
            ddls[1]
        );
        assert!(ddls[2].contains("PARTITION BY RANGE"), "{e}: {}", ddls[2]);
        // A function and a procedure sharing a name: both definitions.
        let same = m
            .ddl(&sch.child(ObjectKind::Routine, "ninesql_meta_same"))
            .await
            .unwrap();
        assert!(same.contains("FUNCTION `ninesql_meta_same`"), "{same}");
        assert!(same.contains("PROCEDURE `ninesql_meta_same`"), "{same}");
        let schema_ddl = m.ddl(&sch).await.unwrap();
        assert!(
            schema_ddl.starts_with("CREATE DATABASE `nine`"),
            "{schema_ddl}"
        );
        assert!(matches!(
            m.ddl(&parent.child(ObjectKind::Index, "PRIMARY")).await,
            Err(DriverError::Unsupported(_))
        ));
        assert!(matches!(
            m.ddl(&sch.child(ObjectKind::Table, "nope")).await,
            Err(DriverError::Query { .. })
        ));

        exec_all(admin.as_mut(), e, DROP).await;
        if *e == EngineId::MARIADB {
            exec_all(admin.as_mut(), e, &["DROP SEQUENCE ninesql_meta_seq"]).await;
        }
        for d in &ddls {
            exec(admin.as_mut(), d)
                .await
                .unwrap_or_else(|err| panic!("{e}: {d}: {err}"));
        }
        for (o, before) in objects.iter().zip(&ddls) {
            let after = m.ddl(o).await.unwrap();
            assert_eq!(&after, before, "{e}: {o:?}");
        }
        // The two routines sharing a name come back as one runnable script (L2).
        assert!(same.starts_with("DELIMITER "), "{same}");
        let items = admin.dialect().split_script(&same);
        for item in &items {
            if item.kind == ninesql_driver_api::ScriptItemKind::Statement {
                exec(admin.as_mut(), item.text(&same))
                    .await
                    .unwrap_or_else(|err| panic!("{e}: {err}"));
            }
        }
        assert_eq!(
            m.ddl(&sch.child(ObjectKind::Routine, "ninesql_meta_same"))
                .await
                .unwrap(),
            same
        );
        s.close().await.unwrap();
        exec_all(admin.as_mut(), e, DROP).await;
        if *e == EngineId::MARIADB {
            exec_all(
                admin.as_mut(),
                e,
                &["DROP SEQUENCE IF EXISTS ninesql_meta_seq"],
            )
            .await;
        }
        admin.close().await.unwrap();
    }
}

/// A listing larger than a page comes back in keyset pages; keyed kinds clamp their page.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_listings_page() {
    let _serial = SERIAL.lock().await;
    for dev in devs() {
        let e = &dev.engine;
        let mut admin = connect(&dev, false).await;
        for i in 1..=120 {
            exec(
                admin.as_mut(),
                &format!("CREATE TABLE IF NOT EXISTS ninesql_meta_big_{i:03} (id INT)"),
            )
            .await
            .unwrap();
        }
        let s = connect(&dev, true).await;
        let m = s.metadata();
        let sch = ObjectPath::root().child(ObjectKind::Schema, &dev.database);
        let mut seen = Vec::new();
        let mut cursor = None;
        let mut pages = 0;
        loop {
            let p = m
                .list(
                    &sch,
                    ObjectKind::Table,
                    PageRequest::first()
                        .with_limit(50)
                        .with_cursor(cursor)
                        .with_filter(Some("ninesql_meta_big_".into())),
                )
                .await
                .unwrap();
            assert!(p.items.len() <= 50);
            pages += 1;
            seen.extend(p.items.into_iter().map(|o| o.name().to_owned()));
            match p.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        assert_eq!((pages, seen.len()), (3, 120), "{e}");
        assert_eq!(seen[0], "ninesql_meta_big_001");
        assert_eq!(seen[119], "ninesql_meta_big_120");
        // Keyed kinds (constraints, foreign keys, indexes) never return more than 500.
        let p = m
            .list(
                &sch.child(ObjectKind::Table, "orders"),
                ObjectKind::Index,
                PageRequest::first().with_limit(u32::MAX),
            )
            .await
            .unwrap();
        assert!(p.items.len() <= 500 && p.next_cursor.is_none());
        s.close().await.unwrap();
        for i in 1..=120 {
            exec(
                admin.as_mut(),
                &format!("DROP TABLE ninesql_meta_big_{i:03}"),
            )
            .await
            .unwrap();
        }
        admin.close().await.unwrap();
    }
}

/// Security review L3: a listing past the internal 32 MiB cap fails without closing the
/// session; a smaller page works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_listing_fails_without_closing_the_session() {
    let _serial = SERIAL.lock().await;
    for dev in devs() {
        let e = &dev.engine;
        let mut admin = connect(&dev, false).await;
        let comment = "c".repeat(60_000);
        for i in 0..600 {
            exec(
                admin.as_mut(),
                &format!(
                    "CREATE FUNCTION IF NOT EXISTS ninesql_meta_huge_{i:03}() RETURNS INT \
                     DETERMINISTIC COMMENT '{comment}' RETURN 1"
                ),
            )
            .await
            .unwrap_or_else(|err| panic!("{e}: {err}"));
        }
        let s = connect(&dev, false).await;
        let m = s.metadata();
        let sch = ObjectPath::root().child(ObjectKind::Schema, &dev.database);
        let page = PageRequest::first()
            .with_limit(600)
            .with_filter(Some("ninesql_meta_huge_".into()));
        match m.list(&sch, ObjectKind::Routine, page).await {
            Err(DriverError::Query { message, .. }) => assert!(message.contains("32 MiB")),
            other => panic!("{e}: {:?}", other.map(|p| p.items.len())),
        }
        let small = m
            .list(
                &sch,
                ObjectKind::Routine,
                PageRequest::first()
                    .with_limit(100)
                    .with_filter(Some("ninesql_meta_huge_".into())),
            )
            .await
            .unwrap();
        assert_eq!(small.items.len(), 100);
        s.close().await.unwrap();
        for i in 0..600 {
            exec(
                admin.as_mut(),
                &format!("DROP FUNCTION IF EXISTS ninesql_meta_huge_{i:03}"),
            )
            .await
            .unwrap();
        }
        admin.close().await.unwrap();
    }
}

/// The first column of the first row of `sql`, as text.
async fn scalar(s: &mut dyn Session, sql: &str) -> Option<String> {
    use ninesql_driver_api::ResultItem;
    use ninesql_driver_api::arrow_array::Array;
    use ninesql_driver_api::arrow_array::cast::AsArray;
    let mut stream = s
        .execute(Statement::sql(sql), ExecOptions::default())
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let mut out = None;
    while let Some(item) = stream.next().await {
        if let ResultItem::Rows(b) = item.unwrap_or_else(|e| panic!("{sql}: {e}"))
            && out.is_none()
            && b.num_rows() > 0
            && !b.column(0).is_null(0)
        {
            out = Some(b.column(0).as_string::<i32>().value(0).to_owned());
        }
    }
    out
}

const TXN_TABLE: &str = "ninesql_meta_txn";

/// Every kind of navigator call on `TXN_TABLE`; after each one, `in_transaction()` must still
/// be `expected`.
async fn browse_txn_table(s: &dyn Session, dev: &Dev, expected: bool, case: &str) {
    let e = &dev.engine;
    let m = s.metadata();
    let check = |what: &str| {
        assert_eq!(
            s.in_transaction(),
            expected,
            "{e} {case}: in_transaction after {what}"
        );
    };
    let schemas = m
        .list(
            &ObjectPath::root(),
            ObjectKind::Schema,
            PageRequest::first(),
        )
        .await
        .unwrap();
    assert!(names(&schemas.items).contains(&dev.database.as_str()));
    check("listing schemas");
    let sch = ObjectPath::root().child(ObjectKind::Schema, &dev.database);
    let tables = m
        .list(
            &sch,
            ObjectKind::Table,
            PageRequest::first().with_filter(Some(TXN_TABLE.into())),
        )
        .await
        .unwrap();
    assert_eq!(names(&tables.items), [TXN_TABLE], "{e} {case}");
    check("listing tables");
    let table = sch.child(ObjectKind::Table, TXN_TABLE);
    for kind in [
        ObjectKind::Column,
        ObjectKind::Constraint,
        ObjectKind::Index,
        ObjectKind::ForeignKey,
        ObjectKind::Trigger,
        ObjectKind::Partition,
    ] {
        m.list(&table, kind, PageRequest::first())
            .await
            .unwrap_or_else(|err| panic!("{e} {case} {kind:?}: {err}"));
        check(&format!("listing {kind:?}"));
    }
    let ddl = m.ddl(&table).await.unwrap();
    assert!(ddl.contains(TXN_TABLE), "{e} {case}: {ddl}");
    check("the table's DDL");
    m.ddl(&sch).await.unwrap();
    check("the schema's DDL");
    // A failing call (an object that does not exist) does not leave one either.
    assert!(
        m.ddl(&sch.child(ObjectKind::Table, "ninesql_meta_txn_missing"))
            .await
            .is_err()
    );
    check("a failed DDL call");
}

/// Real-engine UI test finding: in manual-commit mode (`autocommit = 0`, the production
/// default) a navigator read implicitly started a transaction, so browsing alone showed "in
/// transaction" and closing asked to commit or roll back. Navigator reads now end the
/// transaction they started, never one the user started; another session's DDL on a browsed
/// table does not wait (short `lock_wait_timeout`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn navigator_reads_leave_no_transaction_and_keep_the_users() {
    let _serial = SERIAL.lock().await;
    for dev in devs() {
        let e = &dev.engine;
        let mut admin = connect(&dev, false).await;
        exec_all(
            admin.as_mut(),
            e,
            &[
                "DROP TABLE IF EXISTS ninesql_meta_txn",
                "CREATE TABLE ninesql_meta_txn (id INT PRIMARY KEY, v INT, KEY v_idx (v)) \
                 ENGINE = InnoDB",
                "SET SESSION lock_wait_timeout = 2",
                "SET SESSION innodb_lock_wait_timeout = 2",
            ],
        )
        .await;

        // Manual commit, no user statement: browsing leaves no transaction.
        let mut s = connect(&dev, false).await;
        s.set_auto_commit(false).await.unwrap();
        assert!(!s.auto_commit() && !s.in_transaction(), "{e}");
        browse_txn_table(s.as_ref(), &dev, false, "manual commit").await;
        // No snapshot or metadata lock is held: another session's DDL does not wait.
        let started = std::time::Instant::now();
        exec_all(
            admin.as_mut(),
            e,
            &[
                "ALTER TABLE ninesql_meta_txn ADD COLUMN extra INT",
                "ALTER TABLE ninesql_meta_txn DROP COLUMN extra",
            ],
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{e}: DDL waited"
        );
        assert!(!s.in_transaction(), "{e}");

        // A transaction the user started implicitly (manual commit) stays open, with its row.
        exec(s.as_mut(), "INSERT INTO ninesql_meta_txn VALUES (1, 10)")
            .await
            .unwrap();
        assert!(s.in_transaction(), "{e}");
        browse_txn_table(s.as_ref(), &dev, true, "manual commit, user transaction").await;
        let count = "SELECT CAST(COUNT(*) AS CHAR) FROM ninesql_meta_txn";
        assert_eq!(scalar(s.as_mut(), count).await.as_deref(), Some("1"), "{e}");
        assert_eq!(
            scalar(admin.as_mut(), count).await.as_deref(),
            Some("0"),
            "{e}"
        );
        s.rollback().await.unwrap();
        assert!(!s.in_transaction(), "{e}");
        assert_eq!(scalar(s.as_mut(), count).await.as_deref(), Some("0"), "{e}");
        // That `SELECT` (a user statement) opened a transaction of its own: the user's.
        assert!(s.in_transaction(), "{e}");
        browse_txn_table(s.as_ref(), &dev, true, "manual commit, user read").await;
        s.rollback().await.unwrap();

        // An explicit transaction in auto-commit mode stays open too, and commits its row.
        s.set_auto_commit(true).await.unwrap();
        browse_txn_table(s.as_ref(), &dev, false, "auto-commit").await;
        s.begin().await.unwrap();
        exec(s.as_mut(), "INSERT INTO ninesql_meta_txn VALUES (2, 20)")
            .await
            .unwrap();
        browse_txn_table(s.as_ref(), &dev, true, "explicit transaction").await;
        s.commit().await.unwrap();
        assert!(!s.in_transaction(), "{e}");
        assert_eq!(
            scalar(admin.as_mut(), count).await.as_deref(),
            Some("1"),
            "{e}"
        );
        s.close().await.unwrap();

        // A read-only session in manual-commit mode: no transaction, and writes still fail.
        let mut ro = connect(&dev, true).await;
        ro.set_auto_commit(false).await.unwrap();
        browse_txn_table(ro.as_ref(), &dev, false, "read-only manual commit").await;
        assert!(ro.is_read_only(), "{e}");
        assert!(
            matches!(
                exec(ro.as_mut(), "INSERT INTO ninesql_meta_txn VALUES (3, 30)").await,
                Err(DriverError::ReadOnly(_))
            ),
            "{e}"
        );
        ro.close().await.unwrap();

        exec_all(admin.as_mut(), e, &["DROP TABLE ninesql_meta_txn"]).await;
        admin.close().await.unwrap();
    }
}

/// Creates `TXN_TABLE` with the committed row `(1, 10)`; returns the admin session, whose DDL
/// and row locks give up after one or two seconds.
async fn txn_fixture(dev: &Dev) -> Box<dyn Session> {
    let mut admin = connect(dev, false).await;
    exec_all(
        admin.as_mut(),
        &dev.engine,
        &[
            "DROP TABLE IF EXISTS ninesql_meta_txn",
            "CREATE TABLE ninesql_meta_txn (id INT PRIMARY KEY, v INT, KEY v_idx (v)) \
             ENGINE = InnoDB",
            "INSERT INTO ninesql_meta_txn VALUES (1, 10)",
            "SET SESSION lock_wait_timeout = 2",
            "SET SESSION innodb_lock_wait_timeout = 1",
        ],
    )
    .await;
    admin
}

/// Security re-review L2-a: a user's one-shot `SET TRANSACTION …` (next transaction only) was
/// consumed by a navigator read's implicit transaction and then discarded by the navigator's
/// rollback, so the user's next transaction ran read-write or at the default isolation level.
/// The driver now sets the same characteristics again after a navigator read; a form it cannot
/// restore makes navigator reads refuse until the pending settings are consumed or dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn navigator_reads_keep_one_shot_transaction_settings() {
    let _serial = SERIAL.lock().await;
    for dev in devs() {
        let e = &dev.engine;
        let mut admin = txn_fixture(&dev).await;
        let insert = "INSERT INTO ninesql_meta_txn VALUES (2, 20)";
        let mut s = connect(&dev, false).await;

        // Manual commit: a one-shot READ ONLY survives browsing, so the INSERT that starts the
        // next transaction fails as it would have without the browse.
        s.set_auto_commit(false).await.unwrap();
        exec(s.as_mut(), "SET TRANSACTION READ ONLY").await.unwrap();
        browse_txn_table(s.as_ref(), &dev, false, "one-shot read only").await;
        assert!(
            matches!(
                exec(s.as_mut(), insert).await,
                Err(DriverError::ReadOnly(_))
            ),
            "{e}: the one-shot READ ONLY was lost"
        );
        s.rollback().await.unwrap();
        // Consumed by that transaction: the next one is read-write again.
        exec(s.as_mut(), insert).await.unwrap();
        s.rollback().await.unwrap();

        // Isolation and access mode together, set in two statements (they accumulate), with
        // an unrelated session statement in between.
        exec(s.as_mut(), "SET TRANSACTION READ ONLY").await.unwrap();
        exec(s.as_mut(), "SET @ninesql_x = 1").await.unwrap();
        exec(s.as_mut(), "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .await
            .unwrap();
        browse_txn_table(s.as_ref(), &dev, false, "one-shot serializable").await;
        // SERIALIZABLE turns this plain read into a locking read: the admin's UPDATE of the
        // row waits and times out (it would not under REPEATABLE READ).
        assert_eq!(
            scalar(
                s.as_mut(),
                "SELECT CAST(v AS CHAR) FROM ninesql_meta_txn WHERE id = 1"
            )
            .await
            .as_deref(),
            Some("10"),
            "{e}"
        );
        assert!(s.in_transaction(), "{e}");
        match exec(
            admin.as_mut(),
            "UPDATE ninesql_meta_txn SET v = 11 WHERE id = 1",
        )
        .await
        {
            Err(DriverError::Query { code, .. }) => {
                assert_eq!(
                    code.as_deref(),
                    Some("1205"),
                    "{e}: not a lock wait timeout"
                );
            }
            other => panic!("{e}: the one-shot SERIALIZABLE was lost: {other:?}"),
        }
        assert!(
            matches!(
                exec(s.as_mut(), insert).await,
                Err(DriverError::ReadOnly(_))
            ),
            "{e}: the one-shot READ ONLY was lost"
        );
        s.rollback().await.unwrap();

        // `SET SESSION TRANSACTION READ WRITE` replaces the pending READ ONLY: the driver must
        // not bring it back.
        exec(s.as_mut(), "SET TRANSACTION READ ONLY").await.unwrap();
        exec(s.as_mut(), "SET SESSION TRANSACTION READ WRITE")
            .await
            .unwrap();
        browse_txn_table(s.as_ref(), &dev, false, "one-shot replaced").await;
        exec(s.as_mut(), insert).await.unwrap();
        s.rollback().await.unwrap();

        // Auto-commit: the navigator's own reads are transactions of their own; the explicit
        // transaction the user starts next still gets the one-shot READ ONLY.
        s.set_auto_commit(true).await.unwrap();
        exec(s.as_mut(), "SET TRANSACTION READ ONLY").await.unwrap();
        browse_txn_table(s.as_ref(), &dev, false, "auto-commit one-shot").await;
        s.begin().await.unwrap();
        assert!(
            matches!(
                exec(s.as_mut(), insert).await,
                Err(DriverError::ReadOnly(_))
            ),
            "{e}: the one-shot READ ONLY was lost in auto-commit mode"
        );
        s.rollback().await.unwrap();

        // A form the driver cannot restore: navigator reads are refused (nothing sent), the
        // pending setting stays, and browsing works again once it is consumed.
        s.set_auto_commit(false).await.unwrap();
        exec(s.as_mut(), "SET @@transaction_read_only = 1")
            .await
            .unwrap();
        let sch = ObjectPath::root().child(ObjectKind::Schema, &dev.database);
        let refused = s
            .metadata()
            .list(&sch, ObjectKind::Table, PageRequest::first())
            .await;
        assert!(
            matches!(&refused, Err(DriverError::Query { message, .. }) if message.contains("pending")),
            "{e}: {refused:?}"
        );
        assert!(
            matches!(
                exec(s.as_mut(), insert).await,
                Err(DriverError::ReadOnly(_))
            ),
            "{e}: the pending setting was lost"
        );
        s.rollback().await.unwrap();
        browse_txn_table(s.as_ref(), &dev, false, "after the unknown form").await;
        exec(s.as_mut(), insert).await.unwrap();
        s.rollback().await.unwrap();
        s.close().await.unwrap();

        exec_all(admin.as_mut(), e, &["DROP TABLE ninesql_meta_txn"]).await;
        admin.close().await.unwrap();
    }
}

/// `true` when a navigator read is refused because transaction settings are pending in a form
/// the driver cannot restore.
async fn navigator_refused(s: &dyn Session, dev: &Dev) -> bool {
    let sch = ObjectPath::root().child(ObjectKind::Schema, &dev.database);
    let r = s
        .metadata()
        .list(&sch, ObjectKind::Table, PageRequest::first())
        .await;
    match r {
        Err(DriverError::Query { message, .. }) if message.contains("pending") => true,
        Ok(_) => false,
        Err(e) => panic!("{}: unexpected navigator error: {e}", dev.engine),
    }
}

/// Final review L-1: a statement may overwrite or consume the one-shot characteristics the
/// driver tracked without the driver knowing (a session-scope assignment to
/// `transaction_isolation`, which also sets the next transaction's level; an implicit commit
/// such as `LOCK TABLES`). Re-issuing the tracked ones after a navigator read would then
/// re-apply stale settings (here a weaker isolation level), so navigator reads are refused
/// until `COMMIT` / `ROLLBACK`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn navigator_reads_never_reapply_stale_one_shot_settings() {
    let _serial = SERIAL.lock().await;
    for dev in devs() {
        let e = &dev.engine;
        let mut admin = txn_fixture(&dev).await;
        let mut s = connect(&dev, false).await;
        s.set_auto_commit(false).await.unwrap();

        for assign in [
            "SET SESSION transaction_isolation = 'SERIALIZABLE'",
            "SET @@Session.`Transaction_Isolation` = 'SERIALIZABLE'",
            "SET SESSION `transaction_isolation` = 'SERIALIZABLE'",
        ] {
            exec(
                s.as_mut(),
                "SET SESSION transaction_isolation = 'REPEATABLE-READ'",
            )
            .await
            .unwrap();
            exec(
                s.as_mut(),
                "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
            )
            .await
            .unwrap();
            exec(s.as_mut(), assign).await.unwrap();
            assert!(
                navigator_refused(s.as_ref(), &dev).await,
                "{e} `{assign}`: the navigator read (and the stale READ UNCOMMITTED re-issue) \
                 was not refused"
            );
            assert!(!s.in_transaction(), "{e}");
            // The user's next transaction runs at the level the server has: SERIALIZABLE
            // turns this plain read into a locking read, so the admin's UPDATE times out.
            assert_eq!(
                scalar(
                    s.as_mut(),
                    "SELECT CAST(v AS CHAR) FROM ninesql_meta_txn WHERE id = 1"
                )
                .await
                .as_deref(),
                Some("10"),
                "{e}"
            );
            assert!(s.in_transaction(), "{e}");
            match exec(
                admin.as_mut(),
                "UPDATE ninesql_meta_txn SET v = 11 WHERE id = 1",
            )
            .await
            {
                Err(DriverError::Query { code, .. }) => {
                    assert_eq!(
                        code.as_deref(),
                        Some("1205"),
                        "{e}: not a lock wait timeout"
                    );
                }
                other => panic!("{e} `{assign}`: SERIALIZABLE was not kept: {other:?}"),
            }
            s.rollback().await.unwrap();
            // Consumed: browsing works again.
            browse_txn_table(s.as_ref(), &dev, false, "after the session assignment").await;
        }

        // A plain read of the variable is not an assignment: the pending setting stays
        // restorable and browsing goes on.
        exec(s.as_mut(), "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .await
            .unwrap();
        exec(s.as_mut(), "SET @ninesql_iso = @@transaction_isolation")
            .await
            .unwrap();
        assert!(!navigator_refused(s.as_ref(), &dev).await, "{e}");
        s.rollback().await.unwrap();

        // Implicit commit: `LOCK TABLES` / `UNLOCK TABLES` commit, which drops the pending
        // READ UNCOMMITTED. The driver must not bring it back: either the navigator waits for
        // `COMMIT` / `ROLLBACK` (auto-commit mode), or the driver saw the transaction
        // `LOCK TABLES` opened in manual-commit mode (InnoDB) and dropped it itself. Either
        // way the user's next transaction must not read the admin's uncommitted change.
        exec(
            s.as_mut(),
            "SET SESSION transaction_isolation = 'REPEATABLE-READ'",
        )
        .await
        .unwrap();
        admin.set_auto_commit(false).await.unwrap();
        for auto_commit in [false, true] {
            s.set_auto_commit(auto_commit).await.unwrap();
            exec(
                s.as_mut(),
                "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
            )
            .await
            .unwrap();
            exec(s.as_mut(), "LOCK TABLES ninesql_meta_txn READ")
                .await
                .unwrap();
            exec(s.as_mut(), "UNLOCK TABLES").await.unwrap();
            let refused = navigator_refused(s.as_ref(), &dev).await;
            eprintln!(
                "{e} auto_commit={auto_commit}: navigator refused after LOCK TABLES: {refused}"
            );
            if auto_commit {
                assert!(refused, "{e}: navigator read not refused after LOCK TABLES");
            }
            assert!(!s.in_transaction(), "{e}");
            exec(
                admin.as_mut(),
                "UPDATE ninesql_meta_txn SET v = 99 WHERE id = 1",
            )
            .await
            .unwrap();
            assert_eq!(
                scalar(
                    s.as_mut(),
                    "SELECT CAST(v AS CHAR) FROM ninesql_meta_txn WHERE id = 1"
                )
                .await
                .as_deref(),
                Some("10"),
                "{e} auto_commit={auto_commit}: a stale READ UNCOMMITTED was re-applied"
            );
            admin.rollback().await.unwrap();
            s.rollback().await.unwrap();
            browse_txn_table(s.as_ref(), &dev, false, "after LOCK TABLES").await;
        }
        admin.set_auto_commit(true).await.unwrap();
        s.close().await.unwrap();

        exec_all(admin.as_mut(), e, &["DROP TABLE ninesql_meta_txn"]).await;
        admin.close().await.unwrap();
    }
}

/// Security review L-A: with one-shot characteristics pending, the switch to auto-commit (the
/// driver's own `SET SESSION autocommit = 1` or a user's assignment) commits, and MariaDB's
/// `SET STATEMENT … FOR <write>` runs the write in a transaction of its own (auto-commit mode),
/// which consumes them. The tracker kept them, so a navigator read re-issued stale settings
/// (here a weaker isolation level); navigator reads are now refused until `COMMIT` /
/// `ROLLBACK`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn autocommit_switches_and_set_statement_make_one_shot_settings_unknown() {
    let _serial = SERIAL.lock().await;
    for dev in devs() {
        let e = &dev.engine;
        let mut admin = txn_fixture(&dev).await;
        let insert = "INSERT INTO ninesql_meta_txn VALUES (2, 20)";
        let mut s = connect(&dev, false).await;

        // The driver's switch to auto-commit, and a user's assignment (any scope or quoting).
        for user_form in [
            None,
            Some("SET autocommit = 1"),
            Some("SET @@session.`AutoCommit` = ON"),
        ] {
            s.set_auto_commit(false).await.unwrap();
            exec(s.as_mut(), "SET TRANSACTION READ ONLY").await.unwrap();
            match user_form {
                None => s.set_auto_commit(true).await.unwrap(),
                Some(sql) => exec(s.as_mut(), sql).await.unwrap(),
            }
            assert!(
                navigator_refused(s.as_ref(), &dev).await,
                "{e} {user_form:?}: navigator read not refused after the switch to auto-commit"
            );
            assert!(!s.in_transaction(), "{e}");
            // As checked on MySQL 8.4 and MariaDB 11.8, the server kept the one-shot READ ONLY
            // through the switch (no transaction was open): the driver does not rely on it,
            // but it did not drop it either.
            s.begin().await.unwrap();
            assert!(
                matches!(
                    exec(s.as_mut(), insert).await,
                    Err(DriverError::ReadOnly(_))
                ),
                "{e} {user_form:?}: the server dropped the one-shot READ ONLY"
            );
            s.rollback().await.unwrap();
            browse_txn_table(s.as_ref(), &dev, false, "after the autocommit switch").await;
        }

        // MariaDB `SET STATEMENT … FOR <write>` in auto-commit mode: the write is a transaction
        // of its own and consumes the pending READ UNCOMMITTED; a re-issue would let the next
        // transaction read the admin's uncommitted change. On MySQL the statement is a syntax
        // error: a failed access-mode statement makes the state unknown all the same.
        s.set_auto_commit(true).await.unwrap();
        exec(
            s.as_mut(),
            "SET SESSION transaction_isolation = 'REPEATABLE-READ'",
        )
        .await
        .unwrap();
        exec(
            s.as_mut(),
            "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
        )
        .await
        .unwrap();
        let set_statement = exec(
            s.as_mut(),
            "SET STATEMENT max_statement_time = 10 FOR \
             DELETE FROM ninesql_meta_txn WHERE id = 99",
        )
        .await;
        let mariadb = *e == EngineId::MARIADB;
        assert_eq!(set_statement.is_ok(), mariadb, "{e}: {set_statement:?}");
        assert!(
            navigator_refused(s.as_ref(), &dev).await,
            "{e}: navigator read not refused after SET STATEMENT"
        );
        assert!(!s.in_transaction(), "{e}");
        if mariadb {
            admin.set_auto_commit(false).await.unwrap();
            exec(
                admin.as_mut(),
                "UPDATE ninesql_meta_txn SET v = 99 WHERE id = 1",
            )
            .await
            .unwrap();
            s.begin().await.unwrap();
            assert_eq!(
                scalar(
                    s.as_mut(),
                    "SELECT CAST(v AS CHAR) FROM ninesql_meta_txn WHERE id = 1"
                )
                .await
                .as_deref(),
                Some("10"),
                "{e}: SET STATEMENT did not consume the one-shot READ UNCOMMITTED"
            );
            admin.rollback().await.unwrap();
            admin.set_auto_commit(true).await.unwrap();
        }
        s.rollback().await.unwrap();
        browse_txn_table(s.as_ref(), &dev, false, "after SET STATEMENT").await;
        s.close().await.unwrap();

        exec_all(admin.as_mut(), e, &["DROP TABLE ninesql_meta_txn"]).await;
        admin.close().await.unwrap();
    }
}

/// Security re-review L2-b / L2-c: an error packet carries no status flags. A failed navigator
/// read leaves no transaction (the rollback is sent whatever the stale flag says), and a user
/// statement that fails but leaves work in the transaction (a procedure that inserts and then
/// raises `SIGNAL`; on MariaDB also an anonymous `BEGIN NOT ATOMIC` block) is seen as an open
/// transaction, so browsing never rolls that work back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_statements_neither_leave_nor_lose_a_transaction() {
    let _serial = SERIAL.lock().await;
    for dev in devs() {
        let e = &dev.engine;
        let mut admin = txn_fixture(&dev).await;
        exec_all(
            admin.as_mut(),
            e,
            &[
                "DROP PROCEDURE IF EXISTS ninesql_meta_sig",
                "CREATE PROCEDURE ninesql_meta_sig(IN n INT) BEGIN \
                 INSERT INTO ninesql_meta_txn VALUES (n, n); \
                 SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'ninesql test signal'; END",
            ],
        )
        .await;
        let count = "SELECT CAST(COUNT(*) AS CHAR) FROM ninesql_meta_txn";
        let mut s = connect(&dev, false).await;
        s.set_auto_commit(false).await.unwrap();
        let sch = ObjectPath::root().child(ObjectKind::Schema, &dev.database);

        // L2-b: a failed navigator call, then the server's own flag (a table-less SELECT
        // carries it and starts nothing) and another session's DDL that does not wait.
        assert!(
            s.metadata()
                .ddl(&sch.child(ObjectKind::Table, "ninesql_meta_txn_missing"))
                .await
                .is_err()
        );
        assert!(!s.in_transaction(), "{e}");
        assert_eq!(scalar(s.as_mut(), "SELECT 'x'").await.as_deref(), Some("x"));
        assert!(
            !s.in_transaction(),
            "{e}: a failed navigator read left a transaction"
        );
        let started = std::time::Instant::now();
        exec_all(
            admin.as_mut(),
            e,
            &[
                "ALTER TABLE ninesql_meta_txn ADD COLUMN extra INT",
                "ALTER TABLE ninesql_meta_txn DROP COLUMN extra",
            ],
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{e}: DDL waited"
        );

        // A failed plain statement: the driver's flag matches the server's after it.
        assert!(
            exec(s.as_mut(), "INSERT INTO ninesql_meta_txn VALUES (1, 1)")
                .await
                .is_err()
        );
        let after_error = s.in_transaction();
        assert_eq!(scalar(s.as_mut(), "SELECT 'x'").await.as_deref(), Some("x"));
        assert_eq!(
            s.in_transaction(),
            after_error,
            "{e}: stale transaction flag after an error"
        );
        s.rollback().await.unwrap();

        // L2-c: the procedure's INSERT stays in the user's transaction after the SIGNAL.
        let mut blocks = vec![("CALL", "CALL ninesql_meta_sig(5)")];
        if *e == EngineId::MARIADB {
            blocks.push((
                "BEGIN NOT ATOMIC",
                "BEGIN NOT ATOMIC INSERT INTO ninesql_meta_txn VALUES (6, 6); \
                 SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'ninesql test signal'; END",
            ));
        }
        for (what, sql) in blocks {
            assert!(!s.in_transaction(), "{e} {what}");
            match exec(s.as_mut(), sql).await {
                Err(DriverError::Query { code, .. }) => {
                    assert_eq!(code.as_deref(), Some("1644"), "{e} {what}");
                }
                other => panic!("{e} {what}: {other:?}"),
            }
            assert!(
                s.in_transaction(),
                "{e} {what}: the insert's transaction is not seen"
            );
            browse_txn_table(s.as_ref(), &dev, true, what).await;
            assert_eq!(
                scalar(s.as_mut(), count).await.as_deref(),
                Some("2"),
                "{e} {what}: the user's row was rolled back by the navigator"
            );
            assert_eq!(
                scalar(admin.as_mut(), count).await.as_deref(),
                Some("1"),
                "{e} {what}"
            );
            s.rollback().await.unwrap();
            assert_eq!(scalar(s.as_mut(), count).await.as_deref(), Some("1"));
            s.rollback().await.unwrap();
        }
        s.close().await.unwrap();

        exec_all(
            admin.as_mut(),
            e,
            &[
                "DROP PROCEDURE ninesql_meta_sig",
                "DROP TABLE ninesql_meta_txn",
            ],
        )
        .await;
        admin.close().await.unwrap();
    }
}
