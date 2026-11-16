//! Lazy, paged metadata for the navigator (ADR-0004).

use crate::capabilities::ObjectKind;
use crate::error::DriverResult;

/// One step of an [`ObjectPath`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PathSegment {
    /// Kind of the object at this step.
    pub kind: ObjectKind,
    /// Unquoted name as stored by the engine.
    pub name: String,
}

/// Path of an object from the root, e.g. `[Catalog app, Schema public, Table users]`.
/// The empty path is the server root.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectPath(pub Vec<PathSegment>);

impl ObjectPath {
    /// The server root.
    #[must_use]
    pub fn root() -> Self {
        Self::default()
    }

    /// This path extended by one segment.
    #[must_use]
    pub fn child(&self, kind: ObjectKind, name: impl Into<String>) -> Self {
        let mut v = self.0.clone();
        v.push(PathSegment {
            kind,
            name: name.into(),
        });
        Self(v)
    }

    /// Last segment, `None` for the root.
    #[must_use]
    pub fn leaf(&self) -> Option<&PathSegment> {
        self.0.last()
    }

    /// Kind of the last segment, `None` for the root.
    #[must_use]
    pub fn kind(&self) -> Option<ObjectKind> {
        self.leaf().map(|s| s.kind)
    }
}

/// Request for one page of a listing. Build with [`PageRequest::first`] and the `with_*`
/// methods.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PageRequest {
    /// Opaque cursor from the previous [`Page::next_cursor`]; `None` for the first page.
    pub cursor: Option<String>,
    /// Maximum items in the page.
    pub limit: u32,
    /// Optional name filter: a literal substring for SQL engines, a `MATCH` pattern for
    /// key-value stores. Drivers apply it server-side where possible.
    ///
    /// The filter is **user input** (L5): drivers pass it as a bind parameter (or a separate
    /// protocol argument, such as the `MATCH` argument of `SCAN`), never spliced into statement
    /// text. For SQL engines it is a literal: wildcards are escaped with [`escape_like`] and the
    /// query uses `LIKE ? ESCAPE '!'` (or the engine's equivalent), so `%` and `_` in a name
    /// match only themselves. `!` is the recommended escape character: a backslash would itself
    /// need escaping in the statement text under MySQL's default `sql_mode` and PostgreSQL with
    /// `standard_conforming_strings = off`.
    pub filter: Option<String>,
}

impl PageRequest {
    /// Default page size of the navigator.
    pub const DEFAULT_LIMIT: u32 = 1000;

    /// First page with the default size and no filter.
    #[must_use]
    pub fn first() -> Self {
        Self {
            cursor: None,
            limit: Self::DEFAULT_LIMIT,
            filter: None,
        }
    }

    /// Continues after `cursor`.
    #[must_use]
    pub fn with_cursor(mut self, cursor: Option<String>) -> Self {
        self.cursor = cursor;
        self
    }

    /// Sets the page size.
    #[must_use]
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = limit;
        self
    }

    /// Sets the name filter.
    #[must_use]
    pub fn with_filter(mut self, filter: Option<String>) -> Self {
        self.filter = filter;
        self
    }
}

impl Default for PageRequest {
    fn default() -> Self {
        Self::first()
    }
}

