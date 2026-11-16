//! PostgreSQL dialect: quoting, literals, `$n` placeholders, explain, and the lexical rules kept
//! in sync with the session's `standard_conforming_strings` (docs/08, ADR-0004).
//!
//! Splitting follows psql on top of the shared lexer's tokens (P3-D):
//! - `;` inside parentheses does not end a statement (`CREATE RULE … DO ALSO (DELETE …;
//!   DELETE …)` is one statement);
//! - a `CREATE [OR REPLACE] FUNCTION | PROCEDURE` whose SQL-standard body is `BEGIN ATOMIC …
//!   END` (PostgreSQL 14+) is one statement: `;` inside the body does not split it (psql's
//!   heuristic: `BEGIN` / `CASE` / `END` counted outside parentheses);
//! - in [`split_script`](Dialect::split_script), `COPY … FROM STDIN` and its inline data block
//!   (up to a `\.` line) are one refused item, so data lines are never read as SQL, and a psql
//!   meta-command (a statement starting with `\`, to the end of its line) is refused too.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ninesql_driver_api::dialect::classify_with_rules;
use ninesql_driver_api::dml::{ColumnType, DmlError};
use ninesql_driver_api::{
    CodeToken, CodeTokenKind, Dialect, ExplainFormat, LexicalRules, Param, ScriptItem,
    ScriptItemKind, ScriptRefusal, ScriptState, StatementClass, StatementSpan, TextPosition,
    code_tokens, script_items, script_items_from,
};

use crate::engine::Flavor;

/// The PostgreSQL dialect.
///
/// The lexical rules start from [`LexicalRules::POSTGRES`] (`standard_conforming_strings = on`,
/// `E'…'` strings, dollar quotes, nested comments, `$` inside identifiers). A session dialect
/// shares the session's `standard_conforming_strings` flag, updated from every `ParameterStatus`
/// message, so the classifier and the splitter follow the session: with the setting off, a
/// backslash escapes the next character in every string literal.
///
/// Per-engine overrides (P4-C, docs/08): TimescaleDB's functions that drop, move or convert data
/// classify as `Unknown` (confirmation in production, refused in read-only sessions); QuestDB's
/// statements and functions that change data or the server outside PostgreSQL's grammar do the
/// same, and QuestDB has no `EXPLAIN ANALYZE`.
#[derive(Clone, Debug)]
pub struct PgDialect {
    standard_conforming_strings: Arc<AtomicBool>,
    flavor: Flavor,
    /// The server has `UNIQUE NULLS NOT DISTINCT` (PostgreSQL 15+); unknown offline: `false`.
    nulls_not_distinct: bool,
}

impl Default for PgDialect {
    fn default() -> Self {
        Self::new()
    }
}

impl PgDialect {
    /// The dialect of a server with `standard_conforming_strings = on` (the default since 9.1).
    #[must_use]
    pub fn new() -> Self {
        Self::for_engine(Flavor::Postgres)
    }

    /// The dialect of one engine of the family.
    pub(crate) fn for_engine(flavor: Flavor) -> Self {
        Self {
            standard_conforming_strings: Arc::new(AtomicBool::new(true)),
            flavor,
            nulls_not_distinct: false,
        }
    }

    /// The TimescaleDB dialect (PostgreSQL plus TimescaleDB's data-changing functions).
    #[must_use]
    pub fn timescaledb() -> Self {
        Self::for_engine(Flavor::Timescale)
    }

    /// The YugabyteDB (YSQL) dialect.
    #[must_use]
    pub fn yugabytedb() -> Self {
        Self::for_engine(Flavor::Yugabyte)
    }

    /// The QuestDB dialect.
    #[must_use]
    pub fn questdb() -> Self {
        Self::for_engine(Flavor::QuestDb)
    }

    /// A dialect with a fixed `standard_conforming_strings` value (tests, offline splitting).
    #[must_use]
    pub fn with_standard_conforming_strings(on: bool) -> Self {
        Self {
            standard_conforming_strings: Arc::new(AtomicBool::new(on)),
            flavor: Flavor::Postgres,
            nulls_not_distinct: false,
        }
    }

