//! DML generation for row edits and SQL `INSERT` export (P5-B, F-DATA-04, F-DATA-06).
//!
//! [`generate`] turns one row change ([`DmlRequest`]) into a [`DmlStatement`] with two outputs:
//! - [`DmlStatement::statement`]: the statement that is **executed**, with every value bound as
//!   a parameter (never inlined) and the dialect's placeholders;
//! - [`DmlStatement::preview`]: the same statement with typed literals, for display (the DML
//!   preview before saving), copy and the "SQL `INSERT`" export. It is never executed by the
//!   core; running it by hand gives the same row state as the bound form (tested per engine).
//!
//! [`insert_preview`] renders an `INSERT` preview for export without editing capabilities
//! (nothing is executed). [`cell_value`] reads a grid cell (an Arrow value) as a [`Param`].
//!
//! # Rules (I3, ADR-0007, I8)
//! - Identifiers are quoted with the dialect's quoting ([`Dialect::quote_identifier`]) and
//!   schema-qualified with [`Dialect::qualified_name`]; the dialect refuses names its engine
//!   would change or reject ([`Dialect::dml_check_identifier`]: empty names, NUL, names longer
//!   than the engine keeps).
//! - `UPDATE` and `DELETE` always carry a `WHERE` on the row key: an empty key is refused, and
//!   so is a `NULL` key value unless the key is a unique key whose `NULL`s are not distinct
//!   ([`KeyKind`]). A key column whose type has no reliable equality on the engine
//!   ([`Dialect::dml_comparable`]) is refused. [`Concurrency::EditedColumns`] adds the original
//!   values of the edited columns to the `WHERE` (`IS NULL` for a `NULL` original); columns
//!   without reliable equality are left out and listed in
//!   [`DmlStatement::unchecked_columns`].
//! - Every statement expects exactly one affected row ([`DmlStatement::expected_rows`]); the
//!   caller checks the engine's update count with [`DmlStatement::check_affected`] and rolls
//!   back otherwise.
//! - [`EditingCapabilities`] are honoured: a kind of edit the driver does not declare is
//!   refused before anything is rendered (QuestDB declares none).
//! - **Self-check**: both texts must classify as [`StatementClass::Write`] with the dialect's
//!   own classifier and split into exactly one statement with
//!   [`Dialect::split_statements`]; otherwise generation fails. A preview holds no NUL.
//!
//! # Lexical modes
//! Literals in the preview depend on the session's lexical modes (MySQL
//! `NO_BACKSLASH_ESCAPES`, PostgreSQL `standard_conforming_strings` for `bytea`), recorded in
//! [`DmlStatement::lexical_rules`]. A caller that shows a preview after the session's
//! [`Dialect::lexical_rules`] changed generates it again. The bound form does not depend on
//! them.
//!
//! # Values in logs and errors
//! `Debug` of [`DmlStatement`] and [`DmlBatch`] never prints the preview or the parameter
//! values; [`DmlError`] names columns, never values.

use std::collections::BTreeSet;
use std::fmt;

use arrow_array::Array;
use arrow_array::cast::AsArray;
use arrow_array::types::{
    Date32Type, Date64Type, Decimal128Type, Decimal256Type, Float32Type, Float64Type, Int8Type,
    Int16Type, Int32Type, Int64Type, Time32MillisecondType, Time32SecondType,
    Time64MicrosecondType, Time64NanosecondType, TimestampMicrosecondType,
    TimestampMillisecondType, TimestampNanosecondType, TimestampSecondType, UInt8Type, UInt16Type,
    UInt32Type, UInt64Type,
};
use arrow_schema::{DataType, Field, TimeUnit};

use crate::capabilities::{EditingCapabilities, Feature};
use crate::dialect::{Dialect, LexicalRules, StatementClass, civil_from_days, is_strict_decimal};
use crate::error::DriverError;
use crate::result::{ENGINE_TYPE_METADATA_KEY, TRUNCATED_VALUES_METADATA_KEY};
use crate::statement::{Param, Statement};

/// Longest column or table name echoed in a [`DmlError`] (characters).
const MAX_ECHOED_NAME: usize = 64;

/// The type of a column, as the result set describes it: the Arrow type and, when the driver
/// gives it, the engine's type name ([`ENGINE_TYPE_METADATA_KEY`]).
///
/// The type must describe the **table column** the value is written to (a result column that
/// references it directly). Dialects use it to type preview literals and to decide whether a
/// column can be compared in a `WHERE`; the engine type name is never copied into SQL unless it
/// is on the dialect's own list of built-in type names.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ColumnType {
    /// Arrow type of the column in the result.
    pub data_type: DataType,
    /// Engine type name (`uuid`, `integer[]`, `jsonb`, `decimal`, `json`…), when known.
    pub engine_type: Option<String>,
}

impl ColumnType {
    /// A type with no engine type name.
    #[must_use]
    pub fn new(data_type: DataType) -> Self {
        Self {
            data_type,
            engine_type: None,
        }
    }

    /// Sets the engine type name.
    #[must_use]
    pub fn with_engine_type(mut self, engine_type: impl Into<String>) -> Self {
        self.engine_type = Some(engine_type.into());
        self
    }

    /// The type of a result field: its Arrow type and its [`ENGINE_TYPE_METADATA_KEY`].
    #[must_use]
    pub fn from_field(field: &Field) -> Self {
        Self {
            data_type: field.data_type().clone(),
            engine_type: field.metadata().get(ENGINE_TYPE_METADATA_KEY).cloned(),
        }
    }

    /// The engine type name, trimmed and lower-cased.
    #[must_use]
    pub fn engine_type_lower(&self) -> Option<String> {
        self.engine_type
            .as_deref()
            .map(|t| t.trim().to_ascii_lowercase())
            .filter(|t| !t.is_empty())
    }
}

/// A column of the edited table.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DmlColumn {
    /// Column name, unquoted.
    pub name: String,
    /// Column type.
    pub ty: ColumnType,
}

impl DmlColumn {
    /// A column.
    #[must_use]
    pub fn new(name: impl Into<String>, ty: ColumnType) -> Self {
        Self {
            name: name.into(),
            ty,
        }
    }

    /// A column described by a result field (name, Arrow type, engine type).
    #[must_use]
    pub fn from_field(field: &Field) -> Self {
        Self::new(field.name().clone(), ColumnType::from_field(field))
    }
}

/// A column and one value. `Debug` shows the value's type only ([`Param`]).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct ColumnValue {
    /// Column.
    pub column: DmlColumn,
    /// Value.
    pub value: Param,
}

impl ColumnValue {
    /// A column value.
    #[must_use]
    pub fn new(column: DmlColumn, value: Param) -> Self {
        Self { column, value }
    }
}

