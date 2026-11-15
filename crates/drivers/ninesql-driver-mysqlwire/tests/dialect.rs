//! MySQL / MariaDB dialect without a server (P3-D): the shared classification corpus, script
//! splitting (mysql client conventions: `DELIMITER`) and property tests. Always runs.

use ninesql_driver_api::conformance::{
    self, STATEMENT_POOL, assert_join_split_roundtrip, assert_script_split_invariants,
};
use ninesql_driver_api::{Dialect, ScriptItemKind, ScriptRefusal, StatementClass};
use ninesql_driver_mysqlwire::MyDialect;
use proptest::prelude::*;

fn make_with(base: fn() -> MyDialect) -> impl Fn(&[String]) -> Option<Box<dyn Dialect>> {
    move |modes: &[String]| {
        let mut sql_mode = Vec::new();
        for m in modes {
            match m.as_str() {
                "no_backslash_escapes" => sql_mode.push("NO_BACKSLASH_ESCAPES"),
                "ansi_quotes" => sql_mode.push("ANSI_QUOTES"),
                _ => return None,
            }
        }
        Some(Box::new(base().with_sql_mode(&sql_mode.join(","))) as Box<dyn Dialect>)
    }
}

#[test]
fn classification_corpus_mysql() {
    let n = conformance::assert_classification_corpus("mysql", &make_with(MyDialect::mysql));
    assert!(n > 100, "only {n} cases ran");
}

#[test]
fn classification_corpus_mariadb() {
    let n = conformance::assert_classification_corpus("mariadb", &make_with(MyDialect::mariadb));
    assert!(n > 100, "only {n} cases ran");
}

fn split_with(d: &MyDialect, script: &str) -> Vec<(ScriptItemKind, String)> {
    assert_script_split_invariants(d, script);
    d.split_script(script)
        .iter()
        .map(|i| (i.kind, i.text(script).to_owned()))
        .collect()
}

fn split(script: &str) -> Vec<(ScriptItemKind, String)> {
    split_with(&MyDialect::mysql(), script)
}

fn statements(script: &str) -> Vec<String> {
    split(script)
        .into_iter()
        .filter(|(k, _)| *k != ScriptItemKind::ClientDirective)
        .map(|(k, t)| {
            assert_eq!(k, ScriptItemKind::Statement, "{t}");
            t
        })
        .collect()
}

const S: ScriptItemKind = ScriptItemKind::Statement;
const D: ScriptItemKind = ScriptItemKind::ClientDirective;

#[test]
fn delimiter_directive() {
    let s = "SELECT 1;\n\
             DELIMITER //\n\
             CREATE PROCEDURE p()\nBEGIN\n  SELECT 1; SELECT ';//';\nEND //\n\
             CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW BEGIN SET NEW.a = 1; END//\n\
             SELECT 2; SELECT 3 //\n\
             delimiter ;\n\
             SELECT 4;";
    let got = split(s);
    let kinds: Vec<_> = got.iter().map(|(k, _)| *k).collect();
    assert_eq!(kinds, [S, D, S, S, S, S, D, S], "{got:?}");
    assert_eq!(got[1].1, "DELIMITER //");
    assert!(got[2].1.starts_with("CREATE PROCEDURE") && got[2].1.ends_with("END"));
    assert!(got[3].1.starts_with("CREATE TRIGGER") && got[3].1.ends_with("END"));
    // Under a custom delimiter, the text between two delimiters is split again.
    assert_eq!(got[4].1, "SELECT 2");
    assert_eq!(got[5].1, "SELECT 3");
    assert_eq!(got[6].1, "delimiter ;");
    assert_eq!(got[7].1, "SELECT 4");
}

#[test]
fn delimiter_variants() {
    // `$$`, case-insensitive multi-character delimiters, inside strings and comments ignored.
    let s = "DELIMITER $$\nSELECT '$$' $$ /* $$ */ SELECT 2 $$\nDELIMITER ;\n";
    assert_eq!(statements(s), ["SELECT '$$'", "SELECT 2"]);
    let s = "DELIMITER end_of\nSELECT 1 END_OF SELECT 2";
    assert_eq!(statements(s), ["SELECT 1", "SELECT 2"]);
    // A refused delimiter keeps the previous one.
    for bad in [
        "DELIMITER",
        "DELIMITER \\\\",
        "DELIMITER '//'",
        "DELIMITER #",
        "DELIMITER /*",
    ] {
        let got = split(&format!("{bad}\nSELECT 1; SELECT 2"));
        assert_eq!(
            got[0].0,
            ScriptItemKind::Refused(ScriptRefusal::InvalidDelimiter),
            "{bad}: {got:?}"
        );
        assert_eq!(got.len(), 3, "{bad}: {got:?}");
    }
    // `delimiters` is not the directive.
    assert_eq!(split("delimiters\n").len(), 1);
    // Comments before the directive.
    let got = split("-- setup\n/* x */ # y\nDELIMITER ;;\nSELECT 1;; SELECT 2;;");
    assert_eq!(got.len(), 3, "{got:?}");
}

