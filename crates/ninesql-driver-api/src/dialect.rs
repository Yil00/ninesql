//! SQL dialects (ADR-0004): identifier quoting, literals, statement splitting, paging, explain,
//! statement classification for the production guard (ADR-0007) and DML generation for edits.
//!
//! Every method has an ANSI default so a driver only overrides what its engine does differently.
//! The defaults are conservative: anything the classifier does not recognise is
//! [`StatementClass::Unknown`], which counts as destructive.
//!
//! # Security
//! The classifier is a guard rail, not the read-only mechanism (I3): engines enforce read-only
//! themselves. It must still never under-classify, so its lexer follows the engine's lexical
//! rules exactly ([`LexicalRules`]). Drivers track the session modes that change them and build
//! the rules from their current values:
//! - MySQL / MariaDB `sql_mode`: `NO_BACKSLASH_ESCAPES`, `ANSI_QUOTES` (`"…"` becomes an
//!   identifier);
//! - PostgreSQL `standard_conforming_strings`;
//! - the client encoding (PostgreSQL `client_encoding` via `ParameterStatus`, MySQL
//!   `character_set_client`): multi-byte encodings such as SJIS, Big5 or GBK can hide a quote
//!   behind a backslash, so drivers connect in UTF-8 and the classifier treats any non-UTF-8
//!   encoding change as [`StatementClass::ChangesAccessMode`].
//!
//! Drivers also send exactly one statement per protocol call (see [`ensure_single_statement`]).
//!
//! # Limits of the classifier (L4)
//! The classifier sees statement text, not what the server will run. A call to a function with
//! side effects is only recognised when the function is on a known list (`nextval`, `dblink_exec`,
//! replication-slot, backup and statistics-reset functions, large-object writers…): **any other
//! function, in particular user-defined functions and procedures called from a `SELECT`, is
//! classified [`StatementClass::Read`]** even if it writes. Consequences:
//! - read-only sessions stay safe: the engine-side mechanism (I3) rejects the write, and the
//!   driver verifies the access mode after every statement for liftable mechanisms;
//! - the production guard (ADR-0007 §3) does **not** ask for confirmation for such calls, and
//!   on an engine declared [`ReadOnlyMechanism::ClientSideOnly`](crate::ReadOnlyMechanism::ClientSideOnly)
//!   nothing stops them, which is why the UI recommends a read-only account there.
//!
//! PostgreSQL Unicode-escaped names (`U&"d\0061ta"`, `U&'…'`) are decoded before matching;
//! a malformed escape or a `UESCAPE` clause makes the statement [`StatementClass::Unknown`].
//!
//! # Statements that run another statement
//! An `EXPLAIN` / `DESCRIBE` / `DESC` that runs its statement (a parenthesised option list, or
//! `ANALYZE` / `ANALYSE` in any form: bare, quoted, backquoted, `U&`-escaped) is classified as
//! the statement it runs, never lower; a plain `EXPLAIN` is a read. A statement starting with
//! `ANALYZE` (MariaDB `ANALYZE <statement>` runs it) is [`StatementClass::Unknown`].
//! `CREATE OR REPLACE` overwrites an existing object and is [`StatementClass::Destructive`]
//! (P1-E review M1, L1).
//!
//! # Statements whose effect is deferred or conditional (P3-D)
//! - A `CREATE TRIGGER` / `CREATE RULE` is classified as the riskiest statement of its body
//!   (at least [`StatementClass::Ddl`]): a trigger that empties a table is destructive. A
//!   MySQL / MariaDB `CREATE EVENT` (its body may run at once) is [`StatementClass::Unknown`].
//! - `BEGIN` is transaction control only when every word after it is a transaction mode;
//!   MariaDB `BEGIN NOT ATOMIC …` and `BEGIN ATOMIC …` are [`StatementClass::Unknown`].
//! - MySQL / MariaDB conditional executable comments (`/*!80000 … */`, `/*M! … */`) may or may
//!   not run: every combination is classified and the riskiest wins (see
//!   [`classify_with_rules`]).
//!
//! # Scripts (P3-D, F-SQL-18)
//! [`Dialect::split_script`] splits a script for "execute script": statements with byte offsets,
//! lines and columns ([`ScriptItem`]), client directives (MySQL `DELIMITER`) and refused pieces
//! (PostgreSQL `COPY … FROM STDIN` data, psql and mysql client commands). Each statement item
//! is then executed (and classified, just before it runs) on its own;
//! [`Dialect::split_statements`] stays the single-statement check of
//! [`ensure_single_statement`]. Items are valid only for the lexical modes at split time: after
//! an item that may change them ([`item_may_change_lexical_modes`]), callers re-split the rest
//! with [`Dialect::split_script_from`] (contract on [`Dialect::split_script`]).

use crate::capabilities::ExplainFormat;
use crate::dml::{ColumnType, DmlError};
use crate::error::{DriverError, DriverResult};
use crate::statement::{Param, Statement};

/// Lexical rules used by the default splitter, classifier and literal formatter.
///
/// Build them from a preset ([`LexicalRules::ANSI`], [`POSTGRES`](Self::POSTGRES),
/// [`MYSQL`](Self::MYSQL), [`SQLITE`](Self::SQLITE)) and the `with_*` methods.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
#[allow(clippy::struct_excessive_bools)]
pub struct LexicalRules {
    /// Backslash escapes inside every string literal (MySQL / MariaDB unless the session's
    /// `sql_mode` has `NO_BACKSLASH_ESCAPES`; PostgreSQL with `standard_conforming_strings = off`).
    pub backslash_escapes: bool,
    /// Backslash escapes inside `E'…'` strings (PostgreSQL family).
    pub e_string_backslash: bool,
    /// Block comments nest (PostgreSQL).
    pub nested_block_comments: bool,
    /// `$tag$ … $tag$` dollar-quoted strings (PostgreSQL family).
    pub dollar_quotes: bool,
    /// `#` starts a line comment (MySQL family).
    pub hash_comments: bool,
    /// `--` starts a comment only when followed by whitespace, a control character or the end
    /// of input (MySQL family): `1--1` is an expression.
    pub dash_comment_needs_space: bool,
    /// `/*! … */` and `/*M! … */` (optionally with a version number) are executed as code
    /// (MySQL / MariaDB executable comments).
    pub executable_comments: bool,
    /// `` ` `` quotes identifiers (MySQL family, SQLite, ClickHouse).
    pub backtick_identifiers: bool,
    /// With [`backslash_escapes`](Self::backslash_escapes), a backslash also escapes inside
    /// `"…"` (MySQL / MariaDB, where `"…"` is a string unless `ANSI_QUOTES`). Off for
    /// PostgreSQL: with `standard_conforming_strings = off` a backslash escapes in strings
    /// only, never in `"…"` or `U&"…"` identifiers (security review M1).
    pub double_quote_backslash: bool,
    /// `$` is an identifier character, also at the start of a word (MySQL / MariaDB: `t$x`,
    /// `$x`; SQLite: `t$x`, its `$x` variables being rewritten by the driver first). Without it,
    /// `DELETE FROM t$WHERE` would show a `WHERE` the engine never sees. PostgreSQL continues
    /// identifiers with `$` through [`dollar_quotes`](Self::dollar_quotes) instead.
    pub dollar_in_identifiers: bool,
    /// `@name` is one user-variable token, never a keyword (MySQL / MariaDB: `UPDATE t SET
    /// a = @WHERE` has no `WHERE` clause). `@@name` (system variables) and `@'…'` are unchanged.
    pub at_variables: bool,
    /// A carriage return ends a `--` comment, as a line feed does (PostgreSQL: its lexer's
    /// `non_newline` is `[^\n\r]`). MySQL, MariaDB and SQLite end it at a line feed only.
    pub line_comment_ends_at_cr: bool,
}

impl LexicalRules {
    /// ANSI SQL: `'…'` strings with `''`, `"…"` identifiers, `--` and `/* */` comments.
    pub const ANSI: Self = Self {
        backslash_escapes: false,
        e_string_backslash: false,
        nested_block_comments: false,
        dollar_quotes: false,
        hash_comments: false,
        dash_comment_needs_space: false,
        executable_comments: false,
        backtick_identifiers: false,
        double_quote_backslash: false,
        dollar_in_identifiers: false,
        at_variables: false,
        line_comment_ends_at_cr: false,
    };

    /// PostgreSQL family with `standard_conforming_strings = on` (the default since 9.1).
    pub const POSTGRES: Self = Self::ANSI
        .with_e_string_backslash(true)
        .with_nested_block_comments(true)
        .with_dollar_quotes(true)
        .with_line_comment_ends_at_cr(true);

    /// MySQL / MariaDB in the default `sql_mode` (backslash escapes on).
    pub const MYSQL: Self = Self::ANSI
        .with_backslash_escapes(true)
        .with_hash_comments(true)
        .with_dash_comment_needs_space(true)
        .with_executable_comments(true)
        .with_backtick_identifiers(true)
        .with_double_quote_backslash(true)
        .with_dollar_in_identifiers(true)
        .with_at_variables(true);

    /// SQLite.
    pub const SQLITE: Self = Self::ANSI
        .with_backtick_identifiers(true)
        .with_dollar_in_identifiers(true);

    /// Sets [`backslash_escapes`](Self::backslash_escapes).
    #[must_use]
    pub const fn with_backslash_escapes(mut self, on: bool) -> Self {
        self.backslash_escapes = on;
        self
    }

    /// Sets [`e_string_backslash`](Self::e_string_backslash).
    #[must_use]
    pub const fn with_e_string_backslash(mut self, on: bool) -> Self {
        self.e_string_backslash = on;
        self
    }

    /// Sets [`nested_block_comments`](Self::nested_block_comments).
    #[must_use]
    pub const fn with_nested_block_comments(mut self, on: bool) -> Self {
        self.nested_block_comments = on;
        self
    }

    /// Sets [`dollar_quotes`](Self::dollar_quotes).
    #[must_use]
    pub const fn with_dollar_quotes(mut self, on: bool) -> Self {
        self.dollar_quotes = on;
        self
    }

    /// Sets [`hash_comments`](Self::hash_comments).
    #[must_use]
    pub const fn with_hash_comments(mut self, on: bool) -> Self {
        self.hash_comments = on;
        self
    }

    /// Sets [`dash_comment_needs_space`](Self::dash_comment_needs_space).
    #[must_use]
    pub const fn with_dash_comment_needs_space(mut self, on: bool) -> Self {
        self.dash_comment_needs_space = on;
        self
    }

    /// Sets [`executable_comments`](Self::executable_comments).
    #[must_use]
    pub const fn with_executable_comments(mut self, on: bool) -> Self {
        self.executable_comments = on;
        self
    }

    /// Sets [`backtick_identifiers`](Self::backtick_identifiers).
    #[must_use]
    pub const fn with_backtick_identifiers(mut self, on: bool) -> Self {
        self.backtick_identifiers = on;
        self
    }

    /// Sets [`double_quote_backslash`](Self::double_quote_backslash).
    #[must_use]
    pub const fn with_double_quote_backslash(mut self, on: bool) -> Self {
        self.double_quote_backslash = on;
        self
    }

    /// Sets [`dollar_in_identifiers`](Self::dollar_in_identifiers).
    #[must_use]
    pub const fn with_dollar_in_identifiers(mut self, on: bool) -> Self {
        self.dollar_in_identifiers = on;
        self
    }

    /// Sets [`at_variables`](Self::at_variables).
    #[must_use]
    pub const fn with_at_variables(mut self, on: bool) -> Self {
        self.at_variables = on;
        self
    }

    /// Sets [`line_comment_ends_at_cr`](Self::line_comment_ends_at_cr).
    #[must_use]
    pub const fn with_line_comment_ends_at_cr(mut self, on: bool) -> Self {
        self.line_comment_ends_at_cr = on;
        self
    }
}

/// Byte range of one statement inside a script, excluding surrounding whitespace, leading
/// comments and the terminating `;`. Always on `char` boundaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatementSpan {
    /// Start offset (inclusive).
    pub start: usize,
    /// End offset (exclusive).
    pub end: usize,
}

impl StatementSpan {
    /// The statement text inside `script`.
    #[must_use]
    pub fn text<'a>(&self, script: &'a str) -> &'a str {
        script.get(self.start..self.end).unwrap_or("")
    }
}

/// A position in a script, for error positions (F-SQL-12) and editor navigation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub struct TextPosition {
    /// Byte offset in the script.
    pub offset: usize,
    /// Line, 1-based. Lines end at `\n`; a `\r` before it belongs to the line.
    pub line: usize,
    /// Column, 1-based, in Unicode scalar values (`char`s) from the start of the line.
    pub column: usize,
}

impl TextPosition {
    /// The start of a text.
    pub const START: Self = Self {
        offset: 0,
        line: 1,
        column: 1,
    };
}

/// Splitter state carried between calls of [`Dialect::split_script_ahead`]: the MySQL
/// `DELIMITER` in force (`None`: `;`). Updated item by item with
/// [`Dialect::script_state_after`].
///
/// A delimiter that a `DELIMITER` directive would refuse (empty, with whitespace, a
/// backslash, a quote or a comment opener) can never be in force: mysqlwire then returns one
/// [`ScriptRefusal::InvalidDelimiter`] item for the rest of the script (fails closed). Dialects
/// without a `DELIMITER` directive (pgwire, sqlite, the defaults) never produce a delimiter and
/// ignore one: they split on `;` as always.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScriptState {
    /// The client statement delimiter in force, when not `;`.
    pub delimiter: Option<String>,
}

impl ScriptState {
    /// A state with `delimiter` in force (`None`: `;`).
    #[must_use]
    pub fn with_delimiter(delimiter: Option<String>) -> Self {
        Self { delimiter }
    }
}

/// Line starts of a script, to turn byte offsets into [`TextPosition`]s in `O(log lines)`.
#[derive(Clone, Debug)]
pub struct LineIndex<'a> {
    text: &'a str,
    /// Byte offset of the start of every line.
    starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    /// Indexes `text`.
    #[must_use]
    pub fn new(text: &'a str) -> Self {
        let mut starts = vec![0];
        starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self { text, starts }
    }

    /// The position of byte `offset`, clamped to the text and moved back to a `char` boundary.
    #[must_use]
    pub fn position(&self, offset: usize) -> TextPosition {
        let mut offset = offset.min(self.text.len());
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        let line = self.starts.partition_point(|s| *s <= offset);
        let start = self.starts[line - 1];
        let column = self.text[start..offset].chars().count() + 1;
        TextPosition {
            offset,
            line,
            column,
        }
    }
}

/// The position of byte `offset` in `text` (see [`LineIndex`] for many lookups).
#[must_use]
pub fn position_at(text: &str, offset: usize) -> TextPosition {
    LineIndex::new(text).position(offset)
}

/// Why a piece of a script cannot run as a statement ([`ScriptItemKind::Refused`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ScriptRefusal {
    /// PostgreSQL `COPY … FROM STDIN` with its inline data block (up to a `\.` line): the
    /// data is not SQL and is never split or sent; NineSQL does not stream `COPY` input.
    CopyFromStdin,
    /// A psql meta-command (`\connect`, `\set`, `\i`…): a client command, not SQL.
    PsqlMetaCommand,
    /// A mysql client command other than `DELIMITER` (`\d`, `\g`, `source`…).
    MysqlClientCommand,
    /// A `DELIMITER` directive without a delimiter, or with one NineSQL refuses (a
    /// backslash, a quote, or a comment opener in it). The previous delimiter stays.
    InvalidDelimiter,
}

impl ScriptRefusal {
    /// A user-presentable explanation.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::CopyFromStdin => "COPY … FROM STDIN with inline data is not supported",
            Self::PsqlMetaCommand => "psql meta-commands are not supported",
            Self::MysqlClientCommand => {
                "mysql client commands other than DELIMITER are not supported"
            }
            Self::InvalidDelimiter => {
                "invalid DELIMITER: it must be non-empty and contain no backslash, quote or comment opener"
            }
        }
    }
}

/// What a [`ScriptItem`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ScriptItemKind {
    /// One statement, to be sent as [`ScriptItem::span`]'s text: it passes
    /// [`ensure_single_statement`] and is classified with [`Dialect::classify`] like any other.
    Statement,
    /// A client directive the splitter applied and that is never sent (MySQL `DELIMITER //`).
    ClientDirective,
    /// A piece the splitter recognised but that cannot run. A caller running the script must
    /// stop before it (or refuse the whole script); it is never sent.
    Refused(ScriptRefusal),
}

