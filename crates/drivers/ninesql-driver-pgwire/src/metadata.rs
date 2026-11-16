//! [`Metadata`] of the pgwire driver (P4-A): the navigator's object kinds and DDL generation.
//!
//! Every statement here is a constant: user input (filter, names, cursors) is always bound, and
//! identifiers in generated DDL are quoted by the server (`pg_catalog.format('%I')`,
//! `pg_catalog.quote_ident`), never spliced from names in Rust.
//!
//! Internal SQL never depends on the session's `search_path` (CVE-2018-1058 class: a user
//! object such as `public.||(name, text)` is a better match than the built-in and would run with
//! the browsing user's privileges). Every operator is `OPERATOR(pg_catalog.…)`, every function,
//! type, relation and collation is schema-qualified, and `LIKE … ESCAPE` is written as
//! `OPERATOR(pg_catalog.~~)` with `pg_catalog.like_escape`. `pg_temp` is never searched for
//! functions or operators, and qualified relations bypass it.
//!
//! Path shapes: `[Schema]`, `[Schema, Table | View | MaterializedView | Routine | Sequence |
//! Type]`, `[Schema, Table, Partition…]` (partitions nest), and below a relation `Column`,
//! `Constraint`, `ForeignKey`, `Index`, `Trigger`. Routines carry their identity arguments in
//! the path (`f(integer, text)`) so overloads stay distinct.

use std::sync::Arc;

use ninesql_driver_api::{
    ColumnInfo, ConstraintInfo, ConstraintKind, DriverError, DriverResult, Feature, ForeignKeyInfo,
    IndexInfo, Metadata, ObjectDetails, ObjectInfo, ObjectKind, ObjectPath, Page, PageRequest,
    PartitionInfo, ReferentialAction, RoutineInfo, RoutineKind, SequenceInfo, TableInfo,
    TriggerInfo, TypeInfo, escape_like,
};

use crate::actor::Rows;
use crate::cancel::CancelTarget;
use crate::session::Link;

/// Largest DDL text returned; a longer definition is an error, never returned cut.
const MAX_DDL_BYTES: usize = 1024 * 1024;

/// Navigator metadata, served on the session's connection: a call made while a result streams
/// fails with [`DriverError::Busy`]. User input (filter, names, cursor) is always bound. A call
/// whose future is dropped is cancelled server-side.
pub struct PgMetadata {
    pub(crate) link: Link,
    pub(crate) cancel_target: Arc<CancelTarget>,
}

impl std::fmt::Debug for PgMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgMetadata").finish_non_exhaustive()
    }
}

/// `AND <filter> AND <cursor> ORDER BY <key> LIMIT $3`, with `$1` the escaped filter and `$2`
/// the cursor (both text, `NULL` when absent).
macro_rules! page_tail {
    ($filter:literal, $key:literal) => {
        concat!(
            " AND ($1::pg_catalog.text IS NULL OR ",
            $filter,
            " OPERATOR(pg_catalog.~~) pg_catalog.like_escape($1::pg_catalog.text, '!'::pg_catalog.text))",
            " AND ($2::pg_catalog.text IS NULL OR ",
            $key,
            " COLLATE pg_catalog.\"C\" OPERATOR(pg_catalog.>) $2::pg_catalog.text COLLATE pg_catalog.\"C\")",
            " ORDER BY ",
            $key,
            " COLLATE pg_catalog.\"C\" LIMIT $3::pg_catalog.int8"
        )
    };
}

/// Hex-encoded UTF-8 names of the attributes numbered in array `$arr` of relation `$rel`, in
/// array order, joined by `,` (PostgreSQL identifiers may hold any character but NUL, so no
/// separator is safe in plain text).
macro_rules! hex_attnames {
    ($arr:literal, $rel:literal) => {
        concat!(
            "(SELECT pg_catalog.string_agg(pg_catalog.encode(pg_catalog.convert_to(",
            "a.attname::pg_catalog.text, 'UTF8'::pg_catalog.name), 'hex'::pg_catalog.text), ",
            "','::pg_catalog.text ORDER BY k.ord) FROM pg_catalog.unnest(",
            $arr,
            ") WITH ORDINALITY AS k(attnum, ord) JOIN pg_catalog.pg_attribute a ",
            "ON a.attrelid OPERATOR(pg_catalog.=) ",
            $rel,
            " AND a.attnum OPERATOR(pg_catalog.=) k.attnum)"
        )
    };
}

const SCHEMAS_SQL: &str = concat!(
    "SELECT n.nspname::pg_catalog.text, ",
    "pg_catalog.obj_description(n.oid, 'pg_namespace'::pg_catalog.name) ",
    "FROM pg_catalog.pg_namespace n ",
    "WHERE NOT (n.nspname::pg_catalog.text OPERATOR(pg_catalog.~) '^pg_toast'::pg_catalog.text) ",
    "AND NOT (n.nspname::pg_catalog.text OPERATOR(pg_catalog.~) '^pg_temp_'::pg_catalog.text)",
    page_tail!("n.nspname::pg_catalog.text", "n.nspname::pg_catalog.text")
);

/// Tables, views and materialized views of schema `$4`, relkinds `$5`. Partitions are listed
/// under their parent, not here.
const RELATIONS_SQL: &str = concat!(
    "SELECT c.relname::pg_catalog.text, ",
    "pg_catalog.obj_description(c.oid, 'pg_class'::pg_catalog.name), ",
    "c.reltuples::pg_catalog.int8::pg_catalog.text, c.relkind::pg_catalog.text, ",
    "CASE WHEN c.relkind OPERATOR(pg_catalog.=) 'p'::pg_catalog.\"char\" ",
    "THEN pg_catalog.pg_get_partkeydef(c.oid) END ",
    "FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ",
    "ON n.oid OPERATOR(pg_catalog.=) c.relnamespace ",
    "WHERE n.nspname::pg_catalog.text OPERATOR(pg_catalog.=) $4::pg_catalog.text ",
    "AND c.relkind::pg_catalog.text OPERATOR(pg_catalog.=) ANY($5::pg_catalog.text[]) ",
    "AND NOT c.relispartition",
    page_tail!("c.relname::pg_catalog.text", "c.relname::pg_catalog.text")
);

/// Columns of relation `$4` (oid); the cursor `$2` is an attribute number.
const COLUMNS_SQL: &str = concat!(
    "SELECT a.attname::pg_catalog.text, ",
    "pg_catalog.col_description(a.attrelid, a.attnum::pg_catalog.int4), ",
    "a.attnum::pg_catalog.text, pg_catalog.format_type(a.atttypid, a.atttypmod), ",
    "(NOT a.attnotnull)::pg_catalog.text, pg_catalog.pg_get_expr(d.adbin, d.adrelid), ",
    "(EXISTS (SELECT 1 FROM pg_catalog.pg_index i ",
    "WHERE i.indrelid OPERATOR(pg_catalog.=) a.attrelid AND i.indisprimary ",
    "AND a.attnum OPERATOR(pg_catalog.=) ANY(i.indkey::pg_catalog.int2[])))::pg_catalog.text, ",
    "(a.attidentity OPERATOR(pg_catalog.<>) ''::pg_catalog.\"char\" ",
    "OR a.attgenerated OPERATOR(pg_catalog.<>) ''::pg_catalog.\"char\" ",
    "OR pg_catalog.strpos(COALESCE(pg_catalog.pg_get_expr(d.adbin, d.adrelid), ",
    "''::pg_catalog.text), 'nextval('::pg_catalog.text) OPERATOR(pg_catalog.=) 1)::pg_catalog.text ",
    "FROM pg_catalog.pg_attribute a ",
    "LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid OPERATOR(pg_catalog.=) a.attrelid ",
    "AND d.adnum OPERATOR(pg_catalog.=) a.attnum ",
    "WHERE a.attrelid OPERATOR(pg_catalog.=) $4::pg_catalog.oid ",
    "AND a.attnum OPERATOR(pg_catalog.>) 0::pg_catalog.int2 AND NOT a.attisdropped ",
    "AND ($1::pg_catalog.text IS NULL OR a.attname::pg_catalog.text OPERATOR(pg_catalog.~~) ",
    "pg_catalog.like_escape($1::pg_catalog.text, '!'::pg_catalog.text)) ",
    "AND ($2::pg_catalog.int4 IS NULL OR a.attnum::pg_catalog.int4 OPERATOR(pg_catalog.>) $2::pg_catalog.int4) ",
    "ORDER BY a.attnum LIMIT $3::pg_catalog.int8"
);