/// Escapes `%`, `_` and `escape` itself in `filter` so a `LIKE` / `ILIKE` pattern built from
/// it matches the text literally, and wraps it in `%…%` for a substring match. Bind the result
/// and declare the same escape character (`LIKE ? ESCAPE '!'`; `!` is recommended, see
/// [`PageRequest::filter`]). `escape` must not be `%` or `_`.
///
/// ```
/// # use ninesql_driver_api::metadata::escape_like;
/// assert_eq!(escape_like("a_b%c!d", '!'), "%a!_b!%c!!d%");
/// ```
#[must_use]
pub fn escape_like(filter: &str, escape: char) -> String {
    debug_assert!(
        escape != '%' && escape != '_',
        "the LIKE escape character cannot be a wildcard"
    );
    let mut out = String::with_capacity(filter.len() + 4);
    out.push('%');
    for c in filter.chars() {
        if c == '%' || c == '_' || c == escape {
            out.push(escape);
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// One page of results.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page<T> {
    /// Items of this page.
    pub items: Vec<T>,
    /// Cursor for the next page; `None` when this is the last one.
    pub next_cursor: Option<String>,
}

/// An object listed by [`Metadata::list`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectInfo {
    /// Full path, including the object itself as the last segment.
    pub path: ObjectPath,
    /// Engine comment / description.
    pub comment: Option<String>,
    /// Kind-specific details.
    pub details: ObjectDetails,
}

impl ObjectInfo {
    /// Name of the object (last path segment).
    #[must_use]
    pub fn name(&self) -> &str {
        self.path.leaf().map_or("", |s| s.name.as_str())
    }
}

/// Kind-specific details of an object.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ObjectDetails {
    /// Nothing beyond the name.
    #[default]
    None,
    /// A column.
    Column(ColumnInfo),
    /// A table-like object.
    Table(TableInfo),
    /// A primary-key, unique, check or exclusion constraint.
    Constraint(ConstraintInfo),
    /// A foreign key.
    ForeignKey(ForeignKeyInfo),
    /// An index.
    Index(IndexInfo),
    /// A trigger.
    Trigger(TriggerInfo),
    /// A function or procedure.
    Routine(RoutineInfo),
    /// A sequence.
    Sequence(SequenceInfo),
    /// A user-defined type or domain.
    Type(TypeInfo),
    /// A partition of a table.
    Partition(PartitionInfo),
}

/// Column details. Build with [`ColumnInfo::new`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ColumnInfo {
    /// 1-based position.
    pub ordinal: u32,
    /// Engine type name as the engine spells it (`character varying(20)`).
    pub data_type: String,
    /// Accepts `NULL`.
    pub nullable: bool,
    /// Default expression text.
    pub default: Option<String>,
    /// Part of the primary key.
    pub primary_key: bool,
    /// Generated by the engine (identity, auto-increment, serial).
    pub auto_generated: bool,
}

impl ColumnInfo {
    /// A nullable, non-key column without default.
    #[must_use]
    pub fn new(ordinal: u32, data_type: impl Into<String>) -> Self {
        Self {
            ordinal,
            data_type: data_type.into(),
            nullable: true,
            default: None,
            primary_key: false,
            auto_generated: false,
        }
    }
}

/// Table details. Build with `TableInfo::default()` and set fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TableInfo {
    /// Row estimate from statistics, if cheap to get.
    pub estimated_rows: Option<u64>,
    /// Engine-specific flavour (`hypertable`, `partitioned`, `temporary`, `system`…).
    pub flavour: Option<String>,
    /// Partition key of a partitioned table, as the engine spells it (`RANGE (ordered_at)`).
    pub partition_key: Option<String>,
    /// Changes to the table are transactional: `Some(false)` for a MySQL-family table whose
    /// storage engine has no transactions (MyISAM, MEMORY, ARCHIVE, CSV…), where a rolled-back
    /// edit stays written; `Some(true)` for InnoDB and other transactional engines; `None` when
    /// the driver does not know (P5-B review M2; PostgreSQL and SQLite tables are always
    /// transactional and report `Some(true)`).
    pub transactional: Option<bool>,
}

/// Kind of a [`ConstraintInfo`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ConstraintKind {
    /// Primary key.
    PrimaryKey,
    /// Unique constraint.
    Unique,
    /// Check constraint.
    Check,
    /// Exclusion constraint (PostgreSQL).
    Exclusion,
}

/// Details of an [`ObjectKind::Constraint`] (foreign keys are [`ForeignKeyInfo`]). Build with
/// [`ConstraintInfo::new`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConstraintInfo {
    /// Kind of constraint.
    pub kind: ConstraintKind,
    /// Constrained columns, unquoted, in key order (empty for a check constraint the engine
    /// does not attach to columns).
    pub columns: Vec<String>,
    /// Definition as the engine spells it (the check expression, `PRIMARY KEY (id)`…), when
    /// the engine provides it.
    pub definition: Option<String>,
}