/// An edited column: its original value (as read) and the new value.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct ColumnChange {
    /// Column.
    pub column: DmlColumn,
    /// Value as read, used by [`Concurrency::EditedColumns`].
    pub original: Param,
    /// New value.
    pub new: Param,
}

impl ColumnChange {
    /// A column change.
    #[must_use]
    pub fn new(column: DmlColumn, original: Param, new: Param) -> Self {
        Self {
            column,
            original,
            new,
        }
    }
}

/// What kind of key identifies the row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum KeyKind {
    /// The primary key. Its values are never `NULL`; a `NULL` is refused.
    PrimaryKey,
    /// A unique constraint or a full (non-partial, non-expression) unique index.
    Unique {
        /// `NULL`s are distinct (the default on every MVP engine): a `NULL` key value does not
        /// identify one row and is refused. `false` only for PostgreSQL `NULLS NOT DISTINCT`;
        /// `NULL` key values are then matched with `IS NULL`.
        nulls_distinct: bool,
    },
}

/// How the `WHERE` of an `UPDATE` / `DELETE` guards against concurrent changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Concurrency {
    /// The key only.
    #[default]
    KeyOnly,
    /// The key, plus the original values of the edited columns (`UPDATE`) or of the
    /// [`DmlChange::Delete::check`] columns (`DELETE`): the statement changes nothing (and
    /// [`DmlStatement::check_affected`] fails) when another session changed them.
    EditedColumns,
}

/// One row change.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum DmlChange {
    /// Insert a row with these values (none: the engine's defaults).
    Insert {
        /// Column values.
        values: Vec<ColumnValue>,
    },
    /// Update the row identified by `key`.
    Update {
        /// Key columns with their original values. Must not be empty.
        key: Vec<ColumnValue>,
        /// Edited columns. Must not be empty.
        changes: Vec<ColumnChange>,
    },
    /// Delete the row identified by `key`.
    Delete {
        /// Key columns with their original values. Must not be empty.
        key: Vec<ColumnValue>,
        /// Original values compared under [`Concurrency::EditedColumns`] (ignored otherwise).
        check: Vec<ColumnValue>,
    },
}

/// A row change on a table.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct DmlRequest {
    /// Table path, unquoted parts (`["public", "users"]`, `["main", "t"]`).
    pub table: Vec<String>,
    /// Kind of the key in `change` (ignored for inserts).
    pub key_kind: KeyKind,
    /// Concurrency guard (ignored for inserts).
    pub concurrency: Concurrency,
    /// The change.
    pub change: DmlChange,
}

impl DmlRequest {
    /// Insert a row.
    #[must_use]
    pub fn insert(table: Vec<String>, values: Vec<ColumnValue>) -> Self {
        Self {
            table,
            key_kind: KeyKind::PrimaryKey,
            concurrency: Concurrency::KeyOnly,
            change: DmlChange::Insert { values },
        }
    }

    /// Update the row identified by `key` (a key of `key_kind`).
    #[must_use]
    pub fn update(
        table: Vec<String>,
        key_kind: KeyKind,
        key: Vec<ColumnValue>,
        changes: Vec<ColumnChange>,
    ) -> Self {
        Self {
            table,
            key_kind,
            concurrency: Concurrency::KeyOnly,
            change: DmlChange::Update { key, changes },
        }
    }

    /// Delete the row identified by `key` (a key of `key_kind`).
    #[must_use]
    pub fn delete(table: Vec<String>, key_kind: KeyKind, key: Vec<ColumnValue>) -> Self {
        Self {
            table,
            key_kind,
            concurrency: Concurrency::KeyOnly,
            change: DmlChange::Delete {
                key,
                check: Vec::new(),
            },
        }
    }

    /// Sets the concurrency guard.
    #[must_use]
    pub fn with_concurrency(mut self, concurrency: Concurrency) -> Self {
        self.concurrency = concurrency;
        self
    }

    /// Sets the original values a `DELETE` compares under [`Concurrency::EditedColumns`]
    /// (no effect on other changes).
    #[must_use]
    pub fn with_delete_check(mut self, values: Vec<ColumnValue>) -> Self {
        if let DmlChange::Delete { check, .. } = &mut self.change {
            *check = values;
        }
        self
    }
}

/// Kind of a generated statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DmlKind {
    /// `INSERT`.
    Insert,
    /// `UPDATE`.
    Update,
    /// `DELETE`.
    Delete,
}

impl fmt::Display for DmlKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
        })
    }
}

/// Why a row change cannot be generated, or why its outcome is refused. Messages name
/// columns (escaped, cut to 64 characters), never values.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DmlError {
    /// The driver does not declare this kind of edit for the engine (I8).
    #[error("{0} is not supported on this engine")]
    NotSupported(DmlKind),
    /// `UPDATE` / `DELETE` without a key: the grid stays read-only for this table.
    #[error(
        "the table has no primary key or unique key with non-null values: rows cannot be identified"
    )]
    NoKey,
    /// A key value is `NULL` and the key does not match `NULL`s as one row.
    #[error("the key column {0} is NULL for this row, so the key does not identify one row")]
    NullKey(String),
    /// A key column has no reliable equality on the engine.
    #[error("the key column {0} cannot be compared reliably on this engine")]
    KeyNotComparable(String),
    /// An `UPDATE` without edited columns.
    #[error("nothing to update")]
    NoChanges,
    /// The same column appears twice in one list.
    #[error("the column {0} appears twice")]
    DuplicateColumn(String),
    /// The table path is empty.
    #[error("no table")]
    NoTable,
    /// A table or column name the engine would reject or change.
    #[error("the name {name} cannot be used: {reason}")]
    InvalidIdentifier {
        /// The name, escaped and cut.
        name: String,
        /// Why.
        reason: &'static str,
    },
    /// A value with no faithful literal or parameter on the engine.
    #[error("the value of column {column} cannot be written: {reason}")]
    UnsupportedValue {
        /// The column, escaped and cut.
        column: String,
        /// Why (never the value).
        reason: &'static str,
    },
    /// A grid value cut at the fetch budget: writing it back would destroy data.
    #[error("the value of column {0} was cut when it was read and cannot be written back")]
    TruncatedValue(String),
    /// An Arrow value with no [`Param`] mapping.
    #[error("the type of column {0} cannot be edited")]
    UnsupportedType(String),
    /// The generated text failed the self-check (classification or splitting).
    #[error("the generated statement failed a safety check: {0}")]
    SelfCheck(&'static str),
    /// The update count is not the expected one: the caller rolls back.
    #[error(
        "expected {expected} row(s) to change, the engine reported {}: the row was changed or deleted by someone else, or the key does not identify one row",
        actual.map_or_else(|| "no count".to_owned(), |n| n.to_string())
    )]
    RowCount {
        /// Expected count.
        expected: u64,
        /// Reported count.
        actual: Option<u64>,
    },
}