/// Primary-key, unique, check and exclusion constraints of relation `$4`.
const CONSTRAINTS_SQL: &str = concat!(
    "SELECT co.conname::pg_catalog.text, ",
    "pg_catalog.obj_description(co.oid, 'pg_constraint'::pg_catalog.name), ",
    "co.contype::pg_catalog.text, ",
    hex_attnames!("co.conkey", "co.conrelid"),
    ", pg_catalog.pg_get_constraintdef(co.oid) ",
    "FROM pg_catalog.pg_constraint co ",
    "WHERE co.conrelid OPERATOR(pg_catalog.=) $4::pg_catalog.oid ",
    "AND co.contype OPERATOR(pg_catalog.=) ANY('{p,u,c,x}'::pg_catalog.\"char\"[])",
    page_tail!("co.conname::pg_catalog.text", "co.conname::pg_catalog.text")
);

/// Foreign keys of relation `$4`.
const FOREIGN_KEYS_SQL: &str = concat!(
    "SELECT co.conname::pg_catalog.text, ",
    "pg_catalog.obj_description(co.oid, 'pg_constraint'::pg_catalog.name), ",
    hex_attnames!("co.conkey", "co.conrelid"),
    ", rn.nspname::pg_catalog.text, rc.relname::pg_catalog.text, ",
    hex_attnames!("co.confkey", "co.confrelid"),
    ", co.confupdtype::pg_catalog.text, co.confdeltype::pg_catalog.text ",
    "FROM pg_catalog.pg_constraint co ",
    "JOIN pg_catalog.pg_class rc ON rc.oid OPERATOR(pg_catalog.=) co.confrelid ",
    "JOIN pg_catalog.pg_namespace rn ON rn.oid OPERATOR(pg_catalog.=) rc.relnamespace ",
    "WHERE co.conrelid OPERATOR(pg_catalog.=) $4::pg_catalog.oid ",
    "AND co.contype OPERATOR(pg_catalog.=) 'f'::pg_catalog.\"char\"",
    page_tail!("co.conname::pg_catalog.text", "co.conname::pg_catalog.text")
);

/// Indexes of relation `$4`: key parts are column names, or expression text (hex-encoded).
const INDEXES_SQL: &str = concat!(
    "SELECT ic.relname::pg_catalog.text, ",
    "pg_catalog.obj_description(ic.oid, 'pg_class'::pg_catalog.name), ",
    "(SELECT pg_catalog.string_agg(pg_catalog.encode(pg_catalog.convert_to(",
    "CASE WHEN i.indkey[k.ord OPERATOR(pg_catalog.-) 1] OPERATOR(pg_catalog.<>) 0::pg_catalog.int2 ",
    "THEN (SELECT a.attname::pg_catalog.text FROM pg_catalog.pg_attribute a ",
    "WHERE a.attrelid OPERATOR(pg_catalog.=) i.indrelid ",
    "AND a.attnum OPERATOR(pg_catalog.=) i.indkey[k.ord OPERATOR(pg_catalog.-) 1]) ",
    "ELSE pg_catalog.pg_get_indexdef(i.indexrelid, k.ord, true) END, ",
    "'UTF8'::pg_catalog.name), 'hex'::pg_catalog.text), ','::pg_catalog.text ORDER BY k.ord) ",
    "FROM pg_catalog.generate_series(1, i.indnkeyatts::pg_catalog.int4) AS k(ord)), ",
    "i.indisunique::pg_catalog.text, i.indisprimary::pg_catalog.text, am.amname::pg_catalog.text, ",
    "pg_catalog.pg_get_expr(i.indpred, i.indrelid, true) ",
    "FROM pg_catalog.pg_index i ",
    "JOIN pg_catalog.pg_class ic ON ic.oid OPERATOR(pg_catalog.=) i.indexrelid ",
    "JOIN pg_catalog.pg_am am ON am.oid OPERATOR(pg_catalog.=) ic.relam ",
    "WHERE i.indrelid OPERATOR(pg_catalog.=) $4::pg_catalog.oid",
    page_tail!("ic.relname::pg_catalog.text", "ic.relname::pg_catalog.text")
);

/// User triggers of relation `$4` (internal constraint triggers hidden).
const TRIGGERS_SQL: &str = concat!(
    "SELECT t.tgname::pg_catalog.text, ",
    "pg_catalog.obj_description(t.oid, 'pg_trigger'::pg_catalog.name), ",
    "t.tgtype::pg_catalog.int4::pg_catalog.text, ",
    "(t.tgenabled OPERATOR(pg_catalog.<>) 'D'::pg_catalog.\"char\")::pg_catalog.text ",
    "FROM pg_catalog.pg_trigger t ",
    "WHERE t.tgrelid OPERATOR(pg_catalog.=) $4::pg_catalog.oid AND NOT t.tgisinternal",
    page_tail!("t.tgname::pg_catalog.text", "t.tgname::pg_catalog.text")
);

/// Partitions of relation `$4`.
const PARTITIONS_SQL: &str = concat!(
    "SELECT c.relname::pg_catalog.text, ",
    "pg_catalog.obj_description(c.oid, 'pg_class'::pg_catalog.name), ",
    "pg_catalog.pg_get_expr(c.relpartbound, c.oid), n.nspname::pg_catalog.text, ",
    "c.reltuples::pg_catalog.int8::pg_catalog.text, ",
    "(c.relkind OPERATOR(pg_catalog.=) 'p'::pg_catalog.\"char\")::pg_catalog.text, ",
    "(SELECT pt.partstrat::pg_catalog.text FROM pg_catalog.pg_partitioned_table pt ",
    "WHERE pt.partrelid OPERATOR(pg_catalog.=) i.inhparent) ",
    "FROM pg_catalog.pg_inherits i ",
    "JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) i.inhrelid ",
    "JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace ",
    "WHERE i.inhparent OPERATOR(pg_catalog.=) $4::pg_catalog.oid AND c.relispartition",
    page_tail!("c.relname::pg_catalog.text", "c.relname::pg_catalog.text")
);