    /// The same engine's dialect with a fixed `standard_conforming_strings` value (tests,
    /// offline splitting).
    #[must_use]
    pub fn with_scs(self, on: bool) -> Self {
        Self {
            standard_conforming_strings: Arc::new(AtomicBool::new(on)),
            flavor: self.flavor,
            nulls_not_distinct: self.nulls_not_distinct,
        }
    }

    /// The same dialect for a server of major version `major` (`NULLS NOT DISTINCT` from 15 on).
    #[must_use]
    pub fn with_server_major(mut self, major: Option<u32>) -> Self {
        self.nulls_not_distinct = self.flavor != Flavor::QuestDb && major.is_some_and(|m| m >= 15);
        self
    }

    /// A dialect that follows a session's `standard_conforming_strings` flag.
    #[must_use]
    pub(crate) fn tracking(flag: Arc<AtomicBool>, flavor: Flavor) -> Self {
        Self {
            standard_conforming_strings: flag,
            flavor,
            nulls_not_distinct: false,
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    s
}

impl Dialect for PgDialect {
    fn lexical_rules(&self) -> LexicalRules {
        let scs = self.standard_conforming_strings.load(Ordering::SeqCst);
        LexicalRules::POSTGRES.with_backslash_escapes(!scs)
    }

    fn format_literal(&self, value: &Param) -> String {
        match value {
            // `X'…'` is a bit string in PostgreSQL; `decode` is independent of
            // `standard_conforming_strings`.
            Param::Bytes(b) => format!("decode('{}', 'hex')", hex(b)),
            Param::Timestamp { utc: true, .. } => {
                let ansi = ninesql_driver_api::AnsiDialect.format_literal(value);
                // `TIMESTAMP '… +00:00'` would drop the offset: use `timestamptz`.
                ansi.replacen("TIMESTAMP", "TIMESTAMPTZ", 1)
            }
            other => AnsiWithRules(self.lexical_rules()).format_literal(other),
        }
    }

    fn placeholder(&self, index: usize) -> String {
        format!("${index}")
    }

    fn dml_literal(&self, value: &Param, ty: &ColumnType) -> Result<String, &'static str> {
        // Independent of `standard_conforming_strings` (security review H1, L3).
        crate::dml::literal(value, ty)
    }

    fn dml_check_identifier(&self, name: &str) -> Result<(), DmlError> {
        crate::dml::check_identifier(name)
    }

    fn dml_comparable(&self, ty: &ColumnType) -> bool {
        crate::dml::comparable(ty)
    }

    fn supports_unique_nulls_not_distinct(&self) -> bool {
        self.nulls_not_distinct
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split_psql(script, self.lexical_rules())
    }

    fn split_script(&self, script: &str) -> Vec<ScriptItem> {
        script_items(script, self.pieces(script, 0, false))
    }

    fn split_script_ahead(
        &self,
        script: &str,
        from: TextPosition,
        _state: &ScriptState,
    ) -> Vec<ScriptItem> {
        if from.offset > script.len() || !script.is_char_boundary(from.offset) {
            return self.split_script_from(script, from.offset);
        }
        script_items_from(script, from, self.pieces(script, from.offset, true))
    }

    fn explain(&self, query: &str, format: ExplainFormat) -> Option<String> {
        let q = query.trim().trim_end_matches(';');
        Some(match format {
            ExplainFormat::Text => format!("EXPLAIN {q}"),
            ExplainFormat::Json => format!("EXPLAIN (FORMAT JSON) {q}"),
            ExplainFormat::Analyze if self.flavor != Flavor::QuestDb => {
                format!("EXPLAIN ANALYZE {q}")
            }
            _ => return None,
        })
    }

    fn classify(&self, text: &str) -> StatementClass {
        let rules = self.lexical_rules();
        let base = classify_with_rules(text, rules);
        // TimescaleDB's functions apply to every flavour (security review M1): a `postgresql`
        // connection to a database with the extension is accepted, and the names are very
        // unlikely to be user functions, so the cost is at most an extra confirmation.
        let timescale = engine_functions(text, rules, TIMESCALE_SIDE_EFFECT_FUNCTIONS)
            .max(timescale_internal_calls(text, rules));
        let extra = match self.flavor {
            Flavor::QuestDb => questdb_statement(text, rules).max(engine_functions(
                text,
                rules,
                QUESTDB_SIDE_EFFECT_FUNCTIONS,
            )),
            Flavor::Postgres | Flavor::Timescale | Flavor::Yugabyte => StatementClass::Read,
        };
        let extra = extra.max(timescale);
        base.max(extra)
    }
}

/// TimescaleDB functions (and procedures) that drop, move, convert or schedule changes to data
/// or schema, callable from a `SELECT` (`SELECT drop_chunks('t', older_than => …)` deletes
/// rows). TimescaleDB 2.x API (multi-node functions of 2.x before 2.14 included), lower case.
/// Checked in every pgwire dialect.
const TIMESCALE_SIDE_EFFECT_FUNCTIONS: &[&str] = &[
    "add_data_node",
    "alter_data_node",
    "attach_data_node",
    "cleanup_copy_chunk_operation",
    "copy_chunk",
    "create_distributed_hypertable",
    "create_distributed_restore_point",
    "delete_data_node",
    "detach_data_node",
    "distributed_exec",
    "freeze_chunk",
    "set_replication_factor",
    "unfreeze_chunk",
    "add_columnstore_policy",
    "add_compression_policy",
    "add_continuous_aggregate_policy",
    "add_dimension",
    "add_job",
    "add_reorder_policy",
    "add_retention_policy",
    "alter_job",
    "attach_chunk",
    "attach_tablespace",
    "compress_chunk",
    "convert_to_columnstore",
    "convert_to_rowstore",
    "create_hypertable",
    "decompress_chunk",
    "delete_job",
    "detach_chunk",
    "detach_tablespace",
    "detach_tablespaces",
    "disable_chunk_skipping",
    "drop_chunks",
    "enable_chunk_skipping",
    "merge_chunks",
    "move_chunk",
    "refresh_continuous_aggregate",
    "remove_columnstore_policy",
    "remove_compression_policy",
    "remove_continuous_aggregate_policy",
    "remove_reorder_policy",
    "remove_retention_policy",
    "reorder_chunk",
    "run_job",
    "set_chunk_time_interval",
    "set_integer_now_func",
    "set_number_partitions",
    "set_partitioning_interval",
    "split_chunk",
    "timescaledb_post_restore",
    "timescaledb_pre_restore",
];

/// Schemas of TimescaleDB's internal and experimental functions: any call qualified with one of
/// them is `Unknown` (security review L1).
const TIMESCALE_INTERNAL_SCHEMAS: &[&str] = &[
    "_timescaledb_functions",
    "_timescaledb_internal",
    "timescaledb_experimental",
];

/// `Unknown` when `text` calls a function qualified with a [`TIMESCALE_INTERNAL_SCHEMAS`]
/// schema (`schema.name(`, either part plain or quoted), `Read` otherwise.
fn timescale_internal_calls(text: &str, rules: LexicalRules) -> StatementClass {
    let toks = code_tokens(text, rules);
    let name = |t: &CodeToken| -> String {
        let raw = t.text(text);
        match t.kind {
            CodeTokenKind::Word => raw.to_ascii_lowercase(),
            CodeTokenKind::QuotedIdent => raw
                .get(1..raw.len().saturating_sub(1))
                .unwrap_or("")
                .to_owned(),
            _ => String::new(),
        }
    };
    let calls = toks.windows(4).any(|w| {
        w[1].text(text) == "."
            && w[3].kind == CodeTokenKind::OpenParen
            && matches!(w[2].kind, CodeTokenKind::Word | CodeTokenKind::QuotedIdent)
            && TIMESCALE_INTERNAL_SCHEMAS.contains(&name(&w[0]).as_str())
    });
    if calls {
        StatementClass::Unknown
    } else {
        StatementClass::Read
    }
}

/// QuestDB functions with side effects callable from a `SELECT` (all present on QuestDB 10.2):
/// configuration reload, query cancel, the query-cache flush, and the dumps written to the
/// server log.
const QUESTDB_SIDE_EFFECT_FUNCTIONS: &[&str] = &[
    "reload_config",
    "cancel_query",
    "flush_query_cache",
    "dump_memory_usage",
    "dump_thread_stacks",
];

/// `Unknown` when `text` calls one of `functions` (a word or quoted name right before `(`,
/// schema-qualified or not), `Read` otherwise.
fn engine_functions(text: &str, rules: LexicalRules, functions: &[&str]) -> StatementClass {
    let toks = code_tokens(text, rules);
    let calls = toks.windows(2).any(|w| {
        w[1].kind == CodeTokenKind::OpenParen
            && match w[0].kind {
                CodeTokenKind::Word => {
                    let name = w[0].text(text).to_ascii_lowercase();
                    functions.contains(&name.as_str())
                }
                CodeTokenKind::QuotedIdent => {
                    let t = w[0].text(text);
                    let name = t.get(1..t.len().saturating_sub(1)).unwrap_or("");
                    functions.contains(&name)
                }
                _ => false,
            }
    });
    if calls {
        StatementClass::Unknown
    } else {
        StatementClass::Read
    }
}

/// QuestDB statements outside PostgreSQL's grammar that change data, files or the server, by
/// their first words: they classify as `Unknown` whatever the shared classifier reads.
/// `ALTER TABLE … DROP | DETACH PARTITION`, `DROP ALL TABLES`, `TRUNCATE` and `UPDATE` without
/// `WHERE` are already `Destructive` there.
fn questdb_statement(text: &str, rules: LexicalRules) -> StatementClass {
    let toks = code_tokens(text, rules);
    let words: Vec<String> = toks
        .iter()
        .take_while(|t| t.kind == CodeTokenKind::Word)
        .take(2)
        .map(|t| t.text(text).to_ascii_uppercase())
        .collect();
    match words.first().map(String::as_str) {
        Some(
            "RENAME" | "VACUUM" | "REINDEX" | "SNAPSHOT" | "CHECKPOINT" | "BACKUP" | "COPY"
            | "CANCEL" | "REFRESH" | "DECLARE",
        ) => StatementClass::Unknown,
        _ => StatementClass::Read,
    }
}

impl PgDialect {
    /// The pieces of `script` from byte `start`; with `lazy`, only the first window's worth
    /// (at least one piece unless only blanks and comments are left).
    fn pieces(
        &self,
        script: &str,
        start: usize,
        lazy: bool,
    ) -> Vec<(StatementSpan, ScriptItemKind)> {
        let rules = self.lexical_rules();
        let mut pieces = Vec::new();
        let mut base = start;
        let mut window = MIN_WINDOW;
        'outer: while base < script.len() {
            if lazy && !pieces.is_empty() {
                break;
            }
            // The window follows what the last step consumed (twice it, within bounds), so a
            // script with a meta-command or a COPY block every few bytes lexes a few hundred
            // bytes per step, not a full window (security review M4).
            // Split a window of the rest. Every statement the window ends with a `;` is exactly
            // what splitting the whole rest gives (the lexer reads left to right, and under
            // PostgreSQL's rules no state crosses a statement boundary); the last one may be
            // cut by the window and is split again with the next window. So each COPY block
            // or meta-command costs one window, not a pass over the rest of the script.
            let mut end = (base + window).min(script.len());
            while !script.is_char_boundary(end) {
                end += 1;
            }
            let complete = end == script.len();
            let step_base = base;
            let mut spans = split_psql_terminated(&script[base..end], rules);
            // Where the next window starts: the first token of the statement the window may
            // have cut (a token start of the whole text, so the lexer restarts in step).
            let mut next = None;
            // A statement's start is exact even when the window cuts it, and a meta-command
            // only needs its start (it ends with its line): keep it.
            let last_is_meta = spans
                .last()
                .is_some_and(|(sp, _)| script.as_bytes()[sp.start + base] == b'\\');
            if !complete && !last_is_meta {
                next = spans.pop().map(|(s, _)| s.start + base);
                if spans.is_empty() {
                    window = window.saturating_mul(2);
                    continue;
                }
            }
            for (span, terminator) in spans {
                let terminator = terminator.map(|t| t + base);
                let span = StatementSpan {
                    start: span.start + base,
                    end: span.end + base,
                };
                let text = span.text(script);
                if text.starts_with('\\') {
                    // psql meta-command: runs to the end of its line.
                    let eol = line_end(script, span.start);
                    pieces.push((
                        trimmed(script, span.start, eol),
                        ScriptItemKind::Refused(ScriptRefusal::PsqlMetaCommand),
                    ));
                    base = eol;
                    window = next_window(base - step_base);
                    continue 'outer;
                }
                if is_copy_from_stdin(text, rules) {
                    // The data starts after the `;` the lexer found (none: no data).
                    let data_end = terminator.map_or(span.end, |t| copy_data_end(script, t));
                    pieces.push((
                        trimmed(script, span.start, data_end),
                        ScriptItemKind::Refused(ScriptRefusal::CopyFromStdin),
                    ));
                    base = data_end;
                    window = next_window(base - step_base);
                    continue 'outer;
                }
                pieces.push((span, ScriptItemKind::Statement));
            }
            match next {
                Some(n) if !complete => {
                    base = n;
                    window = next_window(base - step_base);
                }
                _ => break,
            }
        }
        pieces
    }
}

