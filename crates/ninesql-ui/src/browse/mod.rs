//! Metadata browsing (P4-B): the navigator's object tree (F-NAV-03), navigator filters
//! (F-NAV-04), "Find object" (F-NAV-05), the object editor (F-NAV-06), "Generate SQL"
//! (F-NAV-07) and the editor's object completion.
//!
//! Everything here reads `ninesql_core::MetadataService` through the hub
//! ([`crate::connections::hub::ConnectionHub::metadata`]). Each call returns at once; answers
//! (`MetadataRequest`, `Reply`) are awaited in GPUI tasks, which only wait on channels filled by
//! the driver runtime, so the foreground thread never waits on I/O (I6). Dropping a task drops
//! its request, which cancels the driver call when no one else waits for it.
//!
//! - [`tree`]: the pure tree model (expansion, load states, flattened rows), no GPUI.
//! - [`generate`]: the SQL templates of "Generate SQL", quoted with the session dialect.
//! - [`completion`]: the editor's `MetadataProvider` over `completion_snapshot`.
//! - [`filters`], [`search`], [`object_editor`]: the dialogs and the object editor tab.

pub mod completion;
pub mod filters;
pub mod generate;
pub mod object_editor;
pub mod search;
pub mod tree;

#[cfg(test)]
mod tests;

use ninesql_core::driver_api::ObjectKind;
use ninesql_core::driver_api::metadata::ObjectPath;

/// The label of a group of objects of `kind` in the tree ("Tables").
#[must_use]
pub fn kind_plural(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Catalog => "Databases",
        ObjectKind::Schema => "Schemas",
        ObjectKind::Table => "Tables",
        ObjectKind::View => "Views",
        ObjectKind::MaterializedView => "Materialized views",
        ObjectKind::Column => "Columns",
        ObjectKind::Index => "Indexes",
        ObjectKind::Constraint => "Constraints",
        ObjectKind::ForeignKey => "Foreign keys",
        ObjectKind::Trigger => "Triggers",
        ObjectKind::Routine => "Routines",
        ObjectKind::Sequence => "Sequences",
        ObjectKind::Type => "Types",
        ObjectKind::Partition => "Partitions",
        ObjectKind::User => "Users",
        ObjectKind::Role => "Roles",
        ObjectKind::Collection => "Collections",
        ObjectKind::Key => "Keys",
        ObjectKind::Label => "Labels",
        ObjectKind::RelationshipType => "Relationship types",
        _ => "Objects",
    }
}

/// The label of one object of `kind` ("table").
#[must_use]
pub fn kind_singular(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Catalog => "database",
        ObjectKind::Schema => "schema",
        ObjectKind::Table => "table",
        ObjectKind::View => "view",
        ObjectKind::MaterializedView => "materialized view",
        ObjectKind::Column => "column",
        ObjectKind::Index => "index",
        ObjectKind::Constraint => "constraint",
        ObjectKind::ForeignKey => "foreign key",
        ObjectKind::Trigger => "trigger",
        ObjectKind::Routine => "routine",
        ObjectKind::Sequence => "sequence",
        ObjectKind::Type => "type",
        ObjectKind::Partition => "partition",
        ObjectKind::User => "user",
        ObjectKind::Role => "role",
        ObjectKind::Collection => "collection",
        ObjectKind::Key => "key",
        ObjectKind::Label => "label",
        ObjectKind::RelationshipType => "relationship type",
        _ => "object",
    }
}

/// Kinds that open in the object editor and offer "Generate SQL" (table-like objects).
#[must_use]
pub fn is_table_like(kind: ObjectKind) -> bool {
    matches!(
        kind,
        ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView
    )
}

/// A display form of a path, `nine.public.t000`. Names are shown as stored: this is a label,
/// never SQL ([`generate`] quotes with the dialect).
#[must_use]
pub fn display_path(path: &ObjectPath) -> String {
    path.0
        .iter()
        .map(|s| display_name(&s.name))
        .collect::<Vec<_>>()
        .join(".")
}

/// Longest label shown for a name (characters); longer ones end with `…`.
pub const MAX_LABEL: usize = 200;
/// Longest engine message shown in the tree or the object editor (characters).
pub const MAX_MESSAGE: usize = 400;

/// The visible marker that replaces invisible or line-breaking characters in labels.
pub const INVISIBLE_MARKER: char = '\u{FFFD}';

/// Whether `c` is shown as [`INVISIBLE_MARKER`] in a label: control characters, line and
/// paragraph separators, bidirectional controls and zero-width / format characters, which
/// would hide or reorder text (P4-B review L1).
#[must_use]
pub fn is_invisible(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
        )
}

/// A name as a one-line label: invisible characters ([`is_invisible`]) replaced by
/// [`INVISIBLE_MARKER`], at most [`MAX_LABEL`] characters.
#[must_use]
pub fn display_name(name: &str) -> String {
    display_capped(name, MAX_LABEL)
}

/// An engine message as shown in the tree or the object editor: like [`display_name`], at
/// most [`MAX_MESSAGE`] characters (P4-B review L3).
#[must_use]
pub fn display_message(message: &str) -> String {
    display_capped(message, MAX_MESSAGE)
}

fn display_capped(text: &str, max: usize) -> String {
    let mut out: String = text
        .chars()
        .take(max)
        .map(|c| if is_invisible(c) { INVISIBLE_MARKER } else { c })
        .collect();
    if text.chars().nth(max).is_some() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod label_tests {
    use super::*;

    #[test]
    fn labels_show_invisible_characters_and_are_capped() {
        for c in [
            '\u{1}', '\n', '\u{2028}', '\u{202E}', '\u{2066}', '\u{200E}', '\u{200B}', '\u{2060}',
            '\u{FEFF}',
        ] {
            let shown = display_name(&format!("a{c}b"));
            assert_eq!(
                shown,
                format!("a{INVISIBLE_MARKER}b"),
                "{:04X}",
                u32::from(c)
            );
        }
        assert_eq!(display_name("é漢"), "é漢");
        let long = display_name(&"x".repeat(500));
        assert_eq!(long.chars().count(), MAX_LABEL + 1);
        assert!(long.ends_with('…'));
        assert_eq!(
            display_message(&"y".repeat(MAX_MESSAGE)).chars().count(),
            MAX_MESSAGE
        );
    }
}
