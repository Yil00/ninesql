//! QuestDB metadata (P4-C). QuestDB has no schemas and only a partial `pg_catalog`, so the
//! navigator reads QuestDB's own table functions: `tables()` (tables `T`, materialized views
//! `M`, views `V`), `table_columns(name)` and `table_partitions(name)`, and DDL comes from
//! `SHOW CREATE TABLE | VIEW | MATERIALIZED VIEW`.
//!
//! Injection safety: every statement is assembled from constant fragments; user input (filter,
//! names, cursor) is a bound parameter wherever QuestDB accepts one, and the page size is a
//! number formatted by the driver. Two places cannot take a parameter: `table_columns()` and
//! `table_partitions()` only accept a string constant, and `SHOW CREATE` takes a name. There an
//! object name is written as a constant (`'name'`, `"name"`) only after it was found in
//! `tables()` with a bound parameter **and** checked to hold none of `'`, `"`, `\` and no
//! control character ([`quotable`]): QuestDB refuses these characters in table names, so a
//! listed name never holds them, and a name holding them is refused before any SQL is built.
//! QuestDB has no `search_path`, no user operators and no user functions, so the PostgreSQL
//! rule of qualified operators does not apply here.
//!
//! Paths: `[Table | View | MaterializedView]`, below a table or a materialized view `Column`
//! and `Partition` (QuestDB time partitions, none for a table `PARTITION BY NONE`), below a
//! view `Column`.

use ninesql_driver_api::{
    ColumnInfo, DriverError, DriverResult, Feature, ObjectDetails, ObjectInfo, ObjectKind,
    ObjectPath, Page, PageRequest, PartitionInfo, TableInfo,
};

use crate::metadata::PgMetadata;

/// Largest DDL text returned (as for PostgreSQL).
const MAX_DDL_BYTES: usize = 1024 * 1024;

pub(crate) fn child_kinds(parent: Option<ObjectKind>) -> Vec<ObjectKind> {
    match parent {
        None => vec![
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::MaterializedView,
        ],
        Some(ObjectKind::Table | ObjectKind::MaterializedView) => {
            vec![ObjectKind::Column, ObjectKind::Partition]
        }
        Some(ObjectKind::View) => vec![ObjectKind::Column],
        Some(_) => Vec::new(),
    }
}

fn table_type(kind: ObjectKind) -> Option<&'static str> {
    match kind {
        ObjectKind::Table => Some("T"),
        ObjectKind::View => Some("V"),
        ObjectKind::MaterializedView => Some("M"),
        _ => None,
    }
}

fn bad_path() -> DriverError {
    DriverError::InvalidParams("not a QuestDB object path".into())
}

fn not_found() -> DriverError {
    DriverError::Query {
        code: None,
        message: "object not found".into(),
        position: None,
    }
}

fn cell(row: &[Option<String>], i: usize) -> Option<String> {
    row.get(i).cloned().flatten()
}

/// `name` may be written as a QuestDB string constant or double-quoted identifier: no `'`,
/// `"` or `\` (QuestDB refuses them in table names anyway) and no control character.
fn quotable(name: &str) -> bool {
    !name.is_empty() && !name.contains(['\'', '"', '\\']) && !name.chars().any(char::is_control)
}

/// `SELECT … FROM <source> WHERE <cond> [AND strpos(<key>, $f) > 0] [AND <key> > $c]
/// ORDER BY <key> LIMIT n`, with the parameters in order.
fn paged(
    select: &str,
    cond: &str,
    key: &str,
    page: &PageRequest,
    mut params: Vec<Option<String>>,
    fetch: u32,
) -> (String, Vec<Option<String>>) {
    let mut sql = format!("{select} WHERE {cond}");
    if let Some(f) = page.filter.as_deref().filter(|f| !f.is_empty()) {
        params.push(Some(f.to_owned()));
        sql.push_str(&format!(" AND strpos({key}, ${}) > 0", params.len()));
    }
    if let Some(c) = &page.cursor {
        params.push(Some(c.clone()));
        sql.push_str(&format!(" AND {key} > ${}", params.len()));
    }
    sql.push_str(&format!(" ORDER BY {key} LIMIT {fetch}"));
    (sql, params)
}