/// Splits like psql (`psqlscan.l`): a `;` ends a statement only outside parentheses and
/// outside a `BEGIN … END` block of a routine definition. A statement starting `CREATE [OR
/// REPLACE] FUNCTION | PROCEDURE` counts, outside parentheses, `BEGIN` (opens a block), `CASE`
/// (opens one inside a block) and `END` (closes one). The server parses each piece as one
/// statement, and the extended protocol refuses anything else, so a heuristic mistake fails
/// on the server instead of running a second statement. The classifier still splits on every
/// `;`, so nothing a merged piece holds escapes it.
fn split_psql(script: &str, rules: LexicalRules) -> Vec<StatementSpan> {
    split_psql_terminated(script, rules)
        .into_iter()
        .map(|(s, _)| s)
        .collect()
}

/// [`split_psql`], with the offset of the `;` that ends each statement (`None` for the last
/// one when the text ends without it).
fn split_psql_terminated(script: &str, rules: LexicalRules) -> Vec<(StatementSpan, Option<usize>)> {
    let mut spans = Vec::new();
    let mut current: Option<StatementSpan> = None;
    let mut paren = 0u32;
    let mut begin = 0u32;
    // First words of the current statement, lower-cased (psql keeps four).
    let mut words: Vec<String> = Vec::new();
    for t in code_tokens(script, rules) {
        if t.kind == CodeTokenKind::Semicolon && paren == 0 && begin == 0 {
            if let Some(s) = current.take() {
                spans.push((s, Some(t.start)));
            }
            words.clear();
            continue;
        }
        match current.as_mut() {
            Some(s) => s.end = t.end,
            None => {
                current = Some(StatementSpan {
                    start: t.start,
                    end: t.end,
                });
            }
        }
        match t.kind {
            CodeTokenKind::OpenParen => paren += 1,
            CodeTokenKind::CloseParen => paren = paren.saturating_sub(1),
            CodeTokenKind::Word => track_routine_blocks(script, &t, &mut words, paren, &mut begin),
            _ => {}
        }
    }
    if let Some(s) = current {
        spans.push((s, None));
    }
    spans
}

