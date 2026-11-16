//! Single-table `SELECT`s built safely for server-side sort and filter (F-DATA-03) and for
//! foreign-key navigation (F-DATA-07).
//!
//! # Rules
//! - Names are checked with [`Dialect::dml_check_identifier`] and quoted with the dialect.
//! - Filter values are **bind parameters**, never inlined ([`BuiltQuery::params`]).
//! - A raw `WHERE` expression typed by the user is accepted only as text that stays inside its
//!   parentheses and keeps the statement a read ([`check_raw_where`]): balanced parentheses, no
//!   `;`, no placeholder of its own, none of the words that change the statement's shape or
//!   write (`UNION`, `ORDER`, `LIMIT`, `INTO`, `FOR`, `UPDATE`, `DELETE`…), no token that runs
//!   past it (an unterminated string or comment), and the whole statement must split into one
//!   statement and classify as [`StatementClass::Read`] with the session's dialect, on every
//!   connection type. A function with side effects that the classifier reads as a read stays
//!   the classifier's documented limit; the statement then runs under the session's own rules
//!   (read-only sessions are enforced engine-side, I3).
//! - Nothing here logs a value or the raw text; `Debug` shows counts only.

use std::fmt;

use ninesql_driver_api::dml::DmlError;
use ninesql_driver_api::metadata::{ForeignKeyInfo, ObjectPath};
use ninesql_driver_api::{CodeTokenKind, Dialect, Param, StatementClass, code_tokens};

use super::key::table_parts;

/// Longest raw `WHERE` text accepted, in bytes.
pub const MAX_RAW_WHERE_BYTES: usize = 16 * 1024;

/// Words a raw `WHERE` may not hold outside strings and quoted names: they would change the
/// statement's shape, take locks or write.
const FORBIDDEN_WORDS: &[&str] = &[
    "UNION",
    "INTERSECT",
    "EXCEPT",
    "MINUS",
    "ORDER",
    "LIMIT",
    "OFFSET",
    "FETCH",
    "GROUP",
    "HAVING",
    "WINDOW",
    "INTO",
    "FOR",
    "LOCK",
    "UPDATE",
    "DELETE",
    "INSERT",
    "MERGE",
    "REPLACE",
    "RETURNING",
    "PROCEDURE",
    "CALL",
    "DO",
    "HANDLER",
];

/// A comparison of a [`ColumnFilter`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FilterOp {
    /// `=`.
    Equal,
    /// `<>`.
    NotEqual,
    /// `<`.
    Less,
    /// `<=`.
    LessOrEqual,
    /// `>`.
    Greater,
    /// `>=`.
    GreaterOrEqual,
    /// `IS NULL` (no value).
    IsNull,
    /// `IS NOT NULL` (no value).
    IsNotNull,
    /// `LIKE '%…%'` on a text value, wildcards in the value matched literally.
    Contains,
    /// `NOT LIKE '%…%'`.
    NotContains,
    /// `LIKE '…%'`.
    StartsWith,
}

impl FilterOp {
    /// Whether the comparison takes a value.
    #[must_use]
    pub fn takes_value(self) -> bool {
        !matches!(self, Self::IsNull | Self::IsNotNull)
    }

    /// A stable name (`=`, `is null`, `contains`…), for filter history and display.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Equal => "=",
            Self::NotEqual => "<>",
            Self::Less => "<",
            Self::LessOrEqual => "<=",
            Self::Greater => ">",
            Self::GreaterOrEqual => ">=",
            Self::IsNull => "is null",
            Self::IsNotNull => "is not null",
            Self::Contains => "contains",
            Self::NotContains => "does not contain",
            Self::StartsWith => "starts with",
        }
    }
}

/// One condition on a column. `Debug` hides the value.
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub struct ColumnFilter {
    /// Column name, unquoted.
    pub column: String,
    /// Comparison.
    pub op: FilterOp,
    /// The value (bound); `None` for [`FilterOp::IsNull`] / [`FilterOp::IsNotNull`].
    pub value: Option<Param>,
}

impl ColumnFilter {
    /// A condition.
    #[must_use]
    pub fn new(column: impl Into<String>, op: FilterOp, value: Option<Param>) -> Self {
        Self {
            column: column.into(),
            op,
            value,
        }
    }
}

impl fmt::Debug for ColumnFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ColumnFilter")
            .field("op", &self.op)
            .field("has_value", &self.value.is_some())
            .finish_non_exhaustive()
    }
}

