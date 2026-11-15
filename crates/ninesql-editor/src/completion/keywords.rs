//! Keywords offered by completion, per dialect family. Only completion uses these lists: they
//! never decide what a statement does (classification stays in the drivers).

use ninesql_core::driver_api::EngineId;

/// Keyword list family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeywordSet {
    /// SQL standard keywords common to every engine.
    Ansi,
    /// PostgreSQL family.
    Postgres,
    /// MySQL / MariaDB.
    Mysql,
    /// SQLite.
    Sqlite,
}

const ANSI: &[&str] = &[
    "ALL",
    "ALTER",
    "AND",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASE",
    "CAST",
    "CHECK",
    "COALESCE",
    "COLUMN",
    "COMMIT",
    "CONSTRAINT",
    "COUNT",
    "CREATE",
    "CROSS",
    "CURRENT_DATE",
    "CURRENT_TIMESTAMP",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "EXPLAIN",
    "FALSE",
    "FOREIGN",
    "FROM",
    "FULL",
    "GRANT",
    "GROUP",
    "HAVING",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LEFT",
    "LIKE",
    "LIMIT",
    "MAX",
    "MIN",
    "NATURAL",
    "NOT",
    "NULL",
    "NULLIF",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "PARTITION",
    "PRIMARY",
    "REFERENCES",
    "REVOKE",
    "RIGHT",
    "ROLLBACK",
    "SAVEPOINT",
    "SELECT",
    "SET",
    "SUM",
    "AVG",
    "TABLE",
    "THEN",
    "TRANSACTION",
    "TRUE",
    "TRUNCATE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
];

const POSTGRES: &[&str] = &[
    "ANALYZE",
    "ARRAY",
    "CONFLICT",
    "DO",
    "FILTER",
    "ILIKE",
    "JSONB",
    "LATERAL",
    "MATERIALIZED",
    "NOTHING",
    "NULLS",
    "RETURNING",
    "SCHEMA",
    "SEQUENCE",
    "SIMILAR",
    "TABLESAMPLE",
    "VACUUM",
];

const MYSQL: &[&str] = &[
    "AUTO_INCREMENT",
    "DATABASE",
    "DATABASES",
    "DESCRIBE",
    "DUPLICATE",
    "ENGINE",
    "IGNORE",
    "PROCEDURE",
    "REGEXP",
    "REPLACE",
    "SHOW",
    "STRAIGHT_JOIN",
    "TABLES",
    "USE",
];

const SQLITE: &[&str] = &[
    "ATTACH",
    "AUTOINCREMENT",
    "CONFLICT",
    "GLOB",
    "IGNORE",
    "PRAGMA",
    "REPLACE",
    "RETURNING",
    "ROWID",
    "VACUUM",
    "WITHOUT",
];

impl KeywordSet {
    /// The set of an engine.
    #[must_use]
    pub fn for_engine(engine: &EngineId) -> Self {
        match engine.as_str() {
            "postgresql" | "timescaledb" | "yugabytedb" | "questdb" => Self::Postgres,
            "mysql" | "mariadb" | "tidb" => Self::Mysql,
            "sqlite" => Self::Sqlite,
            _ => Self::Ansi,
        }
    }

    /// Every keyword of the set, upper case, without duplicates.
    #[must_use]
    pub fn keywords(self) -> Vec<&'static str> {
        let extra: &[&str] = match self {
            Self::Ansi => &[],
            Self::Postgres => POSTGRES,
            Self::Mysql => MYSQL,
            Self::Sqlite => SQLITE,
        };
        let mut all: Vec<&'static str> = ANSI.iter().chain(extra).copied().collect();
        all.sort_unstable();
        all.dedup();
        all
    }

    /// Whether `word` (any case) is a keyword of the set.
    #[must_use]
    pub fn contains(self, word: &str) -> bool {
        let upper = word.to_ascii_uppercase();
        self.keywords().binary_search(&upper.as_str()).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_differ_per_dialect() {
        assert!(KeywordSet::Postgres.contains("returning"));
        assert!(KeywordSet::Postgres.contains("ilike"));
        assert!(!KeywordSet::Mysql.contains("ILIKE"));
        assert!(KeywordSet::Mysql.contains("show"));
        assert!(KeywordSet::Sqlite.contains("PRAGMA"));
        assert!(!KeywordSet::Ansi.contains("PRAGMA"));
        assert_eq!(KeywordSet::for_engine(&EngineId::MYSQL), KeywordSet::Mysql);
        let all = KeywordSet::Sqlite.keywords();
        assert!(all.windows(2).all(|w| w[0] < w[1]));
    }
}