fn track_routine_blocks(
    script: &str,
    t: &CodeToken,
    words: &mut Vec<String>,
    paren: u32,
    begin: &mut u32,
) {
    if words.len() < 4 {
        words.push(t.text(script).to_ascii_lowercase());
    }
    let w = |i: usize| words.get(i).map(String::as_str);
    let routine = |x: Option<&str>| matches!(x, Some("function" | "procedure"));
    let in_routine = w(0) == Some("create")
        && (routine(w(1)) || (w(1) == Some("or") && w(2) == Some("replace") && routine(w(3))));
    if !in_routine || paren > 0 {
        return;
    }
    // `CASE` also ends with `END`: counted only inside a block.
    if t.is_word(script, "begin") || (t.is_word(script, "case") && *begin >= 1) {
        *begin += 1;
    } else if t.is_word(script, "end") && *begin > 0 {
        *begin -= 1;
    }
}

/// Bytes of script split per step by [`PgDialect::split_script`] (doubled while no statement
/// ends inside the window).
const SCRIPT_WINDOW: usize = 64 * 1024;

/// Smallest split window.
const MIN_WINDOW: usize = 512;

/// The next window: twice what the last step consumed, between [`MIN_WINDOW`] and
/// [`SCRIPT_WINDOW`].
fn next_window(consumed: usize) -> usize {
    consumed.saturating_mul(2).clamp(MIN_WINDOW, SCRIPT_WINDOW)
}