/// Functions and procedures of schema `$4` (aggregates, window functions and the implicit
/// constructors of range types are not listed).
const ROUTINES_SQL: &str = concat!(
    "SELECT r.name, r.comment, r.kind, r.proname, r.args, r.result, r.lang FROM ( ",
    "SELECT pg_catalog.concat(p.proname::pg_catalog.text, '('::pg_catalog.text, ",
    "pg_catalog.pg_get_function_identity_arguments(p.oid), ')'::pg_catalog.text) AS name, ",
    "pg_catalog.obj_description(p.oid, 'pg_proc'::pg_catalog.name) AS comment, ",
    "p.prokind::pg_catalog.text AS kind, p.proname::pg_catalog.text AS proname, ",
    "pg_catalog.pg_get_function_arguments(p.oid) AS args, ",
    "CASE WHEN p.prokind OPERATOR(pg_catalog.=) 'f'::pg_catalog.\"char\" ",
    "THEN pg_catalog.pg_get_function_result(p.oid) END AS result, ",
    "l.lanname::pg_catalog.text AS lang ",
    "FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ",
    "ON n.oid OPERATOR(pg_catalog.=) p.pronamespace ",
    "LEFT JOIN pg_catalog.pg_language l ON l.oid OPERATOR(pg_catalog.=) p.prolang ",
    "WHERE n.nspname::pg_catalog.text OPERATOR(pg_catalog.=) $4::pg_catalog.text ",
    "AND p.prokind OPERATOR(pg_catalog.=) ANY('{f,p}'::pg_catalog.\"char\"[]) ",
    // Not the constructors the server creates with a range type (internal dependency).
    "AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend d ",
    "WHERE d.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_proc'::pg_catalog.regclass ",
    "AND d.objid OPERATOR(pg_catalog.=) p.oid ",
    "AND d.deptype OPERATOR(pg_catalog.=) 'i'::pg_catalog.\"char\")) r WHERE TRUE",
    page_tail!("r.proname", "r.name")
);

/// Sequences of schema `$4`.
const SEQUENCES_SQL: &str = concat!(
    "SELECT c.relname::pg_catalog.text, ",
    "pg_catalog.obj_description(c.oid, 'pg_class'::pg_catalog.name), ",
    "pg_catalog.format_type(s.seqtypid, NULL), s.seqstart::pg_catalog.text, ",
    "s.seqincrement::pg_catalog.text, s.seqmin::pg_catalog.text, s.seqmax::pg_catalog.text, ",
    "s.seqcycle::pg_catalog.text ",
    "FROM pg_catalog.pg_class c ",
    "JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace ",
    "JOIN pg_catalog.pg_sequence s ON s.seqrelid OPERATOR(pg_catalog.=) c.oid ",
    "WHERE n.nspname::pg_catalog.text OPERATOR(pg_catalog.=) $4::pg_catalog.text ",
    "AND c.relkind OPERATOR(pg_catalog.=) 'S'::pg_catalog.\"char\"",
    page_tail!("c.relname::pg_catalog.text", "c.relname::pg_catalog.text")
);

/// User-visible types of schema `$4`: base, composite (standalone only), domain, enum and range
/// types; array types, table row types and the implicit multiranges are not listed.
const TYPES_SQL: &str = concat!(
    "SELECT t.typname::pg_catalog.text, ",
    "pg_catalog.obj_description(t.oid, 'pg_type'::pg_catalog.name), t.typtype::pg_catalog.text, ",
    "CASE WHEN t.typtype OPERATOR(pg_catalog.=) 'd'::pg_catalog.\"char\" ",
    "THEN pg_catalog.format_type(t.typbasetype, t.typtypmod) ",
    "WHEN t.typtype OPERATOR(pg_catalog.=) 'r'::pg_catalog.\"char\" ",
    "THEN (SELECT pg_catalog.format_type(r.rngsubtype, NULL) FROM pg_catalog.pg_range r ",
    "WHERE r.rngtypid OPERATOR(pg_catalog.=) t.oid) END, ",
    "(SELECT pg_catalog.string_agg(pg_catalog.encode(pg_catalog.convert_to(",
    "e.enumlabel::pg_catalog.text, 'UTF8'::pg_catalog.name), 'hex'::pg_catalog.text), ",
    "','::pg_catalog.text ORDER BY e.enumsortorder) FROM pg_catalog.pg_enum e ",
    "WHERE e.enumtypid OPERATOR(pg_catalog.=) t.oid) ",
    "FROM pg_catalog.pg_type t ",
    "JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) t.typnamespace ",
    "WHERE n.nspname::pg_catalog.text OPERATOR(pg_catalog.=) $4::pg_catalog.text ",
    "AND t.typtype OPERATOR(pg_catalog.=) ANY('{b,c,d,e,r}'::pg_catalog.\"char\"[]) ",
    "AND (t.typrelid OPERATOR(pg_catalog.=) 0::pg_catalog.oid OR EXISTS (SELECT 1 ",
    "FROM pg_catalog.pg_class c WHERE c.oid OPERATOR(pg_catalog.=) t.typrelid ",
    "AND c.relkind OPERATOR(pg_catalog.=) 'c'::pg_catalog.\"char\")) ",
    "AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_type el ",
    "WHERE el.oid OPERATOR(pg_catalog.=) t.typelem AND el.typarray OPERATOR(pg_catalog.=) t.oid)",
    page_tail!("t.typname::pg_catalog.text", "t.typname::pg_catalog.text")
);

/// Oid and relkind of relation `$2` of schema `$1` with a relkind in `$3`.
const REL_SQL: &str = concat!(
    "SELECT c.oid::pg_catalog.text, c.relkind::pg_catalog.text ",
    "FROM pg_catalog.pg_class c ",
    "JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace ",
    "WHERE n.nspname::pg_catalog.text OPERATOR(pg_catalog.=) $1::pg_catalog.text ",
    "AND c.relname::pg_catalog.text OPERATOR(pg_catalog.=) $2::pg_catalog.text ",
    "AND c.relkind::pg_catalog.text OPERATOR(pg_catalog.=) ANY($3::pg_catalog.text[])"
);

/// Oid and relkind of partition `$2` of relation `$1`.
const PARTITION_SQL: &str = concat!(
    "SELECT c.oid::pg_catalog.text, c.relkind::pg_catalog.text ",
    "FROM pg_catalog.pg_inherits i ",
    "JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) i.inhrelid ",
    "WHERE i.inhparent OPERATOR(pg_catalog.=) $1::pg_catalog.oid ",
    "AND c.relname::pg_catalog.text OPERATOR(pg_catalog.=) $2::pg_catalog.text ",
    "AND c.relispartition LIMIT 2"
);

const SCHEMA_DDL_SQL: &str = concat!(
    "SELECT pg_catalog.format('CREATE SCHEMA %I AUTHORIZATION %I;'::pg_catalog.text, ",
    "n.nspname, pg_catalog.pg_get_userbyid(n.nspowner)) ",
    "FROM pg_catalog.pg_namespace n ",
    "WHERE n.nspname::pg_catalog.text OPERATOR(pg_catalog.=) $1::pg_catalog.text"
);

/// Header of a relation's DDL: qualified name, relkind, persistence, partition key, parent and
/// bound of a partition, quoted comment.
const REL_HEAD_SQL: &str = concat!(
    "SELECT pg_catalog.format('%I.%I'::pg_catalog.text, n.nspname, c.relname), ",
    "c.relkind::pg_catalog.text, c.relpersistence::pg_catalog.text, ",
    "c.relispartition::pg_catalog.text, ",
    "CASE WHEN c.relkind OPERATOR(pg_catalog.=) 'p'::pg_catalog.\"char\" ",
    "THEN pg_catalog.pg_get_partkeydef(c.oid) END, ",
    "CASE WHEN c.relispartition THEN (SELECT pg_catalog.format('%I.%I'::pg_catalog.text, ",
    "pn.nspname, pc.relname) FROM pg_catalog.pg_inherits i ",
    "JOIN pg_catalog.pg_class pc ON pc.oid OPERATOR(pg_catalog.=) i.inhparent ",
    "JOIN pg_catalog.pg_namespace pn ON pn.oid OPERATOR(pg_catalog.=) pc.relnamespace ",
    "WHERE i.inhrelid OPERATOR(pg_catalog.=) c.oid LIMIT 1) END, ",
    "pg_catalog.pg_get_expr(c.relpartbound, c.oid), ",
    "pg_catalog.quote_literal(pg_catalog.obj_description(c.oid, 'pg_class'::pg_catalog.name)), ",
    "CASE WHEN c.relkind OPERATOR(pg_catalog.=) ANY('{v,m}'::pg_catalog.\"char\"[]) ",
    "THEN pg_catalog.pg_get_viewdef(c.oid, true) END ",
    "FROM pg_catalog.pg_class c ",
    "JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace ",
    "WHERE c.oid OPERATOR(pg_catalog.=) $1::pg_catalog.oid"
);

