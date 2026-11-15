//! Autocompletion (F-SQL-02): keywords per dialect, schemas, tables, views, columns, aliases and
//! CTE names, ranked against the typed prefix.
//!
//! Metadata comes from a [`MetadataProvider`], a synchronous view of what the core's metadata
//! cache already holds: completion never waits on I/O (I6). A provider that has nothing cached
//! returns nothing and may ask the core to load it in the background.

pub mod context;
pub mod keywords;
mod provider;
pub mod rank;

use std::rc::Rc;

pub use context::{CursorContext, Expect, TableRef, analyze};
pub use keywords::KeywordSet;
pub use provider::SqlCompletionProvider;

use crate::split::StatementSplitter;
use rank::{MatchQuality, match_quality, score};

/// The kind of a database object offered by completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    /// A table.
    Table,
    /// A view or materialized view.
    View,
    /// A function or procedure.
    Function,
}

/// One object of a schema, as cached by the core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectEntry {
    /// Unquoted name.
    pub name: String,
    /// Kind.
    pub kind: ObjectKind,
}

/// One column of a table, as cached by the core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnEntry {
    /// Unquoted name.
    pub name: String,
    /// Engine type name, for the detail line.
    pub data_type: Option<String>,
}

/// What completion reads from the core's metadata cache. Every method returns at once with what
/// is cached (I6); `None` / empty means "not loaded".
pub trait MetadataProvider {
    /// Schema (or database) names.
    fn schemas(&self) -> Vec<String>;

    /// The schema unqualified names resolve in (`search_path` head, current database).
    fn default_schema(&self) -> Option<String>;

    /// Objects of `schema` (`None`: the default schema).
    fn objects(&self, schema: Option<&str>) -> Vec<ObjectEntry>;

    /// Columns of `schema.table` (`None`: resolved like an unqualified name), if cached.
    fn columns(&self, schema: Option<&str>, table: &str) -> Option<Vec<ColumnEntry>>;
}

/// A provider with nothing cached (no connection, or metadata not loaded yet).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoMetadata;

impl MetadataProvider for NoMetadata {
    fn schemas(&self) -> Vec<String> {
        Vec::new()
    }
    fn default_schema(&self) -> Option<String> {
        None
    }
    fn objects(&self, _: Option<&str>) -> Vec<ObjectEntry> {
        Vec::new()
    }
    fn columns(&self, _: Option<&str>, _: &str) -> Option<Vec<ColumnEntry>> {
        None
    }
}

/// What a candidate is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CandidateKind {
    /// A keyword.
    Keyword,
    /// A schema.
    Schema,
    /// A table.
    Table,
    /// A view.
    View,
    /// A function.
    Function,
    /// A column.
    Column,
    /// A table alias in scope.
    Alias,
    /// A CTE name.
    Cte,
}

/// One completion candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// Shown in the menu.
    pub label: String,
    /// Inserted in place of the prefix (quoted when the name needs it).
    pub insert: String,
    /// Kind.
    pub kind: CandidateKind,
    /// Detail line (column type, owning table…).
    pub detail: Option<String>,
    /// Ranking score, higher first.
    pub score: u32,
}

/// The result of a completion request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completions {
    /// The byte range the chosen candidate replaces (the typed prefix).
    pub range: std::ops::Range<usize>,
    /// Ranked candidates, best first.
    pub candidates: Vec<Candidate>,
}

/// Quotes an identifier for the dialect when it needs quoting.
pub type Quoter = Rc<dyn Fn(&str) -> String>;

/// Completion settings for one editor.
#[derive(Clone)]
pub struct CompletionEngine {
    /// Keywords of the connection's dialect.
    pub keywords: KeywordSet,
    /// The dialect's identifier quoting (`Dialect::quote_identifier`).
    pub quote: Quoter,
    /// At most this many candidates.
    pub limit: usize,
}

impl std::fmt::Debug for CompletionEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompletionEngine")
            .field("keywords", &self.keywords)
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}

impl CompletionEngine {
    /// ANSI quoting (`"name"`), 200 candidates at most.
    #[must_use]
    pub fn new(keywords: KeywordSet) -> Self {
        Self {
            keywords,
            quote: Rc::new(|ident: &str| format!("\"{}\"", ident.replace('"', "\"\""))),
            limit: 200,
        }
    }

