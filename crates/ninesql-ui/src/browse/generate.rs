//! "Generate SQL" (F-NAV-07): `SELECT`, `INSERT`, `UPDATE` and `DELETE` templates for a table or
//! view, quoted with the session dialect's `quote_identifier`.
//!
//! The templates only open in a new SQL editor; nothing runs them. Values and conditions are
//! `<placeholders>`, which no engine accepts, so a template run without editing fails instead of
//! writing; `UPDATE` and `DELETE` always carry a `WHERE` line. Running an edited template goes
//! through the normal execution flow and its production confirmation (I3).
//!
//! Safety rules (P4-B security review):
//! - **Placeholders never carry a name raw** (M1): only `[A-Za-z0-9_]` is kept, everything else
//!   becomes `_`, the length is capped, and a name with no ASCII letter or digit becomes
//!   `<value_N>`. A column name can therefore never add a statement, a comment or a line to a
//!   template; only quoted identifiers (the dialect's quoting) carry real names.
//! - **Names the editor would change are refused** (M2): [`names_survive_editor`] says whether
//!   every name survives the editor's sanitizing (`ninesql_editor::hygiene`); the caller refuses
//!   to open a template otherwise, so a template never targets another object than the one shown.
//! - **A key condition only from complete metadata** (M3): from the primary-key constraint when
//!   the driver lists it, else from the column flags only when the column node is complete and
//!   nothing is hidden by a navigator filter; otherwise `WHERE <condition>` with a comment saying
//!   why. Column lists built from a filtered or incomplete node carry a comment too.

use ninesql_core::NodeChildren;
use ninesql_core::driver_api::ObjectKind;
use ninesql_core::driver_api::capabilities::EditingCapabilities;
use ninesql_core::driver_api::metadata::{ConstraintKind, ObjectDetails, ObjectInfo, ObjectPath};
use ninesql_editor::hygiene::needs_sanitizing;

/// Longest placeholder name kept (characters, without the brackets).
const MAX_PLACEHOLDER: usize = 32;

/// Comment added when the column list comes from a filtered or incomplete node.
pub const COLUMNS_INCOMPLETE: &str =
    "-- Some columns are not listed (a navigator filter hides them, or the list is incomplete).";
/// Comment added when the key condition cannot be built from complete metadata.
pub const KEY_UNKNOWN: &str = "-- The primary key could not be read in full (filtered or incomplete metadata): write the condition.";

/// A template kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Template {
    /// `SELECT … FROM …`.
    Select,
    /// `INSERT INTO … VALUES …`.
    Insert,
    /// `UPDATE … SET … WHERE …`.
    Update,
    /// `DELETE FROM … WHERE …`.
    Delete,
}

impl Template {
    /// Every template, in menu order.
    pub const ALL: [Self; 4] = [Self::Select, Self::Insert, Self::Update, Self::Delete];

    /// Its label ("SELECT").
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Select => "SELECT",
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
        }
    }

    /// Whether the template writes (offered only on sessions that are not read-only, I8).
    #[must_use]
    pub fn writes(self) -> bool {
        !matches!(self, Self::Select)
    }

    /// Whether the template applies to objects of `kind`: `SELECT` to every table-like
    /// object, the DML templates to tables only (a view or a materialized view is not offered
    /// row writes; review L2).
    #[must_use]
    pub fn applies_to(self, kind: ObjectKind) -> bool {
        match self {
            Self::Select => super::is_table_like(kind),
            _ => matches!(kind, ObjectKind::Table),
        }
    }

    /// Whether the template is offered for `kind` with what the session `allows`.
    #[must_use]
    pub fn offered(self, kind: ObjectKind, allows: TemplateAllowance) -> bool {
        self.applies_to(kind) && allows.allows(self)
    }
}

/// Which templates a session allows (I8, review L2): `SELECT` always; `INSERT`, `UPDATE` and
/// `DELETE` only on a session that is not read-only and whose driver declares the matching row
/// edit in `Capabilities::editing` (QuestDB declares none). With no open session only `SELECT`
/// is offered: the capabilities are not known yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TemplateAllowance {
    /// `INSERT` is offered.
    pub insert: bool,
    /// `UPDATE` is offered.
    pub update: bool,
    /// `DELETE` is offered.
    pub delete: bool,
}