/// Sort direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SortDirection {
    /// `ASC`.
    Ascending,
    /// `DESC`.
    Descending,
}

/// One `ORDER BY` key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct SortKey {
    /// Column name, unquoted.
    pub column: String,
    /// Direction.
    pub direction: SortDirection,
}

impl SortKey {
    /// A sort key.
    #[must_use]
    pub fn new(column: impl Into<String>, direction: SortDirection) -> Self {
        Self {
            column: column.into(),
            direction,
        }
    }
}

/// A `SELECT` from one table with optional filters and order ([`build_table_query`]). `Debug`
/// shows counts only.
#[derive(Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct TableQuery {
    /// Table name parts, unquoted ([`table_parts`] of a navigator path).
    pub table: Vec<String>,
    /// Columns to select, unquoted; `None`: `*`.
    pub columns: Option<Vec<String>>,
    /// Conditions, joined with `AND`.
    pub filters: Vec<ColumnFilter>,
    /// A user-typed `WHERE` expression (see the [module documentation](self)), `AND`ed with the
    /// filters.
    pub raw_where: Option<String>,
    /// Sort keys.
    pub order_by: Vec<SortKey>,
}

impl fmt::Debug for TableQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TableQuery")
            .field("parts", &self.table.len())
            .field("filters", &self.filters)
            .field("raw_where", &self.raw_where.as_ref().map(String::len))
            .field("order_by", &self.order_by.len())
            .finish_non_exhaustive()
    }
}

impl TableQuery {
    /// Every row of `table`.
    #[must_use]
    pub fn new(table: Vec<String>) -> Self {
        Self {
            table,
            ..Self::default()
        }
    }

    /// Every row of the table at `path` (a navigator path).
    #[must_use]
    pub fn of_path(path: &ObjectPath) -> Self {
        Self::new(table_parts(path))
    }

    /// Adds a condition.
    #[must_use]
    pub fn filter(mut self, filter: ColumnFilter) -> Self {
        self.filters.push(filter);
        self
    }

    /// Sets the raw `WHERE` expression.
    #[must_use]
    pub fn with_raw_where(mut self, raw: impl Into<String>) -> Self {
        self.raw_where = Some(raw.into());
        self
    }

    /// Adds a sort key.
    #[must_use]
    pub fn order(mut self, key: SortKey) -> Self {
        self.order_by.push(key);
        self
    }

    /// Selects these columns instead of `*`.
    #[must_use]
    pub fn with_columns(mut self, columns: Vec<String>) -> Self {
        self.columns = Some(columns);
        self
    }
}

/// A statement to run with its bind parameters (through the execution service:
/// `RunRequest::with_params`). `Debug` hides the parameters.
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub struct BuiltQuery {
    /// One statement, a read.
    pub text: String,
    /// Bind parameters, in placeholder order.
    pub params: Vec<Param>,
}

impl fmt::Debug for BuiltQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuiltQuery")
            .field("text_len", &self.text.len())
            .field("params", &self.params.len())
            .finish()
    }
}

/// Why a query could not be built. Never holds a value or the raw text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum QueryError {
    /// No table.
    #[error("no table")]
    NoTable,
    /// A name the engine would reject or change.
    #[error(transparent)]
    Name(#[from] DmlError),
    /// A comparison without its value, or `= NULL` (use "is null").
    #[error("the condition on {0} needs a value (use \"is null\" for NULL)")]
    MissingValue(String),
    /// A value given to "is null" / "is not null".
    #[error("the condition on {0} takes no value")]
    UnexpectedValue(String),
    /// "contains" / "starts with" with a value that is not text.
    #[error("the condition on {0} needs a text value")]
    NotText(String),
    /// The raw `WHERE` text was refused.
    #[error("the WHERE expression was refused: {0}")]
    RawWhere(&'static str),
    /// The statement does not classify as a read.
    #[error("the query would not be a read (classified {0:?})")]
    NotARead(StatementClass),
    /// A foreign key without columns, or with column lists of different lengths.
    #[error("the foreign key has no usable column list")]
    InvalidForeignKey,
    /// The values do not match the key's columns.
    #[error("expected {expected} key value(s), got {actual}")]
    ValueCount {
        /// Columns of the key.
        expected: usize,
        /// Values given.
        actual: usize,
    },
    /// A key value is `NULL`: it references no row.
    #[error("the value of {0} is NULL, so it references no row")]
    NullKey(String),
}

struct Builder<'a> {
    dialect: &'a dyn Dialect,
    text: String,
    params: Vec<Param>,
}