    /// Uses `quote` for identifiers that need quoting.
    #[must_use]
    pub fn with_quoter(mut self, quote: Quoter) -> Self {
        self.quote = quote;
        self
    }

    fn insert_ident(&self, name: &str) -> String {
        let simple = name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            && !self.keywords.contains(name);
        if simple {
            name.to_owned()
        } else {
            (self.quote)(name)
        }
    }

    /// Candidates for `cursor` (byte offset) in `text`.
    #[must_use]
    pub fn complete(
        &self,
        text: &str,
        cursor: usize,
        splitter: &dyn StatementSplitter,
        metadata: &dyn MetadataProvider,
    ) -> Completions {
        let ctx = analyze(text, cursor, splitter, self.keywords);
        let mut out = Collector::new(&ctx.prefix);
        match ctx.expect {
            Expect::Nothing => {}
            Expect::Keywords => self.keywords_into(&mut out, &ctx.prefix, 2),
            Expect::Tables => {
                self.tables_into(&mut out, metadata, None, 6);
                for cte in &ctx.ctes {
                    out.push(
                        cte,
                        self.insert_ident(cte),
                        CandidateKind::Cte,
                        Some("CTE".into()),
                        7,
                    );
                }
                for schema in metadata.schemas() {
                    let insert = self.insert_ident(&schema);
                    out.push(&schema, insert, CandidateKind::Schema, None, 4);
                }
            }
            Expect::Columns => {
                self.scope_columns_into(&mut out, &ctx, metadata);
                for t in &ctx.scope {
                    if let Some(alias) = &t.alias {
                        let detail = t.table().map(str::to_owned);
                        out.push(
                            alias,
                            self.insert_ident(alias),
                            CandidateKind::Alias,
                            detail,
                            5,
                        );
                    }
                }
                self.keywords_into(&mut out, &ctx.prefix, 1);
            }
            Expect::Qualified => self.qualified_into(&mut out, &ctx, metadata),
        }
        let mut candidates = out.finish();
        candidates.truncate(self.limit);
        Completions {
            range: ctx.prefix_range,
            candidates,
        }
    }

    fn keywords_into(&self, out: &mut Collector, prefix: &str, boost: u32) {
        let lower = !prefix.is_empty() && prefix.chars().all(|c| !c.is_ascii_uppercase());
        for kw in self.keywords.keywords() {
            let insert = if lower {
                kw.to_ascii_lowercase()
            } else {
                kw.to_owned()
            };
            out.push(kw, insert, CandidateKind::Keyword, None, boost);
        }
    }

    fn tables_into(
        &self,
        out: &mut Collector,
        metadata: &dyn MetadataProvider,
        schema: Option<&str>,
        boost: u32,
    ) {
        for obj in metadata.objects(schema) {
            let kind = match obj.kind {
                ObjectKind::Table => CandidateKind::Table,
                ObjectKind::View => CandidateKind::View,
                ObjectKind::Function => continue,
            };
            let insert = self.insert_ident(&obj.name);
            out.push(&obj.name, insert, kind, schema.map(str::to_owned), boost);
        }
    }

    fn columns_of(&self, metadata: &dyn MetadataProvider, t: &TableRef) -> Vec<ColumnEntry> {
        t.table()
            .and_then(|table| metadata.columns(t.schema(), table))
            .unwrap_or_default()
    }

    fn scope_columns_into(
        &self,
        out: &mut Collector,
        ctx: &CursorContext,
        metadata: &dyn MetadataProvider,
    ) {
        let several = ctx.scope.iter().filter(|t| t.table().is_some()).count() > 1;
        for t in &ctx.scope {
            let owner = t.alias.clone().or_else(|| t.table().map(str::to_owned));
            for col in self.columns_of(metadata, t) {
                let detail = match (&col.data_type, several, &owner) {
                    (Some(ty), true, Some(o)) => Some(format!("{o} · {ty}")),
                    (Some(ty), _, _) => Some(ty.clone()),
                    (None, true, Some(o)) => Some(o.clone()),
                    _ => None,
                };
                let insert = self.insert_ident(&col.name);
                out.push(&col.name, insert, CandidateKind::Column, detail, 8);
            }
        }
    }