impl ConstraintInfo {
    /// A constraint of `kind` with no columns and no definition.
    #[must_use]
    pub fn new(kind: ConstraintKind) -> Self {
        Self {
            kind,
            columns: Vec::new(),
            definition: None,
        }
    }
}

/// Action of a foreign key on update or delete of the referenced row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReferentialAction {
    /// `NO ACTION`.
    NoAction,
    /// `RESTRICT`.
    Restrict,
    /// `CASCADE`.
    Cascade,
    /// `SET NULL`.
    SetNull,
    /// `SET DEFAULT`.
    SetDefault,
}

impl ReferentialAction {
    /// Parses the SQL spelling (`NO ACTION`, `restrict`, `SET NULL`…), case-insensitively.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let eq = |w: &str| s.eq_ignore_ascii_case(w);
        if eq("NO ACTION") {
            Some(Self::NoAction)
        } else if eq("RESTRICT") {
            Some(Self::Restrict)
        } else if eq("CASCADE") {
            Some(Self::Cascade)
        } else if eq("SET NULL") {
            Some(Self::SetNull)
        } else if eq("SET DEFAULT") {
            Some(Self::SetDefault)
        } else {
            None
        }
    }

    /// The SQL spelling (`NO ACTION`, `SET NULL`…).
    #[must_use]
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::NoAction => "NO ACTION",
            Self::Restrict => "RESTRICT",
            Self::Cascade => "CASCADE",
            Self::SetNull => "SET NULL",
            Self::SetDefault => "SET DEFAULT",
        }
    }
}

/// Details of an [`ObjectKind::ForeignKey`]. Build with [`ForeignKeyInfo::new`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ForeignKeyInfo {
    /// Referencing columns of the table the key belongs to, unquoted, in key order.
    pub columns: Vec<String>,
    /// Path of the referenced table, in the same shape as the paths [`Metadata::list`]
    /// returns for tables of this driver (e.g. `[Schema public, Table customers]`).
    pub referenced_table: ObjectPath,
    /// Referenced columns, unquoted, in the order matching [`columns`](Self::columns).
    pub referenced_columns: Vec<String>,
    /// Action on update of the referenced row, when the engine reports it.
    pub on_update: Option<ReferentialAction>,
    /// Action on delete of the referenced row, when the engine reports it.
    pub on_delete: Option<ReferentialAction>,
}

impl ForeignKeyInfo {
    /// A foreign key to `referenced_table` with no columns and no actions.
    #[must_use]
    pub fn new(referenced_table: ObjectPath) -> Self {
        Self {
            columns: Vec::new(),
            referenced_table,
            referenced_columns: Vec::new(),
            on_update: None,
            on_delete: None,
        }
    }
}

/// Details of an [`ObjectKind::Index`]. Build with `IndexInfo::default()` and set fields.
///
/// The defaults are the most conservative for choosing a row key ([`crate::dml::KeyCandidate`],
/// P5-B review M1): not valid, an unknown expression flag per part, `NULL`s distinct, not
/// deferrable. Drivers set what they read from the catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct IndexInfo {
    /// Key parts in order: a column name (unquoted), or the expression text for an expression
    /// key where the engine reports it (`lower(email)`).
    pub columns: Vec<String>,
    /// Unique index.
    pub unique: bool,
    /// Backs the primary key.
    pub primary: bool,
    /// Access method as the engine spells it (`btree`, `hash`, `gin`, `BTREE`, `FULLTEXT`…).
    pub method: Option<String>,
    /// Predicate of a partial index (`WHERE` text without the keyword), when the driver reads
    /// its text.
    pub predicate: Option<String>,
    /// A partial index (it has a predicate), whether or not the driver reads the predicate's
    /// text ([`predicate`](Self::predicate) may be `None`, as on SQLite). Default `false`.
    pub partial: bool,
    /// The index is valid and ready: PostgreSQL `indisvalid` and `indisready` (an index left by a
    /// failed `CREATE INDEX CONCURRENTLY` is not). Default `false`.
    pub valid: bool,
    /// Uniqueness is checked at the end of the transaction rather than at once (PostgreSQL
    /// deferrable constraints, `indimmediate = false`). Default `false`.
    pub deferrable: bool,
    /// `NULL`s are distinct for uniqueness (`false` for PostgreSQL `NULLS NOT DISTINCT`).
    /// Default `true`.
    pub nulls_distinct: bool,
    /// Per key part, in the order of [`columns`](Self::columns): `true` when the part is an
    /// expression rather than a plain column. Empty (or another length than `columns`) means
    /// unknown, which key selection treats as an expression.
    pub expressions: Vec<bool>,
}

