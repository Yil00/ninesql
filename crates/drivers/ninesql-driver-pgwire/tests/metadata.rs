//! Navigator metadata and DDL generation (P4-A, F-NAV-01, F-NAV-02, F-NAV-07) against the
//! `postgresql` service of `dev/compose.yaml`.
//!
//! Skipped when the engine is unreachable, unless `NINESQL_REQUIRE_ENGINES` lists
//! `postgresql`. Start it with `python3 dev/ready.py up core`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::path::PathBuf;
use std::time::Duration;

use ninesql_driver_api::{
    Auth, ConnectParams, ConstraintKind, Driver, DriverError, Endpoint, EngineId, ExecOptions,
    Metadata, ObjectDetails, ObjectInfo, ObjectKind, ObjectPath, PageRequest, ReferentialAction,
    RoutineKind, ScriptItemKind, Secret, Session, Statement, TlsMode, conformance,
};
use ninesql_driver_pgwire::PgWireDriver;
use tokio::sync::Mutex;

/// The tests share schema `ninesql_meta`: one at a time.
static SERIAL: Mutex<()> = Mutex::const_new(());

struct Dev {
    host: String,
    port: u16,
    user: String,
    password: String,
    database: String,
}

fn dev() -> Option<Dev> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../dev/engines.json");
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let e = doc["engines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "postgresql")
        .unwrap();
    let dev = Dev {
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
        .any(|s| s.trim() == "postgresql");
    assert!(
        !required,
        "postgresql is required but unreachable at {addr}"
    );
    eprintln!("skipped: postgresql unreachable at {addr} (python3 dev/ready.py up core)");
    None
}

fn params(dev: &Dev) -> ConnectParams {
    let mut p = ConnectParams::new(EngineId::POSTGRESQL, Endpoint::tcp(&dev.host, dev.port));
    p.auth = Auth::password(&dev.user, Some(Secret::new(dev.password.clone())));
    p.database = Some(dev.database.clone());
    p.tls.mode = TlsMode::Disable;
    p.application_name = "ninesql-pgwire-metadata-tests".into();
    p
}

async fn connect(dev: &Dev, read_only: bool) -> Box<dyn Session> {
    let mut p = params(dev);
    p.read_only = read_only;
    PgWireDriver::new().connect(p).await.unwrap()
}

async fn exec(s: &mut dyn Session, sql: &str) -> Result<Vec<Vec<String>>, DriverError> {
    use ninesql_driver_api::arrow_array::Array;
    use ninesql_driver_api::arrow_array::cast::AsArray;
    let mut stream = s
        .execute(Statement::sql(sql), ExecOptions::default())
        .await?;
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        if let ninesql_driver_api::ResultItem::Rows(b) = item? {
            for r in 0..b.num_rows() {
                out.push(
                    (0..b.num_columns())
                        .map(|c| {
                            let col = b.column(c);
                            if col.is_null(r) {
                                String::new()
                            } else if let Some(a) = col.as_string_opt::<i32>() {
                                a.value(r).to_owned()
                            } else {
                                format!("{col:?}")
                            }
                        })
                        .collect(),
                );
            }
        }
    }
    Ok(out)
}

/// Runs every statement of `script` (the session's own splitter).
async fn script(s: &mut dyn Session, script: &str) {
    let items = s.dialect().split_script(script);
    for item in &items {
        if item.kind == ScriptItemKind::Statement {
            let text = item.text(script);
            if let Err(e) = exec(s, text).await {
                panic!("{text:?} failed: {e}");
            }
        }
    }
}

