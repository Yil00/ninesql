//! Navigator filters (F-NAV-04): include / exclude name patterns per connection and per object
//! kind, stored in the project's `navigator.json` (ADR-0008 §1 and §2: a machine-owned,
//! versioned JSON file written atomically).
//!
//! # Patterns
//! A pattern matches the **whole** name. `*` matches any run of characters (empty included),
//! `?` exactly one character; every other character matches itself. Matching ignores case
//! unless the filter is case-sensitive. A name is shown when it matches at least one include
//! pattern (or there is none) and no exclude pattern.
//!
//! # Where filters apply
//! Always client-side, on every page the driver returns. Server-side only where it is cheap and
//! exact enough: a case-sensitive filter with a single include pattern sends the longest literal
//! run of that pattern as [`PageRequest::filter`](ninesql_driver_api::metadata::PageRequest)
//! (a substring every match contains, bound as a parameter by the driver), and the client-side
//! match then keeps the exact set. A case-insensitive filter is never sent: the drivers' `LIKE`
//! is case-sensitive on some engines and would hide names the pattern accepts.
//!
//! # Limits
//! At most [`MAX_PATTERNS`] patterns per list, each 1 to [`MAX_PATTERN_BYTES`] bytes with no
//! control character. Errors never echo a pattern.

use std::collections::BTreeMap;

use ninesql_driver_api::ObjectKind;
use ninesql_storage::document::{Document, Format};
use serde::{Deserialize, Serialize};

use super::model::{kind_from_name, kind_name};
use crate::connection::ConnectionId;

/// `schema_version` of `navigator.json`.
pub const NAVIGATOR_SCHEMA_VERSION: u32 = 1;
/// Most patterns in one include or exclude list.
pub const MAX_PATTERNS: usize = 64;
/// Longest pattern, in bytes.
pub const MAX_PATTERN_BYTES: usize = 256;

/// Why a filter was refused. Never echoes the pattern.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FilterError {
    /// An empty pattern.
    #[error("{list} pattern #{index} is empty")]
    EmptyPattern {
        /// `include` or `exclude`.
        list: &'static str,
        /// 0-based position.
        index: usize,
    },
    /// A pattern over [`MAX_PATTERN_BYTES`].
    #[error("{list} pattern #{index} is longer than {MAX_PATTERN_BYTES} bytes")]
    PatternTooLong {
        /// `include` or `exclude`.
        list: &'static str,
        /// 0-based position.
        index: usize,
    },
    /// A pattern with a control character.
    #[error("{list} pattern #{index} contains a control character")]
    ControlCharacter {
        /// `include` or `exclude`.
        list: &'static str,
        /// 0-based position.
        index: usize,
    },
    /// More than [`MAX_PATTERNS`] patterns in a list.
    #[error("more than {MAX_PATTERNS} {list} patterns")]
    TooManyPatterns {
        /// `include` or `exclude`.
        list: &'static str,
    },
    /// A kind this build cannot name (it cannot be stored).
    #[error("this object kind cannot be filtered")]
    UnnamedKind,
}

/// One validated name pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamePattern {
    text: String,
    /// The pattern's characters, lower-cased for case-insensitive matching.
    folded: Vec<char>,
    exact: Vec<char>,
}

impl NamePattern {
    /// Validates `text`.
    ///
    /// # Errors
    /// [`FilterError`] for an empty, too long or control-character pattern (`list` and `index`
    /// name it in the message).
    pub fn new(text: &str, list: &'static str, index: usize) -> Result<Self, FilterError> {
        if text.is_empty() {
            return Err(FilterError::EmptyPattern { list, index });
        }
        if text.len() > MAX_PATTERN_BYTES {
            return Err(FilterError::PatternTooLong { list, index });
        }
        if text.chars().any(char::is_control) {
            return Err(FilterError::ControlCharacter { list, index });
        }
        Ok(Self {
            text: text.to_owned(),
            folded: fold(text),
            exact: text.chars().collect(),
        })
    }

    /// The pattern as typed.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    fn matches(&self, name_exact: &[char], name_folded: &[char], case_sensitive: bool) -> bool {
        if case_sensitive {
            glob(&self.exact, name_exact)
        } else {
            glob(&self.folded, name_folded)
        }
    }

    /// The longest run of literal characters (between wildcards), `None` when the pattern is
    /// only wildcards.
    fn longest_literal(&self) -> Option<String> {
        self.text
            .split(['*', '?'])
            .max_by_key(|s| s.chars().count())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }
}