#[test]
fn compound_statements_without_delimiter() {
    let s = "CREATE PROCEDURE p() BEGIN DECLARE x INT; IF x THEN SELECT 1; END IF; \
             CASE x WHEN 1 THEN SELECT 2; END CASE; WHILE x DO SET x = 0; END WHILE; END; SELECT 9";
    let got = statements(s);
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[1], "SELECT 9");
    // MariaDB anonymous block: one statement, which needs confirmation.
    let got = statements("BEGIN NOT ATOMIC DELETE FROM t; SELECT 1; END; SELECT 2");
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(
        MyDialect::mariadb().classify(&got[0]),
        StatementClass::Unknown
    );
}

#[test]
fn lexical_modes() {
    // Backslash escapes, then NO_BACKSLASH_ESCAPES.
    let s = "SELECT 'a\\'; SELECT 2; SELECT 'x'";
    assert_eq!(statements(s).len(), 1);
    let nbe = MyDialect::mysql().with_sql_mode("NO_BACKSLASH_ESCAPES");
    assert_eq!(split_with(&nbe, s).len(), 3);
    // ANSI_QUOTES: `"…"` is an identifier with no escapes.
    let ansi = MyDialect::mysql().with_sql_mode("ANSI_QUOTES");
    assert_eq!(
        split_with(&ansi, "SELECT \"a\\\"; SELECT 2; SELECT \"").len(),
        3
    );
    assert_eq!(statements("SELECT \"a\\\"; SELECT 2; SELECT \"").len(), 1);
    // `#` and `-- ` comments; `--x` is not one.
    assert_eq!(
        statements("SELECT 1 # ;\n; SELECT 2 -- ;\n"),
        ["SELECT 1", "SELECT 2"]
    );
    assert_eq!(statements("SELECT 1--1\n; SELECT 2").len(), 2);
    // A `;` inside an executable comment ends a statement, as for the mysql client (and the
    // single-statement check refuses the whole text); the classifier sees both.
    assert_eq!(
        statements("SELECT 1 /*! ; DELETE FROM t */"),
        ["SELECT 1 /*!", "DELETE FROM t */"]
    );
    // The piece after the `;` starts inside the comment: Unknown (re-review L1).
    assert_eq!(
        MyDialect::mysql().classify("SELECT 1 /*! ; DELETE FROM t */"),
        StatementClass::Unknown
    );
    for junk in ["/*!;*//*", "/*!;*/BEGIN", "SELECT 1 /*! ; */ BEGIN"] {
        assert_script_split_invariants(&MyDialect::mysql(), junk);
        assert_eq!(
            MyDialect::mysql().classify(junk),
            StatementClass::Unknown,
            "{junk}"
        );
    }
    // A statement keeps its executable comment, and so its version condition.
    assert_eq!(
        statements("/*!40101 SET NAMES utf8mb4 */;\n/*M!100100 SELECT 1 */;"),
        ["/*!40101 SET NAMES utf8mb4 */", "/*M!100100 SELECT 1 */"]
    );
    assert_eq!(
        statements("DELETE FROM t /*!99999 WHERE 0 */; SELECT 1"),
        ["DELETE FROM t /*!99999 WHERE 0 */", "SELECT 1"]
    );
}

#[test]
fn client_commands_are_refused() {
    let got = split("\\d //\nSELECT 1//");
    assert_eq!(
        got[0].0,
        ScriptItemKind::Refused(ScriptRefusal::MysqlClientCommand)
    );
}

#[test]
fn positions() {
    let s = "DELIMITER //\nSELECT 1 //\n  SELECT 'ü' //";
    let items = MyDialect::mysql().split_script(s);
    assert_eq!(items.len(), 3);
    assert_eq!((items[1].start.line, items[1].start.column), (2, 1));
    assert_eq!((items[2].start.line, items[2].start.column), (3, 3));
    assert_eq!((items[2].end.line, items[2].end.column), (3, 13));
}