impl Default for IndexInfo {
    fn default() -> Self {
        Self {
            columns: Vec::new(),
            unique: false,
            primary: false,
            method: None,
            predicate: None,
            partial: false,
            valid: false,
            deferrable: false,
            nulls_distinct: true,
            expressions: Vec::new(),
        }
    }
}

impl IndexInfo {
    /// `true` when some key part is an expression, or the expression flags are unknown.
    #[must_use]
    pub fn has_expression(&self) -> bool {
        self.expressions.len() != self.columns.len() || self.expressions.iter().any(|e| *e)
    }
}

/// Details of an [`ObjectKind::Trigger`]. Build with `TriggerInfo::default()` and set fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TriggerInfo {
    /// `BEFORE`, `AFTER` or `INSTEAD OF`, when known.
    pub timing: Option<String>,
    /// Events in upper case: `INSERT`, `UPDATE`, `DELETE`, `TRUNCATE`.
    pub events: Vec<String>,
    /// `FOR EACH ROW` (otherwise per statement).
    pub for_each_row: bool,
    /// Enabled (engines without a disabled state report `true`).
    pub enabled: bool,
}

/// Kind of a [`RoutineInfo`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RoutineKind {
    /// Function (returns a value).
    Function,
    /// Procedure (`CALL`).
    Procedure,
}

/// Details of an [`ObjectKind::Routine`]. Build with [`RoutineInfo::new`].
///
/// The path segment of a routine may carry its signature so overloads stay distinct
/// (pgwire: `f(integer, text)`); [`routine_name`](Self::routine_name) is the bare name to call
/// or complete.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RoutineInfo {
    /// Function or procedure.
    pub kind: RoutineKind,
    /// Bare name, unquoted, without arguments.
    pub routine_name: String,
    /// Argument list as the engine spells it (`a integer, b text`), when known.
    pub arguments: Option<String>,
    /// Return type of a function, when known.
    pub return_type: Option<String>,
    /// Language (`sql`, `plpgsql`, `SQL`…), when known.
    pub language: Option<String>,
}

impl RoutineInfo {
    /// A routine of `kind` named `routine_name`, with nothing else known.
    #[must_use]
    pub fn new(kind: RoutineKind, routine_name: impl Into<String>) -> Self {
        Self {
            kind,
            routine_name: routine_name.into(),
            arguments: None,
            return_type: None,
            language: None,
        }
    }
}

/// Details of an [`ObjectKind::Sequence`]. Build with `SequenceInfo::default()` and set fields.
/// Values the engine does not report are `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SequenceInfo {
    /// Value type (`bigint`).
    pub data_type: Option<String>,
    /// Start value.
    pub start: Option<i64>,
    /// Increment.
    pub increment: Option<i64>,
    /// Minimum value.
    pub min_value: Option<i64>,
    /// Maximum value.
    pub max_value: Option<i64>,
    /// Wraps around at the limit.
    pub cycle: bool,
}

/// Details of an [`ObjectKind::Type`]. Build with [`TypeInfo::new`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct TypeInfo {
    /// `enum`, `composite`, `domain`, `range`, `multirange` or `base`.
    pub flavour: String,
    /// Underlying type of a domain, or subtype of a range.
    pub base_type: Option<String>,
    /// Labels of an enum, in order.
    pub labels: Vec<String>,
}