    fn qualified_into(
        &self,
        out: &mut Collector,
        ctx: &CursorContext,
        metadata: &dyn MetadataProvider,
    ) {
        let q = &ctx.qualifier;
        // `alias.` or `table.`: that table's columns.
        if q.len() == 1
            && let Some(t) = ctx.scope.iter().find(|t| t.answers_to(&q[0]))
        {
            let columns = self.columns_of(metadata, t);
            // `schema.` being typed after FROM also parses as a table reference: fall through
            // to the schema case when that "table" has no columns.
            if !columns.is_empty() {
                for col in columns {
                    let insert = self.insert_ident(&col.name);
                    out.push(
                        &col.name,
                        insert,
                        CandidateKind::Column,
                        col.data_type.clone(),
                        8,
                    );
                }
                return;
            }
        }
        // `schema.table.`: columns.
        if q.len() == 2 {
            if let Some(cols) = metadata.columns(Some(&q[0]), &q[1]) {
                for col in cols {
                    let insert = self.insert_ident(&col.name);
                    out.push(
                        &col.name,
                        insert,
                        CandidateKind::Column,
                        col.data_type.clone(),
                        8,
                    );
                }
            }
            return;
        }
        // `schema.`: its objects.
        if q.len() == 1
            && metadata
                .schemas()
                .iter()
                .any(|s| s.eq_ignore_ascii_case(&q[0]))
        {
            self.tables_into(out, metadata, Some(&q[0]), 6);
            return;
        }
        // `table.` for a table not in scope (typed before its FROM clause): its columns.
        if q.len() == 1 {
            let t = TableRef {
                parts: vec![q[0].clone()],
                alias: None,
            };
            for col in self.columns_of(metadata, &t) {
                let insert = self.insert_ident(&col.name);
                out.push(
                    &col.name,
                    insert,
                    CandidateKind::Column,
                    col.data_type.clone(),
                    8,
                );
            }
        }
    }
}

struct Collector<'a> {
    prefix: &'a str,
    items: Vec<Candidate>,
}

impl<'a> Collector<'a> {
    fn new(prefix: &'a str) -> Self {
        Self {
            prefix,
            items: Vec::new(),
        }
    }

    fn push(
        &mut self,
        label: &str,
        insert: String,
        kind: CandidateKind,
        detail: Option<String>,
        boost: u32,
    ) {
        let Some(quality) = match_quality(label, self.prefix) else {
            return;
        };
        // Do not offer exactly what is already typed, unless it is a keyword completing case.
        if quality == MatchQuality::Exact && label == self.prefix {
            return;
        }
        let score = score(quality, boost, label.chars().count());
        if let Some(existing) = self
            .items
            .iter_mut()
            .find(|c| c.label == label && c.kind == kind)
        {
            existing.score = existing.score.max(score);
            return;
        }
        self.items.push(Candidate {
            label: label.to_owned(),
            insert,
            kind,
            detail,
            score,
        });
    }