/// One piece of a script, from [`Dialect::split_script`]. Built with [`script_items`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScriptItem {
    /// The piece's bytes (no surrounding blanks or leading comments, no terminator).
    pub span: StatementSpan,
    /// What it is.
    pub kind: ScriptItemKind,
    /// Position of `span.start`.
    pub start: TextPosition,
    /// Position of `span.end` (exclusive: just past the last character).
    pub end: TextPosition,
}

impl ScriptItem {
    /// The piece's text inside `script`.
    #[must_use]
    pub fn text<'a>(&self, script: &'a str) -> &'a str {
        self.span.text(script)
    }
}

/// Positions computed in one forward pass: offsets asked in increasing order cost the bytes
/// between them, so a script's positions cost one pass whatever its line lengths (security
/// review M3). An offset behind the cursor restarts from the start of the text.
struct PositionCursor<'a> {
    text: &'a str,
    at: TextPosition,
}

impl<'a> PositionCursor<'a> {
    fn new(text: &'a str) -> Self {
        Self::at(text, TextPosition::START)
    }

    /// A cursor resuming at `at`, a position of `text` computed earlier.
    fn at(text: &'a str, at: TextPosition) -> Self {
        let at = if at.offset <= text.len() && text.is_char_boundary(at.offset) {
            at
        } else {
            TextPosition::START
        };
        Self { text, at }
    }

    fn position(&mut self, offset: usize) -> TextPosition {
        let mut offset = offset.min(self.text.len());
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        if offset < self.at.offset {
            self.at = TextPosition {
                offset: 0,
                line: 1,
                column: 1,
            };
        }
        for c in self.text[self.at.offset..offset].chars() {
            if c == '\n' {
                self.at.line += 1;
                self.at.column = 1;
            } else {
                self.at.column += 1;
            }
        }
        self.at.offset = offset;
        self.at
    }
}

/// Builds [`ScriptItem`]s with their positions, in one pass over the script when the pieces
/// come in order (as every splitter returns them).
#[must_use]
pub fn script_items(
    script: &str,
    pieces: impl IntoIterator<Item = (StatementSpan, ScriptItemKind)>,
) -> Vec<ScriptItem> {
    let mut cursor = PositionCursor::new(script);
    pieces
        .into_iter()
        .map(|(span, kind)| {
            let start = cursor.position(span.start);
            let end = cursor.position(span.end);
            ScriptItem {
                span,
                kind,
                start,
                end,
            }
        })
        .collect()
}

/// [`script_items`] for pieces at or after `start`, a position of `script` computed earlier
/// (an item's `end`): positions cost the bytes from `start` on, not from the start of the
/// script.
#[must_use]
pub fn script_items_from(
    script: &str,
    start: TextPosition,
    pieces: impl IntoIterator<Item = (StatementSpan, ScriptItemKind)>,
) -> Vec<ScriptItem> {
    let mut cursor = PositionCursor::at(script, start);
    pieces
        .into_iter()
        .map(|(span, kind)| {
            let start = cursor.position(span.start);
            let end = cursor.position(span.end);
            ScriptItem {
                span,
                kind,
                start,
                end,
            }
        })
        .collect()
}

/// Smallest window of [`Dialect::split_script_ahead`]'s default.
const AHEAD_WINDOW: usize = 512;

/// Items of `script[from..]` moved to offsets and positions in the whole `script`.
#[must_use]
pub fn shift_items(script: &str, from: usize, items: Vec<ScriptItem>) -> Vec<ScriptItem> {
    script_items(
        script,
        items.into_iter().map(|i| {
            (
                StatementSpan {
                    start: i.span.start + from,
                    end: i.span.end + from,
                },
                i.kind,
            )
        }),
    )
}

/// `true` when running `item` may change the lexical modes the script was split with (MySQL
/// `sql_mode`, PostgreSQL `standard_conforming_strings`, the client encoding): a statement
/// item that `dialect` classifies [`StatementClass::Session`],
/// [`StatementClass::ChangesAccessMode`] or [`StatementClass::Unknown`] (a `SET`, a
/// `set_config(…)`, a procedure call…). After such an item has run, a caller running the
/// script re-splits the rest with the session's current dialect
/// ([`Dialect::split_script_from`]); see the contract on [`Dialect::split_script`].
#[must_use]
pub fn item_may_change_lexical_modes(
    dialect: &(impl Dialect + ?Sized),
    item: &ScriptItem,
    script: &str,
) -> bool {
    item.kind == ScriptItemKind::Statement
        && matches!(
            dialect.classify(item.text(script)),
            StatementClass::Session | StatementClass::ChangesAccessMode | StatementClass::Unknown
        )
}

/// Classification of a statement, ordered by increasing risk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StatementClass {
    /// Reads only (`SELECT`, `SHOW`, `VALUES`…).
    Read,
    /// Session settings that do not affect access mode (`SET search_path`, `USE`).
    Session,
    /// Transaction control (`BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`…).
    Transaction,
    /// Data modification with a filter (`INSERT`, `UPDATE … WHERE`, `DELETE … WHERE`, `MERGE`).
    Write,
    /// Non-destructive schema change (`CREATE`, but not `CREATE OR REPLACE`).
    Ddl,
    /// A session statement that may lift read-only mode (`SET … READ WRITE`,
    /// `PRAGMA query_only = OFF`). Refused in read-only sessions (I3).
    ChangesAccessMode,
    /// Destructive (`DROP`, `TRUNCATE`, `ALTER`, `GRANT`, `REVOKE`, `CREATE OR REPLACE`,
    /// `UPDATE` / `DELETE` without `WHERE`).
    Destructive,
    /// Not recognised. Treated as destructive (ADR-0007 §3).
    Unknown,
}

impl StatementClass {
    /// May modify data, schema or privileges. Refused client-side in read-only sessions in
    /// addition to the engine-side mechanism.
    #[must_use]
    pub fn is_write(self) -> bool {
        matches!(
            self,
            Self::Write | Self::Ddl | Self::ChangesAccessMode | Self::Destructive | Self::Unknown
        )
    }

    /// Needs explicit confirmation on connections whose type asks for it (ADR-0007 §3).
    #[must_use]
    pub fn needs_confirmation(self) -> bool {
        matches!(
            self,
            Self::ChangesAccessMode | Self::Destructive | Self::Unknown
        )
    }
}

/// A row change made in the grid, rendered to DML by [`Dialect::render_edit`].
#[derive(Clone, Debug, PartialEq)]
pub struct RowEdit {
    /// Qualified table name, unquoted parts (`["public", "users"]`).
    pub table: Vec<String>,
    /// The change.
    pub change: RowChange,
}

/// The change of a [`RowEdit`]. Values travel as bind parameters, never inlined.
#[derive(Clone, Debug, PartialEq)]
pub enum RowChange {
    /// Insert a row.
    Insert {
        /// Column / value pairs.
        values: Vec<(String, Param)>,
    },
    /// Update the row identified by `key`.
    Update {
        /// Unique key column / value pairs. Must not be empty.
        key: Vec<(String, Param)>,
        /// Column / new value pairs. Must not be empty.
        values: Vec<(String, Param)>,
    },
    /// Delete the row identified by `key`.
    Delete {
        /// Unique key column / value pairs. Must not be empty.
        key: Vec<(String, Param)>,
    },
}

/// Engine dialect. Obtained from [`Driver::dialect`](crate::Driver::dialect) or
/// [`Session::dialect`](crate::Session::dialect).
pub trait Dialect: Send + Sync {
    /// Lexical rules for the default implementations.
    fn lexical_rules(&self) -> LexicalRules {
        LexicalRules::default()
    }

    /// Quotes an identifier (ANSI: `"name"` with `"` doubled).
    fn quote_identifier(&self, ident: &str) -> String {
        let mut out = String::with_capacity(ident.len() + 2);
        out.push('"');
        for c in ident.chars() {
            if c == '"' {
                out.push('"');
            }
            out.push(c);
        }
        out.push('"');
        out
    }

    /// Quoted, dot-separated qualified name.
    fn qualified_name(&self, parts: &[String]) -> String {
        parts
            .iter()
            .map(|p| self.quote_identifier(p))
            .collect::<Vec<_>>()
            .join(".")
    }

    /// Formats a value as a literal for display / generated scripts. Execution paths use bind
    /// parameters instead.
    fn format_literal(&self, value: &Param) -> String {
        ansi_literal(value, self.lexical_rules().backslash_escapes)
    }

    /// Bind parameter placeholder for the 1-based `index` (ANSI / JDBC: `?`).
    fn placeholder(&self, index: usize) -> String {
        let _ = index;
        "?".to_owned()
    }

    /// Splits a script into statements on top-level `;`, honouring quotes and comments
    /// according to [`lexical_rules`](Self::lexical_rules).
    ///
    /// This is the split [`ensure_single_statement`] uses: what a driver sends in one protocol
    /// call. Engine compound statements whose body holds `;` (MySQL stored programs, SQLite
    /// triggers, PostgreSQL `BEGIN ATOMIC` bodies) are one span in the drivers' overrides.
    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split_with_rules(script, self.lexical_rules())
    }

    /// Splits a script for "execute script" (F-SQL-18): statements, client directives
    /// (MySQL `DELIMITER`) and refused pieces (PostgreSQL `COPY … FROM STDIN` data, psql
    /// meta-commands), in order, with byte offsets, lines and columns.
    ///
    /// Every [`ScriptItemKind::Statement`] item's text is one statement for
    /// [`split_statements`](Self::split_statements) (it passes [`ensure_single_statement`]) and
    /// is run and classified as such, one item per call. Default: the
    /// [`split_statements`](Self::split_statements) spans, all statements.
    ///
    /// # Contract for callers running a script (security review H2)
    /// The items are valid only for the lexical modes in force when the script was split
    /// ([`lexical_rules`](Self::lexical_rules): MySQL `NO_BACKSLASH_ESCAPES` and `ANSI_QUOTES`,
    /// PostgreSQL `standard_conforming_strings`). A statement of the script may change them
    /// (`SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES'; UPDATE t SET a = 'x\' -- ' WHERE …`
    /// is a different `UPDATE` after the `SET`). So a caller:
    /// 1. classifies each item with the session's dialect **just before running it**, and asks
    ///    for confirmation from that class, never from a class computed earlier;
    /// 2. after **every** item, compares the session dialect's `lexical_rules()` with the ones
    ///    the pending items were split with (they capture every mode the splitters use): a
    ///    `SET` changes them, but so can a `ROLLBACK`, a `COMMIT` or a `ROLLBACK TO SAVEPOINT`
    ///    that undoes a `SET`, or a function calling `set_config`
    ///    ([`item_may_change_lexical_modes`] only names the obvious cases). When they differ,
    ///    it drops the pending items and splits on from the end of that item with the new
    ///    dialect.
    ///
    /// To keep that linear, run the script lazily with
    /// [`split_script_ahead`](Self::split_script_ahead) (a few items at a time, from the end of
    /// the last item, with the [`ScriptState`] kept up to date by
    /// [`script_state_after`](Self::script_state_after)) rather than re-splitting the whole
    /// rest. `conformance::script_plan` is a reference implementation.
    fn split_script(&self, script: &str) -> Vec<ScriptItem> {
        script_items(
            script,
            self.split_statements(script)
                .into_iter()
                .map(|s| (s, ScriptItemKind::Statement)),
        )
    }

    /// The next items of `script` from `from` (the `end` of the last item handled, or
    /// [`TextPosition::START`]) with `state` in force: at least one item unless only blanks
    /// and comments are left, every one exactly as [`split_script`](Self::split_script) of the
    /// rest would give it, at a cost of a small window of text (it grows only for a longer
    /// statement). Default: windows of [`split_statements`](Self::split_statements), the last
    /// statement of a window (possibly cut) left for the next call.
    fn split_script_ahead(
        &self,
        script: &str,
        from: TextPosition,
        state: &ScriptState,
    ) -> Vec<ScriptItem> {
        let _ = state;
        if from.offset > script.len() || !script.is_char_boundary(from.offset) {
            return self.split_script_from(script, from.offset);
        }
        let base = from.offset;
        let mut window = AHEAD_WINDOW;
        loop {
            let mut end = base.saturating_add(window).min(script.len());
            while !script.is_char_boundary(end) {
                end += 1;
            }
            let complete = end == script.len();
            let mut spans = self.split_statements(&script[base..end]);
            if !complete {
                spans.pop();
                if spans.is_empty() {
                    window = window.saturating_mul(2);
                    continue;
                }
            }
            return script_items_from(
                script,
                from,
                spans.into_iter().map(|s| {
                    (
                        StatementSpan {
                            start: s.start + base,
                            end: s.end + base,
                        },
                        ScriptItemKind::Statement,
                    )
                }),
            );
        }
    }

    /// The [`ScriptState`] after `item` (an item of `script` handled with `state` in force):
    /// a MySQL `DELIMITER` directive changes the delimiter. Default: unchanged.
    fn script_state_after(
        &self,
        script: &str,
        item: &ScriptItem,
        state: &ScriptState,
    ) -> ScriptState {
        let _ = (script, item);
        state.clone()
    }

    /// Splits the rest of `script` from byte `from` (the end of an item of a previous split,
    /// see the contract on [`split_script`](Self::split_script)). Items carry offsets and
    /// positions in the whole `script`. Splitter state from before `from` is rebuilt (MySQL:
    /// the `DELIMITER` in force). Default: [`split_script`](Self::split_script) of the rest,
    /// shifted. It splits the whole rest (and mysqlwire also re-reads the text before `from`):
    /// prefer [`split_script_ahead`](Self::split_script_ahead) in a loop.
    fn split_script_from(&self, script: &str, from: usize) -> Vec<ScriptItem> {
        let mut from = from.min(script.len());
        while !script.is_char_boundary(from) {
            from -= 1;
        }
        shift_items(script, from, self.split_script(&script[from..]))
    }

    /// Wraps a query to fetch one page (ANSI: `SELECT * FROM (<q>) AS ninesql_page LIMIT n
    /// OFFSET m`). `None` when the engine has no paging syntax.
    fn page_query(&self, query: &str, limit: u64, offset: u64) -> Option<String> {
        let q = query.trim().trim_end_matches(';');
        Some(format!(
            "SELECT * FROM ({q}) AS ninesql_page LIMIT {limit} OFFSET {offset}"
        ))
    }

    /// Explain statement for `query` in `format`, for formats declared in
    /// [`Capabilities::explain`](crate::Capabilities::explain). Default: none.
    fn explain(&self, query: &str, format: ExplainFormat) -> Option<String> {
        let _ = (query, format);
        None
    }

    /// Classifies a statement (or a script: the riskiest statement wins). The default is a
    /// conservative keyword classifier; drivers with a real parser override it.
    ///
    /// Calls to functions with side effects that are not on the known list (user functions,
    /// procedures called from a `SELECT`) classify as [`StatementClass::Read`]: see the
    /// [module documentation](self#limits-of-the-classifier-l4).
    fn classify(&self, text: &str) -> StatementClass {
        classify_with_rules(text, self.lexical_rules())
    }

    /// The typed literal of `value` for a DML preview or an SQL `INSERT` export
    /// ([`crate::dml`]), for a column of type `ty`. It must read back, in the session's lexical
    /// modes, as exactly the value the bound parameter gives, and never show a character
    /// [`needs_escape`](crate::dml::needs_escape) escapes raw. Default:
    /// [`ansi_dml_literal`](crate::dml::ansi_dml_literal).
    ///
    /// # Errors
    /// A reason (never the value) when the engine has no faithful literal or parameter for it.
    fn dml_literal(&self, value: &Param, ty: &ColumnType) -> Result<String, &'static str> {
        let _ = ty;
        crate::dml::ansi_dml_literal(self.lexical_rules(), value)
    }

    /// Refuses a table or column name the engine would reject or silently change (truncation)
    /// in generated DML. Default: [`check_identifier_basic`](crate::dml::check_identifier_basic)
    /// (not empty, no NUL).
    ///
    /// # Errors
    /// [`DmlError::InvalidIdentifier`].
    fn dml_check_identifier(&self, name: &str) -> Result<(), DmlError> {
        crate::dml::check_identifier_basic(name)
    }

    /// `true` when `col = value` matches exactly the rows whose column holds `value` for this
    /// column type, so it may be part of a key or concurrency `WHERE`. Default: `true`.
    fn dml_comparable(&self, ty: &ColumnType) -> bool {
        let _ = ty;
        true
    }

    /// `INSERT` of a row with every column at its default (`table` already quoted). Default:
    /// `INSERT INTO <table> DEFAULT VALUES`.
    fn dml_insert_default_values(&self, table: &str) -> String {
        format!("INSERT INTO {table} DEFAULT VALUES")
    }

    /// Renders a grid edit as a parameterized DML statement. Prefer
    /// [`dml::generate`](crate::dml::generate), which also checks editing capabilities, typed
    /// keys, `NULL` keys and the result, and renders a preview.
    ///
    /// # Errors
    /// [`DriverError::InvalidParams`] when an update / delete has an empty key or an update has
    /// no values (it would touch every row).
    fn render_edit(&self, edit: &RowEdit) -> DriverResult<Statement> {
        render_edit_ansi(self, edit)
    }
}

