//! Quick object search (F-NAV-05) over the metadata cache: fuzzy on names, optionally a
//! substring match in comments and in loaded DDL. Bounded in time ([`MAX_SEARCH_TIME`]),
//! candidates ([`MAX_SEARCH_CANDIDATES`]) and results ([`MAX_SEARCH_RESULTS`]); never loads
//! anything.

use std::cmp::Ordering;
use std::collections::{BTreeSet, BinaryHeap};
use std::time::{Duration, Instant};

use ninesql_driver_api::ObjectKind;
use ninesql_driver_api::metadata::ObjectPath;

use super::cache::CacheValue;
use crate::connection::ConnectionId;

/// Longest query, in characters (longer queries are cut).
pub const MAX_QUERY_CHARS: usize = 128;
/// Most results of one search.
pub const MAX_SEARCH_RESULTS: usize = 1_000;
/// Default number of results.
pub const DEFAULT_SEARCH_RESULTS: usize = 100;
/// Most objects examined by one search.
pub const MAX_SEARCH_CANDIDATES: usize = 500_000;
/// Longest a search runs before it returns what it has.
pub const MAX_SEARCH_TIME: Duration = Duration::from_millis(250);
/// Bytes of a comment examined.
const MAX_COMMENT_BYTES: usize = 4 * 1024;
/// Bytes of a DDL text examined.
const MAX_DEFINITION_BYTES: usize = 256 * 1024;

const SCORE_EXACT: u32 = 10_000;
const SCORE_PREFIX: u32 = 8_000;
const SCORE_WORD: u32 = 6_000;
const SCORE_SUBSTRING: u32 = 5_000;
const SCORE_FUZZY: u32 = 2_000;
const SCORE_COMMENT: u32 = 1_000;
const SCORE_DEFINITION: u32 = 500;

/// What a search looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SearchRequest {
    /// What the user typed.
    pub query: String,
    /// Connections to search; empty: every connection with cached metadata.
    pub connections: Vec<ConnectionId>,
    /// Kinds to keep; `None`: every kind.
    pub kinds: Option<BTreeSet<ObjectKind>>,
    /// Also match comments (substring, case-insensitive).
    pub in_comments: bool,
    /// Also match loaded DDL (substring, case-insensitive).
    pub in_definitions: bool,
    /// Most results (clamped to 1..=[`MAX_SEARCH_RESULTS`]).
    pub limit: usize,
}

impl SearchRequest {
    /// A name search across every cached connection.
    #[must_use]
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            connections: Vec::new(),
            kinds: None,
            in_comments: false,
            in_definitions: false,
            limit: DEFAULT_SEARCH_RESULTS,
        }
    }

    /// Only these connections.
    #[must_use]
    pub fn in_connections(mut self, connections: impl IntoIterator<Item = ConnectionId>) -> Self {
        self.connections = connections.into_iter().collect();
        self
    }

    /// Only these kinds.
    #[must_use]
    pub fn of_kinds(mut self, kinds: impl IntoIterator<Item = ObjectKind>) -> Self {
        self.kinds = Some(kinds.into_iter().collect());
        self
    }

    /// Also match comments.
    #[must_use]
    pub fn with_comments(mut self, on: bool) -> Self {
        self.in_comments = on;
        self
    }

    /// Also match loaded DDL.
    #[must_use]
    pub fn with_definitions(mut self, on: bool) -> Self {
        self.in_definitions = on;
        self
    }

    /// Most results.
    #[must_use]
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }
}

/// Where a hit matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MatchField {
    /// The object name.
    Name,
    /// The comment.
    Comment,
    /// The loaded DDL.
    Definition,
}

/// One result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// The connection.
    pub connection: ConnectionId,
    /// The object.
    pub path: ObjectPath,
    /// Higher is better.
    pub score: u32,
    /// Where it matched.
    pub field: MatchField,
    /// Character positions of the match in the name, for highlighting (empty unless
    /// [`MatchField::Name`]).
    pub positions: Vec<usize>,
}

impl SearchHit {
    /// The object's name.
    #[must_use]
    pub fn name(&self) -> &str {
        self.path.leaf().map_or("", |s| s.name.as_str())
    }

    /// The object's kind.
    #[must_use]
    pub fn kind(&self) -> Option<ObjectKind> {
        self.path.kind()
    }
}

/// What a search found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchResults {
    /// Best first.
    pub hits: Vec<SearchHit>,
    /// Objects examined.
    pub scanned: usize,
    /// The search stopped at its time or candidate limit: results may be missing.
    pub truncated: bool,
}