impl PgMetadata {
    /// The kind (`T`, `V`, `M`) of a top-level object, found with a bound name.
    async fn questdb_object(&self, name: &str, kind: ObjectKind) -> DriverResult<()> {
        let want = table_type(kind).ok_or_else(bad_path)?;
        if !quotable(name) {
            return Err(not_found());
        }
        let rows = self
            .query(
                "SELECT table_type FROM tables() WHERE table_name = $1",
                vec![Some(name.to_owned())],
                2,
                None,
            )
            .await?;
        match rows.first().and_then(|r| cell(r, 0)) {
            Some(t) if t == want => Ok(()),
            _ => Err(not_found()),
        }
    }

    pub(crate) async fn questdb_list(
        &self,
        parent: &ObjectPath,
        kind: ObjectKind,
        page: PageRequest,
    ) -> DriverResult<Page<ObjectInfo>> {
        if !child_kinds(parent.kind()).contains(&kind) {
            return Err(DriverError::Unsupported(Feature::Metadata(kind)));
        }
        let limit = page.limit.clamp(1, 10_000);
        let fetch = limit + 1;
        let (sql, params) = match parent.0.as_slice() {
            [] => {
                let t = table_type(kind).ok_or_else(bad_path)?;
                paged(
                    "SELECT table_name, table_row_count, partitionBy, designatedTimestamp, \
                     walEnabled FROM tables()",
                    "table_type = $1",
                    "table_name",
                    &page,
                    vec![Some(t.to_owned())],
                    fetch,
                )
            }
            [rel] => {
                self.questdb_object(&rel.name, rel.kind).await?;
                match kind {
                    ObjectKind::Column => {
                        // QuestDB lists columns in table order; the cursor is the position.
                        let offset = match &page.cursor {
                            Some(c) => c.parse::<u32>().map_err(|_| {
                                DriverError::InvalidParams("invalid page cursor".into())
                            })?,
                            None => 0,
                        };
                        // `rel.name` passed `quotable` in `questdb_object` above.
                        let mut params = Vec::new();
                        let mut sql = format!(
                            "SELECT \"column\", type, indexed, designated, upsertKey \
                             FROM table_columns('{}')",
                            rel.name
                        );
                        if let Some(f) = page.filter.as_deref().filter(|f| !f.is_empty()) {
                            params.push(Some(f.to_owned()));
                            sql.push_str(" WHERE strpos(\"column\", $1) > 0");
                        }
                        sql.push_str(&format!(
                            " LIMIT {offset}, {}",
                            u64::from(offset) + u64::from(fetch)
                        ));
                        let rows = self
                            .query(&sql, params, i32::try_from(fetch).unwrap_or(i32::MAX), None)
                            .await?;
                        let more = rows.len() > limit as usize;
                        let mut items = Vec::new();
                        for (i, row) in rows.iter().take(limit as usize).enumerate() {
                            let ordinal = offset + u32::try_from(i).unwrap_or(u32::MAX) + 1;
                            let mut c = ColumnInfo::new(ordinal, cell(row, 1).unwrap_or_default());
                            // QuestDB has no NOT NULL constraint.
                            c.nullable = true;
                            c.primary_key = false;
                            items.push(ObjectInfo {
                                path: parent
                                    .child(ObjectKind::Column, cell(row, 0).unwrap_or_default()),
                                comment: None,
                                details: ObjectDetails::Column(c),
                            });
                        }
                        let next = more.then(|| (offset + limit).to_string());
                        return Ok(Page {
                            items,
                            next_cursor: next,
                        });
                    }
                    // `rel.name` passed `quotable` in `questdb_object` above.
                    _ => paged(
                        &format!(
                            "SELECT name, minTimestamp, maxTimestamp, numRows, partitionBy \
                             FROM table_partitions('{}')",
                            rel.name
                        ),
                        "partitionBy <> 'NONE'",
                        "name",
                        &page,
                        Vec::new(),
                        fetch,
                    ),
                }
            }
            _ => return Err(bad_path()),
        };
        let rows = self
            .query(&sql, params, i32::try_from(fetch).unwrap_or(i32::MAX), None)
            .await?;
        let more = rows.len() > limit as usize;
        let mut items = Vec::new();
        let mut last = None;
        for row in rows.iter().take(limit as usize) {
            let name = cell(row, 0).unwrap_or_default();
            let details = if kind == ObjectKind::Partition {
                let mut p = PartitionInfo::default();
                p.bound = match (cell(row, 1), cell(row, 2)) {
                    (Some(lo), Some(hi)) => Some(format!("{lo} .. {hi}")),
                    _ => None,
                };
                p.estimated_rows = cell(row, 3).and_then(|v| v.parse().ok());
                p.method = cell(row, 4);
                ObjectDetails::Partition(p)
            } else {
                let mut t = TableInfo::default();
                t.estimated_rows = cell(row, 1).and_then(|v| v.parse().ok());
                let by = cell(row, 2).filter(|b| b != "NONE" && b != "N/A");
                t.partition_key = match (&by, cell(row, 3)) {
                    (Some(by), Some(ts)) if !ts.is_empty() => Some(format!("{ts} BY {by}")),
                    (Some(by), _) => Some(by.clone()),
                    _ => None,
                };
                t.flavour = match (kind, cell(row, 4).as_deref()) {
                    (ObjectKind::Table, Some("t" | "true")) => Some("wal".to_owned()),
                    (ObjectKind::Table, _) => Some("non-wal".to_owned()),
                    _ => None,
                };
                ObjectDetails::Table(t)
            };
            last = Some(name.clone());
            items.push(ObjectInfo {
                path: parent.child(kind, name),
                comment: None,
                details,
            });
        }
        Ok(Page {
            items,
            next_cursor: if more { last } else { None },
        })
    }