const REL_COLUMNS_DDL_SQL: &str = concat!(
    "SELECT pg_catalog.quote_ident(a.attname::pg_catalog.text), ",
    "pg_catalog.format_type(a.atttypid, a.atttypmod), a.attnotnull::pg_catalog.text, ",
    "pg_catalog.pg_get_expr(d.adbin, d.adrelid), a.attidentity::pg_catalog.text, ",
    "a.attgenerated::pg_catalog.text, ",
    "pg_catalog.quote_literal(pg_catalog.col_description(a.attrelid, a.attnum::pg_catalog.int4)) ",
    "FROM pg_catalog.pg_attribute a ",
    "LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid OPERATOR(pg_catalog.=) a.attrelid ",
    "AND d.adnum OPERATOR(pg_catalog.=) a.attnum ",
    "WHERE a.attrelid OPERATOR(pg_catalog.=) $1::pg_catalog.oid ",
    "AND a.attnum OPERATOR(pg_catalog.>) 0::pg_catalog.int2 AND NOT a.attisdropped ",
    "ORDER BY a.attnum LIMIT 1601"
);

/// Local constraints of a relation in DDL order (primary key, unique, check, exclusion,
/// foreign keys).
const REL_CONSTRAINTS_DDL_SQL: &str = concat!(
    "SELECT pg_catalog.format('CONSTRAINT %I %s'::pg_catalog.text, co.conname, ",
    "pg_catalog.pg_get_constraintdef(co.oid)) ",
    "FROM pg_catalog.pg_constraint co ",
    "WHERE co.conrelid OPERATOR(pg_catalog.=) $1::pg_catalog.oid ",
    "AND co.contype OPERATOR(pg_catalog.=) ANY('{p,u,c,x,f}'::pg_catalog.\"char\"[]) ",
    "AND co.conislocal ",
    "ORDER BY pg_catalog.strpos('pucxf'::pg_catalog.text, co.contype::pg_catalog.text), ",
    "co.conname::pg_catalog.text COLLATE pg_catalog.\"C\" LIMIT 10001"
);

/// Indexes of a relation that no constraint creates and no parent index implies.
const REL_INDEXES_DDL_SQL: &str = concat!(
    "SELECT pg_catalog.pg_get_indexdef(i.indexrelid) FROM pg_catalog.pg_index i ",
    "JOIN pg_catalog.pg_class ic ON ic.oid OPERATOR(pg_catalog.=) i.indexrelid ",
    "WHERE i.indrelid OPERATOR(pg_catalog.=) $1::pg_catalog.oid ",
    "AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_constraint co ",
    "WHERE co.conindid OPERATOR(pg_catalog.=) i.indexrelid ",
    "AND co.conrelid OPERATOR(pg_catalog.=) i.indrelid ",
    "AND co.contype OPERATOR(pg_catalog.=) ANY('{p,u,x}'::pg_catalog.\"char\"[])) ",
    "AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_inherits ii ",
    "WHERE ii.inhrelid OPERATOR(pg_catalog.=) i.indexrelid) ",
    "ORDER BY ic.relname::pg_catalog.text COLLATE pg_catalog.\"C\" LIMIT 10001"
);

/// `ALTER TABLE … ADD CONSTRAINT …` of constraint `$2` (types `$3`) of relation `$1`.
const CONSTRAINT_DDL_SQL: &str = concat!(
    "SELECT pg_catalog.format('ALTER TABLE %I.%I ADD CONSTRAINT %I %s;'::pg_catalog.text, ",
    "n.nspname, c.relname, co.conname, pg_catalog.pg_get_constraintdef(co.oid)) ",
    "FROM pg_catalog.pg_constraint co ",
    "JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) co.conrelid ",
    "JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace ",
    "WHERE co.conrelid OPERATOR(pg_catalog.=) $1::pg_catalog.oid ",
    "AND co.conname::pg_catalog.text OPERATOR(pg_catalog.=) $2::pg_catalog.text ",
    "AND co.contype::pg_catalog.text OPERATOR(pg_catalog.=) ANY($3::pg_catalog.text[])"
);

const INDEX_DDL_SQL: &str = concat!(
    "SELECT pg_catalog.pg_get_indexdef(i.indexrelid) FROM pg_catalog.pg_index i ",
    "JOIN pg_catalog.pg_class ic ON ic.oid OPERATOR(pg_catalog.=) i.indexrelid ",
    "WHERE i.indrelid OPERATOR(pg_catalog.=) $1::pg_catalog.oid ",
    "AND ic.relname::pg_catalog.text OPERATOR(pg_catalog.=) $2::pg_catalog.text"
);

const TRIGGER_DDL_SQL: &str = concat!(
    "SELECT pg_catalog.pg_get_triggerdef(t.oid) FROM pg_catalog.pg_trigger t ",
    "WHERE t.tgrelid OPERATOR(pg_catalog.=) $1::pg_catalog.oid ",
    "AND t.tgname::pg_catalog.text OPERATOR(pg_catalog.=) $2::pg_catalog.text ",
    "AND NOT t.tgisinternal"
);

const ROUTINE_DDL_SQL: &str = concat!(
    "SELECT pg_catalog.pg_get_functiondef(p.oid) FROM pg_catalog.pg_proc p ",
    "JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) p.pronamespace ",
    "WHERE n.nspname::pg_catalog.text OPERATOR(pg_catalog.=) $1::pg_catalog.text ",
    "AND p.prokind OPERATOR(pg_catalog.=) ANY('{f,p}'::pg_catalog.\"char\"[]) ",
    "AND pg_catalog.concat(p.proname::pg_catalog.text, '('::pg_catalog.text, ",
    "pg_catalog.pg_get_function_identity_arguments(p.oid), ')'::pg_catalog.text) ",
    "OPERATOR(pg_catalog.=) $2::pg_catalog.text"
);

const SEQUENCE_DDL_SQL: &str = concat!(
    "SELECT pg_catalog.format('CREATE SEQUENCE %I.%I AS %s INCREMENT BY %s MINVALUE %s ",
    "MAXVALUE %s START WITH %s CACHE %s %s;'::pg_catalog.text, n.nspname, c.relname, ",
    "pg_catalog.format_type(s.seqtypid, NULL), s.seqincrement, s.seqmin, s.seqmax, s.seqstart, ",
    "s.seqcache, CASE WHEN s.seqcycle THEN 'CYCLE'::pg_catalog.text ",
    "ELSE 'NO CYCLE'::pg_catalog.text END) ",
    "FROM pg_catalog.pg_class c ",
    "JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace ",
    "JOIN pg_catalog.pg_sequence s ON s.seqrelid OPERATOR(pg_catalog.=) c.oid ",
    "WHERE n.nspname::pg_catalog.text OPERATOR(pg_catalog.=) $1::pg_catalog.text ",
    "AND c.relname::pg_catalog.text OPERATOR(pg_catalog.=) $2::pg_catalog.text ",
    "AND c.relkind OPERATOR(pg_catalog.=) 'S'::pg_catalog.\"char\""
);