/// The ANSI dialect, with every default.
#[derive(Clone, Copy, Debug, Default)]
pub struct AnsiDialect;

impl Dialect for AnsiDialect {}

// ---------------------------------------------------------------------------------------------
// Lexer

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokKind {
    Word,
    /// String literal (`'…'`, `E'…'`, `$tag$…$tag$`).
    Str,
    /// Quoted identifier (`"…"`, `` `…` ``).
    Ident,
    Semicolon,
    Other,
    /// The opener (`/*!`, `/*M!`, version digits) or the closing `*/` of an executable
    /// comment: part of a statement's span (so the statement sent keeps its comment and its
    /// version condition), invisible to the classifier.
    ExecMarker,
}

#[derive(Clone, Copy, Debug)]
struct Tok {
    kind: TokKind,
    start: usize,
    end: usize,
}

struct Lexed {
    toks: Vec<Tok>,
    /// A `/*!` or `/*M!` comment was seen while executable comments were not enabled.
    executable_comment_as_comment: bool,
    /// Conditional executable comments (`/*!NNNNN …*/` with a version, `/*M!…*/`): byte range
    /// from the opener to just past the closing `*/` (or the end of the text).
    conditional: Vec<(usize, usize)>,
    /// A conditional executable comment is unterminated, or its body holds `/*` or `*/` in a
    /// token (a string, an identifier, a nested comment): an engine that skips the comment
    /// ends it elsewhere than an engine that runs it.
    conditional_ambiguous: bool,
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Index just past the end of a quoted run starting at `start` (the opening quote).
fn skip_quoted(bytes: &[u8], start: usize, quote: u8, backslash: bool) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        let b = bytes[i];
        if backslash && b == b'\\' {
            i += 2;
            continue;
        }
        if b == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

/// If a dollar-quote tag starts at `start`, returns the tag length (including both `$`).
fn dollar_tag(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    if let Some(&b) = bytes.get(i)
        && b.is_ascii_digit()
    {
        return None;
    }
    while let Some(&b) = bytes.get(i) {
        if b == b'$' {
            return Some(i + 1 - start);
        }
        // PostgreSQL's `dolq_cont`: `[A-Za-z\200-\377_0-9]`.
        if !(b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80) {
            return None;
        }
        i += 1;
    }
    None
}

fn find_sub(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// `true` when the executable-comment opener at `i` is conditional: it has a version number
/// (run only from that server version on) or is MariaDB's `/*M!` (a plain comment for MySQL).
fn executable_opener_is_conditional(bytes: &[u8], i: usize) -> bool {
    let rest = &bytes[i..];
    rest.starts_with(b"/*M!") || rest.get(3).is_some_and(u8::is_ascii_digit)
}

/// Length of an executable-comment opener (`/*!`, `/*M!`, plus version digits) at `i`.
fn executable_opener(bytes: &[u8], i: usize) -> Option<usize> {
    let rest = &bytes[i..];
    let base = if rest.starts_with(b"/*!") {
        3
    } else if rest.starts_with(b"/*M!") {
        4
    } else {
        return None;
    };
    let digits = rest[base..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    Some(base + digits)
}

fn lex(text: &str, rules: LexicalRules) -> Lexed {
    let bytes = text.as_bytes();
    let mut toks = Vec::new();
    let mut executable_comment_as_comment = false;
    let mut in_executable = false;
    let mut conditional = Vec::new();
    let mut conditional_ambiguous = false;
    // Start of the open conditional executable comment, if any.
    let mut open_conditional: Option<usize> = None;
    let mut i = 0;
    let push = |toks: &mut Vec<Tok>, kind, start, end| toks.push(Tok { kind, start, end });
    while i < bytes.len() {
        let b = bytes[i];
        let next = bytes.get(i + 1).copied();
        let dash_comment = b == b'-'
            && next == Some(b'-')
            && (!rules.dash_comment_needs_space
                || bytes.get(i + 2).is_none_or(|c| *c <= b' ' || *c == 0x7f));
        if b.is_ascii_whitespace() {
            i += 1;
        } else if in_executable && b == b'*' && next == Some(b'/') {
            // End of a `/*! … */` executable comment: its content was lexed as code.
            in_executable = false;
            push(&mut toks, TokKind::ExecMarker, i, i + 2);
            i += 2;
            if let Some(start) = open_conditional.take() {
                conditional.push((start, i));
                // The body as an engine that skips the comment reads it: up to the first `*/`.
                let body = &bytes[start + 3..i - 2];
                if find_sub(body, b"*/", 0).is_some() || find_sub(body, b"/*", 0).is_some() {
                    conditional_ambiguous = true;
                }
            }
        } else if dash_comment || rules.hash_comments && b == b'#' {
            while i < bytes.len()
                && bytes[i] != b'\n'
                && !(rules.line_comment_ends_at_cr && bytes[i] == b'\r')
            {
                i += 1;
            }
        } else if b == b'/' && next == Some(b'*') {
            if let Some(len) = executable_opener(bytes, i) {
                if rules.executable_comments && !in_executable {
                    in_executable = true;
                    if executable_opener_is_conditional(bytes, i) {
                        open_conditional = Some(i);
                    }
                    push(&mut toks, TokKind::ExecMarker, i, i + len);
                    i += len;
                    continue;
                }
                executable_comment_as_comment = true;
            }
            let mut depth = 1usize;
            i += 2;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                } else if rules.nested_block_comments
                    && bytes[i] == b'/'
                    && bytes.get(i + 1) == Some(&b'*')
                {
                    depth += 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            i = i.min(bytes.len());
        } else if rules.e_string_backslash && matches!(b, b'e' | b'E') && next == Some(b'\'') {
            let end = skip_quoted(bytes, i + 1, b'\'', true).min(bytes.len());
            push(&mut toks, TokKind::Str, i, end);
            i = end;
        } else if matches!(b, b'u' | b'U')
            && next == Some(b'&')
            && matches!(bytes.get(i + 2), Some(b'\'' | b'"'))
        {
            // `U&"…"` / `U&'…'` (SQL standard, PostgreSQL): one token, decoded by `locate`.
            // The quoted run is skipped exactly as without the prefix, so splitting is unchanged.
            let q = bytes[i + 2];
            let backslash = rules.backslash_escapes && (q == b'\'' || rules.double_quote_backslash);
            let end = skip_quoted(bytes, i + 2, q, backslash).min(bytes.len());
            let kind = if q == b'\'' {
                TokKind::Str
            } else {
                TokKind::Ident
            };
            push(&mut toks, kind, i, end);
            i = end;
        } else if b == b'\'' || b == b'"' || rules.backtick_identifiers && b == b'`' {
            let backslash = rules.backslash_escapes
                && (b == b'\'' || b == b'"' && rules.double_quote_backslash);
            let end = skip_quoted(bytes, i, b, backslash).min(bytes.len());
            let kind = if b == b'\'' {
                TokKind::Str
            } else {
                TokKind::Ident
            };
            push(&mut toks, kind, i, end);
            i = end;
        } else if rules.dollar_quotes && b == b'$' {
            if let Some(len) = dollar_tag(bytes, i) {
                let tag = &bytes[i..i + len];
                let end = find_sub(bytes, tag, i + len).map_or(bytes.len(), |p| p + len);
                push(&mut toks, TokKind::Str, i, end);
                i = end;
            } else {
                push(&mut toks, TokKind::Other, i, i + 1);
                i += 1;
            }
        } else if b == b';' {
            if open_conditional.is_some() {
                // A statement boundary inside a conditional comment: what runs depends on
                // the client's splitting and the server's version.
                conditional_ambiguous = true;
            }
            push(&mut toks, TokKind::Semicolon, i, i + 1);
            i += 1;
        } else if rules.at_variables && b == b'@' && next.is_some_and(is_word_byte) {
            // MySQL user variable `@name` (letters, digits, `_`, `$`, `.`): one token that is
            // never a keyword.
            let start = i;
            i += 1;
            while i < bytes.len() && (is_word_byte(bytes[i]) || matches!(bytes[i], b'$' | b'.')) {
                i += 1;
            }
            push(&mut toks, TokKind::Other, start, i);
        } else if rules.at_variables && b == b'@' && next == Some(b'@') {
            // `@@name`: two `@`, then the system variable's name as a word.
            push(&mut toks, TokKind::Other, i, i + 1);
            push(&mut toks, TokKind::Other, i + 1, i + 2);
            i += 2;
        } else if is_word_byte(b) || rules.dollar_in_identifiers && b == b'$' {
            // With dollar quotes (PostgreSQL), `$` continues an identifier after its first
            // character (`a$q$` is one name); a dollar tag only starts at a token start.
            let start = i;
            while i < bytes.len()
                && (is_word_byte(bytes[i])
                    || (rules.dollar_quotes || rules.dollar_in_identifiers) && bytes[i] == b'$')
            {
                i += 1;
            }
            push(&mut toks, TokKind::Word, start, i);
        } else {
            push(&mut toks, TokKind::Other, i, i + 1);
            i += 1;
        }
    }
    if let Some(start) = open_conditional {
        // Unterminated: where it ends depends on the engine.
        conditional.push((start, bytes.len()));
        conditional_ambiguous = true;
    }
    Lexed {
        toks,
        executable_comment_as_comment,
        conditional,
        conditional_ambiguous,
    }
}

/// Splits `script` with the given lexical rules (the default of [`Dialect::split_statements`]).
///
/// A statement's span runs from its first token to its last, executable-comment openers and
/// closers included (`/*!40101 SET NAMES utf8mb4 */` keeps its version condition). A `;`
/// inside an executable comment ends a statement, as for the mysql client, so
/// `SELECT 1 /*! ; DROP TABLE t */` is two statements and refused by
/// [`ensure_single_statement`].
#[must_use]
pub fn split_with_rules(script: &str, rules: LexicalRules) -> Vec<StatementSpan> {
    let mut spans = Vec::new();
    let mut current: Option<StatementSpan> = None;
    for t in lex(script, rules).toks {
        if t.kind == TokKind::Semicolon {
            if let Some(s) = current.take() {
                spans.push(s);
            }
        } else {
            match current.as_mut() {
                Some(s) => s.end = t.end,
                None => {
                    current = Some(StatementSpan {
                        start: t.start,
                        end: t.end,
                    });
                }
            }
        }
    }
    if let Some(s) = current {
        spans.push(s);
    }
    spans
}

/// What a [`CodeToken`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CodeTokenKind {
    /// An unquoted word: keyword or identifier.
    Word,
    /// A string literal (`'…'`, `E'…'`, `$tag$…$tag$`, `U&'…'`).
    Str,
    /// A quoted identifier (`"…"`, `` `…` ``).
    QuotedIdent,
    /// `;`.
    Semicolon,
    /// `(`.
    OpenParen,
    /// `)`.
    CloseParen,
    /// Any other punctuation or operator byte, or a user variable (`@x` under
    /// [`LexicalRules::at_variables`]).
    Other,
}

/// A token of a text outside comments and blanks, from [`code_tokens`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CodeToken {
    /// What it is.
    pub kind: CodeTokenKind,
    /// Start offset (inclusive).
    pub start: usize,
    /// End offset (exclusive).
    pub end: usize,
}

impl CodeToken {
    /// The token inside `text`.
    #[must_use]
    pub fn text<'a>(&self, text: &'a str) -> &'a str {
        text.get(self.start..self.end).unwrap_or("")
    }

    /// `true` when the token is the word `keyword`, ignoring ASCII case.
    #[must_use]
    pub fn is_word(&self, text: &str, keyword: &str) -> bool {
        self.kind == CodeTokenKind::Word && self.text(text).eq_ignore_ascii_case(keyword)
    }
}

/// The tokens of `text` as the shared lexer reads them under `rules` (the same tokens as
/// [`split_with_rules`] and [`classify_with_rules`]): comments and blanks are skipped, the
/// content of executable comments is code. For drivers that recognise engine constructs on top
/// of the shared splitter (PostgreSQL `BEGIN ATOMIC` bodies, `COPY … FROM STDIN`).
#[must_use]
pub fn code_tokens(text: &str, rules: LexicalRules) -> Vec<CodeToken> {
    lex(text, rules)
        .toks
        .into_iter()
        .filter(|t| t.kind != TokKind::ExecMarker)
        .map(|t| CodeToken {
            kind: match (t.kind, text.as_bytes()[t.start]) {
                (TokKind::Word, _) => CodeTokenKind::Word,
                (TokKind::Str, _) => CodeTokenKind::Str,
                (TokKind::Ident, _) => CodeTokenKind::QuotedIdent,
                (TokKind::Semicolon, _) => CodeTokenKind::Semicolon,
                (TokKind::Other, b'(') if t.end == t.start + 1 => CodeTokenKind::OpenParen,
                (TokKind::Other, b')') if t.end == t.start + 1 => CodeTokenKind::CloseParen,
                (TokKind::Other | TokKind::ExecMarker, _) => CodeTokenKind::Other,
            },
            start: t.start,
            end: t.end,
        })
        .collect()
}