    pub(crate) async fn questdb_ddl(&self, object: &ObjectPath) -> DriverResult<String> {
        let [leaf] = object.0.as_slice() else {
            return Err(DriverError::Unsupported(Feature::Ddl));
        };
        let what = match leaf.kind {
            ObjectKind::Table => "TABLE",
            ObjectKind::View => "VIEW",
            ObjectKind::MaterializedView => "MATERIALIZED VIEW",
            _ => return Err(DriverError::Unsupported(Feature::Ddl)),
        };
        if !quotable(&leaf.name) {
            return Err(not_found());
        }
        self.questdb_object(&leaf.name, leaf.kind).await?;
        let sql = format!("SHOW CREATE {what} \"{}\"", leaf.name);
        let rows = self.query(&sql, Vec::new(), 2, Some(MAX_DDL_BYTES)).await?;
        let ddl = rows
            .first()
            .and_then(|r| cell(r, 0))
            .ok_or_else(not_found)?;
        if ddl.len() > MAX_DDL_BYTES {
            return Err(DriverError::Query {
                code: None,
                message: "the definition is larger than 1024 KiB and was not loaded".into(),
                position: None,
            });
        }
        Ok(ddl)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_bind_user_input() {
        let mut page = PageRequest::default();
        page.filter = Some("x' OR 1=1 --".into());
        page.cursor = Some("o\"rders".into());
        let (sql, params) = paged(
            "SELECT a FROM tables()",
            "table_type = $1",
            "a",
            &page,
            vec![Some("T".into())],
            11,
        );
        assert_eq!(
            sql,
            "SELECT a FROM tables() WHERE table_type = $1 AND strpos(a, $2) > 0 AND a > $3 \
             ORDER BY a LIMIT 11"
        );
        assert_eq!(params.len(), 3);
        assert!(!sql.contains("OR 1=1") && !sql.contains("rders"));
        assert!(quotable("orders") && quotable("ventes 2026 ü"));
        assert!(!quotable("a\"b") && !quotable("a\nb") && !quotable(""));
        assert!(!quotable("a'); DROP ALL TABLES; --") && !quotable("a\\'b"));
    }
}