/// `COPY … FROM STDIN` (top-level `FROM` followed by `STDIN`).
fn is_copy_from_stdin(text: &str, rules: LexicalRules) -> bool {
    let toks = code_tokens(text, rules);
    let mut depth = 0u32;
    let mut top_level = Vec::new();
    for t in &toks {
        match t.kind {
            CodeTokenKind::OpenParen => depth += 1,
            CodeTokenKind::CloseParen => depth = depth.saturating_sub(1),
            _ if depth == 0 => top_level.push(*t),
            _ => {}
        }
    }
    toks.first().is_some_and(|t| t.is_word(text, "copy"))
        && top_level
            .windows(2)
            .any(|p| p[0].is_word(text, "from") && p[1].is_word(text, "stdin"))
}

/// End of the line holding `from` (the offset of its `\n`, or the end of the text).
fn line_end(script: &str, from: usize) -> usize {
    script[from..].find('\n').map_or(script.len(), |p| from + p)
}

/// End of the inline data of a `COPY … FROM STDIN` whose terminating `;` (as the lexer found
/// it) is at `semi`: the data starts on the line after it and ends with a line holding only
/// `\.` (psql); without one, it runs to the end of the script.
fn copy_data_end(script: &str, semi: usize) -> usize {
    let mut line_start = line_end(script, semi);
    while line_start < script.len() {
        line_start += 1; // past the `\n`
        let end = line_end(script, line_start);
        if script[line_start..end].trim_end_matches('\r') == "\\." {
            return end;
        }
        line_start = end;
    }
    script.len()
}