impl DmlError {
    /// An [`InvalidIdentifier`](Self::InvalidIdentifier) error for `name`.
    #[must_use]
    pub fn invalid_identifier(name: &str, reason: &'static str) -> Self {
        Self::InvalidIdentifier {
            name: echo_name(name),
            reason,
        }
    }

    /// An [`UnsupportedValue`](Self::UnsupportedValue) error for column `column`.
    #[must_use]
    pub fn unsupported_value(column: &str, reason: &'static str) -> Self {
        Self::UnsupportedValue {
            column: echo_name(column),
            reason,
        }
    }
}

impl From<DmlError> for DriverError {
    fn from(e: DmlError) -> Self {
        match e {
            DmlError::NotSupported(_) => Self::Unsupported(Feature::Editing),
            other => Self::InvalidParams(other.to_string()),
        }
    }
}

/// A name as echoed in errors: Rust-escaped (control and invisible characters visible), cut.
#[must_use]
pub fn echo_name(name: &str) -> String {
    let mut out = String::from("\"");
    for (i, c) in name.chars().enumerate() {
        if i == MAX_ECHOED_NAME {
            out.push('…');
            break;
        }
        if c == '"' || c == '\\' || needs_escape(c) {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out.push('"');
    out
}

/// A generated row statement.
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub struct DmlStatement {
    /// Kind.
    pub kind: DmlKind,
    /// What is executed: values bound as parameters.
    pub statement: Statement,
    /// The same statement with literals, without a trailing `;`. Display, copy and export only.
    pub preview: String,
    /// Rows the statement must affect (always 1).
    pub expected_rows: u64,
    /// Edited (or delete-check) columns left out of the [`Concurrency::EditedColumns`] guard
    /// because the engine has no reliable equality for their type.
    pub unchecked_columns: Vec<String>,
    /// Lexical rules the preview was rendered for.
    pub lexical_rules: LexicalRules,
}

impl DmlStatement {
    /// Checks the update count the engine reported for [`statement`](Self::statement).
    ///
    /// # Errors
    /// [`DmlError::RowCount`] when the count is unknown or not [`expected_rows`](Self::expected_rows);
    /// the caller then rolls the change back.
    pub fn check_affected(&self, count: Option<u64>) -> Result<(), DmlError> {
        if count == Some(self.expected_rows) {
            Ok(())
        } else {
            Err(DmlError::RowCount {
                expected: self.expected_rows,
                actual: count,
            })
        }
    }
}

impl fmt::Debug for DmlStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DmlStatement")
            .field("kind", &self.kind)
            .field("statement", &self.statement)
            .field("preview_len", &self.preview.len())
            .field("expected_rows", &self.expected_rows)
            .field("unchecked_columns", &self.unchecked_columns.len())
            .finish_non_exhaustive()
    }
}

/// Generated statements for several row changes, in order (one statement per row).
#[derive(Clone, Default, PartialEq)]
pub struct DmlBatch {
    /// One statement per request, in request order.
    pub statements: Vec<DmlStatement>,
}

impl DmlBatch {
    /// The previews as one script: each statement followed by `;` and a new line. Splitting it
    /// with the dialect gives exactly one statement per row.
    #[must_use]
    pub fn preview_script(&self) -> String {
        let mut out = String::new();
        for s in &self.statements {
            out.push_str(&s.preview);
            out.push_str(";\n");
        }
        out
    }
}

impl fmt::Debug for DmlBatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DmlBatch")
            .field("statements", &self.statements)
            .finish()
    }
}

// ---------------------------------------------------------------------------------------------
// Generation

/// Generates the statement for one row change with `dialect` (the session's dialect, so its
/// lexical modes are current) and the engine's editing capabilities.
///
/// # Errors
/// A [`DmlError`] when the change is not declared by `editing`, has no usable key, names an
/// invalid identifier, holds a value with no faithful literal, or fails the self-check (see the
/// [module documentation](self)).
pub fn generate<D: Dialect + ?Sized>(
    dialect: &D,
    editing: &EditingCapabilities,
    request: &DmlRequest,
) -> Result<DmlStatement, DmlError> {
    let kind = match &request.change {
        DmlChange::Insert { .. } => DmlKind::Insert,
        DmlChange::Update { .. } => DmlKind::Update,
        DmlChange::Delete { .. } => DmlKind::Delete,
    };
    let allowed = match kind {
        DmlKind::Insert => editing.insert,
        DmlKind::Update => editing.update,
        DmlKind::Delete => editing.delete,
    };
    if !allowed {
        return Err(DmlError::NotSupported(kind));
    }
    build(dialect, request, kind)
}

/// [`generate`] for each request, in order. Fails on the first request that fails.
///
/// # Errors
/// The first [`DmlError`].
pub fn generate_batch<D: Dialect + ?Sized>(
    dialect: &D,
    editing: &EditingCapabilities,
    requests: &[DmlRequest],
) -> Result<DmlBatch, DmlError> {
    let statements = requests
        .iter()
        .map(|r| generate(dialect, editing, r))
        .collect::<Result<Vec<_>, _>>()?;
    let batch = DmlBatch { statements };
    if !requests.is_empty()
        && dialect.split_statements(&batch.preview_script()).len() != requests.len()
    {
        return Err(DmlError::SelfCheck(
            "the preview script does not split into one statement per row",
        ));
    }
    Ok(batch)
}

/// The preview text of an `INSERT` of one row, without a trailing `;`, for the "SQL `INSERT`"
/// export and copy (F-XFER-01, F-DATA-06). Nothing is executed, so no editing capability is
/// needed; the text passes the same self-check as [`generate`].
///
/// # Errors
/// As [`generate`] for an insert.
pub fn insert_preview<D: Dialect + ?Sized>(
    dialect: &D,
    table: &[String],
    values: &[ColumnValue],
) -> Result<String, DmlError> {
    let request = DmlRequest::insert(table.to_vec(), values.to_vec());
    build(dialect, &request, DmlKind::Insert).map(|s| s.preview)
}

/// Both texts, built side by side.
struct Texts<'a, D: Dialect + ?Sized> {
    dialect: &'a D,
    sql: String,
    preview: String,
    params: Vec<Param>,
}