const FIXTURE: &str = r#"
DROP SCHEMA IF EXISTS ninesql_meta CASCADE;
CREATE SCHEMA ninesql_meta;
COMMENT ON SCHEMA ninesql_meta IS 'P4-A metadata tests';
CREATE TYPE ninesql_meta.mood AS ENUM ('sad', 'it''s ok', 'happy');
CREATE DOMAIN ninesql_meta.positive AS integer DEFAULT 1 NOT NULL CONSTRAINT positive_check CHECK (VALUE > 0);
CREATE TYPE ninesql_meta.pair AS (a integer, "b c" text);
CREATE TYPE ninesql_meta.floatrange AS RANGE (subtype = float8);
CREATE SEQUENCE ninesql_meta.counter AS integer INCREMENT BY 5 MINVALUE 1 MAXVALUE 1000 START WITH 10 CACHE 1 CYCLE;
CREATE TABLE ninesql_meta.parent (id integer PRIMARY KEY, code text NOT NULL, CONSTRAINT parent_code_key UNIQUE (code));
CREATE TABLE ninesql_meta."Weird ""Name"" é" (
  "select" integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  parent_id integer NOT NULL REFERENCES ninesql_meta.parent (id) ON DELETE CASCADE ON UPDATE SET NULL,
  mood ninesql_meta.mood DEFAULT 'happy',
  amount ninesql_meta.positive,
  total numeric(10,2) GENERATED ALWAYS AS (amount * 2) STORED,
  note text,
  CONSTRAINT "check ""q""" CHECK (note IS NULL OR length(note) < 100)
);
COMMENT ON TABLE ninesql_meta."Weird ""Name"" é" IS 'it''s weird';
COMMENT ON COLUMN ninesql_meta."Weird ""Name"" é".note IS 'a note';
CREATE UNIQUE INDEX weird_lower_note ON ninesql_meta."Weird ""Name"" é" (lower(note), parent_id) WHERE note IS NOT NULL;
CREATE TABLE ninesql_meta.events (id integer NOT NULL, at date NOT NULL, PRIMARY KEY (id, at)) PARTITION BY RANGE (at);
CREATE TABLE ninesql_meta.events_2025 PARTITION OF ninesql_meta.events FOR VALUES FROM ('2025-01-01') TO ('2026-01-01');
CREATE TABLE ninesql_meta.events_2026 PARTITION OF ninesql_meta.events FOR VALUES FROM ('2026-01-01') TO ('2027-01-01') PARTITION BY LIST (id);
CREATE TABLE ninesql_meta.events_2026_a PARTITION OF ninesql_meta.events_2026 FOR VALUES IN (1, 2);
CREATE VIEW ninesql_meta.parent_codes AS SELECT id, code FROM ninesql_meta.parent;
CREATE MATERIALIZED VIEW ninesql_meta.parent_count AS SELECT count(*) AS n FROM ninesql_meta.parent;
CREATE INDEX parent_count_n ON ninesql_meta.parent_count (n);
CREATE FUNCTION ninesql_meta.touch() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
CREATE FUNCTION ninesql_meta.add(a integer, b integer DEFAULT 1) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT a + b';
CREATE FUNCTION ninesql_meta.add(a text, b text) RETURNS text LANGUAGE sql AS 'SELECT a || b';
CREATE PROCEDURE ninesql_meta.noop() LANGUAGE sql AS 'SELECT 1';
CREATE TRIGGER "before ins" BEFORE INSERT OR UPDATE ON ninesql_meta.parent FOR EACH ROW EXECUTE FUNCTION ninesql_meta.touch();
CREATE TRIGGER view_ins INSTEAD OF INSERT ON ninesql_meta.parent_codes FOR EACH ROW EXECUTE FUNCTION ninesql_meta.touch();
"#;

async fn names(m: &dyn Metadata, parent: &ObjectPath, kind: ObjectKind) -> Vec<String> {
    all(m, parent, kind)
        .await
        .iter()
        .map(|o| o.name().to_owned())
        .collect()
}

