//! [`Metadata`] of the mysqlwire driver (P4-A): the navigator's object kinds from
//! `information_schema`, and DDL from `SHOW CREATE …`.
//!
//! The text protocol has no bind parameters: user input (filter, names, cursor) only reaches a
//! statement as a hexadecimal literal compared as a binary string (`CAST(col AS BINARY) =
//! X'…'`), so neither the session's `sql_mode` nor its character set or collations change
//! what the driver's SQL means. `SHOW CREATE` takes identifiers, not expressions: names are
//! quoted with backticks (a backtick doubled), which no `sql_mode` changes, and a name holding
//! NUL (which MySQL identifiers cannot) is refused.
//!
//! Path shapes: `[Schema]`, `[Schema, Table | View | Routine | Event | Sequence]`, and below a
//! table `Column`, `Constraint`, `ForeignKey`, `Index`, `Trigger`, `Partition` (columns also
//! below a view).

use std::sync::Arc;

use ninesql_driver_api::{
    ColumnInfo, ConstraintInfo, ConstraintKind, DriverError, DriverResult, Feature, ForeignKeyInfo,
    IndexInfo, Metadata, ObjectDetails, ObjectInfo, ObjectKind, ObjectPath, Page, PageRequest,
    PartitionInfo, ReferentialAction, RoutineInfo, RoutineKind, SequenceInfo, TableInfo,
    TriggerInfo, escape_like,
};

use crate::actor::Rows;
use crate::cancel::Control;
use crate::dialect::literal;
use crate::session::Link;

/// Largest DDL text returned; a longer definition is an error, never returned cut.
const MAX_DDL_BYTES: u64 = 1024 * 1024;

/// Page size cap of the kinds whose key columns come from a second query (at most 16 key
/// parts each, so the second query stays under the internal row cap).
const KEYED_PAGE_LIMIT: u32 = 500;

/// Navigator metadata, served on the session's connection: a call made while a result streams
/// fails with [`DriverError::Busy`]. A call whose future is dropped is stopped with
/// `KILL QUERY`.
pub struct MyMetadata {
    pub(crate) link: Link,
    pub(crate) control: Arc<Control>,
    pub(crate) mariadb: bool,
}

impl std::fmt::Debug for MyMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MyMetadata")
            .field("mariadb", &self.mariadb)
            .finish_non_exhaustive()
    }
}

/// A binary-string literal of `s` (inert in any `sql_mode`).
fn bin(s: &str) -> String {
    if s.is_empty() {
        return "X''".to_owned();
    }
    literal(&ninesql_driver_api::Param::Bytes(s.as_bytes().to_vec()))
        .unwrap_or_else(|_| "X''".to_owned())
}

/// A backtick-quoted identifier for `SHOW CREATE`.
fn ident(name: &str) -> DriverResult<String> {
    if name.contains('\0') {
        return Err(DriverError::InvalidParams("invalid object name".into()));
    }
    Ok(format!("`{}`", name.replace('`', "``")))
}

fn segment(parent: &ObjectPath, idx: usize, kinds: &[ObjectKind]) -> DriverResult<String> {
    parent
        .0
        .get(idx)
        .filter(|s| kinds.contains(&s.kind))
        .map(|s| s.name.clone())
        .ok_or_else(bad_path)
}

fn bad_path() -> DriverError {
    DriverError::InvalidParams("unexpected object path".into())
}

fn not_found() -> DriverError {
    DriverError::InvalidParams("object not found".into())
}

fn cell(row: &[Option<String>], i: usize) -> Option<String> {
    row.get(i).cloned().flatten()
}

/// `AND CAST(col AS BINARY) LIKE <filter> ESCAPE '!'` for a filter, empty otherwise.
fn filter_clause(col: &str, filter: Option<&str>) -> String {
    match filter {
        Some(f) => format!(
            " AND CAST({col} AS BINARY) LIKE {} ESCAPE '!'",
            bin(&escape_like(f, '!'))
        ),
        None => String::new(),
    }
}