fn fold(s: &str) -> Vec<char> {
    s.chars().flat_map(char::to_lowercase).collect()
}

/// Whole-string glob match with `*` and `?`. Linear in practice; at worst
/// `pattern.len() * name.len()` steps, both bounded.
fn glob(pattern: &[char], name: &[char]) -> bool {
    let (mut p, mut n) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, n));
            p += 1;
        } else if let Some((sp, sn)) = star {
            p = sp + 1;
            n = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

/// Include / exclude patterns for one object kind of one connection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KindFilter {
    include: Vec<NamePattern>,
    exclude: Vec<NamePattern>,
    case_sensitive: bool,
}

impl KindFilter {
    /// Validates the patterns.
    ///
    /// # Errors
    /// [`FilterError`] for the first invalid pattern or a list over [`MAX_PATTERNS`].
    pub fn new<I, E, S, T>(
        include: I,
        exclude: E,
        case_sensitive: bool,
    ) -> Result<Self, FilterError>
    where
        I: IntoIterator<Item = S>,
        E: IntoIterator<Item = T>,
        S: AsRef<str>,
        T: AsRef<str>,
    {
        Ok(Self {
            include: patterns(include, "include")?,
            exclude: patterns(exclude, "exclude")?,
            case_sensitive,
        })
    }

    /// Include patterns.
    #[must_use]
    pub fn include(&self) -> &[NamePattern] {
        &self.include
    }

    /// Exclude patterns.
    #[must_use]
    pub fn exclude(&self) -> &[NamePattern] {
        &self.exclude
    }

    /// Whether matching respects case.
    #[must_use]
    pub fn case_sensitive(&self) -> bool {
        self.case_sensitive
    }

    /// `true` when the filter has no pattern (it accepts everything).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    /// Whether a name is shown.
    #[must_use]
    pub fn accepts(&self, name: &str) -> bool {
        if self.is_empty() {
            return true;
        }
        let exact: Vec<char> = name.chars().collect();
        let folded = if self.case_sensitive {
            Vec::new()
        } else {
            fold(name)
        };
        let cs = self.case_sensitive;
        (self.include.is_empty() || self.include.iter().any(|p| p.matches(&exact, &folded, cs)))
            && !self.exclude.iter().any(|p| p.matches(&exact, &folded, cs))
    }

    /// The substring sent to the driver as the server-side filter, when that is exact enough
    /// (see the module documentation).
    #[must_use]
    pub fn server_substring(&self) -> Option<String> {
        match (self.case_sensitive, self.include.as_slice()) {
            (true, [only]) => only.longest_literal(),
            _ => None,
        }
    }
}

fn patterns<I, S>(items: I, list: &'static str) -> Result<Vec<NamePattern>, FilterError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = Vec::new();
    for (index, item) in items.into_iter().enumerate() {
        if index >= MAX_PATTERNS {
            return Err(FilterError::TooManyPatterns { list });
        }
        out.push(NamePattern::new(item.as_ref(), list, index)?);
    }
    Ok(out)
}

/// The filters of one connection, by object kind.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConnectionFilters {
    kinds: BTreeMap<ObjectKind, KindFilter>,
}

impl ConnectionFilters {
    /// No filter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets (or, with an empty filter, removes) the filter of `kind`.
    ///
    /// # Errors
    /// [`FilterError::UnnamedKind`] for a kind this build cannot store.
    pub fn set(&mut self, kind: ObjectKind, filter: KindFilter) -> Result<(), FilterError> {
        if kind_name(kind).is_none() {
            return Err(FilterError::UnnamedKind);
        }
        if filter.is_empty() {
            self.kinds.remove(&kind);
        } else {
            self.kinds.insert(kind, filter);
        }
        Ok(())
    }

    /// [`Self::set`], chained.
    ///
    /// # Errors
    /// As [`Self::set`].
    pub fn with(mut self, kind: ObjectKind, filter: KindFilter) -> Result<Self, FilterError> {
        self.set(kind, filter)?;
        Ok(self)
    }

    /// The filter of `kind`, if any.
    #[must_use]
    pub fn get(&self, kind: ObjectKind) -> Option<&KindFilter> {
        self.kinds.get(&kind)
    }