impl TemplateAllowance {
    /// `SELECT` only (no session, or a read-only one).
    #[must_use]
    pub fn select_only() -> Self {
        Self::default()
    }

    /// Every template.
    #[must_use]
    pub fn all() -> Self {
        Self {
            insert: true,
            update: true,
            delete: true,
        }
    }

    /// What a session that is `read_only` or not, with these declared editing capabilities,
    /// allows.
    #[must_use]
    pub fn of(read_only: bool, editing: &EditingCapabilities) -> Self {
        if read_only {
            return Self::select_only();
        }
        Self {
            insert: editing.insert,
            update: editing.update,
            delete: editing.delete,
        }
    }

    /// What `session` allows.
    #[must_use]
    pub fn of_session(session: &ninesql_core::SessionHandle) -> Self {
        let info = session.info();
        Self::of(info.read_only, &info.capabilities.editing)
    }

    /// Whether `template` is allowed.
    #[must_use]
    pub fn allows(self, template: Template) -> bool {
        match template {
            Template::Select => true,
            Template::Insert => self.insert,
            Template::Update => self.update,
            Template::Delete => self.delete,
        }
    }
}

/// A column of the object, as the template needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateColumn {
    /// Unquoted name.
    pub name: String,
    /// Part of the primary key (per the column details).
    pub primary_key: bool,
    /// Generated by the engine (left out of `INSERT` and `SET`).
    pub auto_generated: bool,
}

impl TemplateColumn {
    /// The columns of listed column objects, in table order.
    #[must_use]
    pub fn from_infos(items: &[ObjectInfo]) -> Vec<Self> {
        let mut cols: Vec<(u32, Self)> = items
            .iter()
            .map(|i| match &i.details {
                ObjectDetails::Column(c) => (
                    c.ordinal,
                    Self {
                        name: i.name().to_owned(),
                        primary_key: c.primary_key,
                        auto_generated: c.auto_generated,
                    },
                ),
                _ => (
                    0,
                    Self {
                        name: i.name().to_owned(),
                        primary_key: false,
                        auto_generated: false,
                    },
                ),
            })
            .collect();
        cols.sort_by_key(|(o, _)| *o);
        cols.into_iter().map(|(_, c)| c).collect()
    }
}

/// Where the key condition comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// The key columns, in key order.
    Known(Vec<String>),
    /// The metadata is complete and the object has no primary key.
    None,
    /// The metadata is filtered or incomplete: the key cannot be trusted.
    Unknown,
}

/// What a template is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateInput {
    /// Columns in table order (empty when not listed).
    pub columns: Vec<TemplateColumn>,
    /// The column list is the whole list: its node is complete and nothing is hidden by a
    /// navigator filter. `true` when the driver lists no columns (nothing to miss).
    pub columns_complete: bool,
    /// The key condition's source.
    pub key: KeySource,
}

impl TemplateInput {
    /// No column metadata (the driver lists none): `SELECT *` and `WHERE <condition>`.
    #[must_use]
    pub fn none() -> Self {
        Self {
            columns: Vec::new(),
            columns_complete: true,
            key: KeySource::None,
        }
    }

    /// The input from the column node and the constraint node, when the driver lists them.
    #[must_use]
    pub fn from_nodes(columns: Option<&NodeChildren>, constraints: Option<&NodeChildren>) -> Self {
        let whole = |n: &NodeChildren| n.complete && n.hidden_by_filter == 0;
        let cols = columns
            .map(|c| TemplateColumn::from_infos(&c.items))
            .unwrap_or_default();
        let columns_complete = columns.is_none_or(whole);
        let pk_constraint = constraints.and_then(|c| {
            c.items.iter().find_map(|i| match &i.details {
                ObjectDetails::Constraint(ci)
                    if ci.kind == ConstraintKind::PrimaryKey && !ci.columns.is_empty() =>
                {
                    Some(ci.columns.clone())
                }
                _ => None,
            })
        });
        let constraints_whole = constraints.is_none_or(whole);
        let key = if let Some(pk) = pk_constraint {
            KeySource::Known(pk)
        } else if !constraints_whole {
            // A primary key may be among the hidden or unlisted constraints.
            KeySource::Unknown
        } else if columns.is_some() && columns_complete {
            let flagged: Vec<String> = cols
                .iter()
                .filter(|c| c.primary_key)
                .map(|c| c.name.clone())
                .collect();
            if flagged.is_empty() {
                KeySource::None
            } else {
                KeySource::Known(flagged)
            }
        } else if columns.is_none() {
            KeySource::None
        } else {
            KeySource::Unknown
        };
        Self {
            columns: cols,
            columns_complete,
            key,
        }
    }
}