/// `AND CAST(key AS BINARY) > <cursor>` for a cursor, empty otherwise.
fn cursor_clause(key: &str, cursor: Option<&str>) -> String {
    match cursor {
        Some(c) => format!(" AND CAST({key} AS BINARY) > {}", bin(c)),
        None => String::new(),
    }
}

/// `AND <col> > <n>` for a numeric cursor, empty otherwise.
fn ordinal_clause(col: &str, cursor: Option<&str>) -> DriverResult<String> {
    match cursor {
        Some(c) => {
            let n: u32 = c
                .parse()
                .map_err(|_| DriverError::InvalidParams("invalid page cursor".into()))?;
            Ok(format!(" AND {col} > {n}"))
        }
        None => Ok(String::new()),
    }
}

/// `CAST(col AS BINARY) BETWEEN <first> AND <last>`: the rows of one page's keys.
fn range_clause(col: &str, first: &str, last: &str) -> String {
    format!(
        "CAST({col} AS BINARY) BETWEEN {} AND {}",
        bin(first),
        bin(last)
    )
}

impl MyMetadata {
    async fn query(&self, sql: &str, value_limit: Option<u64>) -> DriverResult<Rows> {
        self.link
            .internal_limited(sql, false, value_limit, Some(&self.control))
            .await
    }

    /// Column `col` of the single row of a `SHOW CREATE` statement.
    async fn show_create(&self, sql: &str, col: usize) -> DriverResult<String> {
        let rows = self.query(sql, Some(MAX_DDL_BYTES)).await?;
        let row = rows.first().ok_or_else(not_found)?;
        cell(row, col).ok_or_else(|| DriverError::Query {
            code: None,
            message: "the definition is not visible to this user (missing privileges)".into(),
            position: None,
        })
    }

    /// Key columns of the page `[first, last]` of a keyed kind, grouped by key name in order:
    /// `(name, column, referenced column)`.
    async fn key_columns(
        &self,
        sql: String,
    ) -> DriverResult<Vec<(String, String, Option<String>)>> {
        let rows = self.query(&sql, None).await?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    cell(r, 0).unwrap_or_default(),
                    cell(r, 1).unwrap_or_default(),
                    cell(r, 2),
                )
            })
            .collect())
    }
}

fn columns_of(keys: &[(String, String, Option<String>)], name: &str) -> (Vec<String>, Vec<String>) {
    let mut cols = Vec::new();
    let mut refs = Vec::new();
    for (k, c, r) in keys {
        if k == name {
            cols.push(c.clone());
            if let Some(r) = r {
                refs.push(r.clone());
            }
        }
    }
    (cols, refs)
}