/// How well `query` (already lower-cased) matches `name`: a score and the matched character
/// positions, `None` when it does not match. Exact > prefix > word-start substring >
/// substring > fuzzy subsequence; within a class, shorter names and tighter matches first.
#[must_use]
pub fn fuzzy_score(query: &[char], name: &str) -> Option<(u32, Vec<usize>)> {
    if query.is_empty() {
        return None;
    }
    let original: Vec<char> = name.chars().collect();
    let lower: Vec<char> = original
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();
    if lower.len() < query.len() {
        return None;
    }
    let extra = u32::try_from((lower.len() - query.len()).min(500)).unwrap_or(500);
    if lower == query {
        return Some((SCORE_EXACT, (0..lower.len()).collect()));
    }
    if lower.starts_with(query) {
        return Some((SCORE_PREFIX - extra, (0..query.len()).collect()));
    }
    // Substring: prefer one starting at a word boundary.
    let mut first = None;
    for start in 1..=lower.len() - query.len() {
        if lower[start..start + query.len()] == *query {
            if is_boundary(&original, start) {
                let r = (start..start + query.len()).collect();
                return Some((SCORE_WORD - extra, r));
            }
            if first.is_none() {
                first = Some(start);
            }
        }
    }
    if let Some(start) = first {
        let r = (start..start + query.len()).collect();
        return Some((SCORE_SUBSTRING - extra, r));
    }
    // Subsequence: greedy, preferring a boundary character when one is ahead.
    let mut positions = Vec::with_capacity(query.len());
    let mut at = 0usize;
    for q in query {
        let next = (at..lower.len()).find(|i| lower[*i] == *q)?;
        let boundary = (next..lower.len()).find(|i| lower[*i] == *q && is_boundary(&original, *i));
        let pick = match boundary {
            // Only jump to the boundary when it does not skip past the rest of the query.
            Some(b)
                if b != next && remaining_fits(&lower, b + 1, &query[positions.len() + 1..]) =>
            {
                b
            }
            _ => next,
        };
        positions.push(pick);
        at = pick + 1;
    }
    let mut bonus = 0u32;
    for (i, p) in positions.iter().enumerate() {
        if is_boundary(&original, *p) {
            bonus += 30;
        }
        if i > 0 && positions[i - 1] + 1 == *p {
            bonus += 20;
        }
    }
    let span = positions.last().map_or(0, |l| l - positions[0]);
    let gaps = u32::try_from(span + 1 - positions.len())
        .unwrap_or(u32::MAX)
        .min(1_000);
    let score = (SCORE_FUZZY + bonus)
        .saturating_sub(gaps * 5 + extra)
        .max(1);
    Some((score, positions))
}

fn remaining_fits(lower: &[char], from: usize, rest: &[char]) -> bool {
    let mut at = from;
    for q in rest {
        match (at..lower.len()).find(|i| lower[*i] == *q) {
            Some(i) => at = i + 1,
            None => return false,
        }
    }
    true
}

/// Start of a word: first character, after `_ . - $` or a space, a letter after a digit, or an
/// upper-case letter after a lower-case one.
fn is_boundary(chars: &[char], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let (prev, cur) = (chars[i - 1], chars[i]);
    matches!(prev, '_' | '.' | '-' | '$' | ' ')
        || (prev.is_ascii_digit() && cur.is_alphabetic())
        || (prev.is_lowercase() && cur.is_uppercase())
}

fn contains_folded(haystack: &str, limit: usize, needle: &str) -> bool {
    let mut end = haystack.len().min(limit);
    while !haystack.is_char_boundary(end) {
        end -= 1;
    }
    haystack[..end].to_lowercase().contains(needle)
}

/// Ordering of hits: score, then shorter name, then name, then path, then connection. The
/// heap keeps the worst hit on top so it can be dropped.
struct Ranked(SearchHit);

impl Ranked {
    fn key(
        &self,
    ) -> (
        std::cmp::Reverse<u32>,
        usize,
        &str,
        &ObjectPath,
        ConnectionId,
    ) {
        (
            std::cmp::Reverse(self.0.score),
            self.0.name().len(),
            self.0.name(),
            &self.0.path,
            self.0.connection,
        )
    }
}

impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}
impl Eq for Ranked {}
impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        // "Greater" = worse, so the max-heap's top is the worst hit kept.
        self.key().cmp(&other.key())
    }
}

/// Searches `sources` (connection, cached value). Runs in the caller's thread: bounded by
/// [`MAX_SEARCH_TIME`] and [`MAX_SEARCH_CANDIDATES`].
pub(crate) fn search(
    request: &SearchRequest,
    sources: &[(ConnectionId, CacheValue)],
) -> SearchResults {
    search_with_deadline(request, sources, Instant::now() + MAX_SEARCH_TIME)
}