/// DDL of an enum, domain, composite or range type (`NULL` for a base type).
const TYPE_DDL_SQL: &str = concat!(
    "SELECT CASE ",
    "WHEN t.typtype OPERATOR(pg_catalog.=) 'e'::pg_catalog.\"char\" THEN pg_catalog.format('CREATE TYPE %I.%I AS ENUM (%s);'",
    "::pg_catalog.text, n.nspname, t.typname, (SELECT pg_catalog.string_agg(",
    "pg_catalog.quote_literal(e.enumlabel::pg_catalog.text), ', '::pg_catalog.text ",
    "ORDER BY e.enumsortorder) FROM pg_catalog.pg_enum e ",
    "WHERE e.enumtypid OPERATOR(pg_catalog.=) t.oid)) ",
    "WHEN t.typtype OPERATOR(pg_catalog.=) 'd'::pg_catalog.\"char\" THEN pg_catalog.format('CREATE DOMAIN %I.%I AS %s%s%s%s;'",
    "::pg_catalog.text, n.nspname, t.typname, pg_catalog.format_type(t.typbasetype, t.typtypmod), ",
    "CASE WHEN t.typdefault IS NOT NULL THEN pg_catalog.format(' DEFAULT %s'::pg_catalog.text, ",
    "t.typdefault) END, ",
    "CASE WHEN t.typnotnull THEN ' NOT NULL'::pg_catalog.text END, ",
    "(SELECT pg_catalog.string_agg(pg_catalog.format(' CONSTRAINT %I %s'::pg_catalog.text, ",
    "co.conname, pg_catalog.pg_get_constraintdef(co.oid)), ''::pg_catalog.text ",
    "ORDER BY co.conname::pg_catalog.text COLLATE pg_catalog.\"C\") ",
    "FROM pg_catalog.pg_constraint co WHERE co.contypid OPERATOR(pg_catalog.=) t.oid ",
    "AND co.contype OPERATOR(pg_catalog.=) 'c'::pg_catalog.\"char\")) ",
    "WHEN t.typtype OPERATOR(pg_catalog.=) 'c'::pg_catalog.\"char\" THEN pg_catalog.format('CREATE TYPE %I.%I AS (%s);'",
    "::pg_catalog.text, n.nspname, t.typname, (SELECT pg_catalog.string_agg(",
    "pg_catalog.format('%I %s'::pg_catalog.text, a.attname, ",
    "pg_catalog.format_type(a.atttypid, a.atttypmod)), ', '::pg_catalog.text ORDER BY a.attnum) ",
    "FROM pg_catalog.pg_attribute a WHERE a.attrelid OPERATOR(pg_catalog.=) t.typrelid ",
    "AND a.attnum OPERATOR(pg_catalog.>) 0::pg_catalog.int2 AND NOT a.attisdropped)) ",
    "WHEN t.typtype OPERATOR(pg_catalog.=) 'r'::pg_catalog.\"char\" THEN (SELECT pg_catalog.format(",
    "'CREATE TYPE %I.%I AS RANGE (SUBTYPE = %s);'::pg_catalog.text, n.nspname, t.typname, ",
    "pg_catalog.format_type(r.rngsubtype, NULL)) FROM pg_catalog.pg_range r ",
    "WHERE r.rngtypid OPERATOR(pg_catalog.=) t.oid) END ",
    "FROM pg_catalog.pg_type t ",
    "JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) t.typnamespace ",
    "WHERE n.nspname::pg_catalog.text OPERATOR(pg_catalog.=) $1::pg_catalog.text ",
    "AND t.typname::pg_catalog.text OPERATOR(pg_catalog.=) $2::pg_catalog.text"
);

fn not_found() -> DriverError {
    DriverError::InvalidParams("object not found".into())
}

fn bad_path() -> DriverError {
    DriverError::InvalidParams("unexpected object path".into())
}

fn cell(row: &[Option<String>], i: usize) -> Option<String> {
    row.get(i).cloned().flatten()
}

fn truthy(row: &[Option<String>], i: usize) -> bool {
    cell(row, i).as_deref() == Some("true")
}

fn int(row: &[Option<String>], i: usize) -> Option<i64> {
    cell(row, i).and_then(|v| v.parse().ok())
}

fn rows_estimate(row: &[Option<String>], i: usize) -> Option<u64> {
    int(row, i).and_then(|v| u64::try_from(v).ok())
}

/// Decodes a `,`-joined list of hex-encoded UTF-8 names (see `hex_attnames!`).
fn hex_list(v: Option<String>) -> Vec<String> {
    let Some(v) = v else { return Vec::new() };
    if v.is_empty() {
        return Vec::new();
    }
    v.split(',')
        .map(|h| {
            let bytes: Vec<u8> = (0..h.len() / 2)
                .filter_map(|i| u8::from_str_radix(h.get(2 * i..2 * i + 2)?, 16).ok())
                .collect();
            String::from_utf8_lossy(&bytes).into_owned()
        })
        .collect()
}

fn referential_action(code: Option<String>) -> Option<ReferentialAction> {
    match code.as_deref() {
        Some("a") => Some(ReferentialAction::NoAction),
        Some("r") => Some(ReferentialAction::Restrict),
        Some("c") => Some(ReferentialAction::Cascade),
        Some("n") => Some(ReferentialAction::SetNull),
        Some("d") => Some(ReferentialAction::SetDefault),
        _ => None,
    }
}

/// Decodes `pg_trigger.tgtype`.
fn trigger_info(tgtype: i64, enabled: bool) -> TriggerInfo {
    const ROW: i64 = 1;
    const BEFORE: i64 = 2;
    const INSERT: i64 = 4;
    const DELETE: i64 = 8;
    const UPDATE: i64 = 16;
    const TRUNCATE: i64 = 32;
    const INSTEAD: i64 = 64;
    let mut t = TriggerInfo::default();
    t.timing = Some(
        if tgtype & INSTEAD != 0 {
            "INSTEAD OF"
        } else if tgtype & BEFORE != 0 {
            "BEFORE"
        } else {
            "AFTER"
        }
        .to_owned(),
    );
    for (bit, name) in [
        (INSERT, "INSERT"),
        (UPDATE, "UPDATE"),
        (DELETE, "DELETE"),
        (TRUNCATE, "TRUNCATE"),
    ] {
        if tgtype & bit != 0 {
            t.events.push(name.to_owned());
        }
    }
    t.for_each_row = tgtype & ROW != 0;
    t.enabled = enabled;
    t
}

fn partition_method(code: Option<String>) -> Option<String> {
    match code.as_deref() {
        Some("r") => Some("RANGE".to_owned()),
        Some("l") => Some("LIST".to_owned()),
        Some("h") => Some("HASH".to_owned()),
        _ => None,
    }
}

const RELATION_KINDS: &[ObjectKind] = &[
    ObjectKind::Table,
    ObjectKind::View,
    ObjectKind::MaterializedView,
    ObjectKind::Partition,
];

fn ensure_ddl_size(ddl: String) -> DriverResult<String> {
    if ddl.len() > MAX_DDL_BYTES {
        return Err(DriverError::Query {
            code: None,
            message: format!(
                "the definition is larger than {} KiB and was not loaded",
                MAX_DDL_BYTES / 1024
            ),
            position: None,
        });
    }
    Ok(ddl)
}

