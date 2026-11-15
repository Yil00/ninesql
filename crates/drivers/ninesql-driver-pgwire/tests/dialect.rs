//! PostgreSQL dialect without a server (P3-D): the shared classification corpus, script
//! splitting (psql conventions) and property tests. Always runs.

use ninesql_driver_api::conformance::{
    self, STATEMENT_POOL, assert_join_split_roundtrip, assert_script_split_invariants,
};
use ninesql_driver_api::{Dialect, ScriptItemKind, ScriptRefusal, StatementClass};
use ninesql_driver_pgwire::PgDialect;
use proptest::prelude::*;

fn make(modes: &[String]) -> Option<Box<dyn Dialect>> {
    let mut scs = true;
    for m in modes {
        match m.as_str() {
            "scs_off" => scs = false,
            _ => return None,
        }
    }
    Some(Box::new(PgDialect::with_standard_conforming_strings(scs)))
}

#[test]
fn classification_corpus() {
    let n = conformance::assert_classification_corpus("pg", &make);
    assert!(n > 100, "only {n} cases ran");
}

fn split(script: &str) -> Vec<(ScriptItemKind, String)> {
    let d = PgDialect::new();
    assert_script_split_invariants(&d, script);
    d.split_script(script)
        .iter()
        .map(|i| (i.kind, i.text(script).to_owned()))
        .collect()
}

fn statements(script: &str) -> Vec<String> {
    split(script)
        .into_iter()
        .map(|(k, t)| {
            assert_eq!(k, ScriptItemKind::Statement, "{t}");
            t
        })
        .collect()
}

#[test]
fn dollar_quotes_and_function_bodies() {
    let s = "CREATE FUNCTION f() RETURNS int LANGUAGE plpgsql AS $body$\n\
             BEGIN\n  DELETE FROM t; RETURN 1;\nEND;\n$body$;\n\
             SELECT $$;$$, $x$ $$ ; $x$;\nSELECT 3";
    let got = statements(s);
    assert_eq!(got.len(), 3, "{got:?}");
    assert!(got[0].ends_with("$body$"));
    assert_eq!(got[1], "SELECT $$;$$, $x$ $$ ; $x$");
    assert_eq!(got[2], "SELECT 3");
    // A dollar tag needs a token start: `a$b$` is a name, `$1` a parameter.
    assert_eq!(
        statements("SELECT a$b$ FROM t; SELECT $1"),
        ["SELECT a$b$ FROM t", "SELECT $1"]
    );
    // Non-ASCII tags.
    assert_eq!(statements("SELECT $é$ ; $é$; SELECT 2").len(), 2);
}

#[test]
fn strings_and_comments() {
    assert_eq!(
        statements("SELECT E'it\\'s;'; SELECT 'a\\'; SELECT 2"),
        ["SELECT E'it\\'s;'", "SELECT 'a\\'", "SELECT 2"]
    );
    // standard_conforming_strings = off: the backslash escapes in every string.
    let off = PgDialect::with_standard_conforming_strings(false);
    assert_eq!(off.split_script("SELECT 'a\\'; SELECT 2'").len(), 1);
    assert_eq!(
        statements("SELECT 1 /* a /* b; */ c; */; -- x;\nSELECT 2"),
        ["SELECT 1", "SELECT 2"]
    );
    // A carriage return ends a line comment.
    assert_eq!(statements("SELECT 1 --x\r; SELECT 2").len(), 2);
    assert_eq!(statements("SELECT U&'d\\0061;ta'; SELECT 2").len(), 2);
}