    fn finish(mut self) -> Vec<Candidate> {
        self.items.sort_by(|a, b| {
            b.score.cmp(&a.score).then_with(|| {
                a.label
                    .to_ascii_lowercase()
                    .cmp(&b.label.to_ascii_lowercase())
            })
        });
        self.items
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::split::DialectSplitter;
    use ninesql_core::driver_api::AnsiDialect;
    use std::sync::Arc;

    struct Fake;

    impl MetadataProvider for Fake {
        fn schemas(&self) -> Vec<String> {
            vec!["public".into(), "audit".into()]
        }
        fn default_schema(&self) -> Option<String> {
            Some("public".into())
        }
        fn objects(&self, schema: Option<&str>) -> Vec<ObjectEntry> {
            let t = |n: &str, kind| ObjectEntry {
                name: n.into(),
                kind,
            };
            match schema {
                None | Some("public") => vec![
                    t("users", ObjectKind::Table),
                    t("user_roles", ObjectKind::Table),
                    t("orders", ObjectKind::Table),
                    t("active_users", ObjectKind::View),
                    t("now", ObjectKind::Function),
                    t("Mixed Case", ObjectKind::Table),
                ],
                Some("audit") => vec![t("events", ObjectKind::Table)],
                _ => Vec::new(),
            }
        }
        fn columns(&self, schema: Option<&str>, table: &str) -> Option<Vec<ColumnEntry>> {
            let c = |n: &str, ty: &str| ColumnEntry {
                name: n.into(),
                data_type: Some(ty.into()),
            };
            match (schema.unwrap_or("public"), table) {
                ("public", "users") => {
                    Some(vec![c("id", "int4"), c("name", "text"), c("email", "text")])
                }
                ("public", "orders") => Some(vec![
                    c("id", "int4"),
                    c("user_id", "int4"),
                    c("total", "numeric"),
                ]),
                ("audit", "events") => Some(vec![c("at", "timestamptz")]),
                _ => None,
            }
        }
    }

    fn complete(text_with_cursor: &str) -> Completions {
        let cursor = text_with_cursor.find('|').unwrap_or(text_with_cursor.len());
        let text = text_with_cursor.replacen('|', "", 1);
        let splitter = DialectSplitter::new(Arc::new(AnsiDialect));
        CompletionEngine::new(KeywordSet::Postgres).complete(&text, cursor, &splitter, &Fake)
    }

    fn labels(c: &Completions) -> Vec<&str> {
        c.candidates.iter().map(|c| c.label.as_str()).collect()
    }

    #[test]
    fn tables_after_from_ranked() {
        let c = complete("select * from us|");
        let l = labels(&c);
        assert_eq!(&l[..2], ["users", "user_roles"], "{l:?}");
        assert!(l.contains(&"active_users"), "part-prefix match: {l:?}");
        assert!(!l.contains(&"now"), "functions are not tables");
        assert_eq!(c.range, 14..16);
    }

    #[test]
    fn identifiers_needing_quotes_are_quoted() {
        let c = complete("select * from Mix|");
        let mixed = c.candidates.iter().find(|c| c.label == "Mixed Case");
        assert_eq!(mixed.map(|c| c.insert.as_str()), Some("\"Mixed Case\""));
    }

    #[test]
    fn alias_columns() {
        let c = complete("select o.| from orders o join users u on u.id = o.user_id");
        assert_eq!(labels(&c), ["id", "total", "user_id"]);
        let c = complete("select u.na| from orders o join users u on u.id = o.user_id");
        assert_eq!(labels(&c), ["name"]);
        // The table name is not an alias once the table has one.
        let c = complete("select users.| from users u");
        assert_eq!(
            labels(&c),
            ["id", "name", "email"],
            "falls back to the table's own columns"
        );
    }

    #[test]
    fn columns_in_scope_then_keywords() {
        let c = complete("select * from orders o, users where to|");
        assert_eq!(c.candidates[0].label, "total");
        let c = complete("select | from orders o join users u on true");
        let l = labels(&c);
        let first_kw = c
            .candidates
            .iter()
            .position(|c| c.kind == CandidateKind::Keyword)
            .unwrap_or(0);
        let last_col = c
            .candidates
            .iter()
            .rposition(|c| c.kind == CandidateKind::Column)
            .unwrap_or(0);
        assert!(last_col < first_kw, "columns rank above keywords: {l:?}");
        let user_id = c.candidates.iter().find(|c| c.label == "user_id");
        assert_eq!(user_id.and_then(|c| c.detail.as_deref()), Some("o · int4"));
        assert!(
            l.contains(&"o") && l.contains(&"u"),
            "aliases offered: {l:?}"
        );
    }

    #[test]
    fn schema_qualified() {
        assert_eq!(labels(&complete("select * from audit.|")), ["events"]);
        assert_eq!(
            labels(&complete("select audit.events.| from audit.events")),
            ["at"]
        );
    }

    #[test]
    fn keywords_follow_typed_case() {
        let c = complete("sel|");
        assert_eq!(c.candidates[0].label, "SELECT");
        assert_eq!(c.candidates[0].insert, "select");
        let c = complete("SEL|");
        assert_eq!(c.candidates[0].insert, "SELECT");
        let c = complete("ret|");
        assert!(labels(&c).contains(&"RETURNING"), "dialect keyword");
    }

    #[test]
    fn ctes_offered_as_tables() {
        let c = complete("with recent as (select 1) select * from rec|");
        assert_eq!(c.candidates[0].label, "recent");
        assert_eq!(c.candidates[0].kind, CandidateKind::Cte);
    }

    #[test]
    fn nothing_inside_strings_or_without_metadata() {
        assert!(complete("select 'fr|'").candidates.is_empty());
        let splitter = DialectSplitter::new(Arc::new(AnsiDialect));
        let c = CompletionEngine::new(KeywordSet::Ansi).complete(
            "select * from ",
            14,
            &splitter,
            &NoMetadata,
        );
        assert!(c.candidates.is_empty());
    }
}