#[async_trait::async_trait]
impl Metadata for MyMetadata {
    fn child_kinds(&self, parent: Option<ObjectKind>) -> Vec<ObjectKind> {
        match parent {
            None => vec![ObjectKind::Schema],
            Some(ObjectKind::Schema) => {
                let mut k = vec![ObjectKind::Table, ObjectKind::View];
                if self.mariadb {
                    k.push(ObjectKind::Sequence);
                }
                k.extend([ObjectKind::Routine, ObjectKind::Event]);
                k
            }
            Some(ObjectKind::Table) => vec![
                ObjectKind::Column,
                ObjectKind::Constraint,
                ObjectKind::ForeignKey,
                ObjectKind::Index,
                ObjectKind::Trigger,
                ObjectKind::Partition,
            ],
            Some(ObjectKind::View) => vec![ObjectKind::Column],
            Some(_) => Vec::new(),
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn list(
        &self,
        parent: &ObjectPath,
        kind: ObjectKind,
        page: PageRequest,
    ) -> DriverResult<Page<ObjectInfo>> {
        if !self.child_kinds(parent.kind()).contains(&kind) {
            return Err(DriverError::Unsupported(Feature::Metadata(kind)));
        }
        let keyed = matches!(
            kind,
            ObjectKind::Constraint | ObjectKind::ForeignKey | ObjectKind::Index
        );
        let limit = page
            .limit
            .clamp(1, if keyed { KEYED_PAGE_LIMIT } else { 10_000 });
        let fetch = u64::from(limit) + 1;
        let filter = page.filter.as_deref();
        let cursor = page.cursor.as_deref();
        let in_table = |schema_col: &str, table_col: &str| -> DriverResult<String> {
            let schema = segment(parent, 0, &[ObjectKind::Schema])?;
            let table = segment(parent, 1, &[ObjectKind::Table, ObjectKind::View])?;
            if parent.0.len() != 2 {
                return Err(bad_path());
            }
            Ok(format!(
                "CAST({schema_col} AS BINARY) = {} AND CAST({table_col} AS BINARY) = {}",
                bin(&schema),
                bin(&table)
            ))
        };
        let in_schema = |col: &str| -> DriverResult<String> {
            let schema = segment(parent, 0, &[ObjectKind::Schema])?;
            if parent.0.len() != 1 {
                return Err(bad_path());
            }
            Ok(format!("CAST({col} AS BINARY) = {}", bin(&schema)))
        };
        // Every statement returns the name, the comment and the cursor key first.
        let sql = match kind {
            ObjectKind::Schema => format!(
                "SELECT SCHEMA_NAME, NULL, CAST(SCHEMA_NAME AS BINARY) \
                 FROM information_schema.SCHEMATA WHERE TRUE{}{} \
                 ORDER BY CAST(SCHEMA_NAME AS BINARY) LIMIT {fetch}",
                filter_clause("SCHEMA_NAME", filter),
                cursor_clause("SCHEMA_NAME", cursor),
            ),
            ObjectKind::Table | ObjectKind::View | ObjectKind::Sequence => {
                let types = match kind {
                    ObjectKind::Table => "'BASE TABLE', 'SYSTEM VERSIONED'",
                    ObjectKind::View => "'VIEW', 'SYSTEM VIEW'",
                    _ => "'SEQUENCE'",
                };
                format!(
                    "SELECT t.TABLE_NAME, t.TABLE_COMMENT, CAST(t.TABLE_NAME AS BINARY), \
                     t.TABLE_ROWS, t.TABLE_TYPE, \
                     CASE WHEN t.CREATE_OPTIONS LIKE '%partitioned%' THEN \
                     (SELECT CONCAT(p.PARTITION_METHOD, ' (', COALESCE(p.PARTITION_EXPRESSION, ''), ')') \
                      FROM information_schema.PARTITIONS p \
                      WHERE p.TABLE_SCHEMA = t.TABLE_SCHEMA AND p.TABLE_NAME = t.TABLE_NAME \
                      AND p.PARTITION_ORDINAL_POSITION = 1 LIMIT 1) END \
                     FROM information_schema.TABLES t \
                     WHERE {} AND t.TABLE_TYPE IN ({types}){}{} \
                     ORDER BY CAST(t.TABLE_NAME AS BINARY) LIMIT {fetch}",
                    in_schema("t.TABLE_SCHEMA")?,
                    filter_clause("t.TABLE_NAME", filter),
                    cursor_clause("t.TABLE_NAME", cursor),
                )
            }
            ObjectKind::Routine => {
                // Functions and procedures may share a name: the key adds the type.
                let key =
                    "CONCAT(CAST(ROUTINE_NAME AS BINARY), X'00', CAST(ROUTINE_TYPE AS BINARY))";
                format!(
                    "SELECT ROUTINE_NAME, ROUTINE_COMMENT, {key}, ROUTINE_TYPE, DTD_IDENTIFIER, \
                     ROUTINE_BODY FROM information_schema.ROUTINES \
                     WHERE {}{}{} ORDER BY {key} LIMIT {fetch}",
                    in_schema("ROUTINE_SCHEMA")?,
                    filter_clause("ROUTINE_NAME", filter),
                    cursor_clause(key, cursor),
                )
            }
            ObjectKind::Event => format!(
                "SELECT EVENT_NAME, EVENT_COMMENT, CAST(EVENT_NAME AS BINARY) \
                 FROM information_schema.EVENTS WHERE {}{}{} \
                 ORDER BY CAST(EVENT_NAME AS BINARY) LIMIT {fetch}",
                in_schema("EVENT_SCHEMA")?,
                filter_clause("EVENT_NAME", filter),
                cursor_clause("EVENT_NAME", cursor),
            ),
            ObjectKind::Column => format!(
                "SELECT COLUMN_NAME, COLUMN_COMMENT, ORDINAL_POSITION, COLUMN_TYPE, \
                 IS_NULLABLE, COLUMN_DEFAULT, COLUMN_KEY, EXTRA \
                 FROM information_schema.COLUMNS WHERE {}{}{} \
                 ORDER BY ORDINAL_POSITION LIMIT {fetch}",
                in_table("TABLE_SCHEMA", "TABLE_NAME")?,
                filter_clause("COLUMN_NAME", filter),
                ordinal_clause("ORDINAL_POSITION", cursor)?,
            ),
            ObjectKind::Constraint => {
                // Check names are unique per schema on MySQL, per table on MariaDB.
                let same_table = if self.mariadb {
                    " AND cc.TABLE_NAME = tc.TABLE_NAME"
                } else {
                    ""
                };
                format!(
                    "SELECT tc.CONSTRAINT_NAME, NULL, CAST(tc.CONSTRAINT_NAME AS BINARY), \
                     tc.CONSTRAINT_TYPE, cc.CHECK_CLAUSE \
                     FROM information_schema.TABLE_CONSTRAINTS tc \
                     LEFT JOIN information_schema.CHECK_CONSTRAINTS cc \
                     ON tc.CONSTRAINT_TYPE = 'CHECK' \
                     AND cc.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA \
                     AND cc.CONSTRAINT_NAME = tc.CONSTRAINT_NAME{same_table} \
                     WHERE {} AND tc.CONSTRAINT_TYPE IN ('PRIMARY KEY', 'UNIQUE', 'CHECK'){}{} \
                     ORDER BY CAST(tc.CONSTRAINT_NAME AS BINARY) LIMIT {fetch}",
                    in_table("tc.TABLE_SCHEMA", "tc.TABLE_NAME")?,
                    filter_clause("tc.CONSTRAINT_NAME", filter),
                    cursor_clause("tc.CONSTRAINT_NAME", cursor),
                )
            }
            ObjectKind::ForeignKey => format!(
                "SELECT CONSTRAINT_NAME, NULL, CAST(CONSTRAINT_NAME AS BINARY), \
                 UNIQUE_CONSTRAINT_SCHEMA, REFERENCED_TABLE_NAME, UPDATE_RULE, DELETE_RULE \
                 FROM information_schema.REFERENTIAL_CONSTRAINTS WHERE {}{}{} \
                 ORDER BY CAST(CONSTRAINT_NAME AS BINARY) LIMIT {fetch}",
                in_table("CONSTRAINT_SCHEMA", "TABLE_NAME")?,
                filter_clause("CONSTRAINT_NAME", filter),
                cursor_clause("CONSTRAINT_NAME", cursor),
            ),
            ObjectKind::Index => format!(
                "SELECT INDEX_NAME, MAX(INDEX_COMMENT), CAST(INDEX_NAME AS BINARY), \
                 MIN(NON_UNIQUE), MAX(INDEX_TYPE) \
                 FROM information_schema.STATISTICS WHERE {}{}{} \
                 GROUP BY CAST(INDEX_NAME AS BINARY), INDEX_NAME \
                 ORDER BY CAST(INDEX_NAME AS BINARY) LIMIT {fetch}",
                in_table("TABLE_SCHEMA", "TABLE_NAME")?,
                filter_clause("INDEX_NAME", filter),
                cursor_clause("INDEX_NAME", cursor),
            ),
            ObjectKind::Trigger => format!(
                "SELECT TRIGGER_NAME, NULL, CAST(TRIGGER_NAME AS BINARY), ACTION_TIMING, \
                 EVENT_MANIPULATION, ACTION_ORIENTATION \
                 FROM information_schema.TRIGGERS WHERE {}{}{} \
                 ORDER BY CAST(TRIGGER_NAME AS BINARY) LIMIT {fetch}",
                in_table("EVENT_OBJECT_SCHEMA", "EVENT_OBJECT_TABLE")?,
                filter_clause("TRIGGER_NAME", filter),
                cursor_clause("TRIGGER_NAME", cursor),
            ),
            ObjectKind::Partition => format!(
                "SELECT PARTITION_NAME, MAX(PARTITION_COMMENT), MIN(PARTITION_ORDINAL_POSITION), \
                 MAX(PARTITION_METHOD), MAX(PARTITION_DESCRIPTION), SUM(TABLE_ROWS), \
                 MAX(SUBPARTITION_NAME IS NOT NULL) \
                 FROM information_schema.PARTITIONS \
                 WHERE {} AND PARTITION_NAME IS NOT NULL{}{} \
                 GROUP BY PARTITION_NAME ORDER BY MIN(PARTITION_ORDINAL_POSITION) LIMIT {fetch}",
                in_table("TABLE_SCHEMA", "TABLE_NAME")?,
                filter_clause("PARTITION_NAME", filter),
                ordinal_clause("PARTITION_ORDINAL_POSITION", cursor)?,
            ),
            _ => return Err(DriverError::Unsupported(Feature::Metadata(kind))),
        };
        let mut rows = self.query(&sql, None).await?;
        let more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        // Key columns of the page, from one more statement over the page's key range.
        let keys = match (
            keyed,
            rows.first().and_then(|r| cell(r, 0)),
            rows.last().and_then(|r| cell(r, 0)),
        ) {
            (true, Some(first), Some(last)) => {
                let range = |col: &str| range_clause(col, &first, &last);
                let sql = match kind {
                    ObjectKind::Constraint => format!(
                        "SELECT CONSTRAINT_NAME, COLUMN_NAME, NULL \
                         FROM information_schema.KEY_COLUMN_USAGE \
                         WHERE {} AND REFERENCED_TABLE_NAME IS NULL AND {} \
                         ORDER BY CAST(CONSTRAINT_NAME AS BINARY), ORDINAL_POSITION LIMIT 10000",
                        in_table("TABLE_SCHEMA", "TABLE_NAME")?,
                        range("CONSTRAINT_NAME"),
                    ),
                    ObjectKind::ForeignKey => format!(
                        "SELECT CONSTRAINT_NAME, COLUMN_NAME, REFERENCED_COLUMN_NAME \
                         FROM information_schema.KEY_COLUMN_USAGE \
                         WHERE {} AND REFERENCED_TABLE_NAME IS NOT NULL AND {} \
                         ORDER BY CAST(CONSTRAINT_NAME AS BINARY), ORDINAL_POSITION LIMIT 10000",
                        in_table("TABLE_SCHEMA", "TABLE_NAME")?,
                        range("CONSTRAINT_NAME"),
                    ),
                    _ => format!(
                        "SELECT INDEX_NAME, COLUMN_NAME, {} \
                         FROM information_schema.STATISTICS WHERE {} AND {} \
                         ORDER BY CAST(INDEX_NAME AS BINARY), SEQ_IN_INDEX LIMIT 10000",
                        // Functional key parts (MySQL 8.0.13+); MariaDB has no such column.
                        if self.mariadb { "NULL" } else { "EXPRESSION" },
                        in_table("TABLE_SCHEMA", "TABLE_NAME")?,
                        range("INDEX_NAME"),
                    ),
                };
                self.key_columns(sql).await?
            }
            _ => Vec::new(),
        };
        let mut items = Vec::with_capacity(rows.len());
        let mut next_cursor = None;
        for row in &rows {
            let name = cell(row, 0).unwrap_or_default();
            let comment = cell(row, 1).filter(|c| !c.is_empty());
            let details = match kind {
                ObjectKind::Table | ObjectKind::View => {
                    let mut t = TableInfo::default();
                    t.estimated_rows = cell(row, 3).and_then(|v| v.parse().ok());
                    t.partition_key = cell(row, 5);
                    t.flavour = match cell(row, 4).as_deref() {
                        Some("SYSTEM VERSIONED") => Some("system-versioned".to_owned()),
                        Some("SYSTEM VIEW") => Some("system".to_owned()),
                        _ if t.partition_key.is_some() => Some("partitioned".to_owned()),
                        _ => None,
                    };
                    ObjectDetails::Table(t)
                }
                ObjectKind::Sequence => ObjectDetails::Sequence(SequenceInfo::default()),
                ObjectKind::Routine => {
                    let k = if cell(row, 3).as_deref() == Some("PROCEDURE") {
                        RoutineKind::Procedure
                    } else {
                        RoutineKind::Function
                    };
                    let mut r = RoutineInfo::new(k, name.clone());
                    r.return_type = cell(row, 4).filter(|_| k == RoutineKind::Function);
                    r.language = cell(row, 5);
                    ObjectDetails::Routine(r)
                }
                ObjectKind::Column => {
                    let ordinal = cell(row, 2).and_then(|v| v.parse().ok()).unwrap_or(0);
                    let mut c = ColumnInfo::new(ordinal, cell(row, 3).unwrap_or_default());
                    c.nullable = cell(row, 4).as_deref() == Some("YES");
                    c.default = cell(row, 5);
                    c.primary_key = cell(row, 6).as_deref() == Some("PRI");
                    let extra = cell(row, 7).unwrap_or_default().to_ascii_lowercase();
                    c.auto_generated = extra.contains("auto_increment")
                        || extra.contains("virtual")
                        || extra.contains("stored")
                        || extra.contains("persistent");
                    next_cursor = Some(ordinal.to_string());
                    ObjectDetails::Column(c)
                }
                ObjectKind::Constraint => {
                    let k = match cell(row, 3).as_deref() {
                        Some("PRIMARY KEY") => ConstraintKind::PrimaryKey,
                        Some("UNIQUE") => ConstraintKind::Unique,
                        _ => ConstraintKind::Check,
                    };
                    let mut c = ConstraintInfo::new(k);
                    c.columns = columns_of(&keys, &name).0;
                    c.definition = cell(row, 4);
                    ObjectDetails::Constraint(c)
                }
                ObjectKind::ForeignKey => {
                    let target = ObjectPath::root()
                        .child(ObjectKind::Schema, cell(row, 3).unwrap_or_default())
                        .child(ObjectKind::Table, cell(row, 4).unwrap_or_default());
                    let mut f = ForeignKeyInfo::new(target);
                    (f.columns, f.referenced_columns) = columns_of(&keys, &name);
                    f.on_update = cell(row, 5).and_then(|a| ReferentialAction::parse(&a));
                    f.on_delete = cell(row, 6).and_then(|a| ReferentialAction::parse(&a));
                    ObjectDetails::ForeignKey(f)
                }
                ObjectKind::Index => {
                    let mut i = IndexInfo::default();
                    // A functional key part has no column name; its expression is the part.
                    i.columns = keys
                        .iter()
                        .filter(|(k, _, _)| *k == name)
                        .map(|(_, c, e)| {
                            if c.is_empty() {
                                e.clone().unwrap_or_default()
                            } else {
                                c.clone()
                            }
                        })
                        .collect();
                    i.unique = cell(row, 3).as_deref() == Some("0");
                    i.primary = name == "PRIMARY";
                    i.method = cell(row, 4);
                    ObjectDetails::Index(i)
                }
                ObjectKind::Trigger => {
                    let mut t = TriggerInfo::default();
                    t.timing = cell(row, 3);
                    t.events = cell(row, 4).into_iter().collect();
                    t.for_each_row = cell(row, 5).as_deref() == Some("ROW");
                    t.enabled = true;
                    ObjectDetails::Trigger(t)
                }
                ObjectKind::Partition => {
                    let mut p = PartitionInfo::default();
                    p.method = cell(row, 3);
                    let desc = cell(row, 4);
                    p.bound = match (p.method.as_deref(), desc) {
                        (Some(m), Some(d)) if m.starts_with("RANGE") => {
                            Some(format!("VALUES LESS THAN ({d})"))
                        }
                        (Some(m), Some(d)) if m.starts_with("LIST") => {
                            Some(format!("VALUES IN ({d})"))
                        }
                        _ => None,
                    };
                    p.estimated_rows = cell(row, 5).and_then(|v| v.parse().ok());
                    p.partitioned = cell(row, 6).as_deref() == Some("1");
                    next_cursor = cell(row, 2);
                    ObjectDetails::Partition(p)
                }
                _ => ObjectDetails::None,
            };
            if !matches!(kind, ObjectKind::Column | ObjectKind::Partition) {
                next_cursor = cell(row, 2);
            }
            items.push(ObjectInfo {
                path: parent.child(kind, name),
                comment,
                details,
            });
        }
        Ok(Page {
            items,
            next_cursor: if more { next_cursor } else { None },
        })
    }

    async fn ddl(&self, object: &ObjectPath) -> DriverResult<String> {
        let leaf = object.leaf().ok_or_else(bad_path)?;
        let segs = object.0.as_slice();
        let schema = match segs.first() {
            Some(s) if s.kind == ObjectKind::Schema => ident(&s.name)?,
            _ => return Err(bad_path()),
        };
        let name = ident(&leaf.name)?;
        match (segs.len(), leaf.kind) {
            (1, ObjectKind::Schema) => {
                self.show_create(&format!("SHOW CREATE DATABASE {schema}"), 1)
                    .await
            }
            (2, ObjectKind::Table) => {
                self.show_create(&format!("SHOW CREATE TABLE {schema}.{name}"), 1)
                    .await
            }
            (2, ObjectKind::View) => {
                self.show_create(&format!("SHOW CREATE VIEW {schema}.{name}"), 1)
                    .await
            }
            (2, ObjectKind::Sequence) if self.mariadb => {
                self.show_create(&format!("SHOW CREATE SEQUENCE {schema}.{name}"), 1)
                    .await
            }
            (2, ObjectKind::Event) => {
                self.show_create(&format!("SHOW CREATE EVENT {schema}.{name}"), 3)
                    .await
            }
            (3, ObjectKind::Trigger) if segs[1].kind == ObjectKind::Table => {
                self.show_create(&format!("SHOW CREATE TRIGGER {schema}.{name}"), 2)
                    .await
            }
            (2, ObjectKind::Routine) => {
                // A function and a procedure may share a name: both definitions, function first.
                let types = self
                    .query(
                        &format!(
                            "SELECT ROUTINE_TYPE FROM information_schema.ROUTINES \
                             WHERE CAST(ROUTINE_SCHEMA AS BINARY) = {} \
                             AND CAST(ROUTINE_NAME AS BINARY) = {} ORDER BY ROUTINE_TYPE",
                            bin(&segs[0].name),
                            bin(&leaf.name)
                        ),
                        None,
                    )
                    .await?;
                let mut out = Vec::new();
                for t in types.iter().filter_map(|r| cell(r, 0)) {
                    let what = if t == "PROCEDURE" {
                        "PROCEDURE"
                    } else {
                        "FUNCTION"
                    };
                    out.push(
                        self.show_create(&format!("SHOW CREATE {what} {schema}.{name}"), 2)
                            .await?,
                    );
                }
                if out.is_empty() {
                    return Err(not_found());
                }
                Ok(out.join("\n\n"))
            }
            (
                _,
                ObjectKind::Schema
                | ObjectKind::Table
                | ObjectKind::View
                | ObjectKind::Event
                | ObjectKind::Trigger
                | ObjectKind::Routine,
            ) => Err(bad_path()),
            _ => Err(DriverError::Unsupported(Feature::Ddl)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_input_becomes_hex() {
        assert_eq!(bin("a'b"), "X'612762'");
        assert_eq!(bin(""), "X''");
        assert_eq!(
            filter_clause("TABLE_NAME", Some("a_%'")),
            " AND CAST(TABLE_NAME AS BINARY) LIKE X'2561215F21252725' ESCAPE '!'"
        );
        assert!(cursor_clause("k", None).is_empty());
        assert_eq!(
            range_clause("N", "a", "b"),
            "CAST(N AS BINARY) BETWEEN X'61' AND X'62'"
        );
        assert!(ordinal_clause("P", Some("1 OR 1")).is_err());
        assert_eq!(ordinal_clause("P", Some("7")).unwrap(), " AND P > 7");
    }

    #[test]
    fn identifiers_are_backtick_quoted() {
        assert_eq!(ident("a`b").unwrap(), "`a``b`");
        assert_eq!(ident("Ünï code").unwrap(), "`Ünï code`");
        assert!(ident("a\0b").is_err());
    }
}
