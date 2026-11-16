//! DML generation (P5-B): offline checks of the MySQL-family dialects in every lexical mode,
//! and engine tests against the `mysql`, `mariadb` and `tidb` services of `dev/compose.yaml`
//! (ADR-0009): every row change runs once through its parameterized form and once through its
//! preview, and both must leave the same row state, in the default `sql_mode` and with
//! `NO_BACKSLASH_ESCAPES` and `ANSI_QUOTES`.
//!
//! Engine tests are skipped when an engine is unreachable, unless `NINESQL_REQUIRE_ENGINES`
//! lists it. Start them with `python3 dev/ready.py up sql --engines mysql,mariadb,tidb`.
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
use ninesql_driver_mysqlwire::{MyDialect, MySqlWireDriver};
use proptest::prelude::*;

// ---------------------------------------------------------------------------------------------
// Offline

fn editing(engine: &EngineId) -> ninesql_driver_api::EditingCapabilities {
    MySqlWireDriver::new()
        .capabilities(engine, None)
        .unwrap()
        .editing
}

fn dialects() -> Vec<MyDialect> {
    vec![
        MyDialect::mysql(),
        MyDialect::mysql().with_sql_mode("NO_BACKSLASH_ESCAPES"),
        MyDialect::mysql().with_sql_mode("ANSI_QUOTES"),
        MyDialect::mariadb().with_sql_mode("ANSI_QUOTES,NO_BACKSLASH_ESCAPES"),
        MyDialect::tidb(),
    ]
}