pub(crate) fn search_with_deadline(
    request: &SearchRequest,
    sources: &[(ConnectionId, CacheValue)],
    deadline: Instant,
) -> SearchResults {
    let query: Vec<char> = request
        .query
        .trim()
        .chars()
        .take(MAX_QUERY_CHARS)
        .flat_map(char::to_lowercase)
        .collect();
    let mut out = SearchResults::default();
    if query.is_empty() {
        return out;
    }
    let needle: String = query.iter().collect();
    let limit = request.limit.clamp(1, MAX_SEARCH_RESULTS);
    let mut heap: BinaryHeap<Ranked> = BinaryHeap::with_capacity(limit + 1);
    let kind_ok = |k: Option<ObjectKind>| match (&request.kinds, k) {
        (None, _) => true,
        (Some(set), Some(k)) => set.contains(&k),
        (Some(_), None) => false,
    };
    let push = |hit: SearchHit, heap: &mut BinaryHeap<Ranked>| {
        heap.push(Ranked(hit));
        if heap.len() > limit {
            heap.pop();
        }
    };
    let mut seen_ddl = std::collections::HashSet::new();
    'outer: for (connection, value) in sources {
        match value {
            CacheValue::Children(node) => {
                for item in &node.items {
                    out.scanned += 1;
                    if out.scanned > MAX_SEARCH_CANDIDATES
                        || (out.scanned % 256 == 0 && Instant::now() >= deadline)
                    {
                        out.truncated = true;
                        break 'outer;
                    }
                    if !kind_ok(item.path.kind()) {
                        continue;
                    }
                    let name = item.name();
                    if let Some((score, positions)) = fuzzy_score(&query, name) {
                        push(
                            SearchHit {
                                connection: *connection,
                                path: item.path.clone(),
                                score,
                                field: MatchField::Name,
                                positions,
                            },
                            &mut heap,
                        );
                    } else if request.in_comments
                        && item
                            .comment
                            .as_deref()
                            .is_some_and(|c| contains_folded(c, MAX_COMMENT_BYTES, &needle))
                    {
                        push(
                            SearchHit {
                                connection: *connection,
                                path: item.path.clone(),
                                score: SCORE_COMMENT,
                                field: MatchField::Comment,
                                positions: Vec::new(),
                            },
                            &mut heap,
                        );
                    }
                }
            }
            CacheValue::Ddl(ddl) => {
                out.scanned += 1;
                if !request.in_definitions || !kind_ok(ddl.path.kind()) {
                    continue;
                }
                if Instant::now() >= deadline {
                    out.truncated = true;
                    break 'outer;
                }
                // A name hit from the listing ranks higher; the DDL hit is kept only when the
                // name did not match.
                if fuzzy_score(&query, ddl.path.leaf().map_or("", |s| s.name.as_str())).is_none()
                    && contains_folded(&ddl.text, MAX_DEFINITION_BYTES, &needle)
                    && seen_ddl.insert((*connection, ddl.path.clone()))
                {
                    push(
                        SearchHit {
                            connection: *connection,
                            path: ddl.path.clone(),
                            score: SCORE_DEFINITION,
                            field: MatchField::Definition,
                            positions: Vec::new(),
                        },
                        &mut heap,
                    );
                }
            }
        }
    }
    let mut hits: Vec<Ranked> = heap.into_vec();
    hits.sort();
    // The same object can match by comment in its listing and by definition: keep the best.
    let mut seen = std::collections::HashSet::new();
    out.hits = hits
        .into_iter()
        .map(|r| r.0)
        .filter(|h| seen.insert((h.connection, h.path.clone())))
        .collect();
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;

    fn q(s: &str) -> Vec<char> {
        s.chars().flat_map(char::to_lowercase).collect()
    }

    fn score(query: &str, name: &str) -> u32 {
        fuzzy_score(&q(query), name).map_or(0, |(s, _)| s)
    }

    #[test]
    fn classes_rank_in_order() {
        let exact = score("users", "users");
        let prefix = score("users", "users_archive");
        let word = score("users", "app_users");
        let sub = score("users", "appusers");
        let fuzzy = score("usr", "user_roles");
        assert!(exact > prefix, "{exact} {prefix}");
        assert!(prefix > word, "{prefix} {word}");
        assert!(word > sub, "{word} {sub}");
        assert!(sub > fuzzy, "{sub} {fuzzy}");
        assert!(fuzzy > 0);
        assert_eq!(score("xyz", "users"), 0);
        assert_eq!(score("", "users"), 0);
    }

    #[test]
    fn case_is_ignored_and_positions_point_at_the_match() {
        let (s, pos) = fuzzy_score(&q("ORD"), "customer_orders").unwrap();
        assert!(s >= SCORE_WORD - 100);
        assert_eq!(pos, vec![9, 10, 11]);
        let (_, pos) = fuzzy_score(&q("oi"), "order_items").unwrap();
        // Boundary characters are preferred: `o` of order, `i` of items.
        assert_eq!(pos, vec![0, 6]);
        let (_, pos) = fuzzy_score(&q("ci"), "CustomerInvoice").unwrap();
        assert_eq!(pos, vec![0, 8]);
    }

    #[test]
    fn shorter_names_win_within_a_class() {
        assert!(score("ord", "orders") > score("ord", "orders_history"));
        assert!(score("oi", "order_items") > score("oi", "order_line_items_archive"));
    }

    #[test]
    fn fuzzy_needs_every_character_in_order() {
        assert!(fuzzy_score(&q("abc"), "a_b_c").is_some());
        assert!(fuzzy_score(&q("acb"), "a_b_c").is_none());
        assert!(fuzzy_score(&q("toolong"), "short").is_none());
    }
}