/// Whether every name of `path` and of `input` (its columns and the columns of a known
/// primary key, which may not be among the listed columns) reaches the editor unchanged (no
/// control character, carriage return or line / paragraph separator, which the editor strips
/// or rewrites). A template is refused otherwise (M2).
#[must_use]
pub fn names_survive_editor(path: &ObjectPath, input: &TemplateInput) -> bool {
    let keys: &[String] = match &input.key {
        KeySource::Known(k) => k,
        _ => &[],
    };
    path.0.iter().all(|s| !needs_sanitizing(&s.name))
        && input.columns.iter().all(|c| !needs_sanitizing(&c.name))
        && keys.iter().all(|k| !needs_sanitizing(k))
}

/// The qualified name of an object: its nearest schema (else its catalog) and its name, each
/// quoted with `quote`.
#[must_use]
pub fn qualified_name(path: &ObjectPath, quote: &dyn Fn(&str) -> String) -> String {
    let Some((leaf, parents)) = path.0.split_last() else {
        return String::new();
    };
    let container = parents
        .iter()
        .rev()
        .find(|s| s.kind == ObjectKind::Schema)
        .or_else(|| parents.iter().rev().find(|s| s.kind == ObjectKind::Catalog));
    match container {
        Some(c) => format!("{}.{}", quote(&c.name), quote(&leaf.name)),
        None => quote(&leaf.name),
    }
}

/// The placeholder of the value of column number `position` (1-based) named `name`:
/// `<name>` with only `[A-Za-z0-9_]` kept (anything else becomes `_`), at most
/// [`MAX_PLACEHOLDER`] characters, or `<value_N>` when the name has no ASCII letter or digit.
#[must_use]
pub fn placeholder(name: &str, position: usize) -> String {
    let clean: String = name
        .chars()
        .take(MAX_PLACEHOLDER)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.chars().any(|c| c.is_ascii_alphanumeric()) {
        format!("<{clean}>")
    } else {
        format!("<value_{position}>")
    }
}