fn hostile_text() -> impl Strategy<Value = String> + Clone {
    let pieces = prop::sample::select(vec![
        "'",
        "''",
        "\"",
        "\"\"",
        "`",
        "``",
        "[",
        "]",
        "\\",
        "\\'",
        "\\\\",
        "--",
        "-- ",
        "#",
        "/*",
        "*/",
        "/*!",
        "/*!50000",
        "*/;",
        "/*M!",
        "/*T!",
        "$$",
        ";",
        "\n",
        "\r",
        "\t",
        "\u{0}",
        "\u{1a}",
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
        "@a",
        "@@b",
        "WHERE",
        "DROP TABLE t",
        "x",
        "_utf8mb4",
        "N'",
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
        Just(ColumnType::new(DataType::Binary)),
        hostile_text().prop_map(|t| ColumnType::new(DataType::Utf8).with_engine_type(t)),
        prop::sample::select(vec!["json", "decimal", "time", "enum", "bit", "geometry"])
            .prop_map(|t| ColumnType::new(DataType::Utf8).with_engine_type(t)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Hostile names, values and modes: generation refuses with a typed error or returns texts
    /// that split as one statement and classify as a write.
    #[test]
    fn hostile_input_is_refused_or_safe(
        table in prop::collection::vec(hostile_text(), 1..3),
        column in hostile_text(),
        ty in any_type(),
        old in any_param(),
        new in any_param(),
    ) {
        for (engine, d) in [EngineId::MYSQL, EngineId::MYSQL, EngineId::MYSQL, EngineId::MARIADB, EngineId::TIDB]
            .into_iter()
            .zip(dialects())
        {
            let caps = editing(&engine);
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
                            prop_assert!(!text.contains('\0'));
                        }
                        prop_assert!(!s.preview.chars().any(|c| needs_escape(c) && !column.contains(c) && !table.iter().any(|t| t.contains(c))));
                    }
                    Err(DmlError::SelfCheck(why)) => prop_assert!(false, "self-check: {}", why),
                    Err(_) => {}
                }
            }
            if let Ok(b) = dml::generate_batch(&d, &caps, &reqs) {
                prop_assert_eq!(d.split_statements(&b.preview_script()).len(), 3);
                prop_assert_eq!(d.split_script(&b.preview_script()).len(), 3);
            }
        }
    }

    /// A literal is the same text in every `sql_mode` and holds no backslash (security review
    /// H1): a preview rendered in one session means the same when replayed in another.
    #[test]
    fn literals_are_mode_independent(v in any_param(), ty in any_type()) {
        let lits: Vec<_> = dialects().iter().map(|d| d.dml_literal(&v, &ty)).collect();
        for l in &lits {
            prop_assert_eq!(l, &lits[0]);
        }
        if let Ok(l) = &lits[0] {
            prop_assert!(!l.contains('\\'), "backslash in a literal");
        }
    }

    /// A preview literal is one token in every mode.
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
fn empty_insert_and_identifier_limits() {
    let caps = editing(&EngineId::MYSQL);
    let d = MyDialect::mysql();
    let s = dml::generate(
        &d,
        &caps,
        &DmlRequest::insert(vec!["db".into(), "t`x".into()], vec![]),
    )
    .unwrap();
    assert_eq!(s.statement.text, "INSERT INTO `db`.`t``x` () VALUES ()");
    assert_eq!(d.classify(&s.preview), StatementClass::Write);
    let long = DmlRequest::insert(vec!["a".repeat(65)], vec![]);
    assert!(matches!(
        dml::generate(&d, &caps, &long),
        Err(DmlError::InvalidIdentifier { .. })
    ));
    let ts = DmlRequest::insert(
        vec!["t".into()],
        vec![ColumnValue::new(
            DmlColumn::new("a", ColumnType::new(DataType::Utf8)),
            Param::Timestamp {
                micros: 0,
                utc: true,
            },
        )],
    );
    assert!(matches!(
        dml::generate(&d, &caps, &ts),
        Err(DmlError::UnsupportedValue { .. })
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
    p.application_name = "ninesql-mysqlwire-dml-tests".into();
    p.connect_timeout = Duration::from_secs(30);
    match MySqlWireDriver::new().connect(p).await {
        Ok(s) => s,
        Err(e) => panic!("{}: connect failed: {e}", dev.engine),
    }
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
        .unwrap_or_else(|e| panic!("{e}: {}", st.preview));
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
    let out = sql(s, &format!("SELECT * FROM {table} WHERE 1 = 0")).await;
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

/// `true` when rows `a` and `b` hold the same values in every column but `id` (null-safe
/// equality and binary equality, so collations and float formatting cannot hide a difference).
async fn same_rows(s: &mut dyn Session, table: &str, cols: &[DmlColumn], a: i64, b: i64) -> bool {
    let d = s.dialect();
    let cond: Vec<String> = cols
        .iter()
        .filter(|c| c.name != "id")
        .map(|c| {
            let q = d.quote_identifier(&c.name);
            format!("(x.{q} <=> y.{q}) AND (CAST(x.{q} AS BINARY) <=> CAST(y.{q} AS BINARY))")
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

// ---------------------------------------------------------------------------------------------
// Engine tests

const ENGINES: [&str; 3] = ["mysql", "mariadb", "tidb"];

const STRICT_ANSI: &str =
    "SET SESSION sql_mode = CONCAT(@@sql_mode, ',NO_BACKSLASH_ESCAPES,ANSI_QUOTES')";

fn types_ddl(t: &str) -> String {
    format!(
        "CREATE TABLE {t} (id BIGINT NOT NULL, k2 VARCHAR(20) NOT NULL, b BOOLEAN, i1 TINYINT, \
         u8 BIGINT UNSIGNED, f4 FLOAT, f8 DOUBLE, n DECIMAL(40,12), t TEXT, vc VARCHAR(200), \
         vb VARBINARY(200), bl BLOB, d DATE, tm TIME(6), dt DATETIME(6), j JSON, \
         e ENUM('a','b''c'), bt BIT(8), yr YEAR, PRIMARY KEY (id, k2)) \
         DEFAULT CHARSET utf8mb4 COLLATE utf8mb4_bin"
    )
}

fn types_values(variant: u8) -> Vec<Param> {
    let text = if variant == 0 {
        "it's \\ \\' -- # /* */ /*! $$ ; \n\r\t\u{0}\u{1a}\u{1b}\u{202e} é 日本 😀".to_owned()
    } else {
        "plain".to_owned()
    };
    vec![
        Param::Text("a".into()),
        Param::Bool(variant == 0),
        Param::Int(-128),
        Param::Decimal("18446744073709551615".into()),
        Param::Float(f64::from(0.1_f32)),
        Param::Float(if variant == 0 { 0.1 } else { -1.5e300 }),
        Param::Decimal(if variant == 0 {
            "-1234567890123456789012345678.000000000123".into()
        } else {
            "0".into()
        }),
        Param::Text(text.clone()),
        Param::Text(text.chars().take(200).collect()),
        Param::Bytes(if variant == 0 {
            vec![0, b'\\', b'\'', 0xff, 0xfe]
        } else {
            vec![]
        }),
        Param::Bytes(vec![b'"', 0x1a, 0]),
        Param::Date(if variant == 0 { -354_285 } else { 2_932_896 }),
        Param::Text(if variant == 0 {
            "-838:59:59.000000".into()
        } else {
            "12:34:56.000001".into()
        }),
        Param::Timestamp {
            micros: if variant == 0 {
                1_700_000_000_123_456
            } else {
                -30_610_224_000_000_000
            },
            utc: false,
        },
        Param::Json(r#"{"a": "it's \"q\" \\ \u0001", "b": [1, 2.5, null]}"#.into()),
        Param::Text(if variant == 0 {
            "b'c".into()
        } else {
            "a".into()
        }),
        Param::Bytes(vec![0xa5]),
        Param::Int(if variant == 0 { 1901 } else { 2155 }),
    ]
}

fn unchecked(engine: &str) -> Vec<String> {
    // FLOAT is read back with six digits; MariaDB's JSON is a LONGTEXT alias (comparable as
    // text); BIT against a hexadecimal literal is numeric.
    let mut v = vec!["f4".to_owned()];
    if engine != "mariadb" {
        v.push("j".to_owned());
    }
    v.push("bt".to_owned());
    v
}

async fn types_round_trip(engine: &str, mode: Option<&str>) {
    let Some(dev) = dev(engine) else { return };
    let mut s = connect(&dev).await;
    let s = s.as_mut();
    if let Some(m) = mode {
        sql(s, m).await;
    }
    let t = format!("p5b_types_{}", if mode.is_some() { "m" } else { "d" });
    sql(s, &format!("DROP TABLE IF EXISTS {t}")).await;
    sql(s, &types_ddl(&t)).await;
    let cols = columns(s, &t).await;
    let d = s.dialect();
    let caps = s.capabilities().editing;
    let first = types_values(0);
    let next = types_values(1);
    let ins_a = dml::generate(
        &*d,
        &caps,
        &DmlRequest::insert(vec![t.clone()], values(&cols, 1, &first)),
    )
    .unwrap();
    let ins_b = dml::generate(
        &*d,
        &caps,
        &DmlRequest::insert(vec![t.clone()], values(&cols, 2, &first)),
    )
    .unwrap();
    run_dml(s, &ins_a).await;
    run_preview(s, &ins_b).await;
    assert!(
        same_rows(s, &t, &cols, 1, 2).await,
        "{engine}: insert: preview and bound rows differ"
    );
    for (id, preview) in [(1, false), (2, true)] {
        let original = read_row(s, &t, id).await;
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
        let req = DmlRequest::update(vec![t.clone()], KeyKind::PrimaryKey, key, changes)
            .with_concurrency(Concurrency::EditedColumns);
        let st = dml::generate(&*d, &caps, &req).unwrap();
        assert_eq!(st.unchecked_columns, unchecked(engine));
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
        same_rows(s, &t, &cols, 1, 2).await,
        "{engine}: update: preview and bound rows differ"
    );
    sql(
        s,
        &format!("UPDATE {t} SET t = CONCAT(t, 'x') WHERE id = 2"),
    )
    .await;
    assert!(!same_rows(s, &t, &cols, 1, 2).await);
    for (id, preview) in [(1, false), (2, true)] {
        let original = read_row(s, &t, id).await;
        let key = vec![
            ColumnValue::new(cols[0].clone(), Param::Int(id)),
            ColumnValue::new(cols[1].clone(), Param::Text("a".into())),
        ];
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
        if preview {
            run_preview(s, &st).await;
        } else {
            run_dml(s, &st).await;
        }
    }
    assert_eq!(
        count(&sql(s, &format!("SELECT COUNT(*) FROM {t}")).await),
        0
    );
    sql(s, &format!("DROP TABLE {t}")).await;
}

#[tokio::test]
async fn types_round_trip_default_mode() {
    for engine in ENGINES {
        types_round_trip(engine, None).await;
    }
}

#[tokio::test]
async fn types_round_trip_no_backslash_escapes_ansi_quotes() {
    for engine in ENGINES {
        types_round_trip(engine, Some(STRICT_ANSI)).await;
    }
}

#[tokio::test]
async fn keys_and_found_rows() {
    for engine in ENGINES {
        let Some(dev) = dev(engine) else { continue };
        let mut s = connect(&dev).await;
        let s = s.as_mut();
        let d = s.dialect();
        let caps = s.capabilities().editing;
        sql(s, "DROP TABLE IF EXISTS p5b_nokey, p5b_uq").await;
        sql(s, "CREATE TABLE p5b_nokey (a INT, b TEXT)").await;
        let cols = columns(s, "p5b_nokey").await;
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

        sql(s, "CREATE TABLE p5b_uq (u INT UNIQUE, v TEXT)").await;
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
        // The same value again: one row matched (CLIENT_FOUND_ROWS), not zero changed.
        run_preview(s, &st).await;
        run_dml(s, &st).await;
        assert!(matches!(
            dml::generate(&*d, &caps, &upd(Param::Null)),
            Err(DmlError::NullKey(_))
        ));
        sql(s, "DROP TABLE p5b_nokey, p5b_uq").await;
    }
}

#[tokio::test]
async fn hostile_identifiers() {
    for engine in ENGINES {
        let Some(dev) = dev(engine) else { continue };
        let mut s = connect(&dev).await;
        let s = s.as_mut();
        let d = s.dialect();
        let caps = s.capabilities().editing;
        let table = "t'`\"[]\\ /*! -- # $$ \u{e9}";
        let column = "select `x` -- ;\n x";
        let qt = d.quote_identifier(table);
        let qc = d.quote_identifier(column);
        sql(s, &format!("DROP TABLE IF EXISTS {qt}")).await;
        sql(
            s,
            &format!("CREATE TABLE {qt} (id BIGINT PRIMARY KEY, {qc} TEXT)"),
        )
        .await;
        let cols = columns(s, &qt).await;
        let path = vec![dev.database.clone(), table.to_owned()];
        for (id, preview) in [(1, false), (2, true)] {
            let st = dml::generate(
                &*d,
                &caps,
                &DmlRequest::insert(
                    path.clone(),
                    values(&cols, id, &[Param::Text("v'; DROP TABLE x; -- \\".into())]),
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
        let st = dml::generate(
            &*d,
            &caps,
            &DmlRequest::delete(path, KeyKind::PrimaryKey, id_key(&cols, 2)),
        )
        .unwrap();
        run_preview(s, &st).await;
        sql(s, &format!("DROP TABLE {qt}")).await;
    }
}

fn prop_row() -> impl Strategy<Value = Vec<Param>> + Clone {
    let text = hostile_text().boxed();
    (
        text.clone(),
        prop::collection::vec(any::<u8>(), 0..24),
        any::<i64>(),
        any::<f64>().prop_filter("finite", |f| f.is_finite()),
        any::<f32>().prop_filter("finite", |f| f.is_finite()),
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
        -354_285i32..2_932_896,
        -30_610_224_000_000_000i64..253_402_300_799_999_999,
        any::<bool>(),
        text.prop_map(|t| serde_json::json!({ "k": t }).to_string()),
    )
        .prop_map(|(t, vb, i8, f8, f4, n, d, dt, b, j)| {
            vec![
                Param::Text(t),
                Param::Bytes(vb),
                Param::Int(i8),
                Param::Float(f8),
                Param::Float(f4.into()),
                Param::Decimal(n),
                Param::Date(d),
                Param::Timestamp {
                    micros: dt,
                    utc: false,
                },
                Param::Bool(b),
                Param::Json(j),
            ]
        })
}

fn property(engine: &str, mode: Option<&str>) {
    let Some(dev) = dev(engine) else { return };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let session = RefCell::new(Some(rt.block_on(connect(&dev))));
    let table = if mode.is_some() {
        "p5b_prop_m"
    } else {
        "p5b_prop_d"
    };
    let cols = with_session(&rt, &session, async |s: &mut dyn Session| {
        if let Some(m) = mode {
            sql(s, m).await;
        }
        sql(s, &format!("DROP TABLE IF EXISTS {table}")).await;
        sql(
            s,
            &format!(
                "CREATE TABLE {table} (id BIGINT PRIMARY KEY, t TEXT, vb VARBINARY(64), i8 BIGINT, \
                 f8 DOUBLE, f4 FLOAT, n DECIMAL(40,12), d DATE, dt DATETIME(6), b BOOLEAN, j JSON) \
                 DEFAULT CHARSET utf8mb4 COLLATE utf8mb4_bin"
            ),
        )
        .await;
        columns(s, table).await
    });
    let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig::with_cases(48));
    runner
        .run(
            &(prop_row(), prop_row()),
            |(first, next): (Vec<Param>, Vec<Param>)| {
                with_session(&rt, &session, async |s: &mut dyn Session| {
                    round_trip(s, table, &cols, &first, &next).await;
                });
                Ok(())
            },
        )
        .unwrap_or_else(|e| panic!("{engine}: {e}"));
    with_session(&rt, &session, async |s: &mut dyn Session| {
        sql(s, &format!("DROP TABLE {table}")).await;
    });
}

#[test]
fn preview_round_trips_property() {
    for engine in ENGINES {
        property(engine, None);
        property(engine, Some(STRICT_ANSI));
    }
}

/// A preview rendered in one `sql_mode` and replayed in a session with the other gives the
/// same row and touches no other row (security review H1, both directions).
#[tokio::test]
async fn previews_replay_across_modes() {
    let hostile = [
        "\\'' -- '",
        "\\'; UPDATE p5b_xmode SET t = 'pwned' -- ",
        "a\\",
        "\\",
        "it's",
        "x' OR '1'='1",
        "\\x27 \u{3a3}\u{5c}",
    ];
    for engine in ENGINES {
        let Some(dev) = dev(engine) else { continue };
        let mut plain = connect(&dev).await;
        let mut moded = connect(&dev).await;
        sql(moded.as_mut(), STRICT_ANSI).await;
        let plain = plain.as_mut();
        let moded = moded.as_mut();
        sql(plain, "DROP TABLE IF EXISTS p5b_xmode").await;
        sql(
            plain,
            "CREATE TABLE p5b_xmode (id BIGINT PRIMARY KEY, t TEXT, vb VARBINARY(64)) \
             DEFAULT CHARSET utf8mb4 COLLATE utf8mb4_bin",
        )
        .await;
        sql(plain, "INSERT INTO p5b_xmode VALUES (0, 'victim', X'00')").await;
        let cols = columns(plain, "p5b_xmode").await;
        let caps = plain.capabilities().editing;
        for (n, text) in hostile.iter().enumerate() {
            let base = i64::try_from(n).unwrap() * 10 + 1;
            let vals = [
                Param::Text((*text).to_owned()),
                Param::Bytes(text.as_bytes().to_vec()),
            ];
            // Reference row through the bound form.
            let st = dml::generate(
                &*plain.dialect(),
                &caps,
                &DmlRequest::insert(vec!["p5b_xmode".into()], values(&cols, base, &vals)),
            )
            .unwrap();
            run_dml(plain, &st).await;
            // Rendered in the moded session, replayed in the plain one, and the reverse.
            let ins = |d: &dyn Dialect, id: i64| {
                dml::generate(
                    d,
                    &caps,
                    &DmlRequest::insert(vec!["p5b_xmode".into()], values(&cols, id, &vals)),
                )
                .unwrap()
            };
            let to_plain = ins(&*moded.dialect(), base + 1);
            run_preview(plain, &to_plain).await;
            let to_moded = ins(&*plain.dialect(), base + 2);
            run_preview(moded, &to_moded).await;
        }
        // Updates rendered in one mode and replayed in the other, there and back.
        let upd = |d: &dyn Dialect, from: &str, to: &str| {
            dml::generate(
                d,
                &caps,
                &DmlRequest::update(
                    vec!["p5b_xmode".into()],
                    KeyKind::PrimaryKey,
                    id_key(&cols, 1),
                    vec![ColumnChange::new(
                        cols[1].clone(),
                        Param::Text(from.into()),
                        Param::Text(to.into()),
                    )],
                )
                .with_concurrency(Concurrency::EditedColumns),
            )
            .unwrap()
        };
        let st = upd(&*moded.dialect(), hostile[0], hostile[1]);
        run_preview(plain, &st).await;
        let st = upd(&*plain.dialect(), hostile[1], hostile[0]);
        run_preview(moded, &st).await;
        let st = upd(&*plain.dialect(), hostile[0], hostile[1]);
        run_preview(moded, &st).await;
        let st = upd(&*moded.dialect(), hostile[1], hostile[0]);
        run_preview(plain, &st).await;
        for n in 0..hostile.len() {
            let base = i64::try_from(n).unwrap() * 10 + 1;
            assert!(
                same_rows(plain, "p5b_xmode", &cols, base, base + 1).await,
                "{engine}: {n} moded -> plain"
            );
            assert!(
                same_rows(plain, "p5b_xmode", &cols, base, base + 2).await,
                "{engine}: {n} plain -> moded"
            );
        }
        let victim = sql(
            plain,
            "SELECT COUNT(*) FROM p5b_xmode WHERE id = 0 AND t = 'victim'",
        )
        .await;
        assert_eq!(count(&victim), 1, "{engine}: another row was changed");
        let total = sql(plain, "SELECT COUNT(*) FROM p5b_xmode").await;
        assert_eq!(count(&total), i64::try_from(hostile.len() * 3 + 1).unwrap());
        sql(plain, "DROP TABLE p5b_xmode").await;
    }
}

/// Index flags and the storage engine's transactional flag on live servers (P5-B review M1,
/// M2).
#[tokio::test]
async fn index_flags_and_transactional_tables() {
    use ninesql_driver_api::{
        KeyCandidate, ObjectDetails, ObjectKind, ObjectPath, PageRequest, choose_row_key,
    };
    for engine in ENGINES {
        let Some(dev) = dev(engine) else { continue };
        let mut s = connect(&dev).await;
        let s = s.as_mut();
        sql(s, "DROP TABLE IF EXISTS p5b_tx, p5b_notx").await;
        sql(s, "CREATE TABLE p5b_tx (id INT PRIMARY KEY, c VARCHAR(20) NOT NULL, UNIQUE KEY p5b_tx_c (c)) ENGINE = InnoDB").await;
        if engine != "tidb" {
            // MySQL 8 functional key part; MariaDB has none.
            if engine == "mysql" {
                sql(s, "CREATE UNIQUE INDEX p5b_tx_l ON p5b_tx ((lower(c)))").await;
            }
            sql(
                s,
                "CREATE TABLE p5b_notx (id INT PRIMARY KEY) ENGINE = MyISAM",
            )
            .await;
        }
        let schema = ObjectPath::root().child(ObjectKind::Schema, dev.database.clone());
        let tables = s
            .metadata()
            .list(
                &schema,
                ObjectKind::Table,
                PageRequest::first().with_filter(Some("p5b_".into())),
            )
            .await
            .unwrap();
        let flag = |name: &str| {
            tables
                .items
                .iter()
                .find(|o| o.name() == name)
                .map(|o| match &o.details {
                    ObjectDetails::Table(t) => t.transactional,
                    other => panic!("{other:?}"),
                })
        };
        assert_eq!(flag("p5b_tx"), Some(Some(true)), "{engine}");
        if engine != "tidb" {
            assert_eq!(flag("p5b_notx"), Some(Some(false)), "{engine}");
        }
        let idx = s
            .metadata()
            .list(
                &schema.child(ObjectKind::Table, "p5b_tx"),
                ObjectKind::Index,
                PageRequest::first(),
            )
            .await
            .unwrap();
        let candidates: Vec<KeyCandidate> = idx
            .items
            .iter()
            .filter_map(|o| match &o.details {
                ObjectDetails::Index(i) => {
                    assert!(
                        i.valid && !i.deferrable && i.nulls_distinct,
                        "{engine}: {i:?}"
                    );
                    if o.name() == "p5b_tx_l" {
                        assert_eq!(i.expressions, vec![true], "{engine}");
                    }
                    KeyCandidate::from_index(i, true)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            choose_row_key(&candidates),
            Some((vec!["id".to_owned()], KeyKind::PrimaryKey)),
            "{engine}"
        );
        let no_pk: Vec<KeyCandidate> = candidates.into_iter().filter(|c| !c.primary).collect();
        assert_eq!(
            choose_row_key(&no_pk),
            Some((
                vec!["c".to_owned()],
                KeyKind::Unique {
                    nulls_distinct: true
                }
            )),
            "{engine}"
        );
        sql(s, "DROP TABLE IF EXISTS p5b_tx, p5b_notx").await;
    }
}

/// Key comparisons use the column's collation (P5-B re-review M-a): a `latin1` unique key
/// holding `u` and `ü`, and an accent- and case-sensitive primary key holding `a` and `A`, each
/// match exactly one row through the bound form and through the preview.
#[tokio::test]
async fn key_comparisons_use_the_column_collation() {
    for engine in ENGINES {
        let Some(dev) = dev(engine) else { continue };
        let mut s = connect(&dev).await;
        let s = s.as_mut();
        let (latin1, cs) = match engine {
            "mysql" => ("latin1_swedish_ci", "utf8mb4_0900_as_cs"),
            "mariadb" => ("latin1_swedish_ci", "utf8mb4_uca1400_as_cs"),
            _ => ("latin1_bin", "utf8mb4_0900_bin"),
        };
        sql(s, "DROP TABLE IF EXISTS p5b_coll1, p5b_coll2").await;
        sql(
            s,
            &format!(
                "CREATE TABLE p5b_coll1 (id INT PRIMARY KEY, \
                 u VARCHAR(10) CHARACTER SET latin1 COLLATE {latin1} NOT NULL, v INT, UNIQUE KEY (u))"
            ),
        )
        .await;
        sql(
            s,
            &format!(
                "CREATE TABLE p5b_coll2 (k VARCHAR(10) CHARACTER SET utf8mb4 COLLATE {cs} \
                 PRIMARY KEY, v INT)"
            ),
        )
        .await;
        sql(
            s,
            "INSERT INTO p5b_coll1 VALUES (1, 'u', 0), (2, _utf8mb4 X'c3bc', 0)",
        )
        .await;
        sql(
            s,
            "INSERT INTO p5b_coll2 VALUES ('a', 0), ('A', 0), ('á', 0)",
        )
        .await;
        let d = s.dialect();
        let caps = s.capabilities().editing;
        let c1 = columns(s, "p5b_coll1").await;
        let c2 = columns(s, "p5b_coll2").await;
        for preview in [false, true] {
            let n = i64::from(preview) + 1;
            let st = dml::generate(
                &*d,
                &caps,
                &DmlRequest::update(
                    vec!["p5b_coll1".into()],
                    KeyKind::Unique {
                        nulls_distinct: true,
                    },
                    vec![ColumnValue::new(c1[1].clone(), Param::Text("u".into()))],
                    vec![ColumnChange::new(
                        c1[2].clone(),
                        Param::Int(n - 1),
                        Param::Int(n),
                    )],
                )
                .with_concurrency(Concurrency::EditedColumns),
            )
            .unwrap();
            let st2 = dml::generate(
                &*d,
                &caps,
                &DmlRequest::update(
                    vec!["p5b_coll2".into()],
                    KeyKind::PrimaryKey,
                    vec![ColumnValue::new(c2[0].clone(), Param::Text("a".into()))],
                    vec![ColumnChange::new(
                        c2[1].clone(),
                        Param::Int(n - 1),
                        Param::Int(n),
                    )],
                )
                .with_concurrency(Concurrency::EditedColumns),
            )
            .unwrap();
            if preview {
                run_preview(s, &st).await;
                run_preview(s, &st2).await;
            } else {
                run_dml(s, &st).await;
                run_dml(s, &st2).await;
            }
        }
        let other = sql(s, "SELECT COUNT(*) FROM p5b_coll1 WHERE id = 2 AND v = 0").await;
        assert_eq!(count(&other), 1, "{engine}: the ü row changed");
        let other = sql(s, "SELECT COUNT(*) FROM p5b_coll2 WHERE v = 0").await;
        assert_eq!(count(&other), 2, "{engine}: another as_cs row changed");
        sql(s, "DROP TABLE p5b_coll1, p5b_coll2").await;
    }
}