impl<D: Dialect + ?Sized> Texts<'_, D> {
    fn both(&mut self, s: &str) {
        self.sql.push_str(s);
        self.preview.push_str(s);
    }

    fn value(&mut self, column: &DmlColumn, value: &Param) -> Result<(), DmlError> {
        let literal = self
            .dialect
            .dml_literal(value, &column.ty)
            .map_err(|reason| DmlError::unsupported_value(&column.name, reason))?;
        self.params.push(value.clone());
        let placeholder = self.dialect.placeholder(self.params.len());
        self.sql.push_str(&placeholder);
        self.preview.push_str(&literal);
        Ok(())
    }

    fn ident(&mut self, name: &str) -> Result<(), DmlError> {
        self.dialect.dml_check_identifier(name)?;
        let q = self.dialect.quote_identifier(name);
        self.both(&q);
        Ok(())
    }
}

fn check_unique<'a>(names: impl Iterator<Item = &'a str>) -> Result<(), DmlError> {
    let mut seen = BTreeSet::new();
    for n in names {
        if !seen.insert(n) {
            return Err(DmlError::DuplicateColumn(echo_name(n)));
        }
    }
    Ok(())
}

fn build<D: Dialect + ?Sized>(
    dialect: &D,
    request: &DmlRequest,
    kind: DmlKind,
) -> Result<DmlStatement, DmlError> {
    if request.table.is_empty() {
        return Err(DmlError::NoTable);
    }
    for part in &request.table {
        dialect.dml_check_identifier(part)?;
    }
    let table = dialect.qualified_name(&request.table);
    let mut t = Texts {
        dialect,
        sql: String::new(),
        preview: String::new(),
        params: Vec::new(),
    };
    let mut unchecked = Vec::new();
    match &request.change {
        DmlChange::Insert { values } => {
            check_unique(values.iter().map(|v| v.column.name.as_str()))?;
            if values.is_empty() {
                let text = dialect.dml_insert_default_values(&table);
                t.both(&text);
            } else {
                t.both(&format!("INSERT INTO {table} ("));
                for (i, v) in values.iter().enumerate() {
                    if i > 0 {
                        t.both(", ");
                    }
                    t.ident(&v.column.name)?;
                }
                t.both(") VALUES (");
                for (i, v) in values.iter().enumerate() {
                    if i > 0 {
                        t.both(", ");
                    }
                    t.value(&v.column, &v.value)?;
                }
                t.both(")");
            }
        }
        DmlChange::Update { key, changes } => {
            check_key(dialect, request.key_kind, key)?;
            if changes.is_empty() {
                return Err(DmlError::NoChanges);
            }
            check_unique(changes.iter().map(|c| c.column.name.as_str()))?;
            t.both(&format!("UPDATE {table} SET "));
            for (i, c) in changes.iter().enumerate() {
                if i > 0 {
                    t.both(", ");
                }
                t.ident(&c.column.name)?;
                t.both(" = ");
                t.value(&c.column, &c.new)?;
            }
            t.both(" WHERE ");
            where_key(&mut t, key)?;
            if request.concurrency == Concurrency::EditedColumns {
                let guard = changes
                    .iter()
                    .map(|c| (&c.column, &c.original))
                    .collect::<Vec<_>>();
                where_guard(&mut t, key, &guard, &mut unchecked)?;
            }
        }
        DmlChange::Delete { key, check } => {
            check_key(dialect, request.key_kind, key)?;
            t.both(&format!("DELETE FROM {table} WHERE "));
            where_key(&mut t, key)?;
            if request.concurrency == Concurrency::EditedColumns {
                check_unique(check.iter().map(|c| c.column.name.as_str()))?;
                let guard = check
                    .iter()
                    .map(|c| (&c.column, &c.value))
                    .collect::<Vec<_>>();
                where_guard(&mut t, key, &guard, &mut unchecked)?;
            }
        }
    }
    let Texts {
        sql,
        preview,
        params,
        ..
    } = t;
    self_check(dialect, &sql, &preview)?;
    Ok(DmlStatement {
        kind,
        statement: Statement::sql(sql).with_params(params),
        preview,
        expected_rows: 1,
        unchecked_columns: unchecked,
        lexical_rules: dialect.lexical_rules(),
    })
}

fn check_key<D: Dialect + ?Sized>(
    dialect: &D,
    kind: KeyKind,
    key: &[ColumnValue],
) -> Result<(), DmlError> {
    if key.is_empty() {
        return Err(DmlError::NoKey);
    }
    check_unique(key.iter().map(|k| k.column.name.as_str()))?;
    for k in key {
        if !dialect.dml_comparable(&k.column.ty) {
            return Err(DmlError::KeyNotComparable(echo_name(&k.column.name)));
        }
        if k.value == Param::Null {
            let null_ok = matches!(
                kind,
                KeyKind::Unique {
                    nulls_distinct: false
                }
            );
            if !null_ok {
                return Err(DmlError::NullKey(echo_name(&k.column.name)));
            }
        }
    }
    Ok(())
}

fn where_key<D: Dialect + ?Sized>(
    t: &mut Texts<'_, D>,
    key: &[ColumnValue],
) -> Result<(), DmlError> {
    for (i, k) in key.iter().enumerate() {
        if i > 0 {
            t.both(" AND ");
        }
        compare(t, &k.column, &k.value)?;
    }
    Ok(())
}

fn where_guard<D: Dialect + ?Sized>(
    t: &mut Texts<'_, D>,
    key: &[ColumnValue],
    guard: &[(&DmlColumn, &Param)],
    unchecked: &mut Vec<String>,
) -> Result<(), DmlError> {
    for (column, original) in guard {
        if key.iter().any(|k| k.column.name == column.name) {
            continue;
        }
        if !t.dialect.dml_comparable(&column.ty) {
            unchecked.push(column.name.clone());
            continue;
        }
        t.both(" AND ");
        compare(t, column, original)?;
    }
    Ok(())
}

fn compare<D: Dialect + ?Sized>(
    t: &mut Texts<'_, D>,
    column: &DmlColumn,
    value: &Param,
) -> Result<(), DmlError> {
    t.ident(&column.name)?;
    if *value == Param::Null {
        t.both(" IS NULL");
        Ok(())
    } else {
        t.both(" = ");
        t.value(column, value)
    }
}