impl PgMetadata {
    async fn query(
        &self,
        sql: &str,
        params: Vec<Option<String>>,
        max_rows: i32,
        value_limit: Option<usize>,
    ) -> DriverResult<Rows> {
        self.link
            .internal_limited(
                sql,
                params,
                max_rows,
                value_limit,
                Some(&self.cancel_target),
            )
            .await
    }

    /// Rows of one part of a rebuilt DDL (columns, constraints, indexes of relation `oid`):
    /// values up to [`MAX_DDL_BYTES`], never cut, and an error rather than a DDL missing
    /// parts when the statement returns its `LIMIT` (`cap` rows; security review M2).
    async fn ddl_part(&self, sql: &str, oid: &str, cap: i32) -> DriverResult<Rows> {
        let rows = self
            .query(sql, vec![Some(oid.to_owned())], cap, Some(MAX_DDL_BYTES))
            .await?;
        if rows.len() >= usize::try_from(cap).unwrap_or(usize::MAX) {
            return Err(DriverError::Query {
                code: None,
                message: "the table has too many columns, constraints or indexes to rebuild \
                          its DDL"
                    .into(),
                position: None,
            });
        }
        Ok(rows)
    }

    /// One text value of a single-row DDL query (`object not found` without a row).
    async fn ddl_value(&self, sql: &str, params: Vec<Option<String>>) -> DriverResult<String> {
        let rows = self.query(sql, params, 2, Some(MAX_DDL_BYTES)).await?;
        rows.first().and_then(|r| cell(r, 0)).ok_or_else(not_found)
    }

    /// Oid and relkind of the relation at `path`: `[Schema, Table | View | MaterializedView,
    /// Partition…]`.
    async fn relation(
        &self,
        path: &[ninesql_driver_api::PathSegment],
    ) -> DriverResult<(String, String)> {
        let (schema, rel) = match path {
            [s, r, ..] if s.kind == ObjectKind::Schema => (s, r),
            _ => return Err(bad_path()),
        };
        let kinds = match rel.kind {
            ObjectKind::Table | ObjectKind::Partition => "{r,p,f}",
            ObjectKind::View => "{v}",
            ObjectKind::MaterializedView => "{m}",
            _ => return Err(bad_path()),
        };
        let rows = self
            .query(
                REL_SQL,
                vec![
                    Some(schema.name.clone()),
                    Some(rel.name.clone()),
                    Some(kinds.to_owned()),
                ],
                2,
                None,
            )
            .await?;
        let first = rows.first().ok_or_else(not_found)?;
        let mut found = (
            cell(first, 0).unwrap_or_default(),
            cell(first, 1).unwrap_or_default(),
        );
        for seg in &path[2..] {
            if seg.kind != ObjectKind::Partition {
                return Err(bad_path());
            }
            let rows = self
                .query(
                    PARTITION_SQL,
                    vec![Some(found.0.clone()), Some(seg.name.clone())],
                    2,
                    None,
                )
                .await?;
            if rows.len() != 1 {
                return Err(not_found());
            }
            found = (
                cell(&rows[0], 0).unwrap_or_default(),
                cell(&rows[0], 1).unwrap_or_default(),
            );
        }
        Ok(found)
    }

    /// Reconstructed DDL of a table, partition, view or materialized view.
    async fn relation_ddl(&self, oid: String) -> DriverResult<String> {
        let head = self
            .query(
                REL_HEAD_SQL,
                vec![Some(oid.clone())],
                2,
                Some(MAX_DDL_BYTES),
            )
            .await?;
        let head = head.first().ok_or_else(not_found)?;
        let name = cell(head, 0).unwrap_or_default();
        let relkind = cell(head, 1).unwrap_or_default();
        let mut out = String::new();
        match relkind.as_str() {
            "v" => {
                out.push_str(&format!(
                    "CREATE OR REPLACE VIEW {name} AS\n{}",
                    cell(head, 8).unwrap_or_default().trim_start()
                ));
            }
            "m" => {
                out.push_str(&format!(
                    "CREATE MATERIALIZED VIEW {name} AS\n{}",
                    cell(head, 8).unwrap_or_default().trim_start()
                ));
            }
            "r" | "p" => {
                let partition = truthy(head, 3);
                let constraints = self.ddl_part(REL_CONSTRAINTS_DDL_SQL, &oid, 10_001).await?;
                let mut lines = Vec::new();
                let mut comments = Vec::new();
                if !partition {
                    let cols = self.ddl_part(REL_COLUMNS_DDL_SQL, &oid, 1601).await?;
                    for c in &cols {
                        let col = cell(c, 0).unwrap_or_default();
                        let mut line = format!("{col} {}", cell(c, 1).unwrap_or_default());
                        let expr = cell(c, 3);
                        match (cell(c, 4).as_deref(), cell(c, 5).as_deref()) {
                            (Some("a"), _) => line.push_str(" GENERATED ALWAYS AS IDENTITY"),
                            (Some("d"), _) => line.push_str(" GENERATED BY DEFAULT AS IDENTITY"),
                            (_, Some("s")) => line.push_str(&format!(
                                " GENERATED ALWAYS AS ({}) STORED",
                                expr.unwrap_or_default()
                            )),
                            (_, Some("v")) => line.push_str(&format!(
                                " GENERATED ALWAYS AS ({}) VIRTUAL",
                                expr.unwrap_or_default()
                            )),
                            _ => {
                                if let Some(e) = expr {
                                    line.push_str(&format!(" DEFAULT {e}"));
                                }
                            }
                        }
                        if truthy(c, 2) {
                            line.push_str(" NOT NULL");
                        }
                        lines.push(line);
                        if let Some(comment) = cell(c, 6) {
                            comments.push(format!("COMMENT ON COLUMN {name}.{col} IS {comment};"));
                        }
                    }
                }
                lines.extend(constraints.iter().filter_map(|r| cell(r, 0)));
                let body = if lines.is_empty() {
                    String::new()
                } else {
                    format!(" (\n    {}\n)", lines.join(",\n    "))
                };
                if partition {
                    out.push_str(&format!(
                        "CREATE TABLE {name} PARTITION OF {}{body}\n{}",
                        cell(head, 5).unwrap_or_default(),
                        cell(head, 6).unwrap_or_default()
                    ));
                } else {
                    let unlogged = if cell(head, 2).as_deref() == Some("u") {
                        "UNLOGGED "
                    } else {
                        ""
                    };
                    out.push_str(&format!("CREATE {unlogged}TABLE {name}{body}"));
                }
                if let Some(key) = cell(head, 4) {
                    out.push_str(&format!("\nPARTITION BY {key}"));
                }
                out.push(';');
                if let Some(comment) = cell(head, 7) {
                    comments.insert(0, format!("COMMENT ON TABLE {name} IS {comment};"));
                }
                let indexes = self.ddl_part(REL_INDEXES_DDL_SQL, &oid, 10_001).await?;
                for i in indexes.iter().filter_map(|r| cell(r, 0)) {
                    out.push_str(&format!("\n{i};"));
                }
                for c in comments {
                    out.push('\n');
                    out.push_str(&c);
                }
                return ensure_ddl_size(out);
            }
            _ => {
                return Err(DriverError::Query {
                    code: None,
                    message: "DDL generation is not available for foreign tables".into(),
                    position: None,
                });
            }
        }
        if relkind == "m" {
            let indexes = self.ddl_part(REL_INDEXES_DDL_SQL, &oid, 10_001).await?;
            for i in indexes.iter().filter_map(|r| cell(r, 0)) {
                out.push_str(&format!("\n{i};"));
            }
        }
        if let Some(comment) = cell(head, 7) {
            let what = if relkind == "m" {
                "MATERIALIZED VIEW"
            } else {
                "VIEW"
            };
            out.push_str(&format!("\nCOMMENT ON {what} {name} IS {comment};"));
        }
        ensure_ddl_size(out)
    }
}