/// The template text for `path` from `input`.
#[must_use]
pub fn template(
    which: Template,
    path: &ObjectPath,
    input: &TemplateInput,
    quote: &dyn Fn(&str) -> String,
) -> String {
    let table = qualified_name(path, quote);
    let columns = &input.columns;
    let position = |name: &str| {
        columns
            .iter()
            .position(|c| c.name == name)
            .map_or(columns.len() + 1, |p| p + 1)
    };
    let ph = |name: &str| placeholder(name, position(name));
    let writable: Vec<&TemplateColumn> = columns.iter().filter(|c| !c.auto_generated).collect();
    let mut notes: Vec<&str> = Vec::new();
    if !input.columns_complete && which != Template::Delete {
        notes.push(COLUMNS_INCOMPLETE);
    }
    let condition = match &input.key {
        KeySource::Known(keys) if !keys.is_empty() => keys
            .iter()
            .map(|k| format!("{} = {}", quote(k), ph(k)))
            .collect::<Vec<_>>()
            .join("\n  AND "),
        KeySource::Unknown => {
            if matches!(which, Template::Update | Template::Delete) {
                notes.push(KEY_UNKNOWN);
            }
            "<condition>".to_owned()
        }
        _ => "<condition>".to_owned(),
    };
    let body = match which {
        Template::Select => {
            if columns.is_empty() {
                format!("SELECT *\nFROM {table};\n")
            } else {
                let list = columns
                    .iter()
                    .map(|c| format!("  {}", quote(&c.name)))
                    .collect::<Vec<_>>()
                    .join(",\n");
                format!("SELECT\n{list}\nFROM {table};\n")
            }
        }
        Template::Insert => {
            if writable.is_empty() {
                format!("INSERT INTO {table} (<columns>)\nVALUES (<values>);\n")
            } else {
                let names = writable
                    .iter()
                    .map(|c| quote(&c.name))
                    .collect::<Vec<_>>()
                    .join(", ");
                let values = writable
                    .iter()
                    .map(|c| ph(&c.name))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("INSERT INTO {table} ({names})\nVALUES ({values});\n")
            }
        }
        Template::Update => {
            let keys: &[String] = match &input.key {
                KeySource::Known(k) => k,
                _ => &[],
            };
            let set: Vec<String> = writable
                .iter()
                .filter(|c| !c.primary_key && !keys.contains(&c.name))
                .map(|c| format!("  {} = {}", quote(&c.name), ph(&c.name)))
                .collect();
            let set = if set.is_empty() {
                "  <column> = <value>".to_owned()
            } else {
                set.join(",\n")
            };
            format!("UPDATE {table}\nSET\n{set}\nWHERE {condition};\n")
        }
        Template::Delete => format!("DELETE FROM {table}\nWHERE {condition};\n"),
    };
    let mut out = String::new();
    for n in notes {
        out.push_str(n);
        out.push('\n');
    }
    out.push_str(&body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninesql_core::driver_api::dialect::{LexicalRules, split_with_rules};
    use ninesql_core::driver_api::metadata::{ColumnInfo, ObjectInfo};

    fn ansi(s: &str) -> String {
        format!("\"{}\"", s.replace('"', "\"\""))
    }

    fn mysql(s: &str) -> String {
        format!("`{}`", s.replace('`', "``"))
    }

    fn table() -> ObjectPath {
        ObjectPath::root()
            .child(ObjectKind::Catalog, "nine")
            .child(ObjectKind::Schema, "public")
            .child(ObjectKind::Table, "order \"items\"")
    }

    fn cols() -> Vec<TemplateColumn> {
        let mut id = ColumnInfo::new(1, "bigint");
        id.primary_key = true;
        id.auto_generated = true;
        let label = ColumnInfo::new(2, "text");
        let p = table();
        TemplateColumn::from_infos(&[
            ObjectInfo {
                path: p.child(ObjectKind::Column, "label"),
                comment: None,
                details: ObjectDetails::Column(label),
            },
            ObjectInfo {
                path: p.child(ObjectKind::Column, "id"),
                comment: None,
                details: ObjectDetails::Column(id),
            },
        ])
    }

    fn complete(columns: Vec<TemplateColumn>) -> TemplateInput {
        let k: Vec<String> = columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.clone())
            .collect();
        TemplateInput {
            columns,
            columns_complete: true,
            key: if k.is_empty() {
                KeySource::None
            } else {
                KeySource::Known(k)
            },
        }
    }

    fn statements(text: &str) -> usize {
        split_with_rules(text, LexicalRules::POSTGRES)
            .iter()
            .filter(|s| !s.text(text).trim().is_empty())
            .count()
    }

    #[test]
    fn names_are_quoted_with_the_dialect_and_qualified_by_the_schema() {
        assert_eq!(
            qualified_name(&table(), &ansi),
            "\"public\".\"order \"\"items\"\"\""
        );
        let mysql_table = ObjectPath::root()
            .child(ObjectKind::Catalog, "shop")
            .child(ObjectKind::Table, "a`b");
        assert_eq!(qualified_name(&mysql_table, &mysql), "`shop`.`a``b`");
        let bare = ObjectPath::root().child(ObjectKind::Table, "t");
        assert_eq!(qualified_name(&bare, &ansi), "\"t\"");
    }

    #[test]
    fn templates_follow_the_columns() {
        let c = cols();
        assert_eq!(c[0].name, "id", "table order");
        let input = complete(c);
        let select = template(Template::Select, &table(), &input, &ansi);
        assert_eq!(
            select,
            "SELECT\n  \"id\",\n  \"label\"\nFROM \"public\".\"order \"\"items\"\"\";\n"
        );
        let insert = template(Template::Insert, &table(), &input, &ansi);
        assert!(
            insert.contains("(\"label\")\nVALUES (<label>)"),
            "auto-generated columns are left out: {insert}"
        );
        let update = template(Template::Update, &table(), &input, &ansi);
        assert!(update.contains("SET\n  \"label\" = <label>"), "{update}");
        assert!(update.contains("WHERE \"id\" = <id>;"), "{update}");
        let delete = template(Template::Delete, &table(), &input, &ansi);
        assert!(delete.contains("WHERE \"id\" = <id>;"), "{delete}");
    }

    #[test]
    fn update_and_delete_always_have_a_where_placeholder() {
        for input in [TemplateInput::none(), complete(cols())] {
            for which in [Template::Update, Template::Delete] {
                let text = template(which, &table(), &input, &ansi);
                let where_line = text
                    .lines()
                    .find(|l| l.starts_with("WHERE "))
                    .unwrap_or_else(|| panic!("no WHERE in {text}"));
                assert!(where_line.contains('<'), "a placeholder: {where_line}");
            }
        }
        let without = template(Template::Delete, &table(), &TemplateInput::none(), &ansi);
        assert!(without.ends_with("WHERE <condition>;\n"));
        let select = template(Template::Select, &table(), &TemplateInput::none(), &ansi);
        assert!(select.starts_with("SELECT *\n"));
    }

    /// M1: a hostile column name never adds a statement, a comment or a line.
    #[test]
    fn hostile_names_stay_inside_quotes_and_placeholders_are_plain() {
        let p = ObjectPath::root().child(ObjectKind::Table, "t");
        let hostile = [
            "x;DELETE FROM payroll;--",
            "a--b",
            "a/*b",
            "*/x",
            "a\u{2028}b",
            "a\u{202e}b\u{2066}c",
            "';--",
            "\u{feff}",
            "<x>",
        ];
        for (i, name) in hostile.iter().enumerate() {
            let columns = vec![
                TemplateColumn {
                    name: (*name).to_owned(),
                    primary_key: true,
                    auto_generated: false,
                },
                TemplateColumn {
                    name: format!("v{i}{name}"),
                    primary_key: false,
                    auto_generated: false,
                },
            ];
            let input = complete(columns);
            for which in Template::ALL {
                let text = template(which, &p, &input, &ansi);
                assert_eq!(statements(&text), 1, "{which:?} for {name:?}: {text}");
                for (at, _) in text.match_indices('<') {
                    let rest = &text[at + 1..];
                    let end = rest.find('>').unwrap_or_else(|| panic!("{text}"));
                    assert!(
                        rest[..end]
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_'),
                        "plain placeholder in {text}"
                    );
                }
            }
        }
        assert_eq!(
            placeholder("x;DELETE FROM payroll;--", 1),
            "<x_DELETE_FROM_payroll___>"
        );
        assert_eq!(placeholder("é€", 3), "<value_3>");
        assert_eq!(placeholder(&"a".repeat(100), 1).len(), MAX_PLACEHOLDER + 2);
    }

    /// M2: names the editor would change are detected.
    #[test]
    fn names_that_the_editor_would_change_are_detected() {
        let ok = ObjectPath::root().child(ObjectKind::Table, "orders");
        assert!(names_survive_editor(&ok, &complete(cols())));
        for bad in [
            "orders\u{1}",
            "orders\r",
            "a\u{2028}b",
            "a\u{2029}",
            "x\u{7f}",
        ] {
            let p = ObjectPath::root().child(ObjectKind::Table, bad);
            assert!(!names_survive_editor(&p, &TemplateInput::none()), "{bad:?}");
            let c = vec![TemplateColumn {
                name: bad.to_owned(),
                primary_key: false,
                auto_generated: false,
            }];
            assert!(!names_survive_editor(&ok, &complete(c)), "{bad:?}");
            // A key column that is not among the (filtered) listed columns is checked too.
            let filtered = TemplateInput {
                columns: cols(),
                columns_complete: false,
                key: KeySource::Known(vec!["tenant".into(), bad.to_owned()]),
            };
            assert!(!names_survive_editor(&ok, &filtered), "key {bad:?}");
        }
    }

    /// M3: no key condition from a filtered or incomplete column list.
    #[test]
    fn filtered_or_incomplete_columns_never_make_a_key() {
        let columns = vec![TemplateColumn {
            name: "id".into(),
            primary_key: true,
            auto_generated: false,
        }];
        let input = TemplateInput {
            columns,
            columns_complete: false,
            key: KeySource::Unknown,
        };
        let delete = template(Template::Delete, &table(), &input, &ansi);
        assert_eq!(
            delete,
            format!(
                "{KEY_UNKNOWN}\nDELETE FROM \"public\".\"order \"\"items\"\"\"\nWHERE <condition>;\n"
            )
        );
        let update = template(Template::Update, &table(), &input, &ansi);
        assert!(
            update.starts_with(&format!("{COLUMNS_INCOMPLETE}\n{KEY_UNKNOWN}\n")),
            "{update}"
        );
        assert!(update.contains("WHERE <condition>;"));
        for which in [Template::Select, Template::Insert] {
            let text = template(which, &table(), &input, &ansi);
            assert!(text.starts_with(COLUMNS_INCOMPLETE), "{text}");
            assert_eq!(statements(&text), 1);
        }
        // A key from the primary-key constraint is trusted even when columns are filtered.
        let input = TemplateInput {
            key: KeySource::Known(vec!["tenant_id".into(), "id".into()]),
            ..input
        };
        let delete = template(Template::Delete, &table(), &input, &ansi);
        assert!(
            delete.ends_with("WHERE \"tenant_id\" = <tenant_id>\n  AND \"id\" = <id>;\n"),
            "{delete}"
        );
    }

    #[test]
    fn templates_are_offered_by_kind_and_session() {
        let all = TemplateAllowance::all();
        assert!(Template::Select.applies_to(ObjectKind::MaterializedView));
        assert!(!Template::Delete.applies_to(ObjectKind::MaterializedView));
        assert!(!Template::Select.applies_to(ObjectKind::Column));
        let read_only = TemplateAllowance::of(true, &full_editing());
        assert!(Template::Select.offered(ObjectKind::Table, read_only));
        for t in [Template::Insert, Template::Update, Template::Delete] {
            assert!(
                !t.offered(ObjectKind::Table, read_only),
                "{t:?} on read-only"
            );
            assert!(t.offered(ObjectKind::Table, all));
            // Review L2: no row write on a view, whatever the session allows.
            assert!(!t.applies_to(ObjectKind::View), "{t:?} on a view");
            assert!(!t.offered(ObjectKind::View, all), "{t:?} on a view");
        }
        assert!(Template::Select.offered(ObjectKind::View, all));
    }

    fn full_editing() -> EditingCapabilities {
        let mut e = EditingCapabilities::default();
        e.insert = true;
        e.update = true;
        e.delete = true;
        e
    }

    /// Review L2: the write templates follow the driver's declared `Capabilities::editing`.
    #[test]
    fn write_templates_follow_the_declared_editing_capabilities() {
        // QuestDB declares no row editing: SELECT only.
        let none = TemplateAllowance::of(false, &EditingCapabilities::default());
        assert_eq!(none, TemplateAllowance::select_only());
        for t in Template::ALL {
            assert_eq!(
                t.offered(ObjectKind::Table, none),
                t == Template::Select,
                "{t:?}"
            );
        }
        // Each write template on its own capability.
        let mut insert_only = EditingCapabilities::default();
        insert_only.insert = true;
        let a = TemplateAllowance::of(false, &insert_only);
        assert!(Template::Insert.offered(ObjectKind::Table, a));
        assert!(!Template::Update.offered(ObjectKind::Table, a));
        assert!(!Template::Delete.offered(ObjectKind::Table, a));
        assert_eq!(
            TemplateAllowance::of(false, &full_editing()),
            TemplateAllowance::all()
        );
    }
}