/// `start..end` without trailing whitespace.
fn trimmed(script: &str, start: usize, end: usize) -> StatementSpan {
    let len = script[start..end].trim_end().len();
    StatementSpan {
        start,
        end: start + len,
    }
}

/// The ANSI defaults under given lexical rules (literal formatting honours backslash escapes).
struct AnsiWithRules(LexicalRules);

impl Dialect for AnsiWithRules {
    fn lexical_rules(&self) -> LexicalRules {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninesql_driver_api::{StatementClass, conformance, ensure_single_statement};

    #[test]
    fn quoting_roundtrip() {
        conformance::assert_dialect_quoting_roundtrip(
            &PgDialect::new(),
            conformance::ALIAS_TEMPLATE,
            conformance::IDENTIFIER_SAMPLES,
        );
    }

    #[test]
    fn placeholders_and_explain() {
        let d = PgDialect::new();
        assert_eq!(d.placeholder(3), "$3");
        assert_eq!(
            d.explain("SELECT 1;", ExplainFormat::Json).as_deref(),
            Some("EXPLAIN (FORMAT JSON) SELECT 1")
        );
        assert_eq!(
            d.explain("SELECT 1", ExplainFormat::Analyze).as_deref(),
            Some("EXPLAIN ANALYZE SELECT 1")
        );
    }

    #[test]
    fn literals() {
        let d = PgDialect::new();
        assert_eq!(
            d.format_literal(&Param::Bytes(vec![0xde, 0xad])),
            "decode('dead', 'hex')"
        );
        assert!(
            d.format_literal(&Param::Timestamp {
                micros: 0,
                utc: true
            })
            .starts_with("TIMESTAMPTZ '1970-01-01 00:00:00")
        );
        assert_eq!(d.format_literal(&Param::Text("a\\b".into())), "'a\\b'");
    }

    #[test]
    fn standard_conforming_strings_drive_the_lexer() {
        let flag = Arc::new(AtomicBool::new(true));
        let d = PgDialect::tracking(Arc::clone(&flag), Flavor::Postgres);
        // With scs on, `\` is an ordinary character: the quote closes the literal.
        let sql = r"SELECT 'a\'; DELETE FROM t; --'";
        assert_eq!(d.split_statements(sql).len(), 2);
        assert!(ensure_single_statement(&d, sql).is_err());
        flag.store(false, Ordering::SeqCst);
        // With scs off, `\'` escapes the quote: one statement holding a string.
        assert_eq!(d.split_statements(sql).len(), 1);
        assert_eq!(d.classify(sql), StatementClass::Read);
        assert_eq!(d.format_literal(&Param::Text("a\\b".into())), "'a\\\\b'");
        assert!(d.lexical_rules().backslash_escapes);
    }
}