#[async_trait::async_trait]
impl Metadata for PgMetadata {
    fn child_kinds(&self, parent: Option<ObjectKind>) -> Vec<ObjectKind> {
        match parent {
            None => vec![ObjectKind::Schema],
            Some(ObjectKind::Schema) => vec![
                ObjectKind::Table,
                ObjectKind::View,
                ObjectKind::MaterializedView,
                ObjectKind::Sequence,
                ObjectKind::Type,
                ObjectKind::Routine,
            ],
            Some(ObjectKind::Table | ObjectKind::Partition) => vec![
                ObjectKind::Column,
                ObjectKind::Constraint,
                ObjectKind::ForeignKey,
                ObjectKind::Index,
                ObjectKind::Trigger,
                ObjectKind::Partition,
            ],
            Some(ObjectKind::View) => vec![ObjectKind::Column, ObjectKind::Trigger],
            Some(ObjectKind::MaterializedView) => vec![ObjectKind::Column, ObjectKind::Index],
            Some(_) => Vec::new(),
        }
    }

    async fn list(
        &self,
        parent: &ObjectPath,
        kind: ObjectKind,
        page: PageRequest,
    ) -> DriverResult<Page<ObjectInfo>> {
        if !self.child_kinds(parent.kind()).contains(&kind) {
            return Err(DriverError::Unsupported(Feature::Metadata(kind)));
        }
        let limit = page.limit.clamp(1, 10_000);
        let filter = page.filter.as_deref().map(|f| escape_like(f, '!'));
        let fetch = (u64::from(limit) + 1).to_string();
        let mut params = vec![filter, page.cursor.clone(), Some(fetch)];
        let sql = match parent.kind() {
            None => SCHEMAS_SQL,
            Some(ObjectKind::Schema) => {
                let [schema] = parent.0.as_slice() else {
                    return Err(bad_path());
                };
                params.push(Some(schema.name.clone()));
                match kind {
                    ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView => {
                        params.push(Some(
                            match kind {
                                ObjectKind::Table => "{r,p,f}",
                                ObjectKind::View => "{v}",
                                _ => "{m}",
                            }
                            .to_owned(),
                        ));
                        RELATIONS_SQL
                    }
                    ObjectKind::Routine => ROUTINES_SQL,
                    ObjectKind::Sequence => SEQUENCES_SQL,
                    _ => TYPES_SQL,
                }
            }
            Some(_) => {
                let (oid, _) = self.relation(&parent.0).await?;
                params.push(Some(oid));
                match kind {
                    ObjectKind::Column => {
                        if let Some(c) = &page.cursor
                            && c.parse::<i32>().is_err()
                        {
                            return Err(DriverError::InvalidParams("invalid page cursor".into()));
                        }
                        COLUMNS_SQL
                    }
                    ObjectKind::Constraint => CONSTRAINTS_SQL,
                    ObjectKind::ForeignKey => FOREIGN_KEYS_SQL,
                    ObjectKind::Index => INDEXES_SQL,
                    ObjectKind::Trigger => TRIGGERS_SQL,
                    _ => PARTITIONS_SQL,
                }
            }
        };
        // The server sends at most `limit + 1` rows (the extra one tells whether a next page
        // exists), and the text sink caps rows and bytes on top.
        let max_rows = i32::try_from(limit).unwrap_or(i32::MAX).saturating_add(1);
        let mut rows = self.query(sql, params, max_rows, None).await?;
        let more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let mut items = Vec::with_capacity(rows.len());
        let mut next_cursor = None;
        for row in &rows {
            let name = cell(row, 0).unwrap_or_default();
            let comment = cell(row, 1);
            let details = match kind {
                ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView => {
                    let mut t = TableInfo::default();
                    t.estimated_rows = rows_estimate(row, 2);
                    t.flavour = match cell(row, 3).as_deref() {
                        Some("p") => Some("partitioned".to_owned()),
                        Some("f") => Some("foreign".to_owned()),
                        _ => None,
                    };
                    t.partition_key = cell(row, 4);
                    ObjectDetails::Table(t)
                }
                ObjectKind::Column => {
                    let ordinal = cell(row, 2).and_then(|v| v.parse().ok()).unwrap_or(0);
                    let mut c = ColumnInfo::new(ordinal, cell(row, 3).unwrap_or_default());
                    c.nullable = truthy(row, 4);
                    c.default = cell(row, 5);
                    c.primary_key = truthy(row, 6);
                    c.auto_generated = truthy(row, 7);
                    next_cursor = Some(ordinal.to_string());
                    ObjectDetails::Column(c)
                }
                ObjectKind::Constraint => {
                    let k = match cell(row, 2).as_deref() {
                        Some("p") => ConstraintKind::PrimaryKey,
                        Some("u") => ConstraintKind::Unique,
                        Some("x") => ConstraintKind::Exclusion,
                        _ => ConstraintKind::Check,
                    };
                    let mut c = ConstraintInfo::new(k);
                    c.columns = hex_list(cell(row, 3));
                    c.definition = cell(row, 4);
                    ObjectDetails::Constraint(c)
                }
                ObjectKind::ForeignKey => {
                    let target = ObjectPath::root()
                        .child(ObjectKind::Schema, cell(row, 3).unwrap_or_default())
                        .child(ObjectKind::Table, cell(row, 4).unwrap_or_default());
                    let mut f = ForeignKeyInfo::new(target);
                    f.columns = hex_list(cell(row, 2));
                    f.referenced_columns = hex_list(cell(row, 5));
                    f.on_update = referential_action(cell(row, 6));
                    f.on_delete = referential_action(cell(row, 7));
                    ObjectDetails::ForeignKey(f)
                }
                ObjectKind::Index => {
                    let mut i = IndexInfo::default();
                    i.columns = hex_list(cell(row, 2));
                    i.unique = truthy(row, 3);
                    i.primary = truthy(row, 4);
                    i.method = cell(row, 5);
                    i.predicate = cell(row, 6);
                    ObjectDetails::Index(i)
                }
                ObjectKind::Trigger => {
                    ObjectDetails::Trigger(trigger_info(int(row, 2).unwrap_or(0), truthy(row, 3)))
                }
                ObjectKind::Partition => {
                    let mut p = PartitionInfo::default();
                    p.bound = cell(row, 2);
                    p.schema = cell(row, 3);
                    p.estimated_rows = rows_estimate(row, 4);
                    p.partitioned = truthy(row, 5);
                    p.method = partition_method(cell(row, 6));
                    ObjectDetails::Partition(p)
                }
                ObjectKind::Routine => {
                    let k = if cell(row, 2).as_deref() == Some("p") {
                        RoutineKind::Procedure
                    } else {
                        RoutineKind::Function
                    };
                    let mut r = RoutineInfo::new(k, cell(row, 3).unwrap_or_default());
                    r.arguments = cell(row, 4);
                    r.return_type = cell(row, 5);
                    r.language = cell(row, 6);
                    ObjectDetails::Routine(r)
                }
                ObjectKind::Sequence => {
                    let mut s = SequenceInfo::default();
                    s.data_type = cell(row, 2);
                    s.start = int(row, 3);
                    s.increment = int(row, 4);
                    s.min_value = int(row, 5);
                    s.max_value = int(row, 6);
                    s.cycle = truthy(row, 7);
                    ObjectDetails::Sequence(s)
                }
                ObjectKind::Type => {
                    let flavour = match cell(row, 2).as_deref() {
                        Some("e") => "enum",
                        Some("c") => "composite",
                        Some("d") => "domain",
                        Some("r") => "range",
                        _ => "base",
                    };
                    let mut t = TypeInfo::new(flavour);
                    t.base_type = cell(row, 3);
                    t.labels = hex_list(cell(row, 4));
                    ObjectDetails::Type(t)
                }
                _ => ObjectDetails::None,
            };
            if kind != ObjectKind::Column {
                next_cursor = Some(name.clone());
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
        if segs.first().map(|s| s.kind) != Some(ObjectKind::Schema) {
            return Err(bad_path());
        }
        let schema = segs[0].name.clone();
        match leaf.kind {
            ObjectKind::Schema if segs.len() == 1 => {
                self.ddl_value(SCHEMA_DDL_SQL, vec![Some(schema)]).await
            }
            ObjectKind::Table
            | ObjectKind::View
            | ObjectKind::MaterializedView
            | ObjectKind::Partition => {
                let (oid, _) = self.relation(segs).await?;
                self.relation_ddl(oid).await
            }
            ObjectKind::Constraint
            | ObjectKind::ForeignKey
            | ObjectKind::Index
            | ObjectKind::Trigger
                if segs.len() >= 3 && RELATION_KINDS.contains(&segs[segs.len() - 2].kind) =>
            {
                let (oid, _) = self.relation(&segs[..segs.len() - 1]).await?;
                let name = Some(leaf.name.clone());
                match leaf.kind {
                    ObjectKind::Constraint => {
                        self.ddl_value(
                            CONSTRAINT_DDL_SQL,
                            vec![Some(oid), name, Some("{p,u,c,x}".to_owned())],
                        )
                        .await
                    }
                    ObjectKind::ForeignKey => {
                        self.ddl_value(
                            CONSTRAINT_DDL_SQL,
                            vec![Some(oid), name, Some("{f}".to_owned())],
                        )
                        .await
                    }
                    ObjectKind::Index => self
                        .ddl_value(INDEX_DDL_SQL, vec![Some(oid), name])
                        .await
                        .map(|d| format!("{d};")),
                    _ => self
                        .ddl_value(TRIGGER_DDL_SQL, vec![Some(oid), name])
                        .await
                        .map(|d| format!("{d};")),
                }
            }
            ObjectKind::Routine if segs.len() == 2 => self
                .ddl_value(ROUTINE_DDL_SQL, vec![Some(schema), Some(leaf.name.clone())])
                .await
                .map(|d| format!("{};", d.trim_end())),
            ObjectKind::Sequence if segs.len() == 2 => {
                self.ddl_value(
                    SEQUENCE_DDL_SQL,
                    vec![Some(schema), Some(leaf.name.clone())],
                )
                .await
            }
            ObjectKind::Type if segs.len() == 2 => {
                let rows = self
                    .query(
                        TYPE_DDL_SQL,
                        vec![Some(schema), Some(leaf.name.clone())],
                        2,
                        Some(MAX_DDL_BYTES),
                    )
                    .await?;
                let row = rows.first().ok_or_else(not_found)?;
                cell(row, 0).ok_or_else(|| DriverError::Query {
                    code: None,
                    message: "DDL generation is not available for base types".into(),
                    position: None,
                })
            }
            ObjectKind::Schema
            | ObjectKind::Constraint
            | ObjectKind::ForeignKey
            | ObjectKind::Index
            | ObjectKind::Trigger
            | ObjectKind::Routine
            | ObjectKind::Sequence
            | ObjectKind::Type => Err(bad_path()),
            _ => Err(DriverError::Unsupported(Feature::Ddl)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_lists_decode_any_name() {
        assert!(hex_list(None).is_empty());
        assert!(hex_list(Some(String::new())).is_empty());
        // `a,b` / `"x"` / `é`
        assert_eq!(
            hex_list(Some("612c62,227822,c3a9".into())),
            vec!["a,b", "\"x\"", "é"]
        );
    }

    #[test]
    fn trigger_types_decode() {
        let t = trigger_info(1 | 2 | 4 | 16, true);
        assert_eq!(t.timing.as_deref(), Some("BEFORE"));
        assert_eq!(t.events, vec!["INSERT", "UPDATE"]);
        assert!(t.for_each_row && t.enabled);
        let t = trigger_info(64 | 1 | 8, false);
        assert_eq!(t.timing.as_deref(), Some("INSTEAD OF"));
        assert_eq!(t.events, vec!["DELETE"]);
        let t = trigger_info(32, true);
        assert_eq!(t.timing.as_deref(), Some("AFTER"));
        assert!(!t.for_each_row);
        assert_eq!(t.events, vec!["TRUNCATE"]);
    }

    /// Every internal statement is search-path independent: no bare comparison operator, no
    /// unqualified function call of the kinds used here.
    #[test]
    fn statements_qualify_operators() {
        for sql in [
            SCHEMAS_SQL,
            RELATIONS_SQL,
            COLUMNS_SQL,
            CONSTRAINTS_SQL,
            FOREIGN_KEYS_SQL,
            INDEXES_SQL,
            TRIGGERS_SQL,
            PARTITIONS_SQL,
            ROUTINES_SQL,
            SEQUENCES_SQL,
            TYPES_SQL,
            REL_SQL,
            PARTITION_SQL,
            SCHEMA_DDL_SQL,
            REL_HEAD_SQL,
            REL_COLUMNS_DDL_SQL,
            REL_CONSTRAINTS_DDL_SQL,
            REL_INDEXES_DDL_SQL,
            CONSTRAINT_DDL_SQL,
            INDEX_DDL_SQL,
            TRIGGER_DDL_SQL,
            ROUTINE_DDL_SQL,
            SEQUENCE_DDL_SQL,
            TYPE_DDL_SQL,
        ] {
            // Remove string literals, then look for operators outside `OPERATOR(...)`.
            let mut code = String::new();
            let mut in_str = false;
            for ch in sql.chars() {
                if ch == '\'' {
                    in_str = !in_str;
                    continue;
                }
                if !in_str {
                    code.push(ch);
                }
            }
            let stripped = code
                .replace("OPERATOR(pg_catalog.=)", " ")
                .replace("OPERATOR(pg_catalog.<>)", " ")
                .replace("OPERATOR(pg_catalog.>)", " ")
                .replace("OPERATOR(pg_catalog.-)", " ")
                .replace("OPERATOR(pg_catalog.~~)", " ")
                .replace("OPERATOR(pg_catalog.~)", " ")
                .replace("::", " ");
            for op in ["=", "<", ">", "||", "~", "+", "-"] {
                assert!(!stripped.contains(op), "bare `{op}` in {sql}");
            }
            // A simple `CASE <expr> WHEN` compares with an unqualified `=` (security review
            // L1): only the searched form is allowed.
            let words: Vec<&str> = code.split_whitespace().collect();
            for w in words.windows(2) {
                if w[0] == "CASE" {
                    assert_eq!(w[1], "WHEN", "simple CASE in {sql}");
                }
            }
            assert!(
                !code
                    .replace("pg_catalog.\"char\"", " ")
                    .contains("\"char\""),
                "unqualified \"char\" in {sql}"
            );
        }
    }
}