/// A mysqldump-like script: many `DELIMITER ;;` blocks and long runs under a custom delimiter,
/// larger than one split window (64 KiB); split in linear time, and without directives the
/// windowed split is the single-pass split.
#[test]
fn delimiter_heavy_scripts_split_in_linear_time() {
    let mut s = String::new();
    for i in 0..300 {
        s.push_str(&format!(
            "CREATE TABLE t{i} (a INT); /* ; */ INSERT INTO t{i} VALUES (1), (2); # ;\n\
             DELIMITER ;;\n\
             CREATE TRIGGER tr{i} BEFORE INSERT ON t{i} FOR EACH ROW BEGIN SET NEW.a = 1; END ;;\n"
        ));
        for r in 0..40 {
            s.push_str(&format!("SELECT {r}, ';;' ;;\n"));
        }
        s.push_str("DELIMITER ;\n");
    }
    let start = std::time::Instant::now();
    let items = MyDialect::mysql().split_script(&s);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(10),
        "{:?}",
        start.elapsed()
    );
    let statements = items.iter().filter(|i| i.kind == S).count();
    assert_eq!(statements, 300 * 43);
    assert_eq!(items.iter().filter(|i| i.kind == D).count(), 600);
    assert_script_split_invariants(&MyDialect::mysql(), &s);
    let plain: String = (0..20_000)
        .map(|i| format!("SELECT {i}, ';' /* ; */;\n"))
        .collect();
    let d = MyDialect::mysql();
    let a: Vec<_> = d.split_script(&plain).iter().map(|i| i.span).collect();
    assert_eq!(a, d.split_statements(&plain));
}

fn nbe_after_set(text: &str) -> Option<Box<dyn Dialect>> {
    text.to_ascii_uppercase()
        .starts_with("SET SESSION SQL_MODE")
        .then(|| {
            Box::new(MyDialect::mysql().with_sql_mode("NO_BACKSLASH_ESCAPES")) as Box<dyn Dialect>
        })
}

/// Security review H2: after `SET sql_mode`, the rest is re-split with the session's new
/// dialect (reference loop `conformance::script_plan`), also under a custom `DELIMITER`.
#[test]
fn script_is_resplit_after_a_lexical_mode_change() {
    use ninesql_driver_api::conformance::script_plan;
    let script = "SET SESSION sql_mode='NO_BACKSLASH_ESCAPES';\n\
                  UPDATE t SET a='x\\' -- ' WHERE id=1;\n\
                  SELECT 2;";
    // The stale split keeps the WHERE in the UPDATE: classified with the old rules, a Write.
    let stale = MyDialect::mysql().split_script(script);
    assert_eq!(
        stale[1].text(script),
        "UPDATE t SET a='x\\' -- ' WHERE id=1"
    );
    assert_eq!(
        MyDialect::mysql().classify(stale[1].text(script)),
        StatementClass::Write
    );
    let plan = script_plan(script, Box::new(MyDialect::mysql()), &mut nbe_after_set);
    let got: Vec<_> = plan.iter().map(|p| (p.text.as_str(), p.class)).collect();
    assert_eq!(
        got,
        [
            (
                "SET SESSION sql_mode='NO_BACKSLASH_ESCAPES'",
                Some(StatementClass::ChangesAccessMode)
            ),
            // With NO_BACKSLASH_ESCAPES the WHERE is in a comment: every row is updated.
            (
                "UPDATE t SET a='x\\' -- ' WHERE id=1;\nSELECT 2",
                Some(StatementClass::Destructive)
            ),
        ]
    );
    // Under a custom delimiter, the delimiter in force survives the re-split.
    let script = "DELIMITER //\n\
                  SET SESSION sql_mode='NO_BACKSLASH_ESCAPES' //\n\
                  UPDATE t SET a='x\\' -- ' WHERE id=1 //\n\
                  SELECT 2 //";
    let plan = script_plan(script, Box::new(MyDialect::mysql()), &mut nbe_after_set);
    let got: Vec<_> = plan.iter().map(|p| (p.text.as_str(), p.class)).collect();
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[1].1, Some(StatementClass::Destructive));
    let rest = MyDialect::mysql()
        .with_sql_mode("NO_BACKSLASH_ESCAPES")
        .split_script_from(script, MyDialect::mysql().split_script(script)[1].span.end);
    assert!(rest.iter().all(|i| i.kind == S), "{rest:?}");
    // ANSI_QUOTES shows in the lexical rules, so a caller comparing them re-splits.
    assert_ne!(
        MyDialect::mysql().lexical_rules(),
        MyDialect::mysql()
            .with_sql_mode("ANSI_QUOTES")
            .lexical_rules()
    );
}

