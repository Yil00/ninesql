//! SQL formatter (F-SQL-09): formatting never changes the meaning. Over the classification
//! corpus with every dialect the drivers ship, and over random statements, the formatted text
//! has the same tokens (keywords compared without case), the same comments and the same
//! classification as the original.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).

use std::sync::Arc;

use ninesql_core::driver_api::conformance::parse_classification_corpus;
use ninesql_core::driver_api::{CodeTokenKind, Dialect, Driver, code_tokens};
use ninesql_core::{FormatOptions, KeywordCase, format_sql};
use proptest::prelude::*;

fn dialects() -> Vec<(String, Arc<dyn Dialect>)> {
    let drivers: Vec<Box<dyn Driver>> = vec![
        Box::new(ninesql_driver_pgwire::PgWireDriver::new()),
        Box::new(ninesql_driver_mysqlwire::MySqlWireDriver::new()),
        Box::new(ninesql_driver_sqlite::SqliteDriver::new()),
    ];
    let mut out = Vec::new();
    for d in drivers {
        for e in d.engines() {
            out.push((e.as_str().to_owned(), d.dialect(e).unwrap()));
        }
    }
    out.push((
        "ansi".to_owned(),
        Arc::new(ninesql_core::driver_api::AnsiDialect) as Arc<dyn Dialect>,
    ));
    out
}

/// Tokens and comments of one script item.
type Signature = (Vec<(CodeTokenKind, String)>, Vec<String>);

/// Per item of the dialect's script split (what the driver runs): the statement tokens and
/// comments, or the text of a directive; then the text between items.
fn signature(d: &dyn Dialect, text: &str) -> Vec<Signature> {
    let mut out = Vec::new();
    let mut at = 0;
    for item in d.split_script(text) {
        let between = text[at..item.span.start].trim();
        if !between.is_empty() {
            out.push((Vec::new(), vec![between.to_owned()]));
        }
        let piece = &text[item.span.start..item.span.end];
        if item.kind == ninesql_core::driver_api::ScriptItemKind::Statement {
            out.push(statement_signature(d, piece));
        } else {
            out.push((Vec::new(), vec![piece.to_owned()]));
        }
        at = item.span.end;
    }
    if !text[at..].trim().is_empty() {
        out.push((Vec::new(), vec![text[at..].trim().to_owned()]));
    }
    out
}

/// Tokens (words upper-cased) and the non-blank text between them (comments), in order.
fn statement_signature(d: &dyn Dialect, text: &str) -> Signature {
    let tokens = code_tokens(text, d.lexical_rules());
    let mut toks = Vec::new();
    let mut gaps = Vec::new();
    let mut at = 0;
    for t in &tokens {
        let gap = &text[at..t.start];
        if !gap.trim().is_empty() {
            gaps.push(gap.trim().to_owned());
        }
        let s = t.text(text);
        toks.push((
            t.kind,
            if t.kind == CodeTokenKind::Word {
                s.to_ascii_uppercase()
            } else {
                s.to_owned()
            },
        ));
        at = t.end;
    }
    if !text[at..].trim().is_empty() {
        gaps.push(text[at..].trim().to_owned());
    }
    (toks, gaps)
}

fn check(d: &dyn Dialect, name: &str, text: &str) {
    for options in [
        FormatOptions::default(),
        FormatOptions::default().with_keyword_case(KeywordCase::Lower),
        FormatOptions::default().with_layout(false).with_indent(4),
    ] {
        let out = format_sql(d, text, &options);
        assert_eq!(
            signature(d, text),
            signature(d, &out),
            "[{name}] tokens or comments changed:\n{text}\n---\n{out}"
        );
        // Statement by statement, the classification is unchanged.
        let a: Vec<_> = d
            .split_statements(text)
            .iter()
            .map(|s| d.classify(s.text(text)))
            .collect();
        let b: Vec<_> = d
            .split_statements(&out)
            .iter()
            .map(|s| d.classify(s.text(&out)))
            .collect();
        assert_eq!(a, b, "[{name}] classification changed:\n{text}\n---\n{out}");
    }
}

#[test]
fn corpus_statements_keep_their_tokens_comments_and_class() {
    let cases =
        parse_classification_corpus(ninesql_core::driver_api::conformance::CLASSIFICATION_CORPUS)
            .unwrap();
    assert!(cases.len() > 100);
    let dialects = dialects();
    for case in &cases {
        for (name, d) in &dialects {
            check(d.as_ref(), name, &case.sql);
        }
    }
}

#[test]
fn layout_example() {
    let d = ninesql_driver_pgwire::PgWireDriver::new();
    let pg = d.dialect(&d.engines()[0]).unwrap();
    let out = format_sql(
        pg.as_ref(),
        "select id, $$ select  x $$ as body from public.orders o join items i on i.o = o.id where o.total > 10 -- big\n order by 1;\nupdate t set a = 1 where b = 2",
        &FormatOptions::default(),
    );
    assert_eq!(
        out,
        "SELECT id,\n  $$ select  x $$ AS body\nFROM public.orders o\nJOIN items i ON i.o = o.id\nWHERE o.total > 10 -- big\n ORDER BY 1;\nUPDATE t\nSET a = 1\nWHERE b = 2"
    );
}

const WORDS: &[&str] = &[
    "select",
    "from",
    "where",
    "and",
    "or",
    "group",
    "by",
    "order",
    "join",
    "left",
    "on",
    "insert",
    "into",
    "values",
    "update",
    "set",
    "delete",
    "with",
    "as",
    "union",
    "all",
    "limit",
    "t",
    "a",
    "b",
    "user",
    "status",
    "end",
    "1",
    "2.5",
    "'x  y'",
    "'it''s'",
    "\"Quoted  Name\"",
    "`bt`",
    "$$ d $$",
    "E'\\''",
    "(",
    ")",
    ",",
    ".",
    "*",
    "=",
    "<",
    ">",
    "-",
    "+",
    "::",
    ";",
    "@v",
    "-- c\n",
    "/* c */",
    "# h\n",
    "/*!50100 select */",
];

fn statement() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        (
            0..WORDS.len(),
            prop_oneof![
                Just(""),
                Just(" "),
                Just("  "),
                Just("\n"),
                Just("\t "),
                Just("\r\n")
            ],
        ),
        0..30,
    )
    .prop_map(|parts| {
        let mut s = String::new();
        for (w, sep) in parts {
            s.push_str(WORDS[w]);
            s.push_str(sep);
        }
        s
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn random_statements_keep_their_tokens(text in statement()) {
        for (name, d) in dialects() {
            check(d.as_ref(), &name, &text);
        }
    }

    #[test]
    fn arbitrary_text_never_panics_and_keeps_tokens(text in "\\PC{0,120}") {
        for (name, d) in dialects() {
            check(d.as_ref(), &name, &text);
        }
    }
}

#[test]
fn sqlite_names_that_are_keywords_elsewhere_keep_their_case() {
    let d = ninesql_driver_sqlite::SqliteDriver::new();
    let sqlite = d.dialect(&d.engines()[0]).unwrap();
    let out = format_sql(
        sqlite.as_ref(),
        "create table t (key text, left int, desc text); select key, left from t order by desc",
        &FormatOptions::default(),
    );
    assert_eq!(
        out,
        "CREATE TABLE t (key text, left int, desc text); SELECT key,\n  left\nFROM t\nORDER BY desc"
    );
}