/// Fails with [`DriverError::MultipleStatements`] when `text` contains more than one statement
/// for `dialect`.
///
/// Drivers call it in [`Session::execute`](crate::Session::execute) and send exactly one
/// statement per protocol call (PostgreSQL extended protocol, MySQL without
/// `CLIENT_MULTI_STATEMENTS`), so the statement the engine runs is the one that was classified.
///
/// # Errors
/// [`DriverError::MultipleStatements`] with the number of statements found.
pub fn ensure_single_statement<D: Dialect + ?Sized>(dialect: &D, text: &str) -> DriverResult<()> {
    let n = dialect.split_statements(text).len();
    if n > 1 {
        Err(DriverError::MultipleStatements(n))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Classifier

/// One significant token of a statement, as seen by the classifier.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Item {
    /// Unquoted word, upper-cased.
    Word(String),
    /// Quoted identifier, unquoted and lower-cased.
    Ident(String),
    /// String literal, contents lower-cased (quotes and `E` prefix removed).
    Str(String),
    /// Punctuation.
    Punct(u8),
}

#[derive(Clone, Debug)]
struct Located {
    item: Item,
    /// Parenthesis depth (0 = top level).
    depth: u32,
}

/// Most conditional executable comments a text may hold before it is classified
/// [`StatementClass::Unknown`] without trying every combination.
const MAX_CONDITIONAL_COMMENTS: usize = 8;

/// Largest text with conditional executable comments that is classified by trying every
/// combination; a longer one is [`StatementClass::Unknown`] (security review L2: at most 2^8
/// passes over 64 KiB).
const MAX_CONDITIONAL_TEXT: usize = 64 * 1024;

/// Classifies `text` with the given lexical rules (the default of [`Dialect::classify`]).
///
/// With [`executable_comments`](LexicalRules::executable_comments), a conditional executable
/// comment (`/*!80000 … */` runs only from that server version on, `/*M! … */` only on
/// MariaDB) may or may not run: the class is the riskiest over every combination of them run or
/// skipped, so `DELETE FROM t /*!99999 WHERE 0 */` is destructive. More than
/// eight of them, a text longer than 64 KiB holding any, an unterminated one, or one whose end
/// depends on whether it runs, make the text [`StatementClass::Unknown`].
#[must_use]
pub fn classify_with_rules(text: &str, rules: LexicalRules) -> StatementClass {
    let lexed = lex(text, rules);
    if lexed.conditional.is_empty() {
        return classify_lexed(text, &lexed);
    }
    if lexed.conditional_ambiguous
        || lexed.conditional.len() > MAX_CONDITIONAL_COMMENTS
        || text.len() > MAX_CONDITIONAL_TEXT
    {
        return StatementClass::Unknown;
    }
    let mut worst = classify_lexed(text, &lexed);
    let n = lexed.conditional.len();
    for mask in 1u32..(1 << n) {
        let mut bytes = text.as_bytes().to_vec();
        for (k, &(s, e)) in lexed.conditional.iter().enumerate() {
            if mask & (1 << k) != 0 {
                // Skipped by the engine: a comment, i.e. blanks. Whole UTF-8 sequences are
                // overwritten, so the text stays valid.
                bytes[s..e].fill(b' ');
            }
        }
        let Ok(variant) = String::from_utf8(bytes) else {
            return StatementClass::Unknown;
        };
        worst = worst.max(classify_lexed(&variant, &lex(&variant, rules)));
        if worst == StatementClass::Unknown {
            break;
        }
    }
    worst
}

fn classify_lexed(text: &str, lexed: &Lexed) -> StatementClass {
    let mut worst = if lexed.executable_comment_as_comment {
        // The engine might execute what we skipped as a comment.
        Some(StatementClass::Unknown)
    } else {
        None
    };
    // Inside an executable comment (`/*! … */`) at the current token.
    let mut in_executable = false;
    for stmt in lexed.toks.split(|t| t.kind == TokKind::Semicolon) {
        if stmt.is_empty() {
            continue;
        }
        // A piece that starts inside an executable comment (after a `;` in it) reads
        // differently once split off and sent alone, and a piece of comment markers only is
        // no statement the classifier can vouch for: both are `Unknown` (re-review L1).
        let starts_inside = in_executable;
        for t in stmt.iter().filter(|t| t.kind == TokKind::ExecMarker) {
            in_executable = text.as_bytes()[t.start] == b'/';
        }
        let stmt: Vec<Tok> = stmt
            .iter()
            .filter(|t| t.kind != TokKind::ExecMarker)
            .copied()
            .collect();
        let (items, opaque) = locate(text, &stmt);
        let c = if starts_inside || stmt.is_empty() || opaque {
            // Or a name the classifier cannot decode, which could be anything.
            StatementClass::Unknown
        } else {
            classify_items(&items)
        };
        worst = Some(worst.map_or(c, |w| w.max(c)));
    }
    worst.unwrap_or(StatementClass::Read)
}

fn unquote_ident(raw: &str) -> String {
    let inner = raw.get(1..raw.len().saturating_sub(1)).unwrap_or("");
    inner
        .replace("\"\"", "\"")
        .replace("``", "`")
        .to_lowercase()
}

/// `U&"…"` / `U&'…'`: strips the prefix and the quotes, undoubles the quote and decodes
/// `\XXXX`, `\+XXXXXX` and `\\` (surrogate pairs included). `None` when the token is
/// unterminated or an escape is malformed.
fn decode_unicode_escaped(raw: &str) -> Option<String> {
    let body = raw.get(2..)?;
    let q = body.chars().next()?;
    if body.len() < 2 || !body.ends_with(q) {
        return None;
    }
    let doubled: String = [q, q].iter().collect();
    let inner = body[1..body.len() - 1].replace(&doubled, &q.to_string());
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    let mut pending_high: Option<u32> = None;
    while let Some(c) = chars.next() {
        if c != '\\' {
            if pending_high.is_some() {
                return None;
            }
            out.push(c);
            continue;
        }
        let digits = match chars.peek() {
            Some('\\') => {
                chars.next();
                if pending_high.is_some() {
                    return None;
                }
                out.push('\\');
                continue;
            }
            Some('+') => {
                chars.next();
                6
            }
            _ => 4,
        };
        let hex: String = chars.by_ref().take(digits).collect();
        if hex.len() != digits || !hex.chars().all(|h| h.is_ascii_hexdigit()) {
            return None;
        }
        let cp = u32::from_str_radix(&hex, 16).ok()?;
        match (pending_high.take(), cp) {
            (None, 0xD800..=0xDBFF) => pending_high = Some(cp),
            (Some(high), 0xDC00..=0xDFFF) => {
                out.push(char::from_u32(
                    0x10000 + ((high - 0xD800) << 10) + (cp - 0xDC00),
                )?);
            }
            (Some(_), _) => return None,
            (None, _) => out.push(char::from_u32(cp)?),
        }
    }
    if pending_high.is_some() {
        return None;
    }
    Some(out)
}

fn has_unicode_prefix(raw: &str) -> bool {
    raw.len() > 2 && raw.as_bytes()[1] == b'&' && matches!(raw.as_bytes()[0], b'u' | b'U')
}

fn unquote_str(raw: &str) -> String {
    let body = raw.strip_prefix(['e', 'E']).unwrap_or(raw);
    let inner = if body.len() >= 2 && body.starts_with('\'') && body.ends_with('\'') {
        &body[1..body.len() - 1]
    } else {
        body
    };
    inner.to_lowercase()
}

/// Items of one statement, and whether a token could not be decoded (`opaque`).
fn locate(text: &str, toks: &[Tok]) -> (Vec<Located>, bool) {
    let mut depth = 0u32;
    let mut opaque = false;
    let mut out = Vec::with_capacity(toks.len());
    for t in toks {
        let raw = &text[t.start..t.end];
        let item = match t.kind {
            TokKind::Word => {
                if raw.eq_ignore_ascii_case("UESCAPE") {
                    // A custom escape character for `U&` names: not decoded, fail closed.
                    opaque = true;
                }
                Item::Word(raw.to_ascii_uppercase())
            }
            TokKind::Ident | TokKind::Str if has_unicode_prefix(raw) => {
                if let Some(decoded) = decode_unicode_escaped(raw) {
                    let lower = decoded.to_lowercase();
                    if t.kind == TokKind::Ident {
                        Item::Ident(lower)
                    } else {
                        Item::Str(lower)
                    }
                } else {
                    opaque = true;
                    Item::Punct(b'?')
                }
            }
            TokKind::Ident => Item::Ident(unquote_ident(raw)),
            TokKind::Str => Item::Str(unquote_str(raw)),
            TokKind::Other | TokKind::Semicolon | TokKind::ExecMarker => {
                Item::Punct(raw.as_bytes()[0])
            }
        };
        if item == Item::Punct(b')') {
            depth = depth.saturating_sub(1);
        }
        out.push(Located {
            item: item.clone(),
            depth,
        });
        if item == Item::Punct(b'(') {
            depth += 1;
        }
    }
    (out, opaque)
}

/// Functions that change the session's access mode.
const ACCESS_MODE_FUNCTIONS: &[&str] = &["SET_CONFIG"];

/// Functions with side effects a `SELECT` can call. Unknown = needs confirmation, refused in
/// read-only sessions. Not exhaustive: see the module's "Limits of the classifier".
const SIDE_EFFECT_FUNCTIONS: &[&str] = &[
    "NEXTVAL",
    "SETVAL",
    "PG_TERMINATE_BACKEND",
    "PG_CANCEL_BACKEND",
    "PG_RELOAD_CONF",
    "PG_ROTATE_LOGFILE",
    "PG_PROMOTE",
    "PG_SWITCH_WAL",
    "PG_FILE_WRITE",
    "PG_START_BACKUP",
    "PG_STOP_BACKUP",
    "PG_DROP_REPLICATION_SLOT",
    "PG_REPLICATION_SLOT_ADVANCE",
    "PG_LOGICAL_SLOT_GET_CHANGES",
    "PG_LOGICAL_SLOT_GET_BINARY_CHANGES",
    "PG_LOGICAL_EMIT_MESSAGE",
    "PG_LOG_BACKEND_MEMORY_CONTEXTS",
    "PG_IMPORT_SYSTEM_COLLATIONS",
    "PG_STAT_STATEMENTS_RESET",
    "LO_IMPORT",
    "LO_EXPORT",
    "LO_UNLINK",
    "LO_CREAT",
    "LO_CREATE",
    "LO_PUT",
    "LO_FROM_BYTEA",
    "LO_TRUNCATE",
    "LO_TRUNCATE64",
    "LOWRITE",
    "LOAD_EXTENSION",
];

/// Prefixes of function families with side effects (F2): replication slots and origins,
/// statistics resets, backups, WAL replay control, `dblink*`, `pg_create_*` (restore points,
/// replication slots) and `pg_copy_*_replication_slot`. A few read-only members
/// (`pg_replication_origin_oid`…) are over-classified, which only asks for confirmation.
const SIDE_EFFECT_PREFIXES: &[&str] = &[
    "DBLINK",
    "PG_CREATE_",
    "PG_COPY_",
    "PG_STAT_RESET",
    "PG_BACKUP_",
    "PG_WAL_REPLAY_",
    "PG_REPLICATION_ORIGIN_",
];

fn is_side_effect_function(upper: &str) -> bool {
    SIDE_EFFECT_FUNCTIONS.contains(&upper)
        || SIDE_EFFECT_PREFIXES.iter().any(|p| upper.starts_with(p))
}

/// Settings `SET` / `RESET` may change without affecting access mode, privileges or the lexical
/// rules. Everything else is [`StatementClass::ChangesAccessMode`].
const HARMLESS_SETTINGS: &[&str] = &[
    "search_path",
    "schema",
    "timezone",
    "time_zone",
    "time",
    "datestyle",
    "intervalstyle",
    "client_encoding",
    "names",
    "character",
    "character_set_client",
    "character_set_results",
    "character_set_connection",
    "collation_connection",
    "application_name",
    "statement_timeout",
    "lock_timeout",
    "idle_in_transaction_session_timeout",
    "extra_float_digits",
    "bytea_output",
    "work_mem",
    "client_min_messages",
    "lc_messages",
    "lc_monetary",
    "lc_numeric",
    "lc_time",
    "max_execution_time",
    "sql_select_limit",
    "net_read_timeout",
    "net_write_timeout",
    "wait_timeout",
    "lock_wait_timeout",
    "innodb_lock_wait_timeout",
    "div_precision_increment",
    "group_concat_max_len",
];

/// SQLite pragmas that only read schema information.
const READ_PRAGMAS: &[&str] = &[
    "table_info",
    "table_xinfo",
    "table_list",
    "index_list",
    "index_info",
    "index_xinfo",
    "foreign_key_list",
    "database_list",
    "collation_list",
    "compile_options",
    "function_list",
    "module_list",
    "pragma_list",
];

fn word(l: &Located) -> Option<&str> {
    match &l.item {
        Item::Word(w) => Some(w),
        _ => None,
    }
}

/// Lower-cased name of a word or quoted identifier.
fn name(l: &Located) -> Option<String> {
    match &l.item {
        Item::Word(w) => Some(w.to_ascii_lowercase()),
        Item::Ident(i) => Some(i.clone()),
        _ => None,
    }
}

fn classify_items(items: &[Located]) -> StatementClass {
    // Statements must start with a keyword, optionally after opening parentheses.
    let Some(first_idx) = items.iter().position(|l| l.item != Item::Punct(b'(')) else {
        return StatementClass::Unknown;
    };
    let Some(first) = word(&items[first_idx]) else {
        return StatementClass::Unknown;
    };
    let base = classify_by_keyword(first, &items[first_idx..]);

    // Function calls anywhere in the statement.
    let mut worst = base;
    for pair in items.windows(2) {
        if pair[1].item != Item::Punct(b'(') {
            continue;
        }
        let Some(f) = word(&pair[0])
            .map(str::to_owned)
            .or_else(|| name(&pair[0]).map(|n| n.to_ascii_uppercase()))
        else {
            continue;
        };
        if ACCESS_MODE_FUNCTIONS.contains(&f.as_str()) {
            worst = worst.max(StatementClass::ChangesAccessMode);
        } else if is_side_effect_function(&f) {
            worst = worst.max(StatementClass::Unknown);
        }
    }
    worst
}

fn has_word(items: &[Located], w: &str) -> bool {
    items.iter().any(|l| word(l) == Some(w))
}

/// The keyword at `items[i]`, unless the word is used as a name: right after a `.`
/// (`x.where`) or after `AS` (`RETURNING 1 AS where`), where PostgreSQL and MySQL accept any
/// keyword as a column or alias name (security review H1). Use it for every keyword test whose
/// *presence* lowers a class, so a name can only over-classify.
fn keyword_at(items: &[Located], i: usize) -> Option<&str> {
    let w = word(items.get(i)?)?;
    let prev = i.checked_sub(1).and_then(|p| items.get(p));
    let as_name = prev.is_some_and(|p| p.item == Item::Punct(b'.') || word(p) == Some("AS"));
    (!as_name).then_some(w)
}

/// A top-level `w` used as a keyword (see [`keyword_at`]).
fn has_top_level_keyword(items: &[Located], w: &str) -> bool {
    (0..items.len()).any(|i| items[i].depth == 0 && keyword_at(items, i) == Some(w))
}

fn classify_by_keyword(first: &str, items: &[Located]) -> StatementClass {
    match first {
        "SELECT" | "VALUES" | "TABLE" => {
            if has_word(items, "INTO") {
                StatementClass::Write
            } else {
                StatementClass::Read
            }
        }
        "SHOW" => StatementClass::Read,
        // MySQL / MariaDB `DESCRIBE` / `DESC` are synonyms of `EXPLAIN` (`DESC ANALYZE
        // DELETE …`); `DESCRIBE t` stays a read.
        "EXPLAIN" | "DESCRIBE" | "DESC" => classify_explain(items),
        // MariaDB `ANALYZE [FORMAT=JSON] <statement>` runs the statement; PostgreSQL / MySQL /
        // SQLite `ANALYZE [TABLE] t` writes statistics. Never below what it may run.
        "ANALYZE" | "ANALYSE" => StatementClass::Unknown,
        "WITH" => {
            if !with_main_statement_found(items) {
                // Unbalanced or unterminated text, or a main statement the classifier cannot
                // see (P1-B sqlite review): never a mere read.
                StatementClass::Unknown
            } else if ["INSERT", "UPDATE", "DELETE", "MERGE"]
                .iter()
                .any(|w| has_word(items, w))
            {
                StatementClass::Unknown
            } else if has_word(items, "INTO") {
                StatementClass::Write
            } else {
                StatementClass::Read
            }
        }
        "INSERT" | "MERGE" | "UPSERT" | "REPLACE" => StatementClass::Write,
        "UPDATE" | "DELETE" => {
            // Only a top-level WHERE filters the statement; a WHERE inside a subquery does not.
            if has_top_level_keyword(items, "WHERE") {
                StatementClass::Write
            } else {
                StatementClass::Destructive
            }
        }
        "CREATE" => {
            let base = if is_create_or_replace(items) {
                // Silently overwrites an existing object (P1-E review L1).
                StatementClass::Destructive
            } else {
                StatementClass::Ddl
            };
            // A top-level `GRANT` / `REVOKE` inside a `CREATE` (PostgreSQL `CREATE SCHEMA …
            // GRANT …`) changes privileges (security review M2).
            let grants = (1..items.len()).any(|i| {
                items[i].depth == items[0].depth
                    && matches!(keyword_at(items, i), Some("GRANT" | "REVOKE"))
            });
            let base = if grants {
                base.max(StatementClass::Destructive)
            } else {
                base
            };
            base.max(classify_create_body(items))
        }
        "DROP" | "TRUNCATE" | "ALTER" | "GRANT" | "REVOKE" | "RENAME" => {
            StatementClass::Destructive
        }
        "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" | "END" => {
            if first == "START" && keyword_at(items, 1) != Some("TRANSACTION") {
                return StatementClass::Unknown;
            }
            if first == "BEGIN" && !is_plain_begin(&items[1..]) {
                // MariaDB `BEGIN NOT ATOMIC … END` is a compound statement that runs its body,
                // PostgreSQL `BEGIN ATOMIC` a function body: never mere transaction control.
                return StatementClass::Unknown;
            }
            if has_word(items, "WRITE") {
                // `BEGIN READ WRITE` / `START TRANSACTION READ WRITE`.
                StatementClass::ChangesAccessMode
            } else {
                StatementClass::Transaction
            }
        }
        "SET" | "RESET" => classify_set(first, &items[1..]),
        "PRAGMA" => classify_pragma(&items[1..]),
        "USE" => StatementClass::Session,
        _ => StatementClass::Unknown,
    }
}

/// Words that may follow `BEGIN` when it starts a transaction: PostgreSQL `BEGIN [WORK |
/// TRANSACTION] [ISOLATION LEVEL …, READ ONLY | READ WRITE, [NOT] DEFERRABLE]`, MySQL `BEGIN
/// [WORK]`, SQLite `BEGIN [DEFERRED | IMMEDIATE | EXCLUSIVE] [TRANSACTION]`.
const BEGIN_WORDS: &[&str] = &[
    "WORK",
    "TRANSACTION",
    "ISOLATION",
    "LEVEL",
    "READ",
    "WRITE",
    "ONLY",
    "COMMITTED",
    "UNCOMMITTED",
    "REPEATABLE",
    "SERIALIZABLE",
    "DEFERRABLE",
    "NOT",
    "DEFERRED",
    "IMMEDIATE",
    "EXCLUSIVE",
];

/// `true` when everything after `BEGIN` is a transaction mode ([`BEGIN_WORDS`] and commas).
fn is_plain_begin(rest: &[Located]) -> bool {
    rest.iter().all(|l| match &l.item {
        Item::Word(w) => BEGIN_WORDS.contains(&w.as_str()),
        Item::Punct(b',') => true,
        _ => false,
    })
}

/// Index of the first item after `CREATE [OR REPLACE] [ALGORITHM = x] [DEFINER = user[@host]]`
/// (`items[0]` is `CREATE`). The definer as the lexer reads it: `CURRENT_USER [()]`, or a user
/// (word, quoted name or string) followed by an `@host` variable token (dots included) or by a
/// bare `@` and a quoted host.
fn skip_definer(items: &[Located]) -> usize {
    let mut i = 1;
    if keyword_at(items, i) == Some("OR") && keyword_at(items, i + 1) == Some("REPLACE") {
        i += 2;
    }
    if keyword_at(items, i) == Some("ALGORITHM") {
        i += 3; // ALGORITHM = x
    }
    if keyword_at(items, i) != Some("DEFINER") {
        return i;
    }
    i += 1;
    if items.get(i).map(|l| &l.item) == Some(&Item::Punct(b'=')) {
        i += 1;
    }
    let user_is_current = items.get(i).and_then(word) == Some("CURRENT_USER");
    i += 1; // the user
    if user_is_current
        && items.get(i).map(|l| &l.item) == Some(&Item::Punct(b'('))
        && items.get(i + 1).map(|l| &l.item) == Some(&Item::Punct(b')'))
    {
        i += 2;
    }
    if items.get(i).map(|l| &l.item) == Some(&Item::Punct(b'@')) {
        i += 1; // `@host` (one token), or a bare `@` before a quoted host
        if matches!(
            items.get(i).map(|l| &l.item),
            Some(Item::Str(_) | Item::Ident(_))
        ) {
            i += 1;
        }
    }
    i
}

/// PostgreSQL `CREATE SCHEMA s [AUTHORIZATION r] [element …]`: every element (`CREATE TABLE`,
/// `CREATE VIEW`, `CREATE INDEX`, `CREATE SEQUENCE`, `CREATE TRIGGER`, `GRANT`) runs; the class
/// is the riskiest element's, at least [`StatementClass::Ddl`] (security review M2). Elements
/// are classified on their own slices, so nothing recurses into the rest of the statement.
fn classify_schema_elements(items: &[Located]) -> StatementClass {
    let base = items[0].depth;
    let starts: Vec<usize> = (1..items.len())
        .filter(|&i| {
            items[i].depth == base && matches!(keyword_at(items, i), Some("CREATE" | "GRANT"))
        })
        .collect();
    let mut class = StatementClass::Ddl;
    for (k, &start) in starts.iter().enumerate() {
        let end = starts.get(k + 1).copied().unwrap_or(items.len());
        class = class.max(classify_items(&items[start..end]));
    }
    class
}

/// Keywords that start a statement inside a trigger or rule body.
const BODY_STATEMENT_KEYWORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "MERGE", "REPLACE", "UPSERT", "TRUNCATE", "DROP", "ALTER",
    "GRANT", "REVOKE", "RENAME", "CALL",
];