impl TypeInfo {
    /// A type of `flavour` with no base type and no labels.
    #[must_use]
    pub fn new(flavour: impl Into<String>) -> Self {
        Self {
            flavour: flavour.into(),
            base_type: None,
            labels: Vec::new(),
        }
    }
}

/// Details of an [`ObjectKind::Partition`]. Build with `PartitionInfo::default()` and set
/// fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PartitionInfo {
    /// Bound as the engine spells it (`FOR VALUES FROM (1) TO (10)`, MySQL's
    /// `PARTITION_DESCRIPTION`).
    pub bound: Option<String>,
    /// Partitioning method of the parent (`RANGE`, `LIST`, `HASH`, `KEY`…).
    pub method: Option<String>,
    /// Row estimate from statistics, if cheap to get.
    pub estimated_rows: Option<u64>,
    /// Schema of the partition when the engine lets it differ from its parent's (pgwire).
    pub schema: Option<String>,
    /// The partition is itself partitioned (it has sub-partitions).
    pub partitioned: bool,
}

/// Navigator metadata. Obtained from [`Session::metadata`](crate::Session::metadata).
///
/// Calls are lazy (one level, one kind at a time) and paged. Dropping a returned future
/// abandons the call, and drivers stop the statement it was running where the engine can
/// (the same server-side cancel as [`Session::cancel_handle`](crate::Session::cancel_handle),
/// which also reaches a running metadata call); the core applies its own timeout around
/// metadata calls.
///
/// Metadata calls only read: they work on read-only sessions and never write (I3). Each page
/// is bounded by [`PageRequest::limit`] (clamped by drivers to at most 10 000) and by the
/// driver's own byte cap on internal results (I6). [`ObjectDetails`] carry what the
/// navigator, the object editor and completion need (column details, key and index columns,
/// foreign-key targets); comments are in [`ObjectInfo::comment`]. Names in paths and details
/// are unquoted.
///
/// When a driver serves metadata from a **separate connection** (so the navigator works while a
/// query streams), that connection is opened with the session's exact security settings (L5):
/// the same TLS configuration and verification (never a weaker mode), the same SSH route, the
/// same read-only mechanism applied and verified before any query, and the same reserved
/// property rules. Its queries bind every user-supplied value ([`PageRequest::filter`], object
/// names) as parameters.
#[async_trait::async_trait]
pub trait Metadata: Send + Sync {
    /// Kinds that can be listed under an object of kind `parent` (`None` = server root), in
    /// display order. Must be a subset of
    /// [`MetadataCapabilities::kinds`](crate::MetadataCapabilities::kinds).
    fn child_kinds(&self, parent: Option<ObjectKind>) -> Vec<ObjectKind>;

    /// Lists objects of `kind` directly under `parent`.
    async fn list(
        &self,
        parent: &ObjectPath,
        kind: ObjectKind,
        page: PageRequest,
    ) -> DriverResult<Page<ObjectInfo>>;

    /// DDL of an object, for kinds declared in
    /// [`MetadataCapabilities::ddl`](crate::MetadataCapabilities::ddl); otherwise
    /// [`DriverError::Unsupported`](crate::DriverError::Unsupported).
    async fn ddl(&self, object: &ObjectPath) -> DriverResult<String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn referential_actions_round_trip() {
        for a in [
            ReferentialAction::NoAction,
            ReferentialAction::Restrict,
            ReferentialAction::Cascade,
            ReferentialAction::SetNull,
            ReferentialAction::SetDefault,
        ] {
            assert_eq!(ReferentialAction::parse(a.as_sql()), Some(a));
            assert_eq!(
                ReferentialAction::parse(&a.as_sql().to_ascii_lowercase()),
                Some(a)
            );
        }
        assert_eq!(ReferentialAction::parse("SET  NULL"), None);
        assert_eq!(ReferentialAction::parse(""), None);
    }
}
