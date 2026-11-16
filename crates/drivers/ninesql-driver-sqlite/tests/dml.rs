//! DML generation (P5-B) on SQLite: offline checks of the dialect, and tests on temporary
//! database files: every row change runs once through its parameterized form and once through
//! its preview, and both must leave the same row state (values and storage classes).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::cell::RefCell;

use ninesql_driver_api::arrow_array::{Array, RecordBatch};
use ninesql_driver_api::arrow_schema::{DataType, SchemaRef};
use ninesql_driver_api::dml::{self, ColumnType, needs_escape};
use ninesql_driver_api::{
    ColumnChange, ColumnValue, Concurrency, ConnectParams, Dialect, DmlColumn, DmlError,
    DmlRequest, Driver, DriverError, Endpoint, EngineId, ExecOptions, KeyKind, Param, ResultItem,
    ResultSetKind, Session, Statement, StatementClass, cell_value,
};
use ninesql_driver_sqlite::{SqliteDialect, SqliteDriver};
use proptest::prelude::*;

fn editing() -> ninesql_driver_api::EditingCapabilities {
    SqliteDriver::new()
        .capabilities(&EngineId::SQLITE, None)
        .unwrap()
        .editing
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
        "[x]",
        "\\",
        "--",
        "/*",
        "*/",
        "$$",
        "$a",
        ":a",
        "@a",
        "#a",
        "$a(b c)",
        "?",
        "?1",
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
        "WHERE",
        "END",
        "BEGIN",
        "DROP TABLE t",
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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Hostile names and values: generation refuses with a typed error or returns texts that
    /// split as one statement and classify as a write.
    #[test]
    fn hostile_input_is_refused_or_safe(
        table in prop::collection::vec(hostile_text(), 1..3),
        column in hostile_text(),
        old in any_param(),
        new in any_param(),
    ) {
        let d = SqliteDialect::new();
        let caps = editing();
        let c = DmlColumn::new(column.clone(), ColumnType::new(DataType::Utf8));
        let key = vec![ColumnValue::new(DmlColumn::new("id", ColumnType::new(DataType::Int64)), Param::Int(1))];
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

    /// A preview literal is one token.
    #[test]
    fn literals_stay_in_their_token(v in any_param()) {
        let d = SqliteDialect::new();
        if let Ok(lit) = d.dml_literal(&v, &ColumnType::new(DataType::Utf8)) {
            let text = format!("SELECT {lit}");
            prop_assert_eq!(d.split_statements(&text).len(), 1);
            prop_assert_eq!(d.classify(&text), StatementClass::Read);
            let text = format!("SELECT {lit} -- x\n; DELETE FROM t");
            prop_assert_eq!(d.split_statements(&text).len(), 2);
        }
    }
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

async fn connect(path: &std::path::Path) -> Box<dyn Session> {
    let mut p = ConnectParams::new(EngineId::SQLITE, Endpoint::file(path));
    p.endpoint = Endpoint::File {
        path: path.to_path_buf(),
        create_if_missing: true,
    };
    SqliteDriver::new().connect(p).await.unwrap()
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
    let out = sql(s, &format!("SELECT * FROM {table} WHERE 0")).await;
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
    let schema = out.schema.unwrap();
    let b = out.batches.iter().find(|b| b.num_rows() > 0).unwrap();
    (0..b.num_columns())
        .map(|i| cell_value(schema.field(i), b.column(i).as_ref(), 0).unwrap())
        .collect()
}

/// `true` when rows `a` and `b` hold the same values with the same storage classes in every
/// column but `id`.
async fn same_rows(s: &mut dyn Session, table: &str, cols: &[DmlColumn], a: i64, b: i64) -> bool {
    let d = s.dialect();
    let cond: Vec<String> = cols
        .iter()
        .filter(|c| c.name != "id")
        .map(|c| {
            let q = d.quote_identifier(&c.name);
            format!("x.{q} IS y.{q} AND typeof(x.{q}) = typeof(y.{q})")
        })
        .collect();
    let out = sql(
        s,
        &format!(
            "SELECT COUNT(*) FROM {table} x, {table} y WHERE x.id = {a} AND y.id = {b} AND {}",
            cond.join(" AND ")
        ),
    )
    .await;
    count(&out) == 1
}

fn count(out: &Out) -> i64 {
    let col = out.batches[0].column(0);
    col.as_any()
        .downcast_ref::<ninesql_driver_api::arrow_array::Int64Array>()
        .unwrap()
        .value(0)
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

const TYPES_DDL: &str = "CREATE TABLE p5b_types (id INTEGER NOT NULL, k2 TEXT NOT NULL, \
    b BOOLEAN, i INTEGER, r REAL, n NUMERIC, dc DECIMAL(10,2), t TEXT, bl BLOB, d DATE, \
    ts TIMESTAMP, j JSON, untyped, PRIMARY KEY (id, k2))";

fn types_values(variant: u8) -> Vec<Param> {
    let text = if variant == 0 {
        "it's \\ -- /* $$ :a @a ; \n\r\t\u{0}\u{1b}\u{202e} é 日本 😀".to_owned()
    } else {
        "plain".to_owned()
    };
    vec![
        Param::Text("a".into()),
        Param::Bool(variant == 0),
        Param::Int(i64::MIN),
        Param::Float(if variant == 0 { 0.1 } else { f64::INFINITY }),
        Param::Decimal("12.50".into()),
        Param::Decimal(if variant == 0 {
            "-0.10".into()
        } else {
            "1e3".into()
        }),
        Param::Text(text.clone()),
        Param::Bytes(if variant == 0 {
            vec![0, b'\\', b'\'', 0xff]
        } else {
            vec![]
        }),
        Param::Date(if variant == 0 { -719_162 } else { 2_932_896 }),
        Param::Timestamp {
            micros: 1_700_000_000_123_456,
            utc: variant == 0,
        },
        Param::Json(r#"{"a": "it's \"q\" \\"}"#.into()),
        if variant == 0 {
            Param::Float(-0.0)
        } else {
            Param::Text(text)
        },
    ]
}

#[tokio::test]
async fn types_keys_and_hostile_identifiers() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = connect(&dir.path().join("p5b.db")).await;
    let s = s.as_mut();
    let d = s.dialect();
    let caps = s.capabilities().editing;
    sql(s, TYPES_DDL).await;
    let t = "p5b_types";
    let cols = columns(s, t).await;
    let first = types_values(0);
    let next = types_values(1);
    let ins_a = dml::generate(
        &*d,
        &caps,
        &DmlRequest::insert(vec!["main".into(), t.into()], values(&cols, 1, &first)),
    )
    .unwrap();
    let ins_b = dml::generate(
        &*d,
        &caps,
        &DmlRequest::insert(vec!["main".into(), t.into()], values(&cols, 2, &first)),
    )
    .unwrap();
    run_dml(s, &ins_a).await;
    run_preview(s, &ins_b).await;
    assert!(
        same_rows(s, t, &cols, 1, 2).await,
        "insert: preview and bound rows differ"
    );
    for (id, preview) in [(1, false), (2, true)] {
        let original = read_row(s, t, id).await;
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
        let req = DmlRequest::update(vec![t.into()], KeyKind::PrimaryKey, key, changes)
            .with_concurrency(Concurrency::EditedColumns);
        let st = dml::generate(&*d, &caps, &req).unwrap();
        assert!(st.unchecked_columns.is_empty());
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
        same_rows(s, t, &cols, 1, 2).await,
        "update: preview and bound rows differ"
    );
    sql(s, "UPDATE p5b_types SET t = t || 'x' WHERE id = 2").await;
    assert!(!same_rows(s, t, &cols, 1, 2).await);
    for (id, preview) in [(1, false), (2, true)] {
        let key = vec![
            ColumnValue::new(cols[0].clone(), Param::Int(id)),
            ColumnValue::new(cols[1].clone(), Param::Text("a".into())),
        ];
        let st = dml::generate(
            &*d,
            &caps,
            &DmlRequest::delete(vec![t.into()], KeyKind::PrimaryKey, key),
        )
        .unwrap();
        if preview {
            run_preview(s, &st).await;
        } else {
            run_dml(s, &st).await;
        }
    }
    assert_eq!(count(&sql(s, "SELECT COUNT(*) FROM p5b_types").await), 0);

    // No key: refused. A nullable unique key: NULL refused, a value edited.
    sql(s, "CREATE TABLE p5b_uq (u INTEGER UNIQUE, v TEXT)").await;
    sql(
        s,
        "INSERT INTO p5b_uq VALUES (1, 'a'), (NULL, 'b'), (NULL, 'c')",
    )
    .await;
    let cols = columns(s, "p5b_uq").await;
    let upd = |key: Vec<ColumnValue>| {
        DmlRequest::update(
            vec!["p5b_uq".into()],
            KeyKind::Unique {
                nulls_distinct: true,
            },
            key,
            vec![ColumnChange::new(
                cols[1].clone(),
                Param::Null,
                Param::Text("z".into()),
            )],
        )
    };
    assert_eq!(
        dml::generate(&*d, &caps, &upd(vec![])),
        Err(DmlError::NoKey)
    );
    assert!(matches!(
        dml::generate(
            &*d,
            &caps,
            &upd(vec![ColumnValue::new(cols[0].clone(), Param::Null)])
        ),
        Err(DmlError::NullKey(_))
    ));
    let st = dml::generate(
        &*d,
        &caps,
        &upd(vec![ColumnValue::new(cols[0].clone(), Param::Int(1))]),
    )
    .unwrap();
    run_dml(s, &st).await;
    run_preview(s, &st).await;

    // Hostile identifiers.
    let table = "t'`\"[]\\ /* $$ :a @a \u{e9}";
    let column = "select [x] -- ;\n";
    let qt = d.quote_identifier(table);
    let qc = d.quote_identifier(column);
    sql(
        s,
        &format!("CREATE TABLE {qt} (id INTEGER PRIMARY KEY, {qc} TEXT)"),
    )
    .await;
    let cols = columns(s, &qt).await;
    for (id, preview) in [(1, false), (2, true)] {
        let st = dml::generate(
            &*d,
            &caps,
            &DmlRequest::insert(
                vec![table.into()],
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
    assert!(same_rows(s, &qt, &cols, 1, 2).await);
}

#[test]
fn preview_round_trips_property() {
    let dir = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let session = RefCell::new(Some(rt.block_on(connect(&dir.path().join("prop.db")))));
    let table = "p5b_prop";
    let cols = with_session(&rt, &session, async |s: &mut dyn Session| {
        sql(
            s,
            &format!(
                "CREATE TABLE {table} (id INTEGER PRIMARY KEY, t TEXT, bl BLOB, i INTEGER, \
                 r REAL, n NUMERIC, d DATE, ts TIMESTAMP, b BOOLEAN, any_col)"
            ),
        )
        .await;
        columns(s, table).await
    });
    let row = || {
        (
            hostile_text(),
            prop::collection::vec(any::<u8>(), 0..24),
            any::<i64>(),
            any::<f64>().prop_filter("not NaN", |f| !f.is_nan()),
            any::<f64>().prop_filter("not NaN", |f| !f.is_nan()),
            -719_162i32..2_932_896,
            -62_135_596_800_000_000i64..253_402_300_799_999_999,
            any::<bool>(),
            any_param().prop_filter("bindable", |p| match p {
                Param::Float(f) => !f.is_nan(),
                Param::Date(d) => dml::date_text(*d).is_ok(),
                Param::Timestamp { micros, .. } => dml::timestamp_text(*micros).is_ok(),
                _ => true,
            }),
        )
            .prop_map(|(t, bl, i, r, n, d, ts, b, any)| {
                vec![
                    Param::Text(t),
                    Param::Bytes(bl),
                    Param::Int(i),
                    Param::Float(r),
                    Param::Float(n),
                    Param::Date(d),
                    Param::Timestamp {
                        micros: ts,
                        utc: false,
                    },
                    Param::Bool(b),
                    any,
                ]
            })
    };
    let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig::with_cases(256));
    runner
        .run(
            &(row(), row()),
            |(first, next): (Vec<Param>, Vec<Param>)| {
                with_session(&rt, &session, async |s: &mut dyn Session| {
                    round_trip(s, table, &cols, &first, &next).await;
                });
                Ok(())
            },
        )
        .unwrap();
}