impl Builder<'_> {
    fn ident(&mut self, name: &str) -> Result<(), QueryError> {
        self.dialect.dml_check_identifier(name)?;
        self.text.push_str(&self.dialect.quote_identifier(name));
        Ok(())
    }

    fn param(&mut self, value: Param) {
        self.params.push(value);
        let p = self.dialect.placeholder(self.params.len());
        self.text.push_str(&p);
    }

    fn select_from(
        &mut self,
        table: &[String],
        columns: Option<&[String]>,
    ) -> Result<(), QueryError> {
        if table.is_empty() {
            return Err(QueryError::NoTable);
        }
        for part in table {
            self.dialect.dml_check_identifier(part)?;
        }
        self.text.push_str("SELECT ");
        match columns {
            None => self.text.push('*'),
            Some([]) => self.text.push('*'),
            Some(cols) => {
                for (i, c) in cols.iter().enumerate() {
                    if i > 0 {
                        self.text.push_str(", ");
                    }
                    self.ident(c)?;
                }
            }
        }
        self.text.push_str(" FROM ");
        self.text.push_str(&self.dialect.qualified_name(table));
        Ok(())
    }

    fn condition(&mut self, f: &ColumnFilter) -> Result<(), QueryError> {
        let name = || ninesql_driver_api::dml::echo_name(&f.column);
        if f.op.takes_value() {
            match &f.value {
                None | Some(Param::Null) => return Err(QueryError::MissingValue(name())),
                Some(_) => {}
            }
        } else if f.value.is_some() {
            return Err(QueryError::UnexpectedValue(name()));
        }
        self.ident(&f.column)?;
        let like = |prefix: bool, v: &Param| -> Result<Param, QueryError> {
            match v {
                Param::Text(t) => Ok(Param::Text(like_pattern(t, prefix))),
                _ => Err(QueryError::NotText(name())),
            }
        };
        match (f.op, f.value.as_ref()) {
            (FilterOp::IsNull, _) => self.text.push_str(" IS NULL"),
            (FilterOp::IsNotNull, _) => self.text.push_str(" IS NOT NULL"),
            (FilterOp::Contains | FilterOp::NotContains | FilterOp::StartsWith, Some(v)) => {
                let pattern = like(f.op == FilterOp::StartsWith, v)?;
                self.text.push_str(if f.op == FilterOp::NotContains {
                    " NOT LIKE "
                } else {
                    " LIKE "
                });
                self.param(pattern);
                self.text.push_str(" ESCAPE '!'");
            }
            (op, Some(v)) => {
                self.text.push(' ');
                self.text.push_str(match op {
                    FilterOp::NotEqual => "<>",
                    FilterOp::Less => "<",
                    FilterOp::LessOrEqual => "<=",
                    FilterOp::Greater => ">",
                    FilterOp::GreaterOrEqual => ">=",
                    _ => "=",
                });
                self.text.push(' ');
                self.param(v.clone());
            }
            (_, None) => return Err(QueryError::MissingValue(name())),
        }
        Ok(())
    }

    fn finish(self) -> Result<BuiltQuery, QueryError> {
        if self.dialect.split_statements(&self.text).len() != 1 {
            return Err(QueryError::RawWhere(
                "the query does not stay one statement",
            ));
        }
        let class = self.dialect.classify(&self.text);
        if class != StatementClass::Read {
            return Err(QueryError::NotARead(class));
        }
        Ok(BuiltQuery {
            text: self.text,
            params: self.params,
        })
    }
}