/// Security review M4: a long stored-program body is counted once (it used to be rescanned
/// for every `;`), and directives every few bytes cost a small window each.
#[test]
fn long_bodies_and_many_directives_split_in_linear_time() {
    let body = format!(
        "CREATE PROCEDURE p() BEGIN {} END",
        "SELECT 1; ".repeat(32_768)
    );
    let d = MyDialect::mysql();
    let start = std::time::Instant::now();
    assert_eq!(d.split_statements(&body).len(), 1);
    assert!(ninesql_driver_api::ensure_single_statement(&d, &body).is_ok());
    let elapsed = start.elapsed();
    assert!(elapsed < std::time::Duration::from_secs(5), "{elapsed:?}");
    let s = "DELIMITER //\nSELECT 1 //\nDELIMITER ;\nSELECT 2;\n".repeat(8_192);
    let start = std::time::Instant::now();
    let items = d.split_script(&s);
    let elapsed = start.elapsed();
    assert_eq!(items.len(), 4 * 8_192);
    assert!(elapsed < std::time::Duration::from_secs(5), "{elapsed:?}");
}

/// Security review L3: a stored program needs its kind right after the `CREATE` clauses.
#[test]
fn stored_program_kind_must_follow_create() {
    let d = MyDialect::mysql();
    let two =
        "CREATE VIEW v AS SELECT procedure_name AS procedure, 1 AS begin FROM t; DROP TABLE x";
    assert_eq!(d.split_statements(two).len(), 2);
    // Dotted and quoted definer hosts under DELIMITER (re-review M2): one statement each.
    for def in [
        "root@127.0.0.1",
        "root@localhost.localdomain",
        "'root'@'%'",
        "`root`@`10.0.0.1`",
        "CURRENT_USER()",
        "root",
    ] {
        let script = format!(
            "DELIMITER //\nCREATE DEFINER = {def} PROCEDURE p() BEGIN SELECT 1; DELETE FROM t; END//\nDELIMITER ;"
        );
        let got = split(&script);
        let kinds: Vec<_> = got.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, [D, S, D], "{def}: {got:?}");
        let plain =
            format!("CREATE DEFINER = {def} PROCEDURE p() BEGIN SELECT 1; DELETE FROM t; END");
        assert_eq!(d.split_statements(&plain).len(), 1, "{def}");
    }
    // A view with a definer is no stored program.
    let view = "CREATE DEFINER = root@127.0.0.1 VIEW v AS SELECT procedure_x AS procedure, 1 AS begin FROM t; SELECT 1";
    assert_eq!(d.split_statements(view).len(), 2);
    for one in [
        "CREATE DEFINER = root@localhost PROCEDURE p() BEGIN SELECT 1; END",
        "CREATE DEFINER = CURRENT_USER TRIGGER tr BEFORE INSERT ON t FOR EACH ROW BEGIN SET NEW.a = 1; END",
        "CREATE OR REPLACE DEFINER = `u`@`%` SQL SECURITY INVOKER PROCEDURE p() BEGIN SELECT 1; END",
        "CREATE AGGREGATE FUNCTION f(x INT) RETURNS INT BEGIN DECLARE s INT; RETURN s; END",
        "CREATE EVENT e ON SCHEDULE EVERY 1 DAY DO BEGIN DELETE FROM t WHERE a = 1; END",
    ] {
        assert_eq!(d.split_statements(one).len(), 1, "{one}");
    }
}

/// Security review M3: one long line.
#[test]
fn one_long_line_splits_in_linear_time() {
    let s = "SELECT 1;".repeat(200_000);
    let start = std::time::Instant::now();
    assert_eq!(MyDialect::mysql().split_script(&s).len(), 200_000);
    let elapsed = start.elapsed();
    assert!(elapsed < std::time::Duration::from_secs(5), "{elapsed:?}");
}