    /// Every filtered kind.
    pub fn iter(&self) -> impl Iterator<Item = (ObjectKind, &KindFilter)> {
        self.kinds.iter().map(|(k, f)| (*k, f))
    }

    /// `true` when no kind is filtered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }
}

/// The filters of every connection of a project.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NavigatorFilters {
    connections: BTreeMap<ConnectionId, ConnectionFilters>,
}

impl NavigatorFilters {
    /// The filters of a connection (empty when it has none).
    #[must_use]
    pub fn of(&self, connection: ConnectionId) -> ConnectionFilters {
        self.connections
            .get(&connection)
            .cloned()
            .unwrap_or_default()
    }

    /// Replaces the filters of a connection (an empty value removes them).
    pub fn set(&mut self, connection: ConnectionId, filters: ConnectionFilters) {
        if filters.is_empty() {
            self.connections.remove(&connection);
        } else {
            self.connections.insert(connection, filters);
        }
    }

    /// Drops the filters of connections not in `keep` (deleted connections).
    pub fn retain_connections(&mut self, keep: impl Fn(ConnectionId) -> bool) {
        self.connections.retain(|id, _| keep(*id));
    }

    /// Connections with filters.
    pub fn iter(&self) -> impl Iterator<Item = (ConnectionId, &ConnectionFilters)> {
        self.connections.iter().map(|(k, v)| (*k, v))
    }

    /// The file model.
    #[must_use]
    pub fn to_file(&self) -> NavigatorFile {
        let filters = self
            .connections
            .iter()
            .map(|(id, cf)| {
                let kinds = cf
                    .kinds
                    .iter()
                    .filter_map(|(kind, f)| {
                        Some((
                            kind_name(*kind)?.to_owned(),
                            RawKindFilter {
                                include: f.include.iter().map(|p| p.text.clone()).collect(),
                                exclude: f.exclude.iter().map(|p| p.text.clone()).collect(),
                                case_sensitive: f.case_sensitive,
                            },
                        ))
                    })
                    .collect();
                (*id, kinds)
            })
            .collect();
        NavigatorFile { filters }
    }

    /// Reads the file model: entries with an unknown kind or an invalid pattern are dropped
    /// and reported (never echoing a pattern).
    #[must_use]
    pub fn from_file(file: NavigatorFile) -> (Self, Vec<FilterIssue>) {
        let mut out = Self::default();
        let mut issues = Vec::new();
        for (id, kinds) in file.filters {
            let mut cf = ConnectionFilters::default();
            for (name, raw) in kinds {
                let Some(kind) = kind_from_name(&name) else {
                    issues.push(FilterIssue {
                        connection: id,
                        kind: None,
                        error: FilterError::UnnamedKind,
                    });
                    continue;
                };
                match KindFilter::new(&raw.include, &raw.exclude, raw.case_sensitive) {
                    Ok(f) => {
                        let _ = cf.set(kind, f);
                    }
                    Err(error) => issues.push(FilterIssue {
                        connection: id,
                        kind: Some(kind),
                        error,
                    }),
                }
            }
            out.set(id, cf);
        }
        (out, issues)
    }
}

/// A filter entry of `navigator.json` that was dropped on load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterIssue {
    /// The connection.
    pub connection: ConnectionId,
    /// The kind, when it was known.
    pub kind: Option<ObjectKind>,
    /// What is wrong.
    pub error: FilterError,
}

/// `navigator.json` (one per project).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavigatorFile {
    /// By connection id, then by kind name ([`super::model::KIND_NAMES`]).
    #[serde(default)]
    pub filters: BTreeMap<ConnectionId, BTreeMap<String, RawKindFilter>>,
}

/// One kind's filter, as stored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawKindFilter {
    /// Include patterns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// Exclude patterns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Case-sensitive matching.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub case_sensitive: bool,
}

impl Document for NavigatorFile {
    const NAME: &'static str = "navigator";
    const FORMAT: Format = Format::Json;
    const SCHEMA_VERSION: u32 = NAVIGATOR_SCHEMA_VERSION;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;

    fn f(include: &[&str], exclude: &[&str], cs: bool) -> KindFilter {
        KindFilter::new(include, exclude, cs).unwrap()
    }