/// The body of a `CREATE TRIGGER` / `CREATE RULE` runs later, on other sessions' statements;
/// a `CREATE EVENT` (MySQL / MariaDB) runs its body at a scheduled time, possibly at once.
/// Triggers and rules are classified as the riskiest statement of their body (so a trigger
/// whose body deletes every row of a table is destructive); events are
/// [`StatementClass::Unknown`]. The body starts after `BEGIN`, `DO`, or `FOR EACH ROW |
/// STATEMENT`, so the header's events (`AFTER UPDATE OF c ON t`) do not count. `Read`
/// otherwise, so the caller's class stands.
fn classify_create_body(items: &[Located]) -> StatementClass {
    // The object kind follows `CREATE [OR REPLACE] [ALGORITHM = x] [DEFINER = user[@host]]
    // [SQL SECURITY x] [TEMP | TEMPORARY] [CONSTRAINT]`. The definer is skipped as the lexer
    // reads it (re-review L3), so `CREATE DEFINER = view@localhost TRIGGER …` is a trigger.
    // `CREATE SCHEMA` counts only right after `CREATE`.
    if keyword_at(items, 1) == Some("SCHEMA") {
        return classify_schema_elements(items);
    }
    let start = skip_definer(items);
    let kind = items
        .iter()
        .enumerate()
        .skip(start)
        .filter(|(_, l)| l.depth == items[0].depth)
        .take(12)
        .find_map(|(i, _)| match keyword_at(items, i) {
            Some(w @ ("TRIGGER" | "RULE" | "EVENT")) => Some(w),
            Some("TABLE" | "VIEW" | "FUNCTION" | "PROCEDURE" | "INDEX" | "SEQUENCE" | "SCHEMA") => {
                Some("")
            }
            _ => None,
        });
    match kind {
        Some("EVENT") => return StatementClass::Unknown,
        Some("TRIGGER" | "RULE") => {}
        _ => return StatementClass::Read,
    }
    let body_start =
        items
            .windows(2)
            .enumerate()
            .find_map(|(p, w)| match (word(&w[0]), word(&w[1])) {
                (Some("BEGIN" | "DO"), _) => Some(p + 1),
                (Some("EACH"), Some("ROW" | "STATEMENT")) => Some(p + 2),
                _ => None,
            });
    let Some(body_start) = body_start else {
        return StatementClass::Read;
    };
    let body = items.get(body_start..).unwrap_or(&[]);
    body.iter()
        .enumerate()
        .filter(|(_, l)| word(l).is_some_and(|w| BODY_STATEMENT_KEYWORDS.contains(&w)))
        .map(|(p, _)| classify_items(&body[p..]))
        .fold(StatementClass::Read, StatementClass::max)
}

/// Keywords that can start the statement wrapped by `EXPLAIN` / `DESCRIBE`: PostgreSQL's
/// explainable statements (`SELECT`, `VALUES`, `TABLE`, `WITH`, DML, `CREATE TABLE … AS`,
/// `CREATE MATERIALIZED VIEW`, `REFRESH`, `DECLARE`, `EXECUTE`) and MySQL / MariaDB's
/// (`SELECT`, `TABLE`, `WITH`, `INSERT`, `REPLACE`, `UPDATE`, `DELETE`).
const EXPLAINABLE_KEYWORDS: &[&str] = &[
    "SELECT", "VALUES", "TABLE", "WITH", "INSERT", "UPDATE", "DELETE", "MERGE", "REPLACE",
    "UPSERT", "CREATE", "REFRESH", "DECLARE", "EXECUTE",
];

/// Option words that may precede the wrapped statement outside a parenthesised list:
/// PostgreSQL `ANALYZE` / `ANALYSE` / `VERBOSE`, MySQL / MariaDB `ANALYZE`, `FORMAT = x`,
/// `EXTENDED`, `PARTITIONS`, `INTO @var`, SQLite `QUERY PLAN`.
const EXPLAIN_OPTION_WORDS: &[&str] = &[
    "analyze",
    "analyse",
    "verbose",
    "format",
    "extended",
    "partitions",
    "into",
    "query",
    "plan",
];

/// `EXPLAIN` / `DESCRIBE` / `DESC` (P1-E review M1).
///
/// The wrapped statement *runs* when the option list is parenthesised (PostgreSQL
/// `EXPLAIN (…)`, whose option names may be quoted or `U&`-escaped: `EXPLAIN ("analyze")`,
/// `EXPLAIN (U&"analyze")`) or when `ANALYZE` / `ANALYSE` appears in any form (bare, quoted,
/// backquoted, `U&`-escaped) anywhere in the statement (PostgreSQL, MySQL `EXPLAIN ANALYZE`,
/// MariaDB `EXPLAIN ANALYZE` / `DESC ANALYZE`). Then the class is the riskiest of the
/// statements starting at every explainable keyword (so a misread option prefix cannot hide the
/// real start), at least [`StatementClass::Read`], and [`StatementClass::Unknown`] when the
/// text after the options does not start with an explainable keyword. Otherwise (plain
/// `EXPLAIN SELECT …`, SQLite `EXPLAIN [QUERY PLAN]`, `DESCRIBE t`) nothing runs: a read.
fn classify_explain(items: &[Located]) -> StatementClass {
    let rest = &items[1..];
    let parenthesised = rest.first().map(|l| &l.item) == Some(&Item::Punct(b'('));
    let analyze = rest
        .iter()
        .any(|l| name(l).is_some_and(|n| n == "analyze" || n == "analyse"));
    if !parenthesised && !analyze {
        return StatementClass::Read;
    }
    // Skip the option prefix: a parenthesised list, then option words, `=` and `@`, and the
    // value after `=` / `@`.
    let mut i = 0;
    if parenthesised {
        let base = rest[0].depth;
        i = 1;
        while i < rest.len() && !(rest[i].depth == base && rest[i].item == Item::Punct(b')')) {
            i += 1;
        }
        if i == rest.len() {
            // Unbalanced option list.
            return StatementClass::Unknown;
        }
        i += 1;
    }
    let mut value_next = false;
    while let Some(l) = rest.get(i) {
        let is_option = name(l).is_some_and(|n| EXPLAIN_OPTION_WORDS.contains(&n.as_str()));
        let is_link = matches!(l.item, Item::Punct(b'=' | b'@'));
        if !(value_next || is_option || is_link) {
            break;
        }
        value_next = is_link;
        i += 1;
    }
    let starts_statement = rest.get(i).is_some_and(|l| {
        l.item == Item::Punct(b'(') || word(l).is_some_and(|w| EXPLAINABLE_KEYWORDS.contains(&w))
    });
    if !starts_statement {
        return StatementClass::Unknown;
    }
    rest.iter()
        .enumerate()
        .filter(|(_, l)| word(l).is_some_and(|w| EXPLAINABLE_KEYWORDS.contains(&w)))
        .map(|(p, _)| classify_items(&rest[p..]))
        .fold(StatementClass::Read, StatementClass::max)
        // A parenthesised statement start is looked into by `classify_items` itself.
        .max(classify_items(&rest[i..]))
}

/// `CREATE OR REPLACE …` (PostgreSQL, MySQL / MariaDB, also after an executable comment): the
/// words `OR REPLACE` at the top level of the statement, `REPLACE` not being a function call.
fn is_create_or_replace(items: &[Located]) -> bool {
    items.windows(3).any(|w| {
        w[0].depth == 0
            && word(&w[0]) == Some("OR")
            && word(&w[1]) == Some("REPLACE")
            && w[2].item != Item::Punct(b'(')
    }) || items.len() >= 2
        && word(&items[items.len() - 2]) == Some("OR")
        && word(&items[items.len() - 1]) == Some("REPLACE")
}

/// Keywords that start the main statement of a `WITH`.
const WITH_MAIN_KEYWORDS: &[&str] = &[
    "SELECT", "VALUES", "TABLE", "INSERT", "UPDATE", "DELETE", "MERGE", "REPLACE", "UPSERT",
];

