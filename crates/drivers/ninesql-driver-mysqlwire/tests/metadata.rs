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