    #[test]
    fn glob_matches_the_whole_name() {
        let g = |p: &str, n: &str| {
            glob(
                &p.chars().collect::<Vec<_>>(),
                &n.chars().collect::<Vec<_>>(),
            )
        };
        assert!(g("*", ""));
        assert!(g("app_*", "app_users"));
        assert!(!g("app_*", "my_app_users"));
        assert!(g("*_log", "audit_log"));
        assert!(g("a?c", "abc"));
        assert!(!g("a?c", "ac"));
        assert!(g("*a*b*c*", "xxaxxbxxcxx"));
        assert!(!g("*a*b*c*", "xxaxxcxxbxx"));
        assert!(g("t%_1", "t%_1"));
        assert!(!g("t%_1", "tX11"));
    }

    #[test]
    fn include_and_exclude_and_case() {
        let filter = f(&["APP_*", "core"], &["*_tmp"], false);
        assert!(filter.accepts("app_users"));
        assert!(filter.accepts("Core"));
        assert!(!filter.accepts("app_users_tmp"));
        assert!(!filter.accepts("billing"));
        let only_exclude = f(&[], &["pg_*"], false);
        assert!(only_exclude.accepts("users"));
        assert!(!only_exclude.accepts("PG_stat"));
        let cs = f(&["App*"], &[], true);
        assert!(cs.accepts("AppUsers"));
        assert!(!cs.accepts("appusers"));
        assert!(KindFilter::default().accepts("anything"));
    }

    #[test]
    fn server_substring_only_when_exact_enough() {
        assert_eq!(
            f(&["*user*"], &[], true).server_substring().as_deref(),
            Some("user")
        );
        assert_eq!(
            f(&["ab*long_part?x"], &[], true)
                .server_substring()
                .as_deref(),
            Some("long_part")
        );
        assert_eq!(f(&["*user*"], &[], false).server_substring(), None);
        assert_eq!(f(&["a*", "b*"], &[], true).server_substring(), None);
        assert_eq!(f(&["*"], &[], true).server_substring(), None);
        assert_eq!(f(&[], &["x"], true).server_substring(), None);
    }

    #[test]
    fn invalid_patterns_are_refused_without_echo() {
        let e = KindFilter::new([""], Vec::<String>::new(), false).unwrap_err();
        assert_eq!(
            e,
            FilterError::EmptyPattern {
                list: "include",
                index: 0
            }
        );
        let long = "x".repeat(MAX_PATTERN_BYTES + 1);
        let e = KindFilter::new(Vec::<String>::new(), ["ok", long.as_str()], false).unwrap_err();
        assert_eq!(
            e,
            FilterError::PatternTooLong {
                list: "exclude",
                index: 1
            }
        );
        let e = KindFilter::new(["sec\u{7}ret"], Vec::<String>::new(), false).unwrap_err();
        assert!(!e.to_string().contains("sec"));
        let many: Vec<String> = (0..=MAX_PATTERNS).map(|i| format!("p{i}")).collect();
        assert_eq!(
            KindFilter::new(&many, Vec::<String>::new(), false).unwrap_err(),
            FilterError::TooManyPatterns { list: "include" }
        );
    }

    #[test]
    fn file_round_trip_drops_invalid_entries() {
        let id = ConnectionId::new_random();
        let mut nav = NavigatorFilters::default();
        nav.set(
            id,
            ConnectionFilters::new()
                .with(ObjectKind::Table, f(&["app_*"], &["*_tmp"], false))
                .unwrap()
                .with(ObjectKind::Schema, f(&[], &["pg_*"], true))
                .unwrap(),
        );
        let file = nav.to_file();
        let json = serde_json::to_string(&file).unwrap();
        let back: NavigatorFile = serde_json::from_str(&json).unwrap();
        let (loaded, issues) = NavigatorFilters::from_file(back);
        assert!(issues.is_empty());
        assert_eq!(loaded, nav);

        let mut file = nav.to_file();
        let kinds = file.filters.get_mut(&id).unwrap();
        kinds.insert("no-such-kind".into(), RawKindFilter::default());
        kinds.insert(
            "view".into(),
            RawKindFilter {
                include: vec![String::new()],
                ..RawKindFilter::default()
            },
        );
        let (loaded, issues) = NavigatorFilters::from_file(file);
        assert_eq!(issues.len(), 2);
        assert_eq!(loaded, nav);
    }
}
