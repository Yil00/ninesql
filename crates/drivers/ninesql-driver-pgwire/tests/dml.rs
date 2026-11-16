//! DML generation (P5-B): offline checks of the PostgreSQL-family dialects, and engine tests
//! against the `postgresql` and `timescaledb` services of `dev/compose.yaml` (ADR-0009): every
//! row change runs once through its parameterized form and once through its preview, and both
//! must leave the same row state.
//!
//! Engine tests are skipped when an engine is unreachable, unless `NINESQL_REQUIRE_ENGINES`
//! lists it. Start them with `python3 dev/ready.py up sql --engines postgresql,timescaledb`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Duration;

use ninesql_driver_api::arrow_array::{Array, RecordBatch};
use ninesql_driver_api::arrow_schema::{DataType, SchemaRef};
use ninesql_driver_api::dml::{self, ColumnType, needs_escape};
use ninesql_driver_api::{
    Auth, ColumnChange, ColumnValue, Concurrency, ConnectParams, Dialect, DmlColumn, DmlError,
    DmlRequest, Driver, DriverError, Endpoint, EngineId, ExecOptions, KeyKind, Param, ResultItem,
    ResultSetKind, Secret, Session, Statement, StatementClass, TlsMode, cell_value,
};
use ninesql_driver_pgwire::{PgDialect, PgWireDriver};
use proptest::prelude::*;

// ---------------------------------------------------------------------------------------------
// Offline

fn editing(engine: &EngineId) -> ninesql_driver_api::EditingCapabilities {
    PgWireDriver::new()
        .capabilities(engine, None)
        .unwrap()
        .editing
}

fn dialects() -> Vec<PgDialect> {
    vec![
        PgDialect::new(),
        PgDialect::new().with_scs(false),
        PgDialect::timescaledb(),
        PgDialect::yugabytedb().with_scs(false),
    ]
}

fn hostile_text() -> impl Strategy<Value = String> + Clone {
    let pieces = prop::sample::select(vec![
        "'",
        "''",
        "\"",
        "\"\"",
        "`",
        "[",
        "]",
        "\\",
        "\\'",
        "--",
        "/*",
        "*/",
        "$$",
        "$a$",
        "$1",
        ";",
        "\n",
        "\r",
        "\t",
        "\u{0}",
        "\u{1b}",
        "\u{7f}",
        "\u{85}",
        "\u{202e}",
        "\u{2066}",
        "\u{200b}",
        "\u{feff}",
        "é",
        "日本",
        "😀",
        "?",
        ":a",
        "@a",
        "E'",
        "U&'",
        "WHERE",
        "DROP TABLE t",
        "select",
        "x",
    ]);
    prop::collection::vec(pieces, 0..10).prop_map(|v| v.concat())
}

fn any_param() -> impl Strategy<Value = Param> {
    prop_oneof![
        Just(Param::Null),
        any::<bool>().prop_map(Param::Bool),
        any::<i64>().prop_map(Param::Int),
        any::<f64>().prop_map(Param::Float),
        hostile_text().prop_map(Param::Decimal),
        hostile_text().prop_map(Param::Text),
        hostile_text().prop_map(Param::Json),
        prop::collection::vec(any::<u8>(), 0..16).prop_map(Param::Bytes),
        any::<i32>().prop_map(Param::Date),
        (any::<i64>(), any::<bool>()).prop_map(|(micros, utc)| Param::Timestamp { micros, utc }),
    ]
}