/// A `LIKE` pattern matching `text` literally (escape character `!`), as a substring or a
/// prefix.
fn like_pattern(text: &str, prefix: bool) -> String {
    let mut out = String::with_capacity(text.len() + 4);
    if !prefix {
        out.push('%');
    }
    for c in text.chars() {
        if matches!(c, '%' | '_' | '!') {
            out.push('!');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// Checks a user-typed `WHERE` expression on its own (see the [module documentation](self));
/// [`build_table_query`] checks it again in place.
///
/// # Errors
/// [`QueryError::RawWhere`] with the reason.
pub fn check_raw_where(dialect: &dyn Dialect, raw: &str) -> Result<(), QueryError> {
    let built = build_table_query(
        dialect,
        &TableQuery::new(vec!["t".into()]).with_raw_where(raw),
    )?;
    drop(built);
    Ok(())
}

/// Verifies the raw text at `range` of `text` (the whole statement).
fn check_raw_in_place(
    dialect: &dyn Dialect,
    text: &str,
    range: std::ops::Range<usize>,
) -> Result<(), QueryError> {
    let raw = &text[range.clone()];
    if raw.trim().is_empty() {
        return Err(QueryError::RawWhere("it is empty"));
    }
    if raw.len() > MAX_RAW_WHERE_BYTES {
        return Err(QueryError::RawWhere("it is too long"));
    }
    if raw.contains('\0') {
        return Err(QueryError::RawWhere("it holds a NUL character"));
    }
    let tokens = code_tokens(text, dialect.lexical_rules());
    // No token across either edge: an unterminated string, quoted name or comment would swallow
    // the closing parenthesis.
    if tokens.iter().any(|t| {
        (t.start < range.start && t.end > range.start) || (t.start < range.end && t.end > range.end)
    }) {
        return Err(QueryError::RawWhere(
            "a string, quoted name or comment is not closed",
        ));
    }
    let inside: Vec<_> = tokens
        .iter()
        .filter(|t| t.start >= range.start && t.end <= range.end)
        .collect();
    if inside.is_empty() {
        return Err(QueryError::RawWhere("it holds only comments"));
    }
    // The closing parenthesis right after the raw text must be ours.
    let after = tokens.iter().find(|t| t.start >= range.end);
    if after.is_none_or(|t| t.kind != CodeTokenKind::CloseParen || t.start != range.end + 1) {
        return Err(QueryError::RawWhere(
            "it does not stay inside its parentheses",
        ));
    }
    let bytes = text.as_bytes();
    let mut depth = 0i64;
    for t in &inside {
        let word = t.text(text);
        match t.kind {
            CodeTokenKind::OpenParen => depth += 1,
            CodeTokenKind::CloseParen => {
                depth -= 1;
                if depth < 0 {
                    return Err(QueryError::RawWhere("its parentheses do not balance"));
                }
            }
            CodeTokenKind::Semicolon => {
                return Err(QueryError::RawWhere("it holds a ';'"));
            }
            CodeTokenKind::Word => {
                if FORBIDDEN_WORDS.iter().any(|w| word.eq_ignore_ascii_case(w)) {
                    return Err(QueryError::RawWhere(
                        "it holds a word that changes the query or writes (UNION, ORDER, LIMIT, INTO, FOR, UPDATE…)",
                    ));
                }
                if word.starts_with(['$', '@']) {
                    return Err(QueryError::RawWhere(
                        "it holds a parameter or variable ($, @, ?, :name)",
                    ));
                }
            }
            CodeTokenKind::Other => {
                let named = word.ends_with(':')
                    && bytes
                        .get(t.end)
                        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
                    && !(word.len() >= 2 && word.ends_with("::"))
                    && !(t.start > 0 && bytes[t.start - 1] == b':');
                if word.contains(['?', '$', '@']) || named {
                    return Err(QueryError::RawWhere(
                        "it holds a parameter or variable ($, @, ?, :name)",
                    ));
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err(QueryError::RawWhere("its parentheses do not balance"));
    }
    Ok(())
}

/// Builds `SELECT <columns | *> FROM <table> [WHERE …] [ORDER BY …]` (see the
/// [module documentation](self)).
///
/// # Errors
/// [`QueryError`].
pub fn build_table_query(
    dialect: &dyn Dialect,
    query: &TableQuery,
) -> Result<BuiltQuery, QueryError> {
    let mut b = Builder {
        dialect,
        text: String::new(),
        params: Vec::new(),
    };
    b.select_from(&query.table, query.columns.as_deref())?;
    let mut first = true;
    let mut raw_range = None;
    for f in &query.filters {
        b.text.push_str(if first { " WHERE " } else { " AND " });
        first = false;
        b.condition(f)?;
    }
    if let Some(raw) = &query.raw_where {
        b.text
            .push_str(if first { " WHERE (\n" } else { " AND (\n" });
        let start = b.text.len();
        b.text.push_str(raw);
        raw_range = Some(start..b.text.len());
        b.text.push_str("\n)");
    }
    for (i, k) in query.order_by.iter().enumerate() {
        b.text.push_str(if i == 0 { " ORDER BY " } else { ", " });
        b.ident(&k.column)?;
        b.text.push_str(match k.direction {
            SortDirection::Ascending => " ASC",
            SortDirection::Descending => " DESC",
        });
    }
    if let Some(range) = raw_range {
        check_raw_in_place(dialect, &b.text, range)?;
    }
    b.finish()
}

fn key_query(
    dialect: &dyn Dialect,
    table: &[String],
    columns: &[String],
    values: &[Param],
) -> Result<BuiltQuery, QueryError> {
    if columns.is_empty() {
        return Err(QueryError::InvalidForeignKey);
    }
    if columns.len() != values.len() {
        return Err(QueryError::ValueCount {
            expected: columns.len(),
            actual: values.len(),
        });
    }
    let mut q = TableQuery::new(table.to_vec());
    for (c, v) in columns.iter().zip(values) {
        if *v == Param::Null {
            return Err(QueryError::NullKey(ninesql_driver_api::dml::echo_name(c)));
        }
        q = q.filter(ColumnFilter::new(
            c.clone(),
            FilterOp::Equal,
            Some(v.clone()),
        ));
    }
    build_table_query(dialect, &q)
}

/// The rows a foreign key points to (F-DATA-07): `SELECT * FROM <referenced table> WHERE
/// <referenced column> = ? AND …`, with `values` the row's values of the key's referencing
/// columns ([`ForeignKeyInfo::columns`], in order; read them with
/// [`row_values_by_name`](super::row_values_by_name)).
///
/// # Errors
/// [`QueryError::InvalidForeignKey`], [`QueryError::ValueCount`], [`QueryError::NullKey`] (a
/// `NULL` value references nothing), name errors.
pub fn referenced_rows_query(
    dialect: &dyn Dialect,
    fk: &ForeignKeyInfo,
    values: &[Param],
) -> Result<BuiltQuery, QueryError> {
    if fk.columns.len() != fk.referenced_columns.len() {
        return Err(QueryError::InvalidForeignKey);
    }
    key_query(
        dialect,
        &table_parts(&fk.referenced_table),
        &fk.referenced_columns,
        values,
    )
}

/// The rows that point to a row through a foreign key (F-DATA-07): `SELECT * FROM
/// <referencing table> WHERE <referencing column> = ? AND …`, where `referencing_table` is the
/// table the key belongs to and `values` the row's values of the referenced columns
/// ([`ForeignKeyInfo::referenced_columns`], in order).
///
/// # Errors
/// As [`referenced_rows_query`].
pub fn referencing_rows_query(
    dialect: &dyn Dialect,
    referencing_table: &ObjectPath,
    fk: &ForeignKeyInfo,
    values: &[Param],
) -> Result<BuiltQuery, QueryError> {
    if fk.columns.len() != fk.referenced_columns.len() {
        return Err(QueryError::InvalidForeignKey);
    }
    key_query(
        dialect,
        &table_parts(referencing_table),
        &fk.columns,
        values,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use ninesql_driver_api::{AnsiDialect, LexicalRules, ObjectKind};

    use super::*;

    struct Pg;
    impl Dialect for Pg {
        fn lexical_rules(&self) -> LexicalRules {
            LexicalRules::POSTGRES
        }
        fn placeholder(&self, index: usize) -> String {
            format!("${index}")
        }
    }

    struct My;
    impl Dialect for My {
        fn lexical_rules(&self) -> LexicalRules {
            LexicalRules::MYSQL
        }
        fn quote_identifier(&self, ident: &str) -> String {
            format!("`{}`", ident.replace('`', "``"))
        }
    }

    #[test]
    fn filters_are_bound_and_names_quoted() {
        let q = TableQuery::new(vec!["s".into(), "t\"x".into()])
            .filter(ColumnFilter::new(
                "a\"; DROP",
                FilterOp::Equal,
                Some(Param::Text("x'; DROP TABLE t; --".into())),
            ))
            .filter(ColumnFilter::new("b", FilterOp::IsNull, None))
            .filter(ColumnFilter::new(
                "c",
                FilterOp::Contains,
                Some(Param::Text("50%_!".into())),
            ))
            .order(SortKey::new("a", SortDirection::Descending));
        let b = build_table_query(&Pg, &q).unwrap();
        assert_eq!(
            b.text,
            "SELECT * FROM \"s\".\"t\"\"x\" WHERE \"a\"\"; DROP\" = $1 AND \"b\" IS NULL AND \"c\" LIKE $2 ESCAPE '!' ORDER BY \"a\" DESC"
        );
        assert_eq!(
            b.params,
            vec![
                Param::Text("x'; DROP TABLE t; --".into()),
                Param::Text("%50!%!_!!%".into())
            ]
        );
        assert!(!format!("{b:?} {q:?}").contains("DROP TABLE"));
        // Value rules.
        for f in [
            ColumnFilter::new("a", FilterOp::Equal, None),
            ColumnFilter::new("a", FilterOp::Equal, Some(Param::Null)),
            ColumnFilter::new("a", FilterOp::IsNull, Some(Param::Int(1))),
            ColumnFilter::new("a", FilterOp::StartsWith, Some(Param::Int(1))),
            ColumnFilter::new("a\0", FilterOp::IsNull, None),
        ] {
            assert!(build_table_query(&Pg, &TableQuery::new(vec!["t".into()]).filter(f)).is_err());
        }
    }

    #[test]
    fn raw_where_stays_a_read_inside_its_parentheses() {
        let ok = [
            "a = 1",
            "a IN (SELECT id FROM u WHERE u.x > 2) AND b <> 'x)y'",
            "lower(\"name\") LIKE 'a%' -- trailing comment",
            "x::int > 3",
            "(a = 1) OR (b = 2)",
        ];
        for raw in ok {
            let q = TableQuery::new(vec!["t".into()]).with_raw_where(raw);
            let b = build_table_query(&Pg, &q).unwrap_or_else(|e| panic!("{raw}: {e}"));
            assert!(b.params.is_empty());
        }
        let refused = [
            "",
            "   ",
            "1=1) UNION SELECT * FROM secrets --",
            "1=1; DROP TABLE t",
            "a = 1) OR (1=1",
            "a = 'unterminated",
            "a = 1 /* open comment",
            "a = \"open name",
            "a = $1",
            "a = ?",
            "a = :name",
            "1=1 ORDER BY 1",
            "a IN (SELECT 1 FOR UPDATE)",
            "a = (SELECT nextval('s'))) --",
            "-- only a comment",
            "x = 1 INTO OUTFILE '/tmp/x'",
            "a = 1\u{0}",
        ];
        for raw in refused {
            let q = TableQuery::new(vec!["t".into()]).with_raw_where(raw);
            assert!(build_table_query(&Pg, &q).is_err(), "{raw:?} accepted");
        }
        // MySQL: a `#` comment, user variables, executable comments.
        for raw in [
            "a = @v",
            "a = 1 /*! UNION SELECT 1 */",
            "a = 1 # open\n) OR (1 = 1",
        ] {
            let q = TableQuery::new(vec!["t".into()]).with_raw_where(raw);
            assert!(build_table_query(&My, &q).is_err(), "{raw:?} accepted");
        }
        // A destructive function the classifier knows is refused on every type.
        let q =
            TableQuery::new(vec!["t".into()]).with_raw_where("a = 1 AND (SELECT 1 FROM t2) = 1");
        assert!(build_table_query(&AnsiDialect, &q).is_ok());
    }

    #[test]
    fn foreign_key_navigation() {
        let mut fk = ForeignKeyInfo::new(
            ObjectPath::root()
                .child(ObjectKind::Schema, "public")
                .child(ObjectKind::Table, "customers"),
        );
        fk.columns = vec!["customer_id".into()];
        fk.referenced_columns = vec!["id".into()];
        let b = referenced_rows_query(&Pg, &fk, &[Param::Int(7)]).unwrap();
        assert_eq!(
            b.text,
            "SELECT * FROM \"public\".\"customers\" WHERE \"id\" = $1"
        );
        assert_eq!(b.params, vec![Param::Int(7)]);
        let orders = ObjectPath::root()
            .child(ObjectKind::Schema, "public")
            .child(ObjectKind::Table, "orders");
        let b = referencing_rows_query(&Pg, &orders, &fk, &[Param::Int(7)]).unwrap();
        assert_eq!(
            b.text,
            "SELECT * FROM \"public\".\"orders\" WHERE \"customer_id\" = $1"
        );
        assert!(matches!(
            referenced_rows_query(&Pg, &fk, &[Param::Null]),
            Err(QueryError::NullKey(_))
        ));
        assert!(matches!(
            referenced_rows_query(&Pg, &fk, &[]),
            Err(QueryError::ValueCount { .. })
        ));
        fk.referenced_columns.clear();
        assert!(referenced_rows_query(&Pg, &fk, &[Param::Int(1)]).is_err());
    }
}