fn self_check<D: Dialect + ?Sized>(dialect: &D, sql: &str, preview: &str) -> Result<(), DmlError> {
    if preview.contains('\0') || sql.contains('\0') {
        return Err(DmlError::SelfCheck("the statement holds a NUL character"));
    }
    for text in [sql, preview] {
        if dialect.split_statements(text).len() != 1 {
            return Err(DmlError::SelfCheck(
                "the statement does not split as one statement",
            ));
        }
        if dialect.classify(text) != StatementClass::Write {
            return Err(DmlError::SelfCheck(
                "the statement does not classify as a filtered write",
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Shared literal helpers for dialects

/// `true` for characters a preview literal never shows raw: control characters (C0, DEL, C1),
/// line and paragraph separators, bidirectional controls and invisible formatting characters
/// (zero-width spaces and joiners, word joiner, invisible operators, BOM, soft hyphen, Mongolian
/// vowel separator, interlinear annotation marks). The SQL editor strips or flags some of them
/// (ADR-0025 §3) and others would hide text, so dialects escape them in strings.
#[must_use]
pub fn needs_escape(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
        )
}

/// `true` when `name` holds a character [`needs_escape`] would escape in a string. A preview
/// with such a name shows it raw inside the quoted identifier; the UI marks it.
#[must_use]
pub fn name_has_hidden_characters(name: &str) -> bool {
    name.chars().any(needs_escape)
}

/// The basic identifier check every dialect applies: not empty, no NUL.
///
/// # Errors
/// [`DmlError::InvalidIdentifier`].
pub fn check_identifier_basic(name: &str) -> Result<(), DmlError> {
    if name.is_empty() {
        return Err(DmlError::invalid_identifier(name, "empty name"));
    }
    if name.contains('\0') {
        return Err(DmlError::invalid_identifier(
            name,
            "the name holds a NUL character",
        ));
    }
    Ok(())
}

/// Years a date or timestamp literal may have: 1 to 9999 on every engine.
const YEARS: std::ops::RangeInclusive<i64> = 1..=9999;

/// `YYYY-MM-DD` for days since 1970-01-01, for years 1 to 9999.
///
/// # Errors
/// A reason when the year is out of range.
pub fn date_text(days: i32) -> Result<String, &'static str> {
    let (y, m, d) = civil_from_days(i64::from(days));
    if !YEARS.contains(&y) {
        return Err("the date is outside the years 1 to 9999");
    }
    Ok(format!("{y:04}-{m:02}-{d:02}"))
}

/// `YYYY-MM-DD HH:MM:SS.ffffff` for microseconds since 1970-01-01, for years 1 to 9999.
///
/// # Errors
/// A reason when the year is out of range.
pub fn timestamp_text(micros: i64) -> Result<String, &'static str> {
    let secs = micros.div_euclid(1_000_000);
    let frac = micros.rem_euclid(1_000_000);
    let days = i32::try_from(secs.div_euclid(86_400))
        .map_err(|_| "the timestamp is outside the years 1 to 9999")?;
    let date = date_text(days).map_err(|_| "the timestamp is outside the years 1 to 9999")?;
    let sod = secs.rem_euclid(86_400);
    Ok(format!(
        "{date} {:02}:{:02}:{:02}.{frac:06}",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    ))
}

/// The shortest decimal text that reads back as `f` (Rust's `{:?}`: `0.1`, `1e300`, `-0.0`).
/// Finite values only.
#[must_use]
pub fn float_text(f: f64) -> String {
    format!("{f:?}")
}

/// `true` for a plain decimal: `^[+-]?\d+(\.\d+)?([eE][+-]?\d+)?$`.
#[must_use]
pub fn is_plain_decimal(s: &str) -> bool {
    is_strict_decimal(s)
}

/// Lower-case hexadecimal digits of `bytes`.
#[must_use]
pub fn hex_lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    s
}

/// The default ([`Dialect::dml_literal`]) typed literal: ANSI forms. Strings holding a
/// character [`needs_escape`] escapes use `U&'…'` (refused when backslashes are escapes).
///
/// # Errors
/// A reason when the value has no faithful ANSI literal.
pub fn ansi_dml_literal(rules: LexicalRules, value: &Param) -> Result<String, &'static str> {
    Ok(match value {
        Param::Null => "NULL".to_owned(),
        Param::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_owned(),
        Param::Int(i) => i.to_string(),
        Param::Float(f) if f.is_finite() => float_text(*f),
        Param::Float(_) => return Err("no literal for NaN or infinite values"),
        Param::Decimal(d) if is_strict_decimal(d) => d.clone(),
        Param::Decimal(_) => return Err("not a plain decimal number"),
        Param::Text(s) | Param::Json(s) => ansi_string(rules, s)?,
        Param::Bytes(b) => format!("X'{}'", hex_lower(b)),
        Param::Date(d) => format!("DATE '{}'", date_text(*d)?),
        Param::Timestamp { micros, utc } => {
            let tz = if *utc { "+00:00" } else { "" };
            format!("TIMESTAMP '{}{tz}'", timestamp_text(*micros)?)
        }
    })
}