#[test]
fn begin_atomic_bodies() {
    let s = "CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql\n\
             BEGIN ATOMIC\n  SELECT CASE WHEN a > 0 THEN 1 ELSE 2 END;\n  SELECT a;\nEND;\n\
             CREATE OR REPLACE PROCEDURE p() LANGUAGE sql BEGIN ATOMIC INSERT INTO t VALUES (1); END;\n\
             SELECT 1";
    let got = statements(s);
    assert_eq!(got.len(), 3, "{got:?}");
    assert!(got[0].starts_with("CREATE FUNCTION") && got[0].ends_with("END"));
    assert!(got[1].starts_with("CREATE OR REPLACE PROCEDURE") && got[1].ends_with("END"));
    assert_eq!(got[2], "SELECT 1");
    // `BEGIN` outside a routine definition is a transaction.
    assert_eq!(
        statements("BEGIN; DELETE FROM t; END"),
        ["BEGIN", "DELETE FROM t", "END"]
    );
    // Text after the closing END stays separate (and classified).
    let d = PgDialect::new();
    let s =
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT 1; END; DELETE FROM t";
    assert_eq!(statements(s).len(), 2);
    assert_eq!(d.classify(s), StatementClass::Destructive);
}

#[test]
fn semicolons_inside_parentheses() {
    let s = "CREATE RULE r AS ON INSERT TO a DO ALSO (DELETE FROM b; DELETE FROM c); SELECT 1";
    let got = statements(s);
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(
        PgDialect::new().classify(&got[0]),
        StatementClass::Destructive
    );
}

#[test]
fn copy_from_stdin_data_is_never_sql() {
    let s = "SELECT 1;\nCOPY t (a, b) FROM STDIN WITH (FORMAT text);\n1\tDROP TABLE x;\n2\t'\n\\.\nSELECT 2;\n";
    let got = split(s);
    assert_eq!(got.len(), 3, "{got:?}");
    assert_eq!(got[0], (ScriptItemKind::Statement, "SELECT 1".into()));
    assert_eq!(
        got[1].0,
        ScriptItemKind::Refused(ScriptRefusal::CopyFromStdin)
    );
    assert!(got[1].1.ends_with("\\."), "{:?}", got[1].1);
    assert_eq!(got[2], (ScriptItemKind::Statement, "SELECT 2".into()));
    // Without the terminator, the data runs to the end.
    let got = split("COPY t FROM stdin;\n1\n; DELETE FROM t;\n");
    assert_eq!(got.len(), 1, "{got:?}");
    // COPY TO STDOUT and COPY FROM a file have no inline data.
    assert_eq!(statements("COPY t TO STDOUT; SELECT 1").len(), 2);
    assert_eq!(statements("COPY t FROM '/tmp/x'; SELECT 1").len(), 2);
}

#[test]
fn psql_meta_commands_are_refused() {
    let got = split("\\connect nine\nSELECT 1;\n\\set x 1\nDELETE FROM t;");
    let kinds: Vec<_> = got.iter().map(|(k, _)| *k).collect();
    assert_eq!(
        kinds,
        [
            ScriptItemKind::Refused(ScriptRefusal::PsqlMetaCommand),
            ScriptItemKind::Statement,
            ScriptItemKind::Refused(ScriptRefusal::PsqlMetaCommand),
            ScriptItemKind::Statement,
        ]
    );
    assert_eq!(got[0].1, "\\connect nine");
    assert_eq!(got[3].1, "DELETE FROM t");
}

#[test]
fn positions() {
    let s = "SELECT 1;\n  -- c\n  SELECT 'é';\r\n\tDELETE\nFROM t";
    let items = PgDialect::new().split_script(s);
    assert_eq!(items.len(), 3);
    assert_eq!((items[1].start.line, items[1].start.column), (3, 3));
    assert_eq!((items[1].end.line, items[1].end.column), (3, 13));
    assert_eq!((items[2].start.line, items[2].start.column), (4, 2));
    assert_eq!((items[2].end.line, items[2].end.column), (5, 7));
    assert_eq!(items[2].start.offset, items[2].span.start);
}