async fn all(m: &dyn Metadata, parent: &ObjectPath, kind: ObjectKind) -> Vec<ObjectInfo> {
    let mut out = Vec::new();
    let mut cursor = None;
    loop {
        let page = m
            .list(
                parent,
                kind,
                PageRequest::first().with_limit(3).with_cursor(cursor),
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

fn one<'a>(items: &'a [ObjectInfo], name: &str) -> &'a ObjectInfo {
    items
        .iter()
        .find(|o| o.name() == name)
        .unwrap_or_else(|| panic!("{name} not in {items:?}"))
}

/// Every declared kind lists the expected objects, with details; works on a read-only session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_declared_kind_lists_its_objects() {
    let Some(dev) = dev() else { return };
    let _serial = SERIAL.lock().await;
    let mut admin = connect(&dev, false).await;
    script(admin.as_mut(), FIXTURE).await;
    // Read-only session (I3): metadata only reads.
    let s = connect(&dev, true).await;
    assert!(s.is_read_only());
    let caps = s.capabilities().metadata.clone();
    let m = s.metadata();
    for parent in [
        None,
        Some(ObjectKind::Schema),
        Some(ObjectKind::Table),
        Some(ObjectKind::View),
        Some(ObjectKind::MaterializedView),
        Some(ObjectKind::Partition),
    ] {
        for k in m.child_kinds(parent) {
            assert!(caps.kinds.contains(&k), "{k:?} listed but not declared");
        }
    }
    let root = ObjectPath::root();
    let schemas = all(m.as_ref(), &root, ObjectKind::Schema).await;
    assert_eq!(
        one(&schemas, "ninesql_meta").comment.as_deref(),
        Some("P4-A metadata tests")
    );
    let sch = root.child(ObjectKind::Schema, "ninesql_meta");
    let weird_name = "Weird \"Name\" é";
    assert_eq!(
        names(m.as_ref(), &sch, ObjectKind::Table).await,
        vec!["Weird \"Name\" é", "events", "parent"],
        "partitions are listed under their parent"
    );
    let tables = all(m.as_ref(), &sch, ObjectKind::Table).await;
    match &one(&tables, "events").details {
        ObjectDetails::Table(t) => {
            assert_eq!(t.flavour.as_deref(), Some("partitioned"));
            assert_eq!(t.partition_key.as_deref(), Some("RANGE (at)"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        one(&tables, weird_name).comment.as_deref(),
        Some("it's weird")
    );
    assert_eq!(
        names(m.as_ref(), &sch, ObjectKind::View).await,
        vec!["parent_codes"]
    );
    assert_eq!(
        names(m.as_ref(), &sch, ObjectKind::MaterializedView).await,
        vec!["parent_count"]
    );
    let seqs = all(m.as_ref(), &sch, ObjectKind::Sequence).await;
    match &one(&seqs, "counter").details {
        ObjectDetails::Sequence(q) => {
            assert_eq!(q.data_type.as_deref(), Some("integer"));
            assert_eq!(
                (q.start, q.increment, q.min_value, q.max_value, q.cycle),
                (Some(10), Some(5), Some(1), Some(1000), true)
            );
        }
        other => panic!("{other:?}"),
    }
    let types = all(m.as_ref(), &sch, ObjectKind::Type).await;
    assert_eq!(
        types.iter().map(ObjectInfo::name).collect::<Vec<_>>(),
        vec!["floatrange", "mood", "pair", "positive"]
    );
    match &one(&types, "mood").details {
        ObjectDetails::Type(t) => {
            assert_eq!(t.flavour, "enum");
            assert_eq!(t.labels, vec!["sad", "it's ok", "happy"]);
        }
        other => panic!("{other:?}"),
    }
    match &one(&types, "positive").details {
        ObjectDetails::Type(t) => {
            assert_eq!(t.flavour, "domain");
            assert_eq!(t.base_type.as_deref(), Some("integer"));
        }
        other => panic!("{other:?}"),
    }
    let routines = all(m.as_ref(), &sch, ObjectKind::Routine).await;
    assert_eq!(
        routines.iter().map(ObjectInfo::name).collect::<Vec<_>>(),
        vec![
            "add(a integer, b integer)",
            "add(a text, b text)",
            "noop()",
            "touch()"
        ]
    );
    match &one(&routines, "add(a integer, b integer)").details {
        ObjectDetails::Routine(r) => {
            assert_eq!(r.kind, RoutineKind::Function);
            assert_eq!(r.routine_name, "add");
            assert_eq!(
                r.arguments.as_deref(),
                Some("a integer, b integer DEFAULT 1")
            );
            assert_eq!(r.return_type.as_deref(), Some("integer"));
            assert_eq!(r.language.as_deref(), Some("sql"));
        }
        other => panic!("{other:?}"),
    }
    match &one(&routines, "noop()").details {
        ObjectDetails::Routine(r) => assert_eq!(r.kind, RoutineKind::Procedure),
        other => panic!("{other:?}"),
    }

    // Below a table.
    let weird = sch.child(ObjectKind::Table, weird_name);
    let cols = all(m.as_ref(), &weird, ObjectKind::Column).await;
    assert_eq!(
        cols.iter().map(ObjectInfo::name).collect::<Vec<_>>(),
        vec!["select", "parent_id", "mood", "amount", "total", "note"]
    );
    match &one(&cols, "select").details {
        ObjectDetails::Column(c) => assert!(c.primary_key && c.auto_generated && !c.nullable),
        other => panic!("{other:?}"),
    }
    match &one(&cols, "mood").details {
        ObjectDetails::Column(c) => {
            assert_eq!(c.data_type, "ninesql_meta.mood");
            assert_eq!(c.default.as_deref(), Some("'happy'::ninesql_meta.mood"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(one(&cols, "note").comment.as_deref(), Some("a note"));
    let cons = all(m.as_ref(), &weird, ObjectKind::Constraint).await;
    let pk_name = "Weird \"Name\" é_pkey";
    match &one(&cons, pk_name).details {
        ObjectDetails::Constraint(c) => {
            assert_eq!(c.kind, ConstraintKind::PrimaryKey);
            assert_eq!(c.columns, vec!["select"]);
        }
        other => panic!("{other:?}"),
    }
    match &one(&cons, "check \"q\"").details {
        ObjectDetails::Constraint(c) => {
            assert_eq!(c.kind, ConstraintKind::Check);
            assert_eq!(c.columns, vec!["note"]);
            assert!(c.definition.as_deref().unwrap().starts_with("CHECK"));
        }
        other => panic!("{other:?}"),
    }
    let fks = all(m.as_ref(), &weird, ObjectKind::ForeignKey).await;
    assert_eq!(fks.len(), 1);
    match &fks[0].details {
        ObjectDetails::ForeignKey(f) => {
            assert_eq!(f.columns, vec!["parent_id"]);
            assert_eq!(f.referenced_table, sch.child(ObjectKind::Table, "parent"));
            assert_eq!(f.referenced_columns, vec!["id"]);
            assert_eq!(f.on_delete, Some(ReferentialAction::Cascade));
            assert_eq!(f.on_update, Some(ReferentialAction::SetNull));
        }
        other => panic!("{other:?}"),
    }
    let idx = all(m.as_ref(), &weird, ObjectKind::Index).await;
    match &one(&idx, "weird_lower_note").details {
        ObjectDetails::Index(i) => {
            assert!(i.unique && !i.primary);
            assert_eq!(i.columns, vec!["lower(note)", "parent_id"]);
            assert_eq!(i.method.as_deref(), Some("btree"));
            assert_eq!(i.predicate.as_deref(), Some("note IS NOT NULL"));
        }
        other => panic!("{other:?}"),
    }
    match &one(&idx, pk_name).details {
        ObjectDetails::Index(i) => assert!(i.primary && i.unique),
        other => panic!("{other:?}"),
    }
    let parent = sch.child(ObjectKind::Table, "parent");
    let trg = all(m.as_ref(), &parent, ObjectKind::Trigger).await;
    match &one(&trg, "before ins").details {
        ObjectDetails::Trigger(t) => {
            assert_eq!(t.timing.as_deref(), Some("BEFORE"));
            assert_eq!(t.events, vec!["INSERT", "UPDATE"]);
            assert!(t.for_each_row && t.enabled);
        }
        other => panic!("{other:?}"),
    }
    let vtrg = all(
        m.as_ref(),
        &sch.child(ObjectKind::View, "parent_codes"),
        ObjectKind::Trigger,
    )
    .await;
    match &one(&vtrg, "view_ins").details {
        ObjectDetails::Trigger(t) => assert_eq!(t.timing.as_deref(), Some("INSTEAD OF")),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        names(
            m.as_ref(),
            &sch.child(ObjectKind::MaterializedView, "parent_count"),
            ObjectKind::Index
        )
        .await,
        vec!["parent_count_n"]
    );
    // Partitions nest.
    let events = sch.child(ObjectKind::Table, "events");
    let parts = all(m.as_ref(), &events, ObjectKind::Partition).await;
    assert_eq!(
        parts.iter().map(ObjectInfo::name).collect::<Vec<_>>(),
        vec!["events_2025", "events_2026"]
    );
    match &one(&parts, "events_2026").details {
        ObjectDetails::Partition(p) => {
            assert_eq!(
                p.bound.as_deref(),
                Some("FOR VALUES FROM ('2026-01-01') TO ('2027-01-01')")
            );
            assert_eq!(p.method.as_deref(), Some("RANGE"));
            assert_eq!(p.schema.as_deref(), Some("ninesql_meta"));
            assert!(p.partitioned);
        }
        other => panic!("{other:?}"),
    }
    let p2026 = events.child(ObjectKind::Partition, "events_2026");
    let sub = all(m.as_ref(), &p2026, ObjectKind::Partition).await;
    match &one(&sub, "events_2026_a").details {
        ObjectDetails::Partition(p) => {
            assert_eq!(p.method.as_deref(), Some("LIST"));
            assert_eq!(p.bound.as_deref(), Some("FOR VALUES IN (1, 2)"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        names(
            m.as_ref(),
            &p2026.child(ObjectKind::Partition, "events_2026_a"),
            ObjectKind::Column
        )
        .await,
        vec!["id", "at"]
    );

    // The "nine" dataset browses fully.
    let public = root.child(ObjectKind::Schema, "public");
    let nine = names(m.as_ref(), &public, ObjectKind::Table).await;
    for t in [
        "all_types",
        "big_events",
        "customers",
        "nine_meta",
        "orders",
        "products",
    ] {
        assert!(nine.contains(&t.to_owned()), "{t} missing from {nine:?}");
    }
    let orders = public.child(ObjectKind::Table, "orders");
    let fks = all(m.as_ref(), &orders, ObjectKind::ForeignKey).await;
    let targets: Vec<_> = fks
        .iter()
        .map(|f| match &f.details {
            ObjectDetails::ForeignKey(f) => (
                f.columns.clone(),
                f.referenced_table.leaf().unwrap().name.clone(),
                f.referenced_columns.clone(),
            ),
            other => panic!("{other:?}"),
        })
        .collect();
    assert!(targets.contains(&(
        vec!["customer_id".into()],
        "customers".into(),
        vec!["id".into()]
    )));
    assert!(targets.contains(&(
        vec!["product_id".into()],
        "products".into(),
        vec!["id".into()]
    )));
    assert!(
        names(m.as_ref(), &orders, ObjectKind::Index)
            .await
            .contains(&"orders_customer_idx".to_owned())
    );
    let ccons = all(
        m.as_ref(),
        &public.child(ObjectKind::Table, "customers"),
        ObjectKind::Constraint,
    )
    .await;
    assert!(ccons.iter().any(|c| matches!(&c.details,
        ObjectDetails::Constraint(i) if i.kind == ConstraintKind::Unique && i.columns == vec!["email"])));
    assert!(
        names(m.as_ref(), &public, ObjectKind::View)
            .await
            .contains(&"customer_order_totals".to_owned())
    );
    assert!(
        names(m.as_ref(), &public, ObjectKind::Type)
            .await
            .contains(&"mood".to_owned())
    );
    // Filters are literal and bound.
    let f = m
        .list(
            &sch,
            ObjectKind::Table,
            PageRequest::first().with_filter(Some("\"Name\"".into())),
        )
        .await
        .unwrap();
    assert_eq!(f.items.len(), 1);
    // A kind that does not belong under the parent, and a path that does not exist.
    assert!(matches!(
        m.list(&sch, ObjectKind::Column, PageRequest::first()).await,
        Err(DriverError::Unsupported(_))
    ));
    assert!(matches!(
        m.list(
            &sch.child(ObjectKind::Table, "nope"),
            ObjectKind::Column,
            PageRequest::first()
        )
        .await,
        Err(DriverError::InvalidParams(_))
    ));
    // Still read-only after all of it.
    assert!(s.is_read_only());
    s.close().await.unwrap();
    admin.close().await.unwrap();
}

/// pgwire's documented DDL refusals (docs/08): base types and foreign tables.
fn pg_ddl_refused(item: &ObjectInfo) -> bool {
    match &item.details {
        ObjectDetails::Type(t) => t.flavour == "base",
        ObjectDetails::Table(t) => t.flavour.as_deref() == Some("foreign"),
        _ => false,
    }
}

/// Phase 4 exit criterion: the "nine" dataset browses fully on a read-only session, every
/// node and every declared DDL (`conformance::assert_read_only_browse`); then the richer
/// `ninesql_meta` fixture (partitions, routines, triggers, materialized views) the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nine_dataset_browses_fully_on_a_read_only_session() {
    let Some(dev) = dev() else { return };
    let mut s = connect(&dev, true).await;
    let root = ObjectPath::root();
    let schemas = names(s.metadata().as_ref(), &root, ObjectKind::Schema).await;
    assert!(schemas.contains(&"public".to_owned()), "{schemas:?}");
    let public = root.child(ObjectKind::Schema, "public");
    let r = conformance::assert_read_only_browse(s.as_ref(), public, &pg_ddl_refused).await;
    assert!(r.count(ObjectKind::Table) >= 6, "{r:?}");
    assert!(r.count(ObjectKind::View) >= 1, "{r:?}");
    assert!(r.count(ObjectKind::Type) >= 1, "{r:?}");
    assert!(r.count(ObjectKind::Column) >= 30, "{r:?}");
    assert_eq!(r.count(ObjectKind::ForeignKey), 2, "{r:?}");
    assert!(r.count(ObjectKind::Index) >= 2, "{r:?}");
    assert!(r.count(ObjectKind::Constraint) >= 6, "{r:?}");
    assert!(r.ddl >= 9, "{r:?}");
    // Writes are still refused after the browse.
    assert!(matches!(
        exec(s.as_mut(), "CREATE TABLE ninesql_p4_browse (a int)").await,
        Err(DriverError::ReadOnly(_))
    ));
    s.close().await.unwrap();

    let _serial = SERIAL.lock().await;
    let mut admin = connect(&dev, false).await;
    script(admin.as_mut(), FIXTURE).await;
    let s = connect(&dev, true).await;
    let sch = root.child(ObjectKind::Schema, "ninesql_meta");
    let r = conformance::assert_read_only_browse(s.as_ref(), sch, &pg_ddl_refused).await;
    for kind in [
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::MaterializedView,
        ObjectKind::Partition,
        ObjectKind::Trigger,
        ObjectKind::Routine,
        ObjectKind::Sequence,
        ObjectKind::Type,
    ] {
        assert!(r.count(kind) >= 1, "{kind:?}: {r:?}");
    }
    s.close().await.unwrap();
    admin.close().await.unwrap();
}

/// DDL of every object of the fixture re-creates it: drop the schema, run the DDL, and the
/// regenerated DDL is identical.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_ddl_round_trips() {
    let Some(dev) = dev() else { return };
    let _serial = SERIAL.lock().await;
    let mut admin = connect(&dev, false).await;
    script(admin.as_mut(), FIXTURE).await;
    let s = connect(&dev, true).await;
    let m = s.metadata();
    let sch = ObjectPath::root().child(ObjectKind::Schema, "ninesql_meta");
    let weird = sch.child(ObjectKind::Table, "Weird \"Name\" é");
    let events = sch.child(ObjectKind::Table, "events");
    let parent = sch.child(ObjectKind::Table, "parent");
    // Dependency order.
    let objects = vec![
        sch.clone(),
        sch.child(ObjectKind::Type, "mood"),
        sch.child(ObjectKind::Type, "positive"),
        sch.child(ObjectKind::Type, "pair"),
        sch.child(ObjectKind::Type, "floatrange"),
        sch.child(ObjectKind::Sequence, "counter"),
        parent.clone(),
        weird.clone(),
        events.clone(),
        events.child(ObjectKind::Partition, "events_2025"),
        events.child(ObjectKind::Partition, "events_2026"),
        events
            .child(ObjectKind::Partition, "events_2026")
            .child(ObjectKind::Partition, "events_2026_a"),
        sch.child(ObjectKind::View, "parent_codes"),
        sch.child(ObjectKind::MaterializedView, "parent_count"),
        sch.child(ObjectKind::Routine, "touch()"),
        sch.child(ObjectKind::Routine, "add(a integer, b integer)"),
        sch.child(ObjectKind::Routine, "add(a text, b text)"),
        sch.child(ObjectKind::Routine, "noop()"),
        parent.child(ObjectKind::Trigger, "before ins"),
        sch.child(ObjectKind::View, "parent_codes")
            .child(ObjectKind::Trigger, "view_ins"),
    ];
    let mut ddls = Vec::new();
    for o in &objects {
        ddls.push(m.ddl(o).await.unwrap_or_else(|e| panic!("{o:?}: {e}")));
    }
    let weird_ddl = &ddls[7];
    assert!(
        weird_ddl.starts_with("CREATE TABLE ninesql_meta.\"Weird \"\"Name\"\" é\" (\n"),
        "{weird_ddl}"
    );
    for part in [
        "\"select\" integer GENERATED ALWAYS AS IDENTITY NOT NULL",
        "total numeric(10,2) GENERATED ALWAYS AS (((amount)::integer * 2)) STORED",
        "CONSTRAINT \"check \"\"q\"\"\" CHECK",
        "REFERENCES ninesql_meta.parent(id) ON UPDATE SET NULL ON DELETE CASCADE",
        "CREATE UNIQUE INDEX weird_lower_note ON",
        "COMMENT ON TABLE ninesql_meta.\"Weird \"\"Name\"\" é\" IS 'it''s weird';",
        "COMMENT ON COLUMN ninesql_meta.\"Weird \"\"Name\"\" é\".note IS 'a note';",
    ] {
        assert!(weird_ddl.contains(part), "{part} not in {weird_ddl}");
    }
    // Sub-object DDL.
    let con = m
        .ddl(&weird.child(ObjectKind::ForeignKey, "Weird \"Name\" é_parent_id_fkey"))
        .await
        .unwrap();
    assert!(con.starts_with("ALTER TABLE ninesql_meta.\"Weird \"\"Name\"\" é\" ADD CONSTRAINT"));
    let idx = m
        .ddl(&weird.child(ObjectKind::Index, "weird_lower_note"))
        .await
        .unwrap();
    assert!(
        idx.starts_with("CREATE UNIQUE INDEX weird_lower_note"),
        "{idx}"
    );
    let chk = m
        .ddl(&weird.child(ObjectKind::Constraint, "check \"q\""))
        .await
        .unwrap();
    assert!(
        chk.contains("ADD CONSTRAINT \"check \"\"q\"\"\" CHECK"),
        "{chk}"
    );
    // A constraint is not a foreign key.
    assert!(
        m.ddl(&weird.child(ObjectKind::ForeignKey, "check \"q\""))
            .await
            .is_err()
    );
    assert!(matches!(
        m.ddl(&weird.child(ObjectKind::Column, "note")).await,
        Err(DriverError::Unsupported(_))
    ));

    // Re-create from the DDL alone.
    exec(admin.as_mut(), "DROP SCHEMA ninesql_meta CASCADE")
        .await
        .unwrap();
    for d in &ddls {
        script(admin.as_mut(), d).await;
    }
    exec(
        admin.as_mut(),
        "COMMENT ON SCHEMA ninesql_meta IS 'P4-A metadata tests'",
    )
    .await
    .unwrap();
    for (o, before) in objects.iter().zip(&ddls) {
        let after = m.ddl(o).await.unwrap();
        assert_eq!(&after, before, "{o:?}");
    }
    s.close().await.unwrap();
    admin.close().await.unwrap();
}

/// Large listings page under the limit; a dropped call is cancelled server-side, and the
/// session's cancel handle reaches a running metadata call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_listings_page_and_calls_cancel() {
    let Some(dev) = dev() else { return };
    let _serial = SERIAL.lock().await;
    let mut admin = connect(&dev, false).await;
    script(
        admin.as_mut(),
        "DROP SCHEMA IF EXISTS ninesql_meta_big CASCADE;\n\
         CREATE SCHEMA ninesql_meta_big;\n\
         DO $$ BEGIN FOR i IN 1..1205 LOOP \
         EXECUTE pg_catalog.format('CREATE TABLE ninesql_meta_big.%I (id int)', 't' || lpad(i::text, 4, '0')); \
         END LOOP; END $$;",
    )
    .await;
    let s = connect(&dev, true).await;
    let m = s.metadata();
    let big = ObjectPath::root().child(ObjectKind::Schema, "ninesql_meta_big");
    let mut seen = Vec::new();
    let mut cursor = None;
    let mut pages = 0;
    loop {
        let p = m
            .list(
                &big,
                ObjectKind::Table,
                PageRequest::first().with_limit(500).with_cursor(cursor),
            )
            .await
            .unwrap();
        assert!(p.items.len() <= 500);
        pages += 1;
        seen.extend(p.items.into_iter().map(|o| o.name().to_owned()));
        match p.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(seen.len(), 1205);
    assert_eq!(seen.first().map(String::as_str), Some("t0001"));
    assert_eq!(seen.last().map(String::as_str), Some("t1205"));
    // A limit above the clamp is clamped.
    let p = m
        .list(
            &big,
            ObjectKind::Table,
            PageRequest::first().with_limit(u32::MAX),
        )
        .await
        .unwrap();
    assert_eq!(p.items.len(), 1205);

    // Block the partition query behind a lock on `pg_inherits` held by another session.
    exec(admin.as_mut(), "BEGIN").await.unwrap();
    exec(
        admin.as_mut(),
        "LOCK TABLE pg_catalog.pg_inherits IN ACCESS EXCLUSIVE MODE",
    )
    .await
    .unwrap();
    let events = ObjectPath::root()
        .child(ObjectKind::Schema, "public")
        .child(ObjectKind::Table, "orders");
    let dropped = tokio::time::timeout(
        Duration::from_millis(500),
        m.list(&events, ObjectKind::Partition, PageRequest::first()),
    )
    .await;
    assert!(dropped.is_err(), "the partition query was not blocked");
    // The dropped call was cancelled: the session serves the navigator again while the lock
    // is still held.
    let mut ok = false;
    for _ in 0..50 {
        match m
            .list(
                &ObjectPath::root(),
                ObjectKind::Schema,
                PageRequest::first(),
            )
            .await
        {
            Ok(_) => {
                ok = true;
                break;
            }
            Err(DriverError::Busy) => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(e) => panic!("{e}"),
        }
    }
    assert!(ok, "the abandoned metadata call kept the session busy");
    // The session's cancel handle reaches a running metadata call.
    let m2 = s.metadata();
    let ev = events.clone();
    let task = tokio::spawn(async move {
        m2.list(&ev, ObjectKind::Partition, PageRequest::first())
            .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    s.cancel_handle().cancel().await.unwrap();
    let r = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    assert!(r.is_err(), "the cancelled metadata call succeeded");
    exec(admin.as_mut(), "ROLLBACK").await.unwrap();
    m.list(&events, ObjectKind::Partition, PageRequest::first())
        .await
        .unwrap();
    exec(admin.as_mut(), "DROP SCHEMA ninesql_meta_big CASCADE")
        .await
        .unwrap();
    s.close().await.unwrap();
    admin.close().await.unwrap();
}

async fn scalar(s: &mut dyn Session, sql: &str) -> String {
    exec(s, sql).await.unwrap()[0][0].clone()
}

/// Security review M1: in an open transaction, neither an abandoned (cancelled) navigator
/// call nor one that fails server-side aborts the user's transaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn navigator_calls_keep_an_open_transaction_usable() {
    let Some(dev) = dev() else { return };
    let _serial = SERIAL.lock().await;
    let mut admin = connect(&dev, false).await;
    script(
        admin.as_mut(),
        "DROP TABLE IF EXISTS public.ninesql_meta_tx; CREATE TABLE public.ninesql_meta_tx (a int);",
    )
    .await;
    let mut s = connect(&dev, false).await;
    let m = s.metadata();
    let orders = ObjectPath::root()
        .child(ObjectKind::Schema, "public")
        .child(ObjectKind::Table, "orders");
    exec(s.as_mut(), "BEGIN").await.unwrap();
    exec(s.as_mut(), "INSERT INTO public.ninesql_meta_tx VALUES (1)")
        .await
        .unwrap();
    // A metadata call inside the transaction works and leaves it open.
    m.list(&orders, ObjectKind::Column, PageRequest::first())
        .await
        .unwrap();
    assert!(s.in_transaction());

    // Abandoned while blocked: cancelled server-side.
    exec(admin.as_mut(), "BEGIN").await.unwrap();
    exec(
        admin.as_mut(),
        "LOCK TABLE pg_catalog.pg_inherits IN ACCESS EXCLUSIVE MODE",
    )
    .await
    .unwrap();
    let dropped = tokio::time::timeout(
        Duration::from_millis(500),
        m.list(&orders, ObjectKind::Partition, PageRequest::first()),
    )
    .await;
    assert!(dropped.is_err(), "the partition query was not blocked");
    let mut free = false;
    for _ in 0..50 {
        match m
            .list(
                &ObjectPath::root(),
                ObjectKind::Schema,
                PageRequest::first(),
            )
            .await
        {
            Ok(_) => {
                free = true;
                break;
            }
            Err(DriverError::Busy) => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(e) => panic!("{e}"),
        }
    }
    assert!(free, "the abandoned call kept the session busy");
    // A navigator statement that fails server-side (lock timeout).
    exec(s.as_mut(), "SET LOCAL lock_timeout = '200ms'")
        .await
        .unwrap();
    assert!(
        m.list(&orders, ObjectKind::Partition, PageRequest::first())
            .await
            .is_err()
    );
    exec(admin.as_mut(), "ROLLBACK").await.unwrap();

    // The transaction is still usable and commits the writes.
    assert!(s.in_transaction());
    exec(s.as_mut(), "INSERT INTO public.ninesql_meta_tx VALUES (2)")
        .await
        .unwrap();
    exec(s.as_mut(), "COMMIT").await.unwrap();
    assert_eq!(
        scalar(
            admin.as_mut(),
            "SELECT count(*)::text FROM public.ninesql_meta_tx"
        )
        .await,
        "2"
    );
    exec(admin.as_mut(), "DROP TABLE public.ninesql_meta_tx")
        .await
        .unwrap();
    s.close().await.unwrap();
    admin.close().await.unwrap();
}

/// Security review M2: a rebuilt `CREATE TABLE` keeps a constraint longer than 64 KiB whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_check_constraint_is_kept_whole() {
    let Some(dev) = dev() else { return };
    let _serial = SERIAL.lock().await;
    let mut admin = connect(&dev, false).await;
    let values: Vec<String> = (0..3000)
        .map(|i| format!("'value_number_{i:05}'"))
        .collect();
    let check = format!("CHECK (note IN ({}))", values.join(", "));
    script(
        admin.as_mut(),
        &format!(
            "DROP SCHEMA IF EXISTS ninesql_meta_m2 CASCADE;\n\
             CREATE SCHEMA ninesql_meta_m2;\n\
             CREATE TABLE ninesql_meta_m2.t (note text, CONSTRAINT big {check});"
        ),
    )
    .await;
    let s = connect(&dev, true).await;
    let m = s.metadata();
    let t = ObjectPath::root()
        .child(ObjectKind::Schema, "ninesql_meta_m2")
        .child(ObjectKind::Table, "t");
    let ddl = m.ddl(&t).await.unwrap();
    assert!(ddl.len() > 64 * 1024, "{}", ddl.len());
    assert!(ddl.contains("value_number_02999"));
    assert!(ddl.ends_with(");"), "{}", &ddl[ddl.len() - 40..]);
    let con = m
        .ddl(&t.child(ObjectKind::Constraint, "big"))
        .await
        .unwrap();
    assert!(con.contains("value_number_02999"));
    // The DDL re-creates the table.
    exec(admin.as_mut(), "DROP TABLE ninesql_meta_m2.t")
        .await
        .unwrap();
    script(admin.as_mut(), &ddl).await;
    assert_eq!(m.ddl(&t).await.unwrap(), ddl);
    exec(admin.as_mut(), "DROP SCHEMA ninesql_meta_m2 CASCADE")
        .await
        .unwrap();
    s.close().await.unwrap();
    admin.close().await.unwrap();
}

/// Security review L3: a listing past the internal 32 MiB cap fails without closing the
/// session or aborting its transaction; a smaller page works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_listing_fails_without_closing_the_session() {
    let Some(dev) = dev() else { return };
    let _serial = SERIAL.lock().await;
    let mut admin = connect(&dev, false).await;
    script(
        admin.as_mut(),
        "DROP SCHEMA IF EXISTS ninesql_meta_l3 CASCADE;\n\
         CREATE SCHEMA ninesql_meta_l3;\n\
         DO $$ BEGIN FOR i IN 1..600 LOOP \
         EXECUTE pg_catalog.format('CREATE TABLE ninesql_meta_l3.%I (id int)', 't' || i); \
         EXECUTE pg_catalog.format('COMMENT ON TABLE ninesql_meta_l3.%I IS %L', 't' || i, \
           pg_catalog.repeat('c', 60000)); \
         END LOOP; END $$;",
    )
    .await;
    let mut s = connect(&dev, false).await;
    exec(s.as_mut(), "BEGIN").await.unwrap();
    let m = s.metadata();
    let sch = ObjectPath::root().child(ObjectKind::Schema, "ninesql_meta_l3");
    match m
        .list(
            &sch,
            ObjectKind::Table,
            PageRequest::first().with_limit(600),
        )
        .await
    {
        Err(DriverError::Query { message, .. }) => assert!(message.contains("32 MiB")),
        other => panic!("{:?}", other.map(|p| p.items.len())),
    }
    assert!(s.in_transaction());
    let small = m
        .list(
            &sch,
            ObjectKind::Table,
            PageRequest::first().with_limit(100),
        )
        .await
        .unwrap();
    assert_eq!(small.items.len(), 100);
    exec(s.as_mut(), "COMMIT").await.unwrap();
    exec(admin.as_mut(), "DROP SCHEMA ninesql_meta_l3 CASCADE")
        .await
        .unwrap();
    s.close().await.unwrap();
    admin.close().await.unwrap();
}