/// `true` when the `WITH` statement starting at `items[0]` has balanced parentheses and a main
/// statement keyword at the `WITH`'s own depth, i.e. outside every CTE body. The CTE list
/// (`name [(columns)] AS [NOT] [MATERIALIZED] (…)`, PostgreSQL's `SEARCH` / `CYCLE` clauses)
/// holds none of these keywords at that depth.
///
/// A main statement written in parentheses (`WITH c AS (…) (SELECT …)`) is not found either:
/// it classifies as [`StatementClass::Unknown`], which only asks for confirmation.
fn with_main_statement_found(items: &[Located]) -> bool {
    let Some(base) = items.first().map(|l| l.depth) else {
        return false;
    };
    // `items` may start after opening parentheses (`(WITH …)`): they count as open.
    let mut depth = i64::from(base);
    for l in items {
        match l.item {
            Item::Punct(b'(') => depth += 1,
            Item::Punct(b')') => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return false;
    }
    (1..items.len()).any(|i| {
        items[i].depth == base
            && keyword_at(items, i).is_some_and(|w| WITH_MAIN_KEYWORDS.contains(&w))
    })
}

/// `true` when a `SET` (after its scope modifiers) targets `TRANSACTION`: `SET [SESSION | LOCAL]
/// TRANSACTION <mode> [, <mode>]…`.
fn is_set_transaction(rest: &[Located]) -> bool {
    rest.iter()
        .find(|l| !matches!(word(l), Some("SESSION" | "LOCAL")))
        .and_then(word)
        .is_some_and(|w| w == "TRANSACTION")
}

/// `SET` / `RESET`: every assignment must target an allowlisted setting.
fn classify_set(first: &str, rest: &[Located]) -> StatementClass {
    if rest.is_empty() {
        return StatementClass::Unknown;
    }
    if first == "SET" && is_set_transaction(rest) && has_word(rest, "WRITE") {
        // Every comma segment is a transaction mode (`SET TRANSACTION ISOLATION LEVEL READ
        // COMMITTED, READ WRITE`): `READ WRITE` in any of them lifts read-only (P1-B review).
        return StatementClass::ChangesAccessMode;
    }
    let segments = rest.split(|l| l.depth == 0 && l.item == Item::Punct(b','));
    let mut class = StatementClass::Session;
    for (n, seg) in segments.enumerate() {
        // After the first, a segment without `=` / `TO` continues the previous value list
        // (`SET search_path TO a, b`); it names no setting.
        let assigns = seg.iter().any(|l| l.item == Item::Punct(b'=')) || has_word(seg, "TO");
        if n > 0 && !assigns {
            continue;
        }
        let c = classify_set_target(first, seg);
        class = class.max(c);
    }
    class
}

fn classify_set_target(first: &str, seg: &[Located]) -> StatementClass {
    let mut i = 0;
    // `@user_var` (MySQL) is harmless; `@@[session.|local.]name` is a system variable.
    if seg.first().map(|l| &l.item) == Some(&Item::Punct(b'@')) {
        if seg.get(1).map(|l| &l.item) != Some(&Item::Punct(b'@')) {
            return StatementClass::Session;
        }
        i = 2;
        if let (Some(scope), Some(dot)) = (seg.get(i), seg.get(i + 1))
            && dot.item == Item::Punct(b'.')
        {
            match word(scope) {
                Some("SESSION" | "LOCAL") => i += 2,
                _ => return StatementClass::ChangesAccessMode,
            }
        }
    } else {
        // Scope modifiers.
        while let Some(w) = seg.get(i).and_then(word) {
            match w {
                "SESSION" | "LOCAL" => i += 1,
                _ => break,
            }
        }
    }
    let Some(target) = seg.get(i).and_then(name) else {
        return StatementClass::Unknown;
    };
    if first == "RESET" && target == "all" {
        return StatementClass::ChangesAccessMode;
    }
    if target == "transaction" && seg.get(i).and_then(word) == Some("TRANSACTION") {
        // `SET TRANSACTION ISOLATION LEVEL …` is harmless; `… READ WRITE` is not. Only the
        // bare keyword: a quoted `"transaction"` names a setting.
        return if has_word(seg, "WRITE") {
            StatementClass::ChangesAccessMode
        } else {
            StatementClass::Session
        };
    }
    if ENCODING_SETTINGS.contains(&target.as_str()) {
        // The client encoding is a lexical mode (multi-byte encodings such as SJIS or GBK can
        // hide a quote behind a backslash): only UTF-8 values are harmless.
        return if encoding_value(&seg[i + 1..]).is_some_and(|v| is_utf8_value(&target, &v)) {
            StatementClass::Session
        } else {
            StatementClass::ChangesAccessMode
        };
    }
    if HARMLESS_SETTINGS.contains(&target.as_str()) {
        StatementClass::Session
    } else {
        StatementClass::ChangesAccessMode
    }
}

/// Settings that change the client encoding / collation (allowed with UTF-8 values only).
const ENCODING_SETTINGS: &[&str] = &[
    "client_encoding",
    "names",
    "character",
    "character_set_client",
    "character_set_results",
    "character_set_connection",
    "collation_connection",
];

/// First value after the setting name, skipping `=`, `:=`, `TO` and the `SET` of
/// `CHARACTER SET`.
fn encoding_value(rest: &[Located]) -> Option<String> {
    rest.iter()
        .find(|l| {
            !matches!(l.item, Item::Punct(b'=' | b':')) && !matches!(word(l), Some("TO" | "SET"))
        })
        .and_then(|l| match &l.item {
            Item::Word(w) => Some(w.to_ascii_lowercase()),
            Item::Ident(v) | Item::Str(v) => Some(v.clone()),
            Item::Punct(_) => None,
        })
}

fn is_utf8_value(target: &str, value: &str) -> bool {
    let v: String = value.chars().filter(|c| *c != '-').collect();
    if target == "collation_connection" {
        v.starts_with("utf8mb4_") || v.starts_with("utf8_")
    } else {
        matches!(v.as_str(), "utf8" | "utf8mb4")
    }
}

/// `PRAGMA [schema.]name[(arg)| = value]`: only schema-reading pragmas are reads.
fn classify_pragma(rest: &[Located]) -> StatementClass {
    let target = rest
        .iter()
        .take_while(|l| !matches!(l.item, Item::Punct(b'(' | b'=')))
        .filter_map(name)
        .last();
    match target {
        Some(t) if READ_PRAGMAS.contains(&t.as_str()) => StatementClass::Read,
        _ => StatementClass::ChangesAccessMode,
    }
}

// ---------------------------------------------------------------------------------------------
// Literals

/// Civil date from days since 1970-01-01 (proleptic Gregorian).
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    // Ranges are guaranteed by the algorithm: 1..=31 and 1..=12.
    (
        y,
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

/// `^[+-]?\d+(\.\d+)?([eE][+-]?\d+)?$`
pub(crate) fn is_strict_decimal(s: &str) -> bool {
    fn digits(b: &[u8]) -> usize {
        b.iter().take_while(|c| c.is_ascii_digit()).count()
    }
    let b = s.as_bytes();
    let mut i = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let n = digits(&b[i..]);
    if n == 0 {
        return false;
    }
    i += n;
    if b.get(i) == Some(&b'.') {
        let n = digits(&b[i + 1..]);
        if n == 0 {
            return false;
        }
        i += 1 + n;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let n = digits(&b[i..]);
        if n == 0 {
            return false;
        }
        i += n;
    }
    i == b.len()
}

fn quote_text(s: &str, backslash: bool) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' if backslash => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}

fn ansi_literal(value: &Param, backslash: bool) -> String {
    match value {
        Param::Null => "NULL".to_owned(),
        Param::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_owned(),
        Param::Int(i) => i.to_string(),
        Param::Float(f) if f.is_finite() => format!("{f:?}"),
        Param::Float(f) => quote_text(&f.to_string(), backslash),
        Param::Decimal(d) => {
            if is_strict_decimal(d) {
                d.clone()
            } else {
                quote_text(d, backslash)
            }
        }
        Param::Text(s) | Param::Json(s) => quote_text(s, backslash),
        Param::Bytes(b) => {
            let mut s = String::with_capacity(b.len() * 2 + 3);
            s.push_str("X'");
            for byte in b {
                s.push_str(&format!("{byte:02X}"));
            }
            s.push('\'');
            s
        }
        Param::Date(days) => {
            let (y, m, d) = civil_from_days(i64::from(*days));
            format!("DATE '{y:04}-{m:02}-{d:02}'")
        }
        Param::Timestamp { micros, utc } => {
            let secs = micros.div_euclid(1_000_000);
            let frac = micros.rem_euclid(1_000_000);
            let days = secs.div_euclid(86_400);
            let sod = secs.rem_euclid(86_400);
            let (y, m, d) = civil_from_days(days);
            let (hh, mm, ss) = (sod / 3600, (sod % 3600) / 60, sod % 60);
            let tz = if *utc { "+00:00" } else { "" };
            format!("TIMESTAMP '{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}.{frac:06}{tz}'")
        }
    }
}

// ---------------------------------------------------------------------------------------------
// DML generation

fn render_edit_ansi<D: Dialect + ?Sized>(d: &D, edit: &RowEdit) -> DriverResult<Statement> {
    if edit.table.is_empty() {
        return Err(DriverError::InvalidParams("edit without a table".into()));
    }
    let table = d.qualified_name(&edit.table);
    let mut params = Vec::new();
    let mut next = |p: &Param, params: &mut Vec<Param>| {
        params.push(p.clone());
        d.placeholder(params.len())
    };
    let text = match &edit.change {
        RowChange::Insert { values } => {
            if values.is_empty() {
                format!("INSERT INTO {table} DEFAULT VALUES")
            } else {
                let cols: Vec<String> = values.iter().map(|(c, _)| d.quote_identifier(c)).collect();
                let phs: Vec<String> = values.iter().map(|(_, v)| next(v, &mut params)).collect();
                format!(
                    "INSERT INTO {table} ({}) VALUES ({})",
                    cols.join(", "),
                    phs.join(", ")
                )
            }
        }
        RowChange::Update { key, values } => {
            if key.is_empty() {
                return Err(DriverError::InvalidParams(
                    "update without a unique key".into(),
                ));
            }
            if values.is_empty() {
                return Err(DriverError::InvalidParams("update without values".into()));
            }
            let sets: Vec<String> = values
                .iter()
                .map(|(c, v)| format!("{} = {}", d.quote_identifier(c), next(v, &mut params)))
                .collect();
            let wh = where_clause(d, key, &mut params, &mut next);
            format!("UPDATE {table} SET {} WHERE {wh}", sets.join(", "))
        }
        RowChange::Delete { key } => {
            if key.is_empty() {
                return Err(DriverError::InvalidParams(
                    "delete without a unique key".into(),
                ));
            }
            let wh = where_clause(d, key, &mut params, &mut next);
            format!("DELETE FROM {table} WHERE {wh}")
        }
    };
    Ok(Statement::sql(text).with_params(params))
}

fn where_clause<D: Dialect + ?Sized>(
    d: &D,
    key: &[(String, Param)],
    params: &mut Vec<Param>,
    next: &mut impl FnMut(&Param, &mut Vec<Param>) -> String,
) -> String {
    key.iter()
        .map(|(c, v)| {
            let col = d.quote_identifier(c);
            if *v == Param::Null {
                format!("{col} IS NULL")
            } else {
                format!("{col} = {}", next(v, params))
            }
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn texts(script: &str) -> Vec<&str> {
        AnsiDialect
            .split_statements(script)
            .iter()
            .map(|s| s.text(script))
            .collect()
    }

    #[test]
    fn splits_on_top_level_semicolons() {
        let s = "select 1; select ';' ; -- c; x\n select \"a;b\" /* ; */ from t;;  ";
        assert_eq!(
            texts(s),
            vec!["select 1", "select ';'", "select \"a;b\" /* ; */ from t"]
        );
    }

    #[test]
    fn comment_only_script_is_empty() {
        assert!(texts("-- nothing\n/* here */ ;").is_empty());
    }

    #[test]
    fn dialect_rules() {
        let pg = LexicalRules::POSTGRES;
        let s = "create function f() as $body$ select 1; $body$; /* a /* b; */ c; */ select $1;";
        let spans = split_with_rules(s, pg);
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert_eq!(spans[1].text(s), "select $1");

        let my = LexicalRules::MYSQL;
        let s = "select 'it\\'s; ok', `a;b` # x; y\n; select 2";
        let spans = split_with_rules(s, my);
        assert_eq!(spans.len(), 2, "{spans:?}");
    }

    #[test]
    fn classifies() {
        use StatementClass as C;
        let d = AnsiDialect;
        for (sql, want) in [
            ("SELECT * FROM t", C::Read),
            ("  (select 1)", C::Read),
            ("-- x\nshow tables", C::Read),
            ("select * into t2 from t", C::Write),
            ("insert into t values (1)", C::Write),
            ("update t set a = 1 where id = 2", C::Write),
            ("update t set a = 1", C::Destructive),
            ("DELETE FROM t", C::Destructive),
            ("delete from t /* where */", C::Destructive),
            ("delete from t where 'x' = 'where'", C::Write),
            ("drop table t", C::Destructive),
            ("truncate t", C::Destructive),
            ("alter table t add c int", C::Destructive),
            ("grant select on t to u", C::Destructive),
            ("create table t (a int)", C::Ddl),
            ("begin", C::Transaction),
            ("commit", C::Transaction),
            ("set search_path = app", C::Session),
            (
                "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
                C::ChangesAccessMode,
            ),
            (
                "set default_transaction_read_only = off",
                C::ChangesAccessMode,
            ),
            ("start transaction read write", C::ChangesAccessMode),
            ("reset all", C::ChangesAccessMode),
            (
                "with x as (delete from t returning *) select * from x",
                C::Unknown,
            ),
            ("with x as (select 1) select * from x", C::Read),
            ("explain select 1", C::Read),
            ("explain analyze delete from t", C::Destructive),
            ("vacuum", C::Unknown),
            ("call p()", C::Unknown),
            ("'oops'", C::Unknown),
            ("select 1; drop table t", C::Destructive),
            ("", C::Read),
        ] {
            assert_eq!(d.classify(sql), want, "{sql}");
        }
        assert!(C::Unknown.needs_confirmation());
        assert!(C::Unknown.is_write());
        assert!(!C::Read.is_write());
    }

    /// Bypasses found in the P0-B security review (H1, H2, M5). Each must stay caught.
    #[test]
    fn security_review_corpus() {
        use StatementClass as C;
        let my = LexicalRules::MYSQL;
        let pg = LexicalRules::POSTGRES;
        let ansi = LexicalRules::ANSI;
        for (rules, sql, at_least, spans) in [
            // H1: MySQL / MariaDB executable comments are code.
            (my, "SELECT 1 /*! ; DROP TABLE users */", C::Destructive, 2),
            (my, "/*!50000 DROP TABLE users */", C::Destructive, 1),
            (my, "/*M! DROP TABLE users */", C::Destructive, 1),
            (my, "/*M!100100 DROP TABLE users */", C::Destructive, 1),
            // ...and count as Unknown when the rules do not execute them.
            (ansi, "/*!50000 DROP TABLE users */", C::Unknown, 0),
            (pg, "SELECT 1 /*! DROP TABLE users */", C::Unknown, 1),
            // H1: `--` needs a following space in MySQL.
            (my, "SELECT 1--1; DROP TABLE users", C::Destructive, 2),
            (my, "SELECT 1-- comment; DROP TABLE users", C::Read, 1),
            // H1: PostgreSQL E-strings take backslash escapes.
            (
                pg,
                "SELECT E'\\''; DROP TABLE users; --'",
                C::Destructive,
                2,
            ),
            (
                pg,
                "SELECT e'\\''; DROP TABLE users; --'",
                C::Destructive,
                2,
            ),
            // H1: MySQL default mode vs NO_BACKSLASH_ESCAPES.
            (my, "SELECT 'a\\'; DROP TABLE users; -- '", C::Read, 1),
            (
                my.with_backslash_escapes(false),
                "SELECT 'a\\'; DROP TABLE users; -- '",
                C::Destructive,
                2,
            ),
            // H2: quoted setting names and functions that lift read-only.
            (
                pg,
                "SET \"default_transaction_read_only\" = off",
                C::ChangesAccessMode,
                1,
            ),
            (
                pg,
                "SET default_transaction_read_only = off",
                C::ChangesAccessMode,
                1,
            ),
            (
                pg,
                "SET SESSION default_transaction_read_only TO off",
                C::ChangesAccessMode,
                1,
            ),
            (
                pg,
                "SET search_path = app, transaction_read_only = off",
                C::ChangesAccessMode,
                1,
            ),
            (
                pg,
                "SELECT set_config('default_transaction_read_only','off',false)",
                C::ChangesAccessMode,
                1,
            ),
            (
                pg,
                "select * from (select SET_CONFIG('a','b',false)) x",
                C::ChangesAccessMode,
                1,
            ),
            (my, "SET `transaction_read_only`=0", C::ChangesAccessMode, 1),
            (
                my,
                "SET @@session.transaction_read_only = 0",
                C::ChangesAccessMode,
                1,
            ),
            (
                my,
                "SET @@global.time_zone = '+00:00'",
                C::ChangesAccessMode,
                1,
            ),
            (
                my,
                "SET GLOBAL time_zone = '+00:00'",
                C::ChangesAccessMode,
                1,
            ),
            (
                my,
                "SET sql_mode = 'NO_BACKSLASH_ESCAPES'",
                C::ChangesAccessMode,
                1,
            ),
            (ansi, "SET `transaction_read_only`=0", C::Unknown, 1),
            (pg, "SET ROLE admin", C::ChangesAccessMode, 1),
            (
                pg,
                "RESET default_transaction_read_only",
                C::ChangesAccessMode,
                1,
            ),
            (ansi, "PRAGMA query_only = OFF", C::ChangesAccessMode, 1),
            (ansi, "PRAGMA query_only(0)", C::ChangesAccessMode, 1),
            (ansi, "PRAGMA main.table_info(users)", C::Read, 1),
            (pg, "select nextval('s')", C::Unknown, 1),
            (pg, "select pg_terminate_backend(42)", C::Unknown, 1),
            // N1: `$` inside PostgreSQL identifiers does not open a dollar quote.
            (
                pg,
                "WITH a$q$ AS (DELETE FROM users RETURNING 1) SELECT 1 AS b$q$",
                C::Unknown,
                1,
            ),
            (
                pg,
                "SELECT x$q$, set_config('default_transaction_read_only','off',false) AS y$q$",
                C::ChangesAccessMode,
                1,
            ),
            (pg, "SELECT $q$ ; $q$ AS a$b", C::Read, 1),
            (pg, "SELECT $1, $q$x$q$", C::Read, 1),
            // N4: encoding changes are lexical-mode changes unless UTF-8.
            (my, "SET NAMES sjis", C::ChangesAccessMode, 1),
            (my, "SET NAMES 'gbk'", C::ChangesAccessMode, 1),
            (my, "SET CHARACTER SET big5", C::ChangesAccessMode, 1),
            (
                my,
                "SET character_set_client = gbk",
                C::ChangesAccessMode,
                1,
            ),
            (
                my,
                "SET collation_connection = gbk_chinese_ci",
                C::ChangesAccessMode,
                1,
            ),
            (pg, "SET client_encoding TO 'SJIS'", C::ChangesAccessMode, 1),
            (pg, "RESET client_encoding", C::ChangesAccessMode, 1),
            (pg, "SET client_encoding TO 'UTF-8'", C::Session, 1),
            (pg, "SET client_encoding = UTF8", C::Session, 1),
            (my, "SET CHARACTER SET utf8mb4", C::Session, 1),
            (
                my,
                "SET collation_connection = utf8mb4_general_ci",
                C::Session,
                1,
            ),
            // Harmless settings stay Session.
            (pg, "SET search_path TO app, public", C::Session, 1),
            (pg, "SET TIME ZONE 'UTC'", C::Session, 1),
            (my, "SET NAMES utf8mb4", C::Session, 1),
            (my, "SET @x = 1", C::Session, 1),
            (my, "SET @@session.time_zone = '+00:00'", C::Session, 1),
            (
                pg,
                "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
                C::Session,
                1,
            ),
            (pg, "SET TRANSACTION READ WRITE", C::ChangesAccessMode, 1),
            // F1: Unicode-escaped names are decoded before matching.
            (
                pg,
                "SELECT U&\"set\\005fconfig\"('default_transaction_read_only','off',false)",
                C::ChangesAccessMode,
                1,
            ),
            (
                pg,
                "SELECT u&\"\\0070g_terminate_backend\"(42)",
                C::Unknown,
                1,
            ),
            (
                pg,
                "SET U&\"default\\005ftransaction\\005fread\\005fonly\" = off",
                C::ChangesAccessMode,
                1,
            ),
            (pg, "SET U&\"search\\005fpath\" = app", C::Session, 1),
            (
                pg,
                "SELECT U&\"!0073et_config\" UESCAPE '!'('a','b',false)",
                C::Unknown,
                1,
            ),
            (pg, "SELECT U&\"bad\\zzzz\"(1)", C::Unknown, 1),
            (pg, "SELECT U&\"\\D83D\\DE00\" FROM t", C::Read, 1),
            (pg, "SELECT U&\"\\D83D\" FROM t", C::Unknown, 1),
            (pg, "SELECT U&'a;b'; DROP TABLE t", C::Destructive, 2),
            (
                pg,
                "SET client_encoding TO U&'\\0053JIS'",
                C::ChangesAccessMode,
                1,
            ),
            (pg, "SET client_encoding TO U&'UTF8'", C::Session, 1),
            // F2: more side-effect functions.
            (
                pg,
                "SELECT pg_create_logical_replication_slot('s', 'pgoutput')",
                C::Unknown,
                1,
            ),
            (
                pg,
                "SELECT * FROM pg_create_physical_replication_slot('s')",
                C::Unknown,
                1,
            ),
            (pg, "SELECT pg_drop_replication_slot('s')", C::Unknown, 1),
            (pg, "SELECT pg_stat_reset()", C::Unknown, 1),
            (pg, "SELECT pg_stat_reset_shared('bgwriter')", C::Unknown, 1),
            (pg, "SELECT pg_backup_start('x')", C::Unknown, 1),
            (pg, "SELECT pg_backup_stop()", C::Unknown, 1),
            (pg, "SELECT pg_wal_replay_pause()", C::Unknown, 1),
            (pg, "SELECT pg_wal_replay_resume()", C::Unknown, 1),
            (
                pg,
                "SELECT pg_logical_emit_message(true, 'p', 'x')",
                C::Unknown,
                1,
            ),
            (
                pg,
                "SELECT pg_replication_origin_create('o')",
                C::Unknown,
                1,
            ),
            (
                pg,
                "SELECT pg_replication_origin_session_setup('o')",
                C::Unknown,
                1,
            ),
            (pg, "SELECT dblink_open('c', 'select 1')", C::Unknown, 1),
            (
                pg,
                "SELECT pg_copy_logical_replication_slot('a', 'b')",
                C::Unknown,
                1,
            ),
            (
                pg,
                "SELECT pg_replication_slot_advance('s', '0/0')",
                C::Unknown,
                1,
            ),
            (
                pg,
                "SELECT pg_logical_slot_get_changes('s', NULL, NULL)",
                C::Unknown,
                1,
            ),
            // The limit is documented: unknown user functions stay Read (L4).
            (pg, "SELECT my_app_cleanup()", C::Read, 1),
            (
                pg,
                "SELECT pg_logical_slot_peek_changes('s', NULL, NULL)",
                C::Read,
                1,
            ),
            // M5: WHERE must be at the top level.
            (
                pg,
                "UPDATE users SET active = (SELECT false FROM flags WHERE id = 1)",
                C::Destructive,
                1,
            ),
            (
                pg,
                "DELETE FROM t WHERE id IN (SELECT id FROM u)",
                C::Write,
                1,
            ),
            (
                pg,
                "DELETE FROM t USING (SELECT 1 WHERE true) x",
                C::Destructive,
                1,
            ),
        ] {
            let got = classify_with_rules(sql, rules);
            assert!(
                got >= at_least,
                "{sql}: got {got:?}, want at least {at_least:?}"
            );
            if at_least <= C::Session {
                assert_eq!(got, at_least, "{sql}");
            }
            assert_eq!(split_with_rules(sql, rules).len(), spans, "{sql}");
        }
    }

    /// Phase 3 exit criterion: a `DELETE` / `UPDATE` without `WHERE` asks for confirmation on
    /// a production connection, whatever the dialect's lexical rules.
    #[test]
    fn delete_and_update_without_where_need_confirmation() {
        for rules in [
            LexicalRules::ANSI,
            LexicalRules::POSTGRES,
            LexicalRules::MYSQL,
            LexicalRules::SQLITE,
        ] {
            for sql in [
                "DELETE FROM t",
                "delete from t;",
                "UPDATE t SET a = 1",
                "update t set a = 1, b = 2 -- where id = 1",
                "SELECT 1; DELETE FROM t",
            ] {
                let c = classify_with_rules(sql, rules);
                assert!(c.needs_confirmation(), "{sql} -> {c:?} ({rules:?})");
                assert!(c.is_write(), "{sql}");
            }
            for sql in [
                "DELETE FROM t WHERE id = 1",
                "UPDATE t SET a = 1 WHERE id = 1",
            ] {
                assert!(
                    !classify_with_rules(sql, rules).needs_confirmation(),
                    "{sql}"
                );
            }
        }
    }

    /// Security review L2: conditional comments in a long text are not enumerated.
    #[test]
    fn long_text_with_conditional_comments_is_unknown() {
        let mut sql = "SELECT 1 /*!50000 , 2 */ FROM t WHERE a IN (".to_owned();
        sql.push_str(&"1, ".repeat(30_000));
        sql.push_str("1)");
        assert!(sql.len() > MAX_CONDITIONAL_TEXT);
        assert_eq!(
            classify_with_rules(&sql, LexicalRules::MYSQL),
            StatementClass::Unknown
        );
        // Without conditional comments, the same text is a read.
        let plain = sql.replace("/*!50000 , 2 */", "");
        assert_eq!(
            classify_with_rules(&plain, LexicalRules::MYSQL),
            StatementClass::Read
        );
    }

    /// Security review M3: positions cost one pass, whatever the line length.
    #[test]
    fn positions_are_linear_on_one_long_line() {
        let script = "SELECT 1;".repeat(200_000);
        let start = std::time::Instant::now();
        let items = AnsiDialect.split_script(&script);
        let elapsed = start.elapsed();
        assert_eq!(items.len(), 200_000);
        let last = items[199_999];
        assert_eq!((last.start.line, last.start.column), (1, 9 * 199_999 + 1));
        assert!(elapsed < std::time::Duration::from_secs(5), "{elapsed:?}");
    }

    #[test]
    fn line_index_positions() {
        let s = "ab\r\nçd\n\nx";
        let ix = LineIndex::new(s);
        let pos = |o| {
            let p = ix.position(o);
            (p.line, p.column)
        };
        assert_eq!(pos(0), (1, 1));
        assert_eq!(pos(2), (1, 3)); // the `\r` belongs to line 1
        assert_eq!(pos(4), (2, 1));
        assert_eq!(pos(6), (2, 2)); // `ç` is one column
        assert_eq!(pos(5), (2, 1)); // inside `ç`: moved back to its start
        assert_eq!(pos(9), (4, 1));
        assert_eq!(pos(100), (4, 2));
        assert_eq!(position_at(s, 6), ix.position(6));
    }

    #[test]
    fn single_statement_guard() {
        struct My;
        impl Dialect for My {
            fn lexical_rules(&self) -> LexicalRules {
                LexicalRules::MYSQL
            }
        }
        assert!(ensure_single_statement(&AnsiDialect, "select 1;").is_ok());
        assert!(ensure_single_statement(&AnsiDialect, "-- only a comment").is_ok());
        assert_eq!(
            ensure_single_statement(&AnsiDialect, "select 1; select 2"),
            Err(DriverError::MultipleStatements(2))
        );
        assert_eq!(
            ensure_single_statement(&My, "SELECT 1 /*! ; DROP TABLE users */"),
            Err(DriverError::MultipleStatements(2))
        );
    }

    #[test]
    fn strict_decimals() {
        for ok in ["0", "-12.50", "+1", "1e10", "1.5E-3"] {
            assert!(is_strict_decimal(ok), "{ok}");
        }
        for bad in [
            "", "-", "1.", ".5", "1e", "1-2", "--1", "1e+", "1.2.3", "1 ", "0x10",
        ] {
            assert!(!is_strict_decimal(bad), "{bad}");
        }
    }

    #[test]
    fn quoting_and_literals() {
        let d = AnsiDialect;
        assert_eq!(d.quote_identifier("a\"b"), "\"a\"\"b\"");
        assert_eq!(
            d.qualified_name(&["public".into(), "users".into()]),
            "\"public\".\"users\""
        );
        assert_eq!(d.format_literal(&Param::Text("it's".into())), "'it''s'");
        assert_eq!(d.format_literal(&Param::Null), "NULL");
        assert_eq!(d.format_literal(&Param::Bytes(vec![0xde, 0xad])), "X'DEAD'");
        assert_eq!(d.format_literal(&Param::Date(0)), "DATE '1970-01-01'");
        assert_eq!(d.format_literal(&Param::Date(19_723)), "DATE '2024-01-01'");
        assert_eq!(d.format_literal(&Param::Date(-1)), "DATE '1969-12-31'");
        assert_eq!(
            d.format_literal(&Param::Timestamp {
                micros: 1_500_000,
                utc: true
            }),
            "TIMESTAMP '1970-01-01 00:00:01.500000+00:00'"
        );
        assert_eq!(d.format_literal(&Param::Decimal("-12.50".into())), "-12.50");
        assert_eq!(
            d.format_literal(&Param::Decimal("1; drop table t".into())),
            "'1; drop table t'"
        );
        assert_eq!(d.format_literal(&Param::Float(1.0)), "1.0");
        assert_eq!(
            d.page_query("select 1;", 10, 20).as_deref(),
            Some("SELECT * FROM (select 1) AS ninesql_page LIMIT 10 OFFSET 20")
        );
        assert_eq!(d.explain("select 1", ExplainFormat::Text), None);
    }

    #[test]
    fn renders_edits_with_params() {
        let d = AnsiDialect;
        let s = d
            .render_edit(&RowEdit {
                table: vec!["app".into(), "users".into()],
                change: RowChange::Update {
                    key: vec![("id".into(), Param::Int(7)), ("tenant".into(), Param::Null)],
                    values: vec![("name".into(), Param::Text("x".into()))],
                },
            })
            .unwrap();
        assert_eq!(
            s.text,
            "UPDATE \"app\".\"users\" SET \"name\" = ? WHERE \"id\" = ? AND \"tenant\" IS NULL"
        );
        assert_eq!(s.params, vec![Param::Text("x".into()), Param::Int(7)]);

        let s = d
            .render_edit(&RowEdit {
                table: vec!["t".into()],
                change: RowChange::Insert {
                    values: vec![("a".into(), Param::Int(1)), ("b".into(), Param::Bool(true))],
                },
            })
            .unwrap();
        assert_eq!(s.text, "INSERT INTO \"t\" (\"a\", \"b\") VALUES (?, ?)");

        let s = d
            .render_edit(&RowEdit {
                table: vec!["t".into()],
                change: RowChange::Delete {
                    key: vec![("id".into(), Param::Int(1))],
                },
            })
            .unwrap();
        assert_eq!(s.text, "DELETE FROM \"t\" WHERE \"id\" = ?");

        let err = d.render_edit(&RowEdit {
            table: vec!["t".into()],
            change: RowChange::Delete { key: vec![] },
        });
        assert!(matches!(err, Err(DriverError::InvalidParams(_))));
    }

    fn any_rules() -> impl Strategy<Value = LexicalRules> {
        prop::collection::vec(any::<bool>(), 8).prop_map(|b| {
            LexicalRules::ANSI
                .with_backslash_escapes(b[0])
                .with_e_string_backslash(b[1])
                .with_nested_block_comments(b[2])
                .with_dollar_quotes(b[3])
                .with_hash_comments(b[4])
                .with_dash_comment_needs_space(b[5])
                .with_executable_comments(b[6])
                .with_backtick_identifiers(b[7])
        })
    }

    #[test]
    fn unicode_escapes_decode() {
        assert_eq!(
            decode_unicode_escaped("U&\"d\\0061t\\+000061\"").as_deref(),
            Some("data")
        );
        assert_eq!(decode_unicode_escaped("u&'it''s'").as_deref(), Some("it's"));
        assert_eq!(
            decode_unicode_escaped("U&\"a\\\\b\"").as_deref(),
            Some("a\\b")
        );
        assert_eq!(
            decode_unicode_escaped("U&\"\\D83D\\DE00\"").as_deref(),
            Some("\u{1F600}")
        );
        for bad in [
            "U&\"\\12\"",
            "U&\"\\+12\"",
            "U&\"\\DE00\"",
            "U&\"\\D83Dx\"",
            "U&\"\\+110000\"",
            "U&\"abc",
            "U&\"",
        ] {
            assert_eq!(decode_unicode_escaped(bad), None, "{bad}");
        }
    }

    proptest! {
        /// Any name, Unicode-escaped character by character, decodes back to itself.
        #[test]
        fn unicode_escaped_names_roundtrip(name in "\\PC{1,20}") {
            let mut quoted = String::from("U&\"");
            for c in name.chars() {
                quoted.push_str(&format!("\\+{:06X}", u32::from(c)));
            }
            quoted.push('"');
            prop_assert_eq!(decode_unicode_escaped(&quoted), Some(name));
        }

        #[test]
        fn spans_are_ordered_and_on_char_boundaries(script in "\\PC{0,300}", rules in any_rules()) {
            let spans = split_with_rules(&script, rules);
            let mut last = 0usize;
            for s in &spans {
                prop_assert!(s.start < s.end);
                prop_assert!(s.start >= last);
                prop_assert!(s.end <= script.len());
                prop_assert!(script.is_char_boundary(s.start));
                prop_assert!(script.is_char_boundary(s.end));
                prop_assert!(!s.text(&script).trim().is_empty());
                last = s.end;
            }
            let _ = classify_with_rules(&script, rules);
        }

        #[test]
        fn simple_statements_roundtrip(
            stmts in prop::collection::vec("select [a-z]{1,8} from [a-z]{1,8}( where [a-z] = '[a-z; ]{0,6}')?", 1..8),
        ) {
            let script = stmts.join(";\n");
            let got: Vec<String> = split_with_rules(&script, LexicalRules::default())
                .iter()
                .map(|s| s.text(&script).to_owned())
                .collect();
            prop_assert_eq!(got, stmts);
        }

        #[test]
        fn quoted_identifiers_roundtrip_as_one_token(name in "\\PC{0,30}") {
            let q = AnsiDialect.quote_identifier(&name);
            let toks = lex(&q, LexicalRules::default()).toks;
            prop_assert_eq!(toks.len(), 1);
            prop_assert_eq!(toks[0].kind, TokKind::Ident);
            prop_assert_eq!(toks[0].end, q.len());
        }

        #[test]
        fn text_literals_are_one_token(value in "\\PC{0,30}", backslash in any::<bool>()) {
            let lit = ansi_literal(&Param::Text(value), backslash);
            let rules = LexicalRules::ANSI.with_backslash_escapes(backslash);
            let toks = lex(&lit, rules).toks;
            prop_assert_eq!(toks.len(), 1);
            prop_assert_eq!(toks[0].end, lit.len());
        }
    }

    /// P1-B review: `SET [SESSION | LOCAL] TRANSACTION` is classified from every comma segment.
    #[test]
    fn set_transaction_comma_forms() {
        use StatementClass as C;
        for rules in [
            LexicalRules::ANSI,
            LexicalRules::POSTGRES,
            LexicalRules::MYSQL,
            LexicalRules::SQLITE,
        ] {
            for sql in [
                "SET TRANSACTION ISOLATION LEVEL READ COMMITTED, READ WRITE",
                "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED, READ WRITE",
                "SET LOCAL TRANSACTION ISOLATION LEVEL SERIALIZABLE, READ WRITE",
                "set session transaction read only, isolation level serializable, read write",
                "SET TRANSACTION READ WRITE, ISOLATION LEVEL REPEATABLE READ",
                "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE, READ WRITE, DEFERRABLE",
                "SET TRANSACTION ISOLATION LEVEL READ COMMITTED,READ/**/WRITE",
            ] {
                assert_eq!(
                    classify_with_rules(sql, rules),
                    C::ChangesAccessMode,
                    "{sql}"
                );
            }
            for sql in [
                "SET TRANSACTION ISOLATION LEVEL READ COMMITTED",
                "SET TRANSACTION ISOLATION LEVEL READ COMMITTED, READ ONLY",
                "SET SESSION TRANSACTION READ ONLY, ISOLATION LEVEL SERIALIZABLE",
            ] {
                assert_eq!(classify_with_rules(sql, rules), C::Session, "{sql}");
            }
        }
    }

    /// P1-B (sqlite) review: a `WITH` whose main statement is never found is `Unknown`.
    #[test]
    fn with_without_a_main_statement_is_unknown() {
        use StatementClass as C;
        let d = AnsiDialect;
        for sql in [
            // Unterminated string swallowing the main statement.
            "WITH c AS (SELECT ') DELETE FROM t WHERE a = 1",
            // Unbalanced parentheses.
            "WITH c AS (SELECT (1) SELECT * FROM c",
            "WITH c AS (SELECT 1)) SELECT * FROM c",
            // No main statement at all.
            "WITH c AS (SELECT 1)",
            "WITH",
            "WITH c AS (SELECT 1), d AS (SELECT 2)",
            // Unterminated quoted identifier inside the CTE list.
            "WITH \"c AS (SELECT 1) SELECT * FROM c",
            // A main statement in parentheses is not looked into (over-classified only).
            "WITH c AS (SELECT 1) (SELECT * FROM c)",
        ] {
            assert_eq!(d.classify(sql), C::Unknown, "{sql}");
        }
        for sql in [
            "WITH c AS (SELECT 1) SELECT * FROM c",
            "with recursive r(n) as (select 1 union all select n + 1 from r where n < 3) \
             select * from r",
            "WITH c AS MATERIALIZED (SELECT 1), d (x) AS NOT MATERIALIZED (SELECT 2) \
             SELECT * FROM c, d",
            "(WITH c AS (SELECT 1) SELECT * FROM c)",
            "WITH c AS (SELECT 1) VALUES (1)",
            "WITH c AS (SELECT 1) TABLE c",
            "WITH RECURSIVE t(id, p) AS (SELECT 1, 0) SEARCH DEPTH FIRST BY id SET ord \
             CYCLE id SET is_cycle USING path SELECT * FROM t",
            "EXPLAIN ANALYZE WITH c AS (SELECT 1) SELECT * FROM c",
        ] {
            assert_eq!(d.classify(sql), C::Read, "{sql}");
        }
        assert_eq!(
            d.classify("WITH c AS (SELECT 1) INSERT INTO t SELECT * FROM c"),
            C::Unknown
        );
        assert_eq!(
            d.classify("EXPLAIN ANALYZE WITH c AS (SELECT ') DELETE FROM t"),
            C::Unknown
        );
    }

    proptest! {
        /// Any comma list of transaction modes with `READ WRITE` somewhere lifts read-only;
        /// without it, it does not.
        #[test]
        fn set_transaction_modes_in_any_order(
            scope in prop::sample::select(vec!["", "SESSION ", "LOCAL ", "session "]),
            modes in prop::collection::vec(
                prop::sample::select(vec![
                    "ISOLATION LEVEL READ COMMITTED",
                    "ISOLATION LEVEL SERIALIZABLE",
                    "isolation level repeatable read",
                    "READ ONLY",
                    "DEFERRABLE",
                    "NOT DEFERRABLE",
                ]),
                0..4,
            ),
            write_at in any::<prop::sample::Index>(),
            sep in prop::sample::select(vec![", ", ",", " ,\n", " /* c */ , "]),
        ) {
            let mut with_write = modes.clone();
            let at = write_at.index(modes.len() + 1);
            with_write.insert(at, "READ WRITE");
            let sql = format!("SET {scope}TRANSACTION {}", with_write.join(sep));
            prop_assert_eq!(AnsiDialect.classify(&sql), StatementClass::ChangesAccessMode, "{}", sql);
            if !modes.is_empty() {
                let sql = format!("SET {scope}TRANSACTION {}", modes.join(sep));
                prop_assert_eq!(AnsiDialect.classify(&sql), StatementClass::Session, "{}", sql);
            }
        }

        /// A `WITH` is a read only when its parentheses balance and a main statement is
        /// found: cutting a read-only `WITH` anywhere inside its CTE list or its first
        /// parenthesis never yields `Read`.
        #[test]
        fn truncated_with_is_never_read(
            body in "[a-z0-9 ,]{0,12}",
            cut in any::<prop::sample::Index>(),
        ) {
            let sql = format!("WITH c AS (SELECT {body}) SELECT * FROM c");
            prop_assert_eq!(AnsiDialect.classify(&sql), StatementClass::Read, "{}", sql);
            let main = sql.rfind(") SELECT").map_or(sql.len(), |i| i + 1);
            // At least `WITH`: an empty text is no statement at all.
            let end = 4 + cut.index(main - 4);
            let truncated = &sql[..end];
            prop_assert_ne!(AnsiDialect.classify(truncated), StatementClass::Read, "{}", truncated);
        }
    }

    /// P1-E review M1: `EXPLAIN` forms that run the wrapped statement are classified as the
    /// statement itself; plain `EXPLAIN` / `DESCRIBE` stay reads.
    #[test]
    fn explain_that_runs_is_classified_as_what_it_runs() {
        use StatementClass as C;
        let pg = LexicalRules::POSTGRES;
        let my = LexicalRules::MYSQL;
        let lite = LexicalRules::SQLITE;
        for (rules, sql, want) in [
            // PostgreSQL: quoted and Unicode-escaped option names.
            (pg, "EXPLAIN (\"analyze\") DELETE FROM t", C::Destructive),
            (
                pg,
                "EXPLAIN (FORMAT JSON, \"analyze\" on) DELETE FROM t",
                C::Destructive,
            ),
            (pg, "EXPLAIN (U&\"analyze\") DELETE FROM t", C::Destructive),
            (
                pg,
                "EXPLAIN (U&\"\\0061nalyze\") DELETE FROM t",
                C::Destructive,
            ),
            (
                pg,
                "EXPLAIN (\"analyse\", BUFFERS) UPDATE t SET a = 1",
                C::Destructive,
            ),
            (pg, "EXPLAIN (ANALYZE) DELETE FROM t WHERE a = 1", C::Write),
            (
                pg,
                "explain (analyze true, verbose) insert into t values (1)",
                C::Write,
            ),
            (pg, "EXPLAIN ANALYZE VERBOSE DELETE FROM t", C::Destructive),
            (pg, "EXPLAIN ANALYSE DELETE FROM t", C::Destructive),
            (pg, "EXPLAIN \"analyze\" DELETE FROM t", C::Destructive),
            (pg, "EXPLAIN ANALYZE CREATE TABLE t2 AS SELECT 1", C::Ddl),
            (pg, "EXPLAIN ANALYZE EXECUTE prepared_stmt", C::Unknown),
            (
                pg,
                "EXPLAIN ANALYZE REFRESH MATERIALIZED VIEW m",
                C::Unknown,
            ),
            (pg, "EXPLAIN (ANALYZE) (SELECT 1)", C::Read),
            (pg, "EXPLAIN ANALYZE SELECT nextval('s')", C::Unknown),
            // A strict reading: any parenthesised option list is treated as running.
            (pg, "EXPLAIN (FORMAT JSON) DELETE FROM t", C::Destructive),
            (pg, "EXPLAIN (FORMAT JSON) SELECT 1", C::Read),
            (pg, "EXPLAIN (ANALYZE) SELECT * FROM t", C::Read),
            // Fail closed: an unbalanced list, nothing explainable after the options.
            (pg, "EXPLAIN (ANALYZE DELETE FROM t", C::Unknown),
            (pg, "EXPLAIN (ANALYZE)", C::Unknown),
            (pg, "EXPLAIN ANALYZE", C::Unknown),
            (pg, "EXPLAIN ANALYZE frobnicate", C::Unknown),
            // Plain EXPLAIN does not run the statement.
            (pg, "EXPLAIN DELETE FROM t", C::Read),
            (pg, "EXPLAIN VERBOSE DELETE FROM t", C::Read),
            // MySQL / MariaDB.
            (my, "EXPLAIN ANALYZE DELETE FROM t", C::Destructive),
            (
                my,
                "EXPLAIN ANALYZE FORMAT=TREE DELETE FROM t",
                C::Destructive,
            ),
            (
                my,
                "EXPLAIN FORMAT = JSON ANALYZE UPDATE t SET a = 1",
                C::Destructive,
            ),
            (my, "EXPLAIN `analyze` DELETE FROM t", C::Destructive),
            (my, "DESC ANALYZE DELETE FROM t", C::Destructive),
            (my, "DESCRIBE ANALYZE DELETE FROM t WHERE a = 1", C::Write),
            (my, "ANALYZE DELETE FROM t", C::Unknown),
            (
                my,
                "ANALYZE FORMAT=JSON DELETE FROM t WHERE a = 1",
                C::Unknown,
            ),
            (my, "ANALYZE SELECT 1", C::Unknown),
            (my, "ANALYZE TABLE t", C::Unknown),
            (my, "/*!50000 ANALYZE */ DELETE FROM t", C::Unknown),
            (my, "EXPLAIN /*! ANALYZE */ DELETE FROM t", C::Destructive),
            (my, "EXPLAIN DELETE FROM t", C::Read),
            (my, "EXPLAIN FORMAT=JSON DELETE FROM t", C::Read),
            (my, "EXPLAIN EXTENDED SELECT 1", C::Read),
            (my, "EXPLAIN ANALYZE SELECT * FROM t", C::Read),
            (my, "DESCRIBE t", C::Read),
            (my, "DESC t c", C::Read),
            // SQLite: EXPLAIN never runs the statement.
            (lite, "EXPLAIN DELETE FROM t", C::Read),
            (lite, "EXPLAIN QUERY PLAN DELETE FROM t", C::Read),
        ] {
            assert_eq!(classify_with_rules(sql, rules), want, "{sql}");
        }
    }

    /// P1-E review L1: `CREATE OR REPLACE` overwrites an existing object.
    #[test]
    fn create_or_replace_is_destructive() {
        use StatementClass as C;
        for (rules, sql) in [
            (
                LexicalRules::POSTGRES,
                "CREATE OR REPLACE FUNCTION f() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            (
                LexicalRules::POSTGRES,
                "create or replace view v as select 1",
            ),
            (
                LexicalRules::POSTGRES,
                "CREATE OR REPLACE TEMP VIEW v AS SELECT 1",
            ),
            (
                LexicalRules::POSTGRES,
                "CREATE OR REPLACE PROCEDURE p() LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            (
                LexicalRules::POSTGRES,
                "CREATE OR REPLACE TRIGGER tr AFTER INSERT ON t EXECUTE FUNCTION f()",
            ),
            (
                LexicalRules::POSTGRES,
                "CREATE OR REPLACE RULE r AS ON INSERT TO t DO NOTHING",
            ),
            (
                LexicalRules::POSTGRES,
                "CREATE /* c */ OR\nREPLACE VIEW v AS SELECT 1",
            ),
            (
                LexicalRules::MYSQL,
                "CREATE OR REPLACE ALGORITHM = MERGE VIEW v AS SELECT 1",
            ),
            (LexicalRules::MYSQL, "CREATE OR REPLACE TABLE t (a INT)"),
            (LexicalRules::MYSQL, "CREATE OR REPLACE SEQUENCE s"),
            (
                LexicalRules::MYSQL,
                "CREATE /*!50001 OR REPLACE */ VIEW v AS SELECT 1",
            ),
            (
                LexicalRules::POSTGRES,
                "EXPLAIN ANALYZE CREATE OR REPLACE VIEW v AS SELECT 1",
            ),
        ] {
            let c = classify_with_rules(sql, rules);
            assert!(c >= C::Destructive, "{sql}: {c:?}");
            assert!(c.needs_confirmation(), "{sql}");
        }
        for sql in [
            "CREATE TABLE t (a INT)",
            "CREATE VIEW v AS SELECT a OR b FROM t",
            "CREATE VIEW v AS SELECT a OR replace(b, 'x', 'y') FROM t",
            "CREATE TABLE IF NOT EXISTS t (a INT)",
        ] {
            assert_eq!(
                classify_with_rules(sql, LexicalRules::POSTGRES),
                C::Ddl,
                "{sql}"
            );
        }
    }

    /// Wrapped statements for the wrapper property test: one per class and shape.
    const WRAPPED: &[&str] = &[
        "SELECT * FROM t",
        "SELECT * FROM t WHERE a IN (SELECT b FROM u)",
        "SELECT nextval('s')",
        "SELECT set_config('default_transaction_read_only', 'off', false)",
        "SELECT * INTO t2 FROM t",
        "VALUES (1)",
        "TABLE t",
        "INSERT INTO t VALUES (1)",
        "INSERT INTO t SELECT * FROM u",
        "REPLACE INTO t VALUES (1)",
        "UPDATE t SET a = 1 WHERE b = 2",
        "UPDATE t SET a = 1",
        "UPDATE t SET a = (SELECT 1 WHERE true)",
        "DELETE FROM t WHERE a = 1",
        "DELETE FROM t",
        "DELETE FROM t WHERE a IN (SELECT b FROM u)",
        "MERGE INTO t USING u ON t.a = u.a WHEN MATCHED THEN DELETE",
        "WITH c AS (SELECT 1) SELECT * FROM c",
        "WITH c AS (DELETE FROM t RETURNING *) SELECT * FROM c",
        "WITH c AS (SELECT 1) DELETE FROM t",
        "CREATE TABLE t2 AS SELECT * FROM t",
        "CREATE OR REPLACE VIEW v AS SELECT 1",
        "(SELECT 1)",
        "EXECUTE p",
        "DECLARE c CURSOR FOR SELECT 1",
    ];

    /// Wrappers that run the wrapped statement on at least one engine.
    const RUNNING_WRAPPERS: &[&str] = &[
        "EXPLAIN ANALYZE ",
        "EXPLAIN ANALYSE ",
        "EXPLAIN ANALYZE VERBOSE ",
        "EXPLAIN (\"analyze\") ",
        "EXPLAIN (FORMAT JSON, \"analyze\" on) ",
        "EXPLAIN (U&\"analyze\") ",
        "EXPLAIN (U&\"\\0061naly\\0073e\" true, BUFFERS) ",
        "EXPLAIN (ANALYZE, FORMAT YAML) ",
        "EXPLAIN (FORMAT JSON) ",
        "explain analyze format = tree ",
        "EXPLAIN FORMAT=JSON ANALYZE ",
        "EXPLAIN `analyze` ",
        "DESC ANALYZE ",
        "DESCRIBE ANALYZE FORMAT=JSON ",
        "ANALYZE ",
        "ANALYZE FORMAT=JSON ",
        "EXPLAIN /* c */ ANALYZE -- x\n",
    ];

    proptest! {
        /// P1-E review M1: an `EXPLAIN` / `ANALYZE` wrapper that runs its statement never
        /// lowers the class of the wrapped statement, in any dialect.
        #[test]
        fn running_explain_wrappers_never_lower_the_class(
            stmt in prop::sample::select(WRAPPED),
            wrapper in prop::sample::select(RUNNING_WRAPPERS),
            rules in prop::sample::select(vec![
                LexicalRules::ANSI,
                LexicalRules::POSTGRES,
                LexicalRules::MYSQL,
                LexicalRules::SQLITE,
            ]),
            case in any::<bool>(),
        ) {
            let wrapper = if case { wrapper.to_lowercase() } else { wrapper.to_uppercase() };
            // Upper-casing a `U&` escape changes nothing: hex digits are case-insensitive.
            let sql = format!("{wrapper}{stmt}");
            let alone = classify_with_rules(stmt, rules);
            let wrapped = classify_with_rules(&sql, rules);
            prop_assert!(wrapped >= alone, "{} -> {:?} < {:?}", sql, wrapped, alone);
            prop_assert!(wrapped >= StatementClass::Read);
        }

        /// The same with random filler around a destructive statement: the wrapper keeps it at
        /// least destructive.
        #[test]
        fn running_explain_of_a_destructive_statement_needs_confirmation(
            wrapper in prop::sample::select(RUNNING_WRAPPERS),
            table in "[a-z_]{1,12}",
            column in "[a-z_]{1,12}",
        ) {
            for stmt in [format!("DELETE FROM {table}"), format!("UPDATE {table} SET {column} = 1")] {
                let sql = format!("{wrapper}{stmt}");
                for rules in [LexicalRules::POSTGRES, LexicalRules::MYSQL] {
                    prop_assert!(classify_with_rules(&sql, rules).needs_confirmation(), "{}", sql);
                }
            }
        }
    }
}