/// Re-review M1: every statement changing the lexical modes keeps the reference loop linear
/// (it re-splits a small window ahead, and the delimiter in force is passed along).
#[test]
fn alternating_mode_changes_run_in_linear_time() {
    use ninesql_driver_api::conformance::script_plan;
    let mut toggle = |text: &str| -> Option<Box<dyn Dialect>> {
        let t = text.to_ascii_uppercase();
        if t.starts_with("SET SESSION SQL_MODE='NO_BACKSLASH_ESCAPES'") {
            Some(Box::new(
                MyDialect::mysql().with_sql_mode("NO_BACKSLASH_ESCAPES"),
            ))
        } else if t.starts_with("SET SESSION SQL_MODE=''") {
            Some(Box::new(MyDialect::mysql()))
        } else {
            None
        }
    };
    let pair =
        "SET SESSION sql_mode='NO_BACKSLASH_ESCAPES';\nSET SESSION sql_mode='';\nSELECT 'a;b';\n";
    let script = pair.repeat(276_000 / pair.len());
    let start = std::time::Instant::now();
    let plan = script_plan(&script, Box::new(MyDialect::mysql()), &mut toggle);
    let elapsed = start.elapsed();
    assert_eq!(plan.len(), 3 * (276_000 / pair.len()));
    assert!(elapsed < std::time::Duration::from_secs(5), "{elapsed:?}");
    // Under a custom delimiter too.
    let pair = "SET SESSION sql_mode='NO_BACKSLASH_ESCAPES' //\nSET SESSION sql_mode='' //\n";
    let script = format!("DELIMITER //\n{}", pair.repeat(100_000 / pair.len()));
    let start = std::time::Instant::now();
    let plan = script_plan(&script, Box::new(MyDialect::mysql()), &mut toggle);
    let elapsed = start.elapsed();
    assert_eq!(plan.len(), 2 * (100_000 / pair.len()));
    assert!(
        plan.iter().all(|p| !p.text.contains("//")),
        "{:?}",
        &plan[..2]
    );
    assert!(elapsed < std::time::Duration::from_secs(5), "{elapsed:?}");
}

fn junk() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(vec![
            "'",
            "\"",
            "`",
            "/*",
            "/*!",
            "/*!50000",
            "/*M!",
            "*/",
            "-- ",
            "--",
            "#",
            "\n",
            ";",
            "(",
            ")",
            "\\",
            "BEGIN",
            "END",
            "CASE",
            "IF",
            "CREATE PROCEDURE p()",
            "DELIMITER //",
            "DELIMITER ;",
            "//",
            "$$",
            "DELETE FROM t",
            "SELECT 1",
            " ",
            "é",
            "@",
            "@@",
            "$",
            "WHERE",
        ]),
        0..40,
    )
    .prop_map(|v| v.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn split_invariants_hold_on_junk(s in junk()) {
        assert_script_split_invariants(&MyDialect::mysql(), &s);
        assert_script_split_invariants(&MyDialect::mariadb().with_sql_mode("ANSI_QUOTES"), &s);
        assert_script_split_invariants(
            &MyDialect::mysql().with_sql_mode("NO_BACKSLASH_ESCAPES"), &s);
    }

    #[test]
    fn split_invariants_hold_on_any_text(s in "\\PC{0,200}") {
        assert_script_split_invariants(&MyDialect::mysql(), &s);
    }

    #[test]
    fn joined_statements_split_back_and_keep_their_class(
        picks in prop::collection::vec(prop::sample::select(STATEMENT_POOL.to_vec()), 1..8),
        sep in prop::sample::select(vec![";\n", " ; ", ";", ";\n# c\n", ";/* ; */\n"]),
    ) {
        assert_join_split_roundtrip(&MyDialect::mysql(), &picks, sep);
    }

    #[test]
    fn statements_under_a_custom_delimiter_keep_their_class(
        picks in prop::collection::vec(prop::sample::select(STATEMENT_POOL.to_vec()), 1..8),
        delim in prop::sample::select(vec!["//", "$$", ";;", "@@@"]),
    ) {
        let d = MyDialect::mysql();
        let body: Vec<String> = picks.iter().map(|p| format!("{p}\n{delim}\n")).collect();
        let script = format!("DELIMITER {delim}\n{}DELIMITER ;\n", body.concat());
        let items = d.split_script(&script);
        let stmts: Vec<_> = items.iter().filter(|i| i.kind == ScriptItemKind::Statement).collect();
        prop_assert_eq!(stmts.len(), picks.len(), "{:?}", script);
        for (item, p) in stmts.iter().zip(&picks) {
            prop_assert_eq!(d.classify(item.text(&script)), d.classify(p));
        }
    }

    #[test]
    fn noise_before_a_statement_never_hides_it(
        noise in junk(),
        w in prop::sample::select(STATEMENT_POOL.to_vec()),
    ) {
        let d = MyDialect::mysql();
        let script = format!("{noise}\n;\n{w}");
        let items = d.split_script(&script);
        let base = d.classify(w);
        if let Some(last) = items.last()
            && last.kind == ScriptItemKind::Statement
            && last.text(&script) == w.trim_end()
            && !items.iter().any(|i| i.kind != ScriptItemKind::Statement)
        {
            prop_assert!(d.classify(&script) >= base, "{script:?}");
        }
    }
}