fn any_type() -> impl Strategy<Value = ColumnType> {
    prop_oneof![
        Just(ColumnType::new(DataType::Utf8)),
        Just(ColumnType::new(DataType::Int64)),
        Just(ColumnType::new(DataType::Float64)),
        Just(ColumnType::new(DataType::Boolean)),
        Just(ColumnType::new(DataType::Binary)),
        hostile_text().prop_map(|t| ColumnType::new(DataType::Utf8).with_engine_type(t)),
        prop::sample::select(vec![
            "uuid",
            "jsonb",
            "integer[]",
            "numeric(10,2)",
            "interval"
        ])
        .prop_map(|t| ColumnType::new(DataType::Utf8).with_engine_type(t)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Hostile names, values and engine type names: generation refuses with a typed error or
    /// returns texts that split as one statement and classify as a write, in every dialect.
    #[test]
    fn hostile_input_is_refused_or_safe(
        table in prop::collection::vec(hostile_text(), 1..3),
        column in hostile_text(),
        ty in any_type(),
        old in any_param(),
        new in any_param(),
    ) {
        let caps = editing(&EngineId::POSTGRESQL);
        for d in dialects() {
            let c = DmlColumn::new(column.clone(), ty.clone());
            let key = vec![ColumnValue::new(DmlColumn::new("id", ColumnType::new(DataType::Int32)), Param::Int(1))];
            let reqs = [
                DmlRequest::insert(table.clone(), vec![ColumnValue::new(c.clone(), new.clone())]),
                DmlRequest::update(table.clone(), KeyKind::PrimaryKey, key.clone(),
                    vec![ColumnChange::new(c.clone(), old.clone(), new.clone())])
                    .with_concurrency(Concurrency::EditedColumns),
                DmlRequest::delete(table.clone(), KeyKind::PrimaryKey, vec![ColumnValue::new(c.clone(), old.clone())]),
            ];
            for req in &reqs {
                match dml::generate(&d, &caps, req) {
                    Ok(s) => {
                        for text in [&s.statement.text, &s.preview] {
                            prop_assert_eq!(d.classify(text), StatementClass::Write);
                            prop_assert_eq!(d.split_statements(text).len(), 1);
                        }
                        prop_assert!(!s.preview.chars().any(|c| needs_escape(c) && !column.contains(c) && !table.iter().any(|t| t.contains(c))));
                    }
                    Err(DmlError::SelfCheck(why)) => prop_assert!(false, "self-check: {}", why),
                    Err(_) => {}
                }
            }
            if let Ok(b) = dml::generate_batch(&d, &caps, &reqs) {
                prop_assert_eq!(d.split_statements(&b.preview_script()).len(), 3);
            }
        }
    }

    /// A literal does not depend on `standard_conforming_strings` (security review H1, L3).
    #[test]
    fn literals_are_mode_independent(v in any_param(), ty in any_type()) {
        let on = PgDialect::new().dml_literal(&v, &ty);
        let off = PgDialect::new().with_scs(false).dml_literal(&v, &ty);
        prop_assert_eq!(on, off);
    }

    /// A preview literal is one token: `SELECT <literal>` is one read statement.
    #[test]
    fn literals_stay_in_their_token(v in any_param(), ty in any_type()) {
        for d in dialects() {
            if let Ok(lit) = d.dml_literal(&v, &ty) {
                let text = format!("SELECT {lit}");
                prop_assert_eq!(d.split_statements(&text).len(), 1);
                prop_assert_eq!(d.classify(&text), StatementClass::Read);
                let text = format!("SELECT {lit} -- x\n; DELETE FROM t");
                prop_assert_eq!(d.split_statements(&text).len(), 2);
            }
        }
    }
}

#[test]
fn questdb_declares_no_editing_and_is_refused() {
    let caps = editing(&EngineId::QUESTDB);
    assert!(!caps.any());
    let d = PgDialect::questdb();
    let req = DmlRequest::insert(vec!["t".into()], vec![]);
    assert!(matches!(
        dml::generate(&d, &caps, &req),
        Err(DmlError::NotSupported(_))
    ));
    let err: DriverError = dml::generate(&d, &caps, &req).unwrap_err().into();
    assert!(matches!(err, DriverError::Unsupported(_)));
    // The export text needs no editing capability.
    assert_eq!(
        dml::insert_preview(
            &d,
            &["t".into()],
            &[ColumnValue::new(
                DmlColumn::new("a", ColumnType::new(DataType::Utf8)),
                Param::Text("x".into())
            )]
        )
        .unwrap(),
        "INSERT INTO \"t\" (\"a\") VALUES ('x')"
    );
}

#[test]
fn identifiers_postgres_would_truncate_are_refused() {
    let caps = editing(&EngineId::POSTGRESQL);
    let long = "a".repeat(64);
    let req = DmlRequest::insert(
        vec![long],
        vec![ColumnValue::new(
            DmlColumn::new("a", ColumnType::new(DataType::Int32)),
            Param::Int(1),
        )],
    );
    assert!(matches!(
        dml::generate(&PgDialect::new(), &caps, &req),
        Err(DmlError::InvalidIdentifier { .. })
    ));
}

// ---------------------------------------------------------------------------------------------
// Engine helpers

struct Dev {
    engine: EngineId,
    host: String,
    port: u16,
    user: String,
    password: String,
    database: String,
}

fn dev(id: &str) -> Option<Dev> {
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
        engine: EngineId::new(id),
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
    eprintln!("skipped: {id} unreachable at {addr} (python3 dev/ready.py up sql)");
    None
}

/// Runs `f` on the session held by `cell` (taken out for the call, so no borrow is held
/// across an await point).
fn with_session<R>(
    rt: &tokio::runtime::Runtime,
    cell: &RefCell<Option<Box<dyn Session>>>,
    f: impl AsyncFnOnce(&mut dyn Session) -> R,
) -> R {
    let mut s = cell.borrow_mut().take().expect("session in use");
    let r = rt.block_on(f(s.as_mut()));
    *cell.borrow_mut() = Some(s);
    r
}

async fn connect(dev: &Dev) -> Box<dyn Session> {
    let mut p = ConnectParams::new(dev.engine.clone(), Endpoint::tcp(&dev.host, dev.port));
    p.auth = Auth::password(&dev.user, Some(Secret::new(dev.password.clone())));
    p.database = Some(dev.database.clone());
    p.tls.mode = TlsMode::Disable;
    p.application_name = "ninesql-pgwire-dml-tests".into();
    p.connect_timeout = Duration::from_secs(30);
    PgWireDriver::new().connect(p).await.unwrap()
}

#[derive(Default)]
struct Out {
    schema: Option<SchemaRef>,
    batches: Vec<RecordBatch>,
    counts: Vec<Option<u64>>,
}

async fn exec(s: &mut dyn Session, st: Statement) -> Result<Out, DriverError> {
    let mut stream = s.execute(st, ExecOptions::default()).await?;
    let mut o = Out::default();
    while let Some(item) = stream.next().await {
        match item? {
            ResultItem::ResultSet(h) => {
                if let ResultSetKind::Tabular(schema) = h.kind {
                    o.schema = Some(schema);
                }
            }
            ResultItem::Rows(b) => o.batches.push(b),
            ResultItem::UpdateCount(n) => o.counts.push(n),
            _ => {}
        }
    }
    Ok(o)
}

async fn sql(s: &mut dyn Session, text: &str) -> Out {
    exec(s, Statement::sql(text)).await.unwrap()
}

/// Runs `st` and checks that exactly one row changed.
async fn run_dml(s: &mut dyn Session, st: &dml::DmlStatement) {
    let out = exec(s, st.statement.clone()).await.unwrap();
    st.check_affected(out.counts.last().copied().flatten())
        .unwrap();
}

/// Runs the preview text of `st` (no parameters) and checks that exactly one row changed.
async fn run_preview(s: &mut dyn Session, st: &dml::DmlStatement) {
    let out = exec(s, Statement::sql(st.preview.clone()))
        .await
        .unwrap_or_else(|e| panic!("preview failed: {e} ({} bytes)", st.preview.len()));
    st.check_affected(out.counts.last().copied().flatten())
        .unwrap();
}

/// The table's columns, as a `SELECT *` describes them.
async fn columns(s: &mut dyn Session, table: &str) -> Vec<DmlColumn> {
    let out = sql(s, &format!("SELECT * FROM {table} WHERE false")).await;
    out.schema
        .unwrap()
        .fields()
        .iter()
        .map(|f| DmlColumn::from_field(f))
        .collect()
}

/// The row whose `id` is `id`, read back as parameters through [`cell_value`].
async fn read_row(s: &mut dyn Session, table: &str, id: i64) -> Vec<Param> {
    let out = sql(s, &format!("SELECT * FROM {table} WHERE id = {id}")).await;
    let b = out.batches.iter().find(|b| b.num_rows() > 0).unwrap();
    (0..b.num_columns())
        .map(|i| cell_value(b, i, 0).unwrap())
        .collect()
}

/// `true` when rows `a` and `b` hold the same values in every column but `id` (compared as
/// text, which is exact for floats and covers types without `=`).
async fn same_rows(s: &mut dyn Session, table: &str, cols: &[DmlColumn], a: i64, b: i64) -> bool {
    let d = s.dialect();
    let cond: Vec<String> = cols
        .iter()
        .filter(|c| c.name != "id")
        .map(|c| {
            let q = d.quote_identifier(&c.name);
            format!("x.{q}::text IS NOT DISTINCT FROM y.{q}::text")
        })
        .collect();
    let out = sql(
        s,
        &format!(
            "SELECT count(*)::int8 FROM {table} x, {table} y WHERE x.id = {a} AND y.id = {b} AND {}",
            cond.join(" AND ")
        ),
    )
    .await;
    let col = out.batches[0].column(0);
    col.as_any()
        .downcast_ref::<ninesql_driver_api::arrow_array::Int64Array>()
        .unwrap()
        .value(0)
        == 1
}

fn values(cols: &[DmlColumn], id: i64, vals: &[Param]) -> Vec<ColumnValue> {
    cols.iter()
        .zip(std::iter::once(Param::Int(id)).chain(vals.iter().cloned()))
        .map(|(c, v)| ColumnValue::new(c.clone(), v))
        .collect()
}

fn id_key(cols: &[DmlColumn], id: i64) -> Vec<ColumnValue> {
    vec![ColumnValue::new(cols[0].clone(), Param::Int(id))]
}

/// Inserts row `a` through the parameterized form and row `b` through the preview, then
/// updates both to `next` the same way (with the concurrency guard), and checks that they
/// match after each step.
async fn round_trip(
    s: &mut dyn Session,
    table: &str,
    cols: &[DmlColumn],
    first: &[Param],
    next: &[Param],
) {
    let d = s.dialect();
    let caps = s.capabilities().editing;
    sql(s, &format!("DELETE FROM {table} WHERE id IN (1, 2)")).await;
    let ins_a = dml::generate(
        &*d,
        &caps,
        &DmlRequest::insert(vec![table.into()], values(cols, 1, first)),
    )
    .unwrap();
    let ins_b = dml::generate(
        &*d,
        &caps,
        &DmlRequest::insert(vec![table.into()], values(cols, 2, first)),
    )
    .unwrap();
    run_dml(s, &ins_a).await;
    run_preview(s, &ins_b).await;
    assert!(
        same_rows(s, table, cols, 1, 2).await,
        "insert: preview and bound rows differ"
    );
    for (id, preview) in [(1, false), (2, true)] {
        let original = read_row(s, table, id).await;
        let changes: Vec<ColumnChange> = cols[1..]
            .iter()
            .zip(&original[1..])
            .zip(next)
            .map(|((c, o), n)| ColumnChange::new(c.clone(), o.clone(), n.clone()))
            .collect();
        let req = DmlRequest::update(
            vec![table.into()],
            KeyKind::PrimaryKey,
            id_key(cols, id),
            changes,
        )
        .with_concurrency(Concurrency::EditedColumns);
        let st = dml::generate(&*d, &caps, &req).unwrap();
        if preview {
            run_preview(s, &st).await;
        } else {
            run_dml(s, &st).await;
        }
        // The guard now holds stale originals: nothing matches.
        let again = exec(s, st.statement.clone()).await.unwrap();
        assert!(
            st.check_affected(again.counts.last().copied().flatten())
                .is_err()
        );
    }
    assert!(
        same_rows(s, table, cols, 1, 2).await,
        "update: preview and bound rows differ"
    );
}

// ---------------------------------------------------------------------------------------------
// Engine tests

const TYPES_DDL: &str = "CREATE TABLE {t} (id int8, k2 text, b boolean, i2 int2, i4 int4, \
    f4 real, f8 double precision, n numeric, t text, vc varchar(50), by bytea, d date, tm time, \
    ts timestamp, tz timestamptz, iv interval, j jsonb, js json, u uuid, arr int4[], tarr text[], \
    PRIMARY KEY (id, k2))";

fn types_values(variant: u8) -> Vec<Param> {
    let text = if variant == 0 {
        "it's \\ -- /* $$ $a$ ; \n\t\u{1b}\u{202e} é 日本 😀".to_owned()
    } else {
        "plain".to_owned()
    };
    vec![
        Param::Text("a".into()),
        Param::Bool(variant == 0),
        Param::Int(-32768),
        Param::Int(2_147_483_647),
        Param::Float(0.1_f32.into()),
        Param::Float(if variant == 0 { 0.1 } else { f64::NAN }),
        Param::Decimal(if variant == 0 {
            "-12345678901234567890.000000000123".into()
        } else {
            "NaN".into()
        }),
        Param::Text(text.clone()),
        Param::Text(text.chars().take(50).collect()),
        Param::Bytes(if variant == 0 {
            vec![0, b'\\', b'\'', 0xff, 0xfe]
        } else {
            vec![]
        }),
        Param::Date(if variant == 0 { -719_162 } else { 2_932_896 }),
        Param::Text(if variant == 0 {
            "23:59:59.999999".into()
        } else {
            "00:00:00".into()
        }),
        Param::Timestamp {
            micros: 1_700_000_000_123_456,
            utc: false,
        },
        Param::Timestamp {
            micros: -1_234_567,
            utc: true,
        },
        Param::Text(if variant == 0 {
            "1 year 2 mons -3 days 04:05:06.789".into()
        } else {
            "00:00:00".into()
        }),
        Param::Json(r#"{"a": "it's \"q\" \\ \u0001", "b": [1, 2.5, null]}"#.into()),
        Param::Json(r#"{"x" : 1}"#.into()),
        Param::Text("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".into()),
        Param::Text(if variant == 0 {
            "{1,NULL,-3}".into()
        } else {
            "{}".into()
        }),
        Param::Text(r#"{"it's","a\"b","c\\d",NULL}"#.into()),
    ]
}

async fn types_round_trip(engine: &str) {
    let Some(dev) = dev(engine) else { return };
    let mut s = connect(&dev).await;
    let s = s.as_mut();
    let t = format!("p5b_types_{}", engine);
    sql(s, &format!("DROP TABLE IF EXISTS {t}")).await;
    sql(s, &TYPES_DDL.replace("{t}", &t)).await;
    let cols = columns(s, &t).await;
    // The composite key is (id, k2); the round trip keys on both.
    round_trip_composite(s, &t, &cols, &types_values(0), &types_values(1)).await;
    // The comparison sees a difference.
    sql(s, &format!("UPDATE {t} SET t = t || 'x' WHERE id = 2")).await;
    assert!(!same_rows(s, &t, &cols, 1, 2).await);
    sql(s, &format!("UPDATE {t} SET t = left(t, -1) WHERE id = 2")).await;
    assert!(same_rows(s, &t, &cols, 1, 2).await);
    // Delete through both forms.
    let d = s.dialect();
    let caps = s.capabilities().editing;
    for (id, preview) in [(1, false), (2, true)] {
        let key = vec![
            ColumnValue::new(cols[0].clone(), Param::Int(id)),
            ColumnValue::new(cols[1].clone(), Param::Text("a".into())),
        ];
        let original = read_row(s, &t, id).await;
        let check: Vec<ColumnValue> = cols
            .iter()
            .cloned()
            .zip(original)
            .map(|(c, v)| ColumnValue::new(c, v))
            .collect();
        let req = DmlRequest::delete(vec![t.clone()], KeyKind::PrimaryKey, key)
            .with_concurrency(Concurrency::EditedColumns)
            .with_delete_check(check);
        let st = dml::generate(&*d, &caps, &req).unwrap();
        assert_eq!(st.unchecked_columns, vec!["js".to_owned()]);
        if preview {
            run_preview(s, &st).await;
        } else {
            run_dml(s, &st).await;
        }
    }
    let left = sql(s, &format!("SELECT count(*)::int8 FROM {t}")).await;
    assert_eq!(
        left.batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<ninesql_driver_api::arrow_array::Int64Array>()
            .unwrap()
            .value(0),
        0
    );
    sql(s, &format!("DROP TABLE {t}")).await;
}

/// [`round_trip`] keyed on `(id, k2)`.
async fn round_trip_composite(
    s: &mut dyn Session,
    table: &str,
    cols: &[DmlColumn],
    first: &[Param],
    next: &[Param],
) {
    let d = s.dialect();
    let caps = s.capabilities().editing;
    let ins_a = dml::generate(
        &*d,
        &caps,
        &DmlRequest::insert(vec![table.into()], values(cols, 1, first)),
    )
    .unwrap();
    let ins_b = dml::generate(
        &*d,
        &caps,
        &DmlRequest::insert(vec![table.into()], values(cols, 2, first)),
    )
    .unwrap();
    run_dml(s, &ins_a).await;
    run_preview(s, &ins_b).await;
    assert!(
        same_rows(s, table, cols, 1, 2).await,
        "insert: preview and bound rows differ"
    );
    for (id, preview) in [(1, false), (2, true)] {
        let original = read_row(s, table, id).await;
        let key = vec![
            ColumnValue::new(cols[0].clone(), Param::Int(id)),
            ColumnValue::new(cols[1].clone(), original[1].clone()),
        ];
        let changes: Vec<ColumnChange> = cols[2..]
            .iter()
            .zip(&original[2..])
            .zip(&next[1..])
            .map(|((c, o), n)| ColumnChange::new(c.clone(), o.clone(), n.clone()))
            .collect();
        let req = DmlRequest::update(vec![table.into()], KeyKind::PrimaryKey, key, changes)
            .with_concurrency(Concurrency::EditedColumns);
        let st = dml::generate(&*d, &caps, &req).unwrap();
        assert_eq!(st.unchecked_columns, vec!["js".to_owned()]);
        if preview {
            run_preview(s, &st).await;
        } else {
            run_dml(s, &st).await;
        }
        let again = exec(s, st.statement.clone()).await.unwrap();
        assert!(
            st.check_affected(again.counts.last().copied().flatten())
                .is_err()
        );
    }
    assert!(
        same_rows(s, table, cols, 1, 2).await,
        "update: preview and bound rows differ"
    );
}

#[tokio::test]
async fn postgresql_types_round_trip() {
    types_round_trip("postgresql").await;
}

#[tokio::test]
async fn timescaledb_types_round_trip() {
    types_round_trip("timescaledb").await;
}

#[tokio::test]
async fn postgresql_keys() {
    let Some(dev) = dev("postgresql") else { return };
    let mut s = connect(&dev).await;
    let s = s.as_mut();
    let d = s.dialect();
    let caps = s.capabilities().editing;
    sql(s, "DROP TABLE IF EXISTS p5b_nokey, p5b_uq, p5b_uqnd").await;
    sql(s, "CREATE TABLE p5b_nokey (a int, b text)").await;
    sql(s, "INSERT INTO p5b_nokey VALUES (1, 'x'), (1, 'x')").await;
    let cols = columns(s, "p5b_nokey").await;
    // No key: refused, nothing is generated.
    let req = DmlRequest::update(
        vec!["p5b_nokey".into()],
        KeyKind::PrimaryKey,
        vec![],
        vec![ColumnChange::new(
            cols[1].clone(),
            Param::Text("x".into()),
            Param::Text("y".into()),
        )],
    );
    assert_eq!(dml::generate(&*d, &caps, &req), Err(DmlError::NoKey));
    assert_eq!(
        dml::generate(
            &*d,
            &caps,
            &DmlRequest::delete(vec!["p5b_nokey".into()], KeyKind::PrimaryKey, vec![])
        ),
        Err(DmlError::NoKey)
    );

    // A nullable unique key: rows with a value are edited, rows with NULL are refused.
    sql(s, "CREATE TABLE p5b_uq (u int UNIQUE, v text)").await;
    sql(
        s,
        "INSERT INTO p5b_uq VALUES (1, 'a'), (NULL, 'b'), (NULL, 'c')",
    )
    .await;
    let cols = columns(s, "p5b_uq").await;
    let uq = KeyKind::Unique {
        nulls_distinct: true,
    };
    let upd = |u: Param| {
        DmlRequest::update(
            vec!["p5b_uq".into()],
            uq,
            vec![ColumnValue::new(cols[0].clone(), u)],
            vec![ColumnChange::new(
                cols[1].clone(),
                Param::Null,
                Param::Text("z".into()),
            )],
        )
    };
    let st = dml::generate(&*d, &caps, &upd(Param::Int(1))).unwrap();
    run_dml(s, &st).await;
    // Writing the same value again still reports one row.
    run_preview(s, &st).await;
    assert!(matches!(
        dml::generate(&*d, &caps, &upd(Param::Null)),
        Err(DmlError::NullKey(_))
    ));

    // `NULLS NOT DISTINCT` (PostgreSQL 15+): a NULL key value identifies one row (`IS NULL`).
    sql(
        s,
        "CREATE TABLE p5b_uqnd (u int UNIQUE NULLS NOT DISTINCT, v text)",
    )
    .await;
    sql(s, "INSERT INTO p5b_uqnd VALUES (1, 'a'), (NULL, 'b')").await;
    let cols = columns(s, "p5b_uqnd").await;
    let req = DmlRequest::delete(
        vec!["p5b_uqnd".into()],
        KeyKind::Unique {
            nulls_distinct: false,
        },
        vec![ColumnValue::new(cols[0].clone(), Param::Null)],
    );
    let st = dml::generate(&*d, &caps, &req).unwrap();
    assert!(
        st.preview.ends_with("WHERE \"u\" IS NULL"),
        "{}",
        st.preview
    );
    run_preview(s, &st).await;
    sql(s, "DROP TABLE p5b_nokey, p5b_uq, p5b_uqnd").await;
}

#[tokio::test]
async fn postgresql_hostile_identifiers() {
    let Some(dev) = dev("postgresql") else { return };
    let mut s = connect(&dev).await;
    let s = s.as_mut();
    let d = s.dialect();
    let caps = s.capabilities().editing;
    let schema = "p5b \"s\"; --";
    let table = "t'`[]\\ /* $$ \u{e9}";
    let column = "select \"x\" -- ;\n";
    let qs = d.quote_identifier(schema);
    let qt = d.qualified_name(&[schema.into(), table.into()]);
    let qc = d.quote_identifier(column);
    sql(s, &format!("DROP SCHEMA IF EXISTS {qs} CASCADE")).await;
    sql(s, &format!("CREATE SCHEMA {qs}")).await;
    sql(
        s,
        &format!("CREATE TABLE {qt} (id int8 PRIMARY KEY, {qc} text)"),
    )
    .await;
    let out = sql(s, &format!("SELECT * FROM {qt} WHERE false")).await;
    let cols: Vec<DmlColumn> = out
        .schema
        .unwrap()
        .fields()
        .iter()
        .map(|f| DmlColumn::from_field(f))
        .collect();
    let path = vec![schema.to_owned(), table.to_owned()];
    for (id, preview) in [(1, false), (2, true)] {
        let st = dml::generate(
            &*d,
            &caps,
            &DmlRequest::insert(
                path.clone(),
                values(&cols, id, &[Param::Text("v'; DROP TABLE x; --".into())]),
            ),
        )
        .unwrap();
        if preview {
            run_preview(s, &st).await;
        } else {
            run_dml(s, &st).await;
        }
    }
    let st = dml::generate(
        &*d,
        &caps,
        &DmlRequest::delete(path.clone(), KeyKind::PrimaryKey, id_key(&cols, 2)),
    )
    .unwrap();
    run_preview(s, &st).await;
    sql(s, &format!("DROP SCHEMA {qs} CASCADE")).await;
}

#[test]
fn postgresql_preview_round_trips_property() {
    let Some(dev) = dev("postgresql") else { return };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let session = RefCell::new(Some(rt.block_on(connect(&dev))));
    let table = "p5b_prop";
    let cols = with_session(&rt, &session, async |s: &mut dyn Session| {
        sql(s, &format!("DROP TABLE IF EXISTS {table}")).await;
        sql(
            s,
            &format!(
                "CREATE TABLE {table} (id int8 PRIMARY KEY, t text, by bytea, i8 int8, f8 float8, \
                 f4 real, n numeric, d date, ts timestamp, tz timestamptz, b boolean, j jsonb)"
            ),
        )
        .await;
        columns(s, table).await
    });
    let text = hostile_text().prop_map(|s| s.replace('\0', "")).boxed();
    let row = (
        text.clone(),
        prop::collection::vec(any::<u8>(), 0..24),
        any::<i64>(),
        any::<f64>(),
        any::<f32>(),
        (any::<i64>(), 0u32..12).prop_map(|(m, scale)| {
            let mut s = m.unsigned_abs().to_string();
            if scale > 0 {
                while s.len() <= scale as usize {
                    s.insert(0, '0');
                }
                s.insert(s.len() - scale as usize, '.');
            }
            if m < 0 { format!("-{s}") } else { s }
        }),
        -719_162i32..2_932_896,
        -62_135_596_800_000_000i64..253_402_300_799_999_999,
        any::<bool>(),
        text.prop_map(|t| serde_json::json!({ "k": t }).to_string()),
    )
        .prop_map(|(t, by, i8, f8, f4, n, d, ts, b, j)| {
            vec![
                Param::Text(t),
                Param::Bytes(by),
                Param::Int(i8),
                Param::Float(f8),
                Param::Float(f4.into()),
                Param::Decimal(n),
                Param::Date(d),
                Param::Timestamp {
                    micros: ts,
                    utc: false,
                },
                Param::Timestamp {
                    micros: ts,
                    utc: true,
                },
                Param::Bool(b),
                Param::Json(j),
            ]
        });
    let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig::with_cases(64));
    runner
        .run(
            &(row.clone(), row),
            |(first, next): (Vec<Param>, Vec<Param>)| {
                with_session(&rt, &session, async |s: &mut dyn Session| {
                    round_trip(s, table, &cols, &first, &next).await;
                });
                Ok(())
            },
        )
        .unwrap();
    with_session(&rt, &session, async |s: &mut dyn Session| {
        sql(s, &format!("DROP TABLE {table}")).await;
    });
}

/// Row-key choice uses the driver's index flags (P5-B review M1): an invalid index (left by a
/// failed `CREATE UNIQUE INDEX CONCURRENTLY`), a deferrable unique constraint, an expression
/// index and a partial index never count; `NULLS NOT DISTINCT` is reported.
#[tokio::test]
async fn postgresql_row_key_from_index_flags() {
    use ninesql_driver_api::{
        KeyCandidate, ObjectDetails, ObjectKind, ObjectPath, PageRequest, choose_row_key,
    };
    let Some(dev) = dev("postgresql") else { return };
    let mut s = connect(&dev).await;
    let s = s.as_mut();
    sql(s, "DROP TABLE IF EXISTS p5b_idx").await;
    sql(
        s,
        "CREATE TABLE p5b_idx (a int NOT NULL, b int NOT NULL, c text NOT NULL, d int NOT NULL, \
         e int, CONSTRAINT p5b_idx_b UNIQUE (b) DEFERRABLE INITIALLY IMMEDIATE)",
    )
    .await;
    sql(
        s,
        "INSERT INTO p5b_idx VALUES (1, 1, 'x', 1, NULL), (1, 2, 'y', 2, 2)",
    )
    .await;
    // Fails on the duplicate `a`, leaving an invalid index behind.
    assert!(
        exec(
            s,
            Statement::sql("CREATE UNIQUE INDEX CONCURRENTLY p5b_idx_a ON p5b_idx (a)")
        )
        .await
        .is_err()
    );
    sql(s, "CREATE UNIQUE INDEX p5b_idx_c ON p5b_idx (lower(c))").await;
    sql(
        s,
        "CREATE UNIQUE INDEX p5b_idx_d ON p5b_idx (d) WHERE d > 0",
    )
    .await;
    let table = ObjectPath::root()
        .child(ObjectKind::Schema, "public")
        .child(ObjectKind::Table, "p5b_idx");
    let page = s
        .metadata()
        .list(&table, ObjectKind::Index, PageRequest::first())
        .await
        .unwrap();
    let index = |name: &str| match &page
        .items
        .iter()
        .find(|o| o.name() == name)
        .unwrap()
        .details
    {
        ObjectDetails::Index(i) => i.clone(),
        other => panic!("{other:?}"),
    };
    let a = index("p5b_idx_a");
    assert!(!a.valid && a.unique, "{a:?}");
    let b = index("p5b_idx_b");
    assert!(b.valid && b.deferrable && b.unique, "{b:?}");
    let c = index("p5b_idx_c");
    assert!(
        c.unique && c.expressions == vec![true] && c.has_expression(),
        "{c:?}"
    );
    let d = index("p5b_idx_d");
    assert!(
        d.unique && d.partial && d.predicate.is_some() && d.expressions == vec![false],
        "{d:?}"
    );
    let mut candidates: Vec<KeyCandidate> = Vec::new();
    for i in [&a, &b, &c, &d] {
        let mut forced = i.clone();
        // Even if reported unique, the flags alone rule them out.
        forced.unique = true;
        candidates.extend(KeyCandidate::from_index(&forced, true));
    }
    assert_eq!(choose_row_key(&candidates), None);
    // A plain unique index, and one with NULLS NOT DISTINCT (PostgreSQL 15+).
    sql(
        s,
        "CREATE UNIQUE INDEX p5b_idx_e ON p5b_idx (e) NULLS NOT DISTINCT",
    )
    .await;
    let page = s
        .metadata()
        .list(&table, ObjectKind::Index, PageRequest::first())
        .await
        .unwrap();
    let e = match &page
        .items
        .iter()
        .find(|o| o.name() == "p5b_idx_e")
        .unwrap()
        .details
    {
        ObjectDetails::Index(i) => i.clone(),
        other => panic!("{other:?}"),
    };
    assert!(
        e.valid && e.unique && !e.nulls_distinct && !e.deferrable,
        "{e:?}"
    );
    candidates.extend(KeyCandidate::from_index(&e, false));
    assert_eq!(
        choose_row_key(&candidates),
        Some((
            vec!["e".to_owned()],
            KeyKind::Unique {
                nulls_distinct: false
            }
        ))
    );
    assert!(s.dialect().supports_unique_nulls_not_distinct());
    sql(s, "DROP TABLE p5b_idx").await;
}

#[test]
fn nulls_not_distinct_needs_postgresql_15() {
    let caps = editing(&EngineId::POSTGRESQL);
    let key = vec![ColumnValue::new(
        DmlColumn::new("u", ColumnType::new(DataType::Int32)),
        Param::Null,
    )];
    let req = DmlRequest::delete(
        vec!["t".into()],
        KeyKind::Unique {
            nulls_distinct: false,
        },
        key,
    );
    assert_eq!(
        dml::generate(&PgDialect::new(), &caps, &req),
        Err(DmlError::NullsNotDistinctUnsupported)
    );
    assert_eq!(
        dml::generate(
            &PgDialect::yugabytedb().with_server_major(Some(11)),
            &caps,
            &req
        ),
        Err(DmlError::NullsNotDistinctUnsupported)
    );
    let st = dml::generate(&PgDialect::new().with_server_major(Some(15)), &caps, &req).unwrap();
    assert!(st.preview.ends_with("WHERE \"u\" IS NULL"));
}