fn junk() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(vec![
            "'",
            "\"",
            "$$",
            "$a$",
            "/*",
            "*/",
            "--",
            "\n",
            "\r",
            ";",
            "(",
            ")",
            "E'",
            "\\",
            "BEGIN",
            "END",
            "CASE",
            "CREATE FUNCTION",
            "COPY t FROM STDIN",
            "\\.",
            "DELETE FROM t",
            "SELECT 1",
            " ",
            "é",
            "U&'",
            "ATOMIC",
        ]),
        0..40,
    )
    .prop_map(|v| v.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn split_invariants_hold_on_junk(s in junk()) {
        assert_script_split_invariants(&PgDialect::new(), &s);
        assert_script_split_invariants(&PgDialect::with_standard_conforming_strings(false), &s);
    }

    #[test]
    fn split_invariants_hold_on_any_text(s in "\\PC{0,200}") {
        assert_script_split_invariants(&PgDialect::new(), &s);
    }

    #[test]
    fn windowed_split_matches_the_single_pass_on_large_scripts(
        parts in prop::collection::vec(junk(), 1..4),
        filler in 60_000usize..70_000,
    ) {
        // Large enough to need several windows, with junk around the window edges.
        let pad = "SELECT 'x';\n".repeat(filler / 12);
        let s = format!("{}{pad}{}{pad}", parts[0], parts[1..].concat());
        let d = PgDialect::new();
        let items = d.split_script(&s);
        if !items.iter().any(|i| i.kind != ScriptItemKind::Statement) {
            let a: Vec<_> = items.iter().map(|i| i.span).collect();
            prop_assert_eq!(a, d.split_statements(&s));
        }
    }

    #[test]
    fn joined_statements_split_back_and_keep_their_class(
        picks in prop::collection::vec(prop::sample::select(STATEMENT_POOL.to_vec()), 1..8),
        sep in prop::sample::select(vec![";\n", " ; ", ";", ";\n-- c\n", ";/* ; */\n"]),
    ) {
        assert_join_split_roundtrip(&PgDialect::new(), &picks, sep);
    }

    #[test]
    fn noise_before_a_statement_never_hides_it(
        noise in junk(),
        w in prop::sample::select(STATEMENT_POOL.to_vec()),
    ) {
        // Whenever the splitter runs `w` as a statement of its own, classifying the whole
        // script covers it: noise cannot make a write look like part of a string or comment
        // to the classifier but not to the splitter.
        let d = PgDialect::new();
        let script = format!("{noise}\n;\n{w}");
        let items = d.split_script(&script);
        let base = d.classify(w);
        if let Some(last) = items.last()
            && last.kind == ScriptItemKind::Statement
            && last.text(&script) == w.trim_end()
        {
            prop_assert!(d.classify(&script) >= base, "{script:?}");
        }
    }
}

/// A pg_dump-like script: many COPY blocks, more than one split window (64 KiB), and the
/// windowed split agrees with the single-pass split on every statement.
#[test]
fn copy_heavy_scripts_split_in_linear_time() {
    let mut s = String::new();
    for i in 0..300 {
        s.push_str(&format!(
            "CREATE TABLE t{i} (a int, b text); /* ; */ SELECT $x$;\n;$x$ -- ;\n;\n\
             COPY t{i} (a, b) FROM stdin;\n"
        ));
        for r in 0..50 {
            s.push_str(&format!("{r}\tvalue '; DROP TABLE t{i}; {r}\n"));
        }
        s.push_str("\\.\n\n");
    }
    let start = std::time::Instant::now();
    let items = PgDialect::new().split_script(&s);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(10),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(items.len(), 900);
    let copies = items
        .iter()
        .filter(|i| i.kind == ScriptItemKind::Refused(ScriptRefusal::CopyFromStdin))
        .count();
    assert_eq!(copies, 300);
    assert_script_split_invariants(&PgDialect::new(), &s);
    // Without COPY blocks, the windowed split is the single-pass split.
    let plain: String = (0..20_000)
        .map(|i| format!("SELECT {i}, ';' /* ; */;\n"))
        .collect();
    let d = PgDialect::new();
    let a: Vec<_> = d.split_script(&plain).iter().map(|i| i.span).collect();
    assert_eq!(a, d.split_statements(&plain));
}
