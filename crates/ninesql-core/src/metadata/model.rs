//! The one place where the driver API's metadata types are read by the core: kind names (for
//! `navigator.json` and logs), the memory estimate of a listed object, and what search and
//! completion extract from an [`ObjectInfo`]. When P4-A adds fields to [`ObjectDetails`]
//! (column details, foreign-key targets, index columns), only this module changes.

use ninesql_driver_api::ObjectKind;
use ninesql_driver_api::metadata::{ObjectDetails, ObjectInfo, ObjectPath};

/// Fixed cost charged per listed object on top of its strings (struct, vector slots, hash
/// entries).
const OBJECT_OVERHEAD: usize = 96;
/// Fixed cost charged per path segment on top of its name.
const SEGMENT_OVERHEAD: usize = 32;

/// Every kind with its stable name, in declaration order. Kinds the core does not know yet
/// (the enum is non-exhaustive) have no name and cannot be filtered or stored.
pub const KIND_NAMES: &[(ObjectKind, &str)] = &[
    (ObjectKind::Catalog, "catalog"),
    (ObjectKind::Schema, "schema"),
    (ObjectKind::Table, "table"),
    (ObjectKind::View, "view"),
    (ObjectKind::MaterializedView, "materialized-view"),
    (ObjectKind::Column, "column"),
    (ObjectKind::Index, "index"),
    (ObjectKind::Constraint, "constraint"),
    (ObjectKind::ForeignKey, "foreign-key"),
    (ObjectKind::Trigger, "trigger"),
    (ObjectKind::Routine, "routine"),
    (ObjectKind::Sequence, "sequence"),
    (ObjectKind::Type, "type"),
    (ObjectKind::Partition, "partition"),
    (ObjectKind::User, "user"),
    (ObjectKind::Role, "role"),
    (ObjectKind::Collection, "collection"),
    (ObjectKind::Key, "key"),
    (ObjectKind::Label, "label"),
    (ObjectKind::RelationshipType, "relationship-type"),
];

/// The stable name of a kind (`"materialized-view"`), `None` for a kind this build does not
/// know.
#[must_use]
pub fn kind_name(kind: ObjectKind) -> Option<&'static str> {
    KIND_NAMES.iter().find(|(k, _)| *k == kind).map(|(_, n)| *n)
}

/// The kind with that stable name.
#[must_use]
pub fn kind_from_name(name: &str) -> Option<ObjectKind> {
    KIND_NAMES.iter().find(|(_, n)| *n == name).map(|(k, _)| *k)
}

/// Kinds completion offers as objects of a schema (tables, views, routines).
#[must_use]
pub fn is_relation(kind: ObjectKind) -> bool {
    matches!(
        kind,
        ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView
    )
}

/// Kinds that act as a schema for completion: the nearest schema, else the catalog (engines
/// without schemas).
#[must_use]
pub fn is_container(kind: ObjectKind) -> bool {
    matches!(kind, ObjectKind::Schema | ObjectKind::Catalog)
}

/// Estimated bytes held by a path.
#[must_use]
pub fn path_bytes(path: &ObjectPath) -> usize {
    path.0
        .iter()
        .map(|s| s.name.len() + SEGMENT_OVERHEAD)
        .sum::<usize>()
}

/// Estimated bytes held by one listed object: its path, comment and details. An estimate on
/// the high side, so the cache stays under its budget.
#[must_use]
pub fn object_bytes(info: &ObjectInfo) -> usize {
    let details = match &info.details {
        ObjectDetails::Column(c) => {
            c.data_type.len() + c.default.as_ref().map_or(0, String::len) + 16
        }
        ObjectDetails::Table(t) => t.flavour.as_ref().map_or(0, String::len) + 16,
        // `None`, and kinds of details added after this build.
        _ => 0,
    };
    OBJECT_OVERHEAD
        + path_bytes(&info.path)
        + info.comment.as_ref().map_or(0, String::len)
        + details
}

/// The engine type name of a column, for completion's detail line.
#[must_use]
pub fn column_type(info: &ObjectInfo) -> Option<&str> {
    match &info.details {
        ObjectDetails::Column(c) if !c.data_type.is_empty() => Some(c.data_type.as_str()),
        _ => None,
    }
}

/// The ordinal of a column (`0` when unknown), to keep completion in table order.
#[must_use]
pub fn column_ordinal(info: &ObjectInfo) -> u32 {
    match &info.details {
        ObjectDetails::Column(c) => c.ordinal,
        _ => 0,
    }
}

/// The name of the nearest container segment (schema, else catalog) of `path`, searching
/// from the end.
#[must_use]
pub fn container_name(path: &ObjectPath) -> Option<&str> {
    path.0
        .iter()
        .rev()
        .find(|s| s.kind == ObjectKind::Schema)
        .or_else(|| path.0.iter().rev().find(|s| s.kind == ObjectKind::Catalog))
        .map(|s| s.name.as_str())
}

/// `true` when `path` is `prefix` or lies under it.
#[must_use]
pub fn is_under(path: &ObjectPath, prefix: &ObjectPath) -> bool {
    path.0.len() >= prefix.0.len() && path.0[..prefix.0.len()] == prefix.0[..]
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;

    #[test]
    fn kind_names_round_trip_and_are_unique() {
        let mut names = std::collections::BTreeSet::new();
        for (kind, name) in KIND_NAMES {
            assert_eq!(kind_from_name(name), Some(*kind));
            assert_eq!(kind_name(*kind), Some(*name));
            assert!(names.insert(*name));
        }
        assert_eq!(kind_from_name("Table"), None);
    }

    #[test]
    fn containers_and_prefixes() {
        let p = ObjectPath::root()
            .child(ObjectKind::Catalog, "app")
            .child(ObjectKind::Schema, "public")
            .child(ObjectKind::Table, "users");
        assert_eq!(container_name(&p), Some("public"));
        let c = ObjectPath::root()
            .child(ObjectKind::Catalog, "app")
            .child(ObjectKind::Table, "t");
        assert_eq!(container_name(&c), Some("app"));
        assert!(is_under(&p, &ObjectPath::root()));
        assert!(is_under(&p, &p));
        assert!(!is_under(&c, &p));
    }
}
