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
}

/// Navigator metadata. Obtained from [`Session::metadata`](crate::Session::metadata).
///
/// Calls are lazy (one level, one kind at a time) and paged. Dropping a returned future
/// abandons the call; the core applies its own timeout around metadata calls.
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
