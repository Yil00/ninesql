//! SQLite dialect (P3-D): the shared classification corpus, script splitting (triggers,
//! `[…]` identifiers, Tcl variables) and property tests. No database needed.

use ninesql_driver_api::conformance::{
    self, STATEMENT_POOL, assert_join_split_roundtrip, assert_script_split_invariants,
};
use ninesql_driver_api::{Dialect, ScriptItemKind, StatementClass};
use ninesql_driver_sqlite::SqliteDialect;
use proptest::prelude::*;

fn make(modes: &[String]) -> Option<Box<dyn Dialect>> {
    modes
        .is_empty()
        .then(|| Box::new(SqliteDialect::new()) as Box<dyn Dialect>)
}

#[test]
fn classification_corpus() {
    let n = conformance::assert_classification_corpus("sqlite", &make);
    assert!(n > 75, "only {n} cases ran");
}

fn statements(script: &str) -> Vec<String> {
    let d = SqliteDialect::new();
    assert_script_split_invariants(&d, script);
    d.split_script(script)
        .iter()
        .map(|i| {
            assert_eq!(i.kind, ScriptItemKind::Statement);
            i.text(script).to_owned()
        })
        .collect()
}

#[test]
fn triggers_are_one_statement() {
    let s = "CREATE TABLE a (x);\n\
             CREATE TEMP TRIGGER IF NOT EXISTS tr AFTER INSERT ON a FOR EACH ROW\n\
             WHEN new.x > 0\nBEGIN\n  INSERT INTO log VALUES (CASE WHEN new.x > 1 THEN 'big' END);\n  \
             UPDATE a SET x = 0 WHERE rowid = new.rowid;\nEND;\n\
             SELECT 1";
    let got = statements(s);
    assert_eq!(got.len(), 3, "{got:?}");
    assert!(got[1].starts_with("CREATE TEMP TRIGGER") && got[1].ends_with("END"));
    assert_eq!(got[2], "SELECT 1");
    // `EXPLAIN` before it, lower case, and a statement after the END on the same line.
    let got = statements(
        "explain create trigger t before delete on a begin select 1; end; delete from a",
    );
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(
        SqliteDialect::new().classify(&got[1]),
        StatementClass::Destructive
    );
    // An `end` column inside the body does not end it; only a statement starting with END.
    let got = statements(
        "CREATE TRIGGER t AFTER INSERT ON a BEGIN UPDATE a SET [end] = 1 WHERE x; END; SELECT 2",
    );
    assert_eq!(got.len(), 2, "{got:?}");
    // A table named trigger_log is not a trigger.
    assert_eq!(
        statements("CREATE TABLE trigger_log (a); SELECT 1").len(),
        2
    );
}

#[test]
fn identifiers_strings_and_variables() {
    assert_eq!(
        statements("SELECT [a;b], `c;d`, \"e;f\", 'g;h' FROM t; SELECT 2"),
        ["SELECT [a;b], `c;d`, \"e;f\", 'g;h' FROM t", "SELECT 2"]
    );
    // Tcl-style variables swallow everything up to whitespace or `)`.
    assert_eq!(statements("SELECT $a(;'x) ; SELECT 2").len(), 2);
    // No backslash escapes; block comments do not nest.
    assert_eq!(
        statements("SELECT 'a\\'; SELECT 2"),
        ["SELECT 'a\\'", "SELECT 2"]
    );
    assert_eq!(statements("SELECT 1 /* a /* b */; SELECT 2").len(), 2);
}

#[test]
fn positions() {
    let s = "SELECT 1;\n\nSELECT [é]";
    let items = SqliteDialect::new().split_script(s);
    assert_eq!((items[1].start.line, items[1].start.column), (3, 1));
    assert_eq!((items[1].end.line, items[1].end.column), (3, 11));
}

fn junk() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(vec![
            "'",
            "\"",
            "`",
            "[",
            "]",
            "/*",
            "*/",
            "--",
            "\n",
            ";",
            "(",
            ")",
            "$a(",
            ":b",
            "@c",
            "#d",
            "CREATE TRIGGER t AFTER INSERT ON a BEGIN",
            "END",
            "CASE",
            "DELETE FROM t",
            "SELECT 1",
            " ",
            "é",
            "WHERE",
            "EXPLAIN",
        ]),
        0..40,
    )
    .prop_map(|v| v.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn split_invariants_hold_on_junk(s in junk()) {
        assert_script_split_invariants(&SqliteDialect::new(), &s);
    }

    #[test]
    fn split_invariants_hold_on_any_text(s in "\\PC{0,200}") {
        assert_script_split_invariants(&SqliteDialect::new(), &s);
    }

    #[test]
    fn joined_statements_split_back_and_keep_their_class(
        picks in prop::collection::vec(prop::sample::select(STATEMENT_POOL.to_vec()), 1..8),
        sep in prop::sample::select(vec![";\n", " ; ", ";", ";\n-- c\n", ";/* ; */\n"]),
    ) {
        assert_join_split_roundtrip(&SqliteDialect::new(), &picks, sep);
    }

    #[test]
    fn noise_before_a_statement_never_hides_it(
        noise in junk(),
        w in prop::sample::select(STATEMENT_POOL.to_vec()),
    ) {
        let d = SqliteDialect::new();
        let script = format!("{noise}\n;\n{w}");
        let items = d.split_script(&script);
        let base = d.classify(w);
        if let Some(last) = items.last()
            && last.text(&script) == w.trim_end()
        {
            prop_assert!(d.classify(&script) >= base, "{script:?}");
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Security review (empty-delimiter audit): this dialect has no `DELIMITER` directive and
    /// ignores a caller-supplied `ScriptState` delimiter, whatever it is (no panic, the same
    /// items as the default state).
    #[test]
    fn a_state_delimiter_is_ignored(
        script in junk(),
        delim in prop_oneof![
            "\\PC{0,6}",
            prop::sample::select(vec!["", " ", ";", "$$", "//", "\\", "'", "--", "/*"])
                .prop_map(str::to_owned),
        ],
    ) {
        use ninesql_driver_api::{ScriptState, TextPosition};
        let d = SqliteDialect;
        let state = ScriptState::with_delimiter(Some(delim));
        let mut froms = vec![TextPosition::START];
        froms.extend(d.split_script(&script).iter().map(|i| i.end));
        for from in froms {
            prop_assert_eq!(
                d.split_script_ahead(&script, from, &state),
                d.split_script_ahead(&script, from, &ScriptState::default())
            );
        }
    }
}