fn ansi_string(rules: LexicalRules, s: &str) -> Result<String, &'static str> {
    if s.contains('\0') {
        return Err("the text holds a NUL character");
    }
    let special = s.chars().any(needs_escape);
    if special && rules.backslash_escapes {
        return Err("the text holds control characters");
    }
    let mut out = String::with_capacity(s.len() + 4);
    out.push_str(if special { "U&'" } else { "'" });
    for c in s.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' if special => out.push_str("\\\\"),
            '\\' if rules.backslash_escapes => out.push_str("\\\\"),
            c if special && needs_escape(c) => {
                let cp = u32::from(c);
                if cp > 0xFFFF {
                    out.push_str(&format!("\\+{cp:06X}"));
                } else {
                    out.push_str(&format!("\\{cp:04X}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('\'');
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Arrow cells

/// Reads row `row` of `array` (a column of a result batch whose field is `field`) as a
/// [`Param`] for [`generate`] or [`insert_preview`].
///
/// Mapping: integers → `Int` (`UInt64` above `i64::MAX` → `Decimal`), floats → `Float`,
/// `Decimal128` / `Decimal256` → `Decimal`, `Utf8` → `Text` (so `numeric`, `uuid`, `interval`,
/// arrays, MySQL `TIME` and `DECIMAL` travel as their text, typed by the column), binary →
/// `Bytes`, `Date32` / `Date64` → `Date`, `Timestamp` → `Timestamp` (UTC when the field has a
/// time zone), `Time32` / `Time64` → `Text` (`HH:MM:SS[.ffffff]`), nulls → `Null`.
///
/// # Errors
/// [`DmlError::TruncatedValue`] when the field carries [`TRUNCATED_VALUES_METADATA_KEY`]
/// (a cut value must never be written back); [`DmlError::UnsupportedType`] for other Arrow
/// types, a row out of range, or a value that loses precision in microseconds.
pub fn cell_value(field: &Field, array: &dyn Array, row: usize) -> Result<Param, DmlError> {
    let name = field.name();
    if field
        .metadata()
        .get(TRUNCATED_VALUES_METADATA_KEY)
        .is_some_and(|v| v == "true")
    {
        return Err(DmlError::TruncatedValue(echo_name(name)));
    }
    let unsupported = || DmlError::UnsupportedType(echo_name(name));
    if row >= array.len() {
        return Err(unsupported());
    }
    if array.is_null(row) {
        return Ok(Param::Null);
    }
    let micros = |v: i64, unit: TimeUnit| -> Option<i64> {
        match unit {
            TimeUnit::Second => v.checked_mul(1_000_000),
            TimeUnit::Millisecond => v.checked_mul(1_000),
            TimeUnit::Microsecond => Some(v),
            TimeUnit::Nanosecond => (v % 1_000 == 0).then_some(v / 1_000),
        }
    };
    Ok(match array.data_type() {
        DataType::Null => Param::Null,
        DataType::Boolean => Param::Bool(array.as_boolean().value(row)),
        DataType::Int8 => Param::Int(i64::from(array.as_primitive::<Int8Type>().value(row))),
        DataType::Int16 => Param::Int(i64::from(array.as_primitive::<Int16Type>().value(row))),
        DataType::Int32 => Param::Int(i64::from(array.as_primitive::<Int32Type>().value(row))),
        DataType::Int64 => Param::Int(array.as_primitive::<Int64Type>().value(row)),
        DataType::UInt8 => Param::Int(i64::from(array.as_primitive::<UInt8Type>().value(row))),
        DataType::UInt16 => Param::Int(i64::from(array.as_primitive::<UInt16Type>().value(row))),
        DataType::UInt32 => Param::Int(i64::from(array.as_primitive::<UInt32Type>().value(row))),
        DataType::UInt64 => {
            let v = array.as_primitive::<UInt64Type>().value(row);
            i64::try_from(v).map_or_else(|_| Param::Decimal(v.to_string()), Param::Int)
        }
        DataType::Float32 => {
            Param::Float(f64::from(array.as_primitive::<Float32Type>().value(row)))
        }
        DataType::Float64 => Param::Float(array.as_primitive::<Float64Type>().value(row)),
        DataType::Decimal128(_, _) => {
            Param::Decimal(array.as_primitive::<Decimal128Type>().value_as_string(row))
        }
        DataType::Decimal256(_, _) => {
            Param::Decimal(array.as_primitive::<Decimal256Type>().value_as_string(row))
        }
        DataType::Utf8 => Param::Text(array.as_string::<i32>().value(row).to_owned()),
        DataType::LargeUtf8 => Param::Text(array.as_string::<i64>().value(row).to_owned()),
        DataType::Utf8View => Param::Text(array.as_string_view().value(row).to_owned()),
        DataType::Binary => Param::Bytes(array.as_binary::<i32>().value(row).to_vec()),
        DataType::LargeBinary => Param::Bytes(array.as_binary::<i64>().value(row).to_vec()),
        DataType::BinaryView => Param::Bytes(array.as_binary_view().value(row).to_vec()),
        DataType::FixedSizeBinary(_) => {
            Param::Bytes(array.as_fixed_size_binary().value(row).to_vec())
        }
        DataType::Date32 => Param::Date(array.as_primitive::<Date32Type>().value(row)),
        DataType::Date64 => {
            let ms = array.as_primitive::<Date64Type>().value(row);
            if ms % 86_400_000 != 0 {
                return Err(unsupported());
            }
            Param::Date(i32::try_from(ms / 86_400_000).map_err(|_| unsupported())?)
        }
        DataType::Timestamp(unit, tz) => {
            let v = match unit {
                TimeUnit::Second => array.as_primitive::<TimestampSecondType>().value(row),
                TimeUnit::Millisecond => {
                    array.as_primitive::<TimestampMillisecondType>().value(row)
                }
                TimeUnit::Microsecond => {
                    array.as_primitive::<TimestampMicrosecondType>().value(row)
                }
                TimeUnit::Nanosecond => array.as_primitive::<TimestampNanosecondType>().value(row),
            };
            Param::Timestamp {
                micros: micros(v, *unit).ok_or_else(unsupported)?,
                utc: tz.is_some(),
            }
        }
        DataType::Time32(unit) => {
            let v = match unit {
                TimeUnit::Second => array.as_primitive::<Time32SecondType>().value(row),
                TimeUnit::Millisecond => array.as_primitive::<Time32MillisecondType>().value(row),
                _ => return Err(unsupported()),
            };
            Param::Text(
                time_text(micros(i64::from(v), *unit).ok_or_else(unsupported)?)
                    .ok_or_else(unsupported)?,
            )
        }
        DataType::Time64(unit) => {
            let v = match unit {
                TimeUnit::Microsecond => array.as_primitive::<Time64MicrosecondType>().value(row),
                TimeUnit::Nanosecond => array.as_primitive::<Time64NanosecondType>().value(row),
                _ => return Err(unsupported()),
            };
            Param::Text(
                time_text(micros(v, *unit).ok_or_else(unsupported)?).ok_or_else(unsupported)?,
            )
        }
        _ => return Err(unsupported()),
    })
}

/// `HH:MM:SS` or `HH:MM:SS.ffffff` for a time of day in microseconds (`24:00:00` allowed).
fn time_text(micros: i64) -> Option<String> {
    if !(0..=86_400_000_000).contains(&micros) {
        return None;
    }
    let secs = micros / 1_000_000;
    let frac = micros % 1_000_000;
    let hms = format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    );
    Some(if frac == 0 {
        hms
    } else {
        format!("{hms}.{frac:06}")
    })
}

// ---------------------------------------------------------------------------------------------
// Row key choice

/// A unique key candidate of a table, from its metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct KeyCandidate {
    /// Key columns, unquoted, in key order.
    pub columns: Vec<String>,
    /// The primary key.
    pub primary: bool,
    /// Every column is `NOT NULL`.
    pub not_null: bool,
    /// `NULL`s are distinct (`false` only for PostgreSQL `NULLS NOT DISTINCT`).
    pub nulls_distinct: bool,
    /// A partial index or an expression key: never identifies a row.
    pub partial_or_expression: bool,
}

impl KeyCandidate {
    /// A candidate over `columns`; set the flags with struct update syntax or the fields.
    #[must_use]
    pub fn new(columns: Vec<String>) -> Self {
        Self {
            columns,
            primary: false,
            not_null: false,
            nulls_distinct: true,
            partial_or_expression: false,
        }
    }
}

/// The key a table's rows are edited by: the primary key, else the unique key with non-null
/// columns that has the fewest columns, else the unique key with nullable columns that has the
/// fewest columns (whose rows with a `NULL` key value then cannot be edited, see
/// [`KeyKind::Unique`]). `None` (the grid stays read-only, [`DmlError::NoKey`]) when there is
/// none; partial and expression keys never count.
#[must_use]
pub fn choose_row_key(candidates: &[KeyCandidate]) -> Option<(Vec<String>, KeyKind)> {
    let usable = candidates
        .iter()
        .filter(|c| !c.partial_or_expression && !c.columns.is_empty());
    if let Some(pk) = usable.clone().find(|c| c.primary) {
        return Some((pk.columns.clone(), KeyKind::PrimaryKey));
    }
    let pick = |not_null: bool| {
        usable
            .clone()
            .filter(|c| c.not_null == not_null)
            .min_by_key(|c| c.columns.len())
    };
    pick(true).or_else(|| pick(false)).map(|c| {
        (
            c.columns.clone(),
            KeyKind::Unique {
                nulls_distinct: c.nulls_distinct,
            },
        )
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::{
        BinaryArray, Decimal128Array, StringArray, Time64MicrosecondArray,
        TimestampNanosecondArray, UInt64Array,
    };
    use proptest::prelude::*;

    use super::*;
    use crate::dialect::AnsiDialect;

    fn all() -> EditingCapabilities {
        EditingCapabilities {
            insert: true,
            update: true,
            delete: true,
            requires_unique_key: true,
        }
    }

    fn col(name: &str, dt: DataType) -> DmlColumn {
        DmlColumn::new(name, ColumnType::new(dt))
    }

    fn cv(name: &str, v: Param) -> ColumnValue {
        ColumnValue::new(col(name, DataType::Int64), v)
    }

    #[test]
    fn update_with_key_and_guard() {
        let req = DmlRequest::update(
            vec!["app".into(), "users".into()],
            KeyKind::PrimaryKey,
            vec![cv("id", Param::Int(7)), cv("tenant", Param::Int(2))],
            vec![
                ColumnChange::new(
                    col("name", DataType::Utf8),
                    Param::Null,
                    Param::Text("it's".into()),
                ),
                ColumnChange::new(col("n", DataType::Int64), Param::Int(1), Param::Int(2)),
            ],
        )
        .with_concurrency(Concurrency::EditedColumns);
        let s = generate(&AnsiDialect, &all(), &req).unwrap();
        assert_eq!(
            s.statement.text,
            "UPDATE \"app\".\"users\" SET \"name\" = ?, \"n\" = ? WHERE \"id\" = ? AND \"tenant\" = ? AND \"name\" IS NULL AND \"n\" = ?"
        );
        assert_eq!(
            s.preview,
            "UPDATE \"app\".\"users\" SET \"name\" = 'it''s', \"n\" = 2 WHERE \"id\" = 7 AND \"tenant\" = 2 AND \"name\" IS NULL AND \"n\" = 1"
        );
        assert_eq!(
            s.statement.params,
            vec![
                Param::Text("it's".into()),
                Param::Int(2),
                Param::Int(7),
                Param::Int(2),
                Param::Int(1)
            ]
        );
        assert_eq!(s.expected_rows, 1);
        assert!(s.check_affected(Some(1)).is_ok());
        assert!(matches!(
            s.check_affected(Some(0)),
            Err(DmlError::RowCount {
                expected: 1,
                actual: Some(0)
            })
        ));
        assert!(s.check_affected(None).is_err());
        assert!(!format!("{s:?}").contains("it's"));
    }

    #[test]
    fn refusals() {
        let d = AnsiDialect;
        let del = |key: Vec<ColumnValue>, kind| DmlRequest::delete(vec!["t".into()], kind, key);
        assert_eq!(
            generate(&d, &all(), &del(vec![], KeyKind::PrimaryKey)),
            Err(DmlError::NoKey)
        );
        assert!(matches!(
            generate(
                &d,
                &all(),
                &del(vec![cv("id", Param::Null)], KeyKind::PrimaryKey)
            ),
            Err(DmlError::NullKey(_))
        ));
        assert!(matches!(
            generate(
                &d,
                &all(),
                &del(
                    vec![cv("u", Param::Null)],
                    KeyKind::Unique {
                        nulls_distinct: true
                    }
                )
            ),
            Err(DmlError::NullKey(_))
        ));
        let s = generate(
            &d,
            &all(),
            &del(
                vec![cv("u", Param::Null)],
                KeyKind::Unique {
                    nulls_distinct: false,
                },
            ),
        )
        .unwrap();
        assert_eq!(s.statement.text, "DELETE FROM \"t\" WHERE \"u\" IS NULL");
        assert!(s.statement.params.is_empty());
        assert!(matches!(
            generate(
                &d,
                &all(),
                &del(
                    vec![cv("a", Param::Int(1)), cv("a", Param::Int(2))],
                    KeyKind::PrimaryKey
                )
            ),
            Err(DmlError::DuplicateColumn(_))
        ));
        assert_eq!(
            generate(
                &d,
                &all(),
                &DmlRequest::update(
                    vec!["t".into()],
                    KeyKind::PrimaryKey,
                    vec![cv("id", Param::Int(1))],
                    vec![]
                )
            ),
            Err(DmlError::NoChanges)
        );
        assert_eq!(
            generate(
                &d,
                &EditingCapabilities::default(),
                &DmlRequest::insert(vec!["t".into()], vec![])
            ),
            Err(DmlError::NotSupported(DmlKind::Insert))
        );
        assert_eq!(
            generate(&d, &all(), &DmlRequest::insert(vec![], vec![])),
            Err(DmlError::NoTable)
        );
        assert!(matches!(
            generate(&d, &all(), &DmlRequest::insert(vec!["a\0".into()], vec![])),
            Err(DmlError::InvalidIdentifier { .. })
        ));
        assert!(matches!(
            generate(
                &d,
                &all(),
                &DmlRequest::insert(vec!["t".into()], vec![cv("f", Param::Float(f64::NAN))])
            ),
            Err(DmlError::UnsupportedValue { .. })
        ));
        let err: DriverError = DmlError::NotSupported(DmlKind::Delete).into();
        assert_eq!(err, DriverError::Unsupported(Feature::Editing));
    }

    #[test]
    fn insert_default_values_and_batches() {
        let d = AnsiDialect;
        let s = generate(&d, &all(), &DmlRequest::insert(vec!["t".into()], vec![])).unwrap();
        assert_eq!(s.statement.text, "INSERT INTO \"t\" DEFAULT VALUES");
        let reqs: Vec<DmlRequest> = (0..3)
            .map(|i| {
                DmlRequest::insert(
                    vec!["t".into()],
                    vec![cv("a", Param::Text(format!("x;{i}")))],
                )
            })
            .collect();
        let b = generate_batch(&d, &all(), &reqs).unwrap();
        assert_eq!(b.statements.len(), 3);
        assert_eq!(d.split_statements(&b.preview_script()).len(), 3);
        assert_eq!(
            insert_preview(&d, &["t".into()], &[cv("a", Param::Int(1))]).unwrap(),
            "INSERT INTO \"t\" (\"a\") VALUES (1)"
        );
    }

    #[test]
    fn ansi_strings_escape_hidden_characters() {
        let r = LexicalRules::ANSI;
        assert_eq!(
            ansi_dml_literal(r, &Param::Text("a\n\u{202e}\\".into())).unwrap(),
            "U&'a\\000A\\202E\\\\'"
        );
        assert!(
            ansi_dml_literal(r.with_backslash_escapes(true), &Param::Text("\n".into())).is_err()
        );
        assert!(ansi_dml_literal(r, &Param::Date(-800_000)).is_err());
        assert_eq!(echo_name("a\"\n"), "\"a\\\"\\n\"");
    }

    #[test]
    fn cells() {
        let f = Field::new("a", DataType::Utf8, true);
        let a = StringArray::from(vec![Some("x"), None]);
        assert_eq!(cell_value(&f, &a, 0).unwrap(), Param::Text("x".into()));
        assert_eq!(cell_value(&f, &a, 1).unwrap(), Param::Null);
        assert!(cell_value(&f, &a, 2).is_err());
        let cut = f
            .clone()
            .with_metadata([(TRUNCATED_VALUES_METADATA_KEY.to_owned(), "true".to_owned())]);
        assert!(matches!(
            cell_value(&cut, &a, 0),
            Err(DmlError::TruncatedValue(_))
        ));
        let u = UInt64Array::from(vec![u64::MAX]);
        assert_eq!(
            cell_value(&Field::new("u", DataType::UInt64, true), &u, 0).unwrap(),
            Param::Decimal(u64::MAX.to_string())
        );
        let d = Decimal128Array::from(vec![-1250_i128])
            .with_precision_and_scale(10, 2)
            .unwrap();
        assert_eq!(
            cell_value(&Field::new("d", d.data_type().clone(), true), &d, 0).unwrap(),
            Param::Decimal("-12.50".into())
        );
        let t = Time64MicrosecondArray::from(vec![3_723_000_001]);
        assert_eq!(
            cell_value(&Field::new("t", t.data_type().clone(), true), &t, 0).unwrap(),
            Param::Text("01:02:03.000001".into())
        );
        let ns = TimestampNanosecondArray::from(vec![1_500, 2_000]).with_timezone(Arc::from("UTC"));
        let nf = Field::new("n", ns.data_type().clone(), true);
        assert!(cell_value(&nf, &ns, 0).is_err());
        assert_eq!(
            cell_value(&nf, &ns, 1).unwrap(),
            Param::Timestamp {
                micros: 2,
                utc: true
            }
        );
        let b = BinaryArray::from(vec![&b"\x00\xff"[..]]);
        assert_eq!(
            cell_value(&Field::new("b", DataType::Binary, true), &b, 0).unwrap(),
            Param::Bytes(vec![0, 0xff])
        );
    }

    #[test]
    fn row_key_choice() {
        let mut pk = KeyCandidate::new(vec!["id".into()]);
        pk.primary = true;
        let mut u2 = KeyCandidate::new(vec!["a".into(), "b".into()]);
        u2.not_null = true;
        let mut u1 = KeyCandidate::new(vec!["c".into()]);
        u1.not_null = true;
        let nullable = KeyCandidate::new(vec!["n".into()]);
        let mut partial = KeyCandidate::new(vec!["p".into()]);
        partial.not_null = true;
        partial.partial_or_expression = true;
        assert_eq!(
            choose_row_key(&[u1.clone(), pk]),
            Some((vec!["id".into()], KeyKind::PrimaryKey))
        );
        assert_eq!(
            choose_row_key(&[u2, u1.clone(), nullable.clone()]).map(|k| k.0),
            Some(vec!["c".into()])
        );
        assert_eq!(
            choose_row_key(&[partial.clone(), nullable]),
            Some((
                vec!["n".into()],
                KeyKind::Unique {
                    nulls_distinct: true
                }
            ))
        );
        assert_eq!(choose_row_key(&[partial]), None);
        assert_eq!(choose_row_key(&[]), None);
    }

    fn hostile_text() -> impl Strategy<Value = String> {
        let pieces = prop::sample::select(vec![
            "'",
            "''",
            "\"",
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
            ";",
            "\n",
            "\r",
            "\t",
            "\u{0}",
            "\u{1b}",
            "\u{202e}",
            "\u{200b}",
            "\u{feff}",
            "é",
            "😀",
            "?",
            "$1",
            ":a",
            "@a",
            "WHERE",
            "DROP TABLE t",
            "x",
        ]);
        prop::collection::vec(pieces, 0..12).prop_map(|v| v.concat())
    }

    proptest! {
        /// Whatever the names and values, the generator either refuses with a typed error or
        /// returns texts that pass the self-check: never a self-check failure (a bug), never a
        /// panic.
        #[test]
        fn hostile_names_and_values_never_fail_the_self_check(
            table in hostile_text(),
            column in hostile_text(),
            value in hostile_text(),
            backslash in any::<bool>(),
        ) {
            struct D(LexicalRules);
            impl Dialect for D {
                fn lexical_rules(&self) -> LexicalRules { self.0 }
            }
            let d = D(LexicalRules::ANSI.with_backslash_escapes(backslash));
            let c = DmlColumn::new(column.clone(), ColumnType::new(DataType::Utf8));
            let key = vec![ColumnValue::new(col("id", DataType::Int64), Param::Int(1))];
            for req in [
                DmlRequest::insert(vec![table.clone()], vec![ColumnValue::new(c.clone(), Param::Text(value.clone()))]),
                DmlRequest::update(vec![table.clone()], KeyKind::PrimaryKey, key.clone(),
                    vec![ColumnChange::new(c.clone(), Param::Text(value.clone()), Param::Text(value.clone()))])
                    .with_concurrency(Concurrency::EditedColumns),
                DmlRequest::delete(vec![table.clone()], KeyKind::PrimaryKey, key.clone()),
            ] {
                match generate(&d, &all(), &req) {
                    Ok(s) => {
                        prop_assert_eq!(d.classify(&s.preview), StatementClass::Write);
                        prop_assert_eq!(d.split_statements(&s.preview).len(), 1);
                    }
                    Err(DmlError::SelfCheck(why)) => prop_assert!(false, "self-check: {}", why),
                    Err(_) => {}
                }
            }
        }
    }
}
