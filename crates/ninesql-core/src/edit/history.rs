//! Filter history per connection (F-DATA-03), following the history rules of
//! [`crate::history`] (ADR-0024 §2):
//! - nothing is kept when history is disabled in the preferences;
//! - on a connection whose type allows query text in logs and that is not production, the whole
//!   filter is kept: conditions with their values, the raw `WHERE` text and the order — except
//!   a raw text holding a credential word or a URI with credentials
//!   ([`has_credential_word`]), which is dropped, and the value of a condition on a column whose
//!   name holds a credential word, or longer than [`MAX_KEPT_VALUE_BYTES`], which is dropped;
//! - on production and non-logging types, only the shape is kept: columns, comparisons and the
//!   order, never a value nor the raw text.
//!
//! The history lives in memory for the session of the application (at most
//! [`MAX_FILTERS_PER_CONNECTION`] entries per connection, newest first); it is never written to
//! disk (follow-up: a persisted store needs its own schema decision).

use std::collections::{HashMap, VecDeque};
use std::time::SystemTime;

use ninesql_driver_api::Param;

use super::query::{ColumnFilter, SortKey, TableQuery};
use crate::connection::ConnectionId;
use crate::connection_type::SafetyPolicy;
use crate::history::{has_credential_word, may_record};
use crate::preferences::HistoryPreferences;

/// Entries kept per connection.
pub const MAX_FILTERS_PER_CONNECTION: usize = 50;
/// Connections with a filter history kept at once (the oldest is dropped).
const MAX_CONNECTIONS: usize = 1024;
/// Longest filter value kept, in bytes (text or bytes); a longer one is dropped.
pub const MAX_KEPT_VALUE_BYTES: usize = 4096;

/// A filter as kept in the history. `Debug` hides values and the raw text.
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub struct FilterHistoryEntry {
    /// Table name parts.
    pub table: Vec<String>,
    /// Conditions; a value is `None` when it was not kept.
    pub filters: Vec<ColumnFilter>,
    /// The raw `WHERE` text, when kept.
    pub raw_where: Option<String>,
    /// Sort keys.
    pub order_by: Vec<SortKey>,
    /// Everything was kept (no value or raw text dropped).
    pub complete: bool,
    /// When it was recorded.
    pub recorded_at: SystemTime,
}

impl std::fmt::Debug for FilterHistoryEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilterHistoryEntry")
            .field("filters", &self.filters.len())
            .field("raw_where", &self.raw_where.is_some())
            .field("order_by", &self.order_by.len())
            .field("complete", &self.complete)
            .finish_non_exhaustive()
    }
}

impl FilterHistoryEntry {
    /// The query this entry describes; conditions whose value was not kept are left out
    /// (the UI asks for them again).
    #[must_use]
    pub fn to_query(&self) -> TableQuery {
        let mut q = TableQuery::new(self.table.clone());
        q.filters = self
            .filters
            .iter()
            .filter(|f| !f.op.takes_value() || f.value.is_some())
            .cloned()
            .collect();
        q.raw_where.clone_from(&self.raw_where);
        q.order_by.clone_from(&self.order_by);
        q
    }

    fn same_filter(&self, other: &Self) -> bool {
        self.table == other.table
            && self.filters == other.filters
            && self.raw_where == other.raw_where
            && self.order_by == other.order_by
    }
}

/// What [`EditService::record_filter`](super::EditService::record_filter) kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FilterRecorded {
    /// The whole filter.
    Complete,
    /// The filter without some or all of its values or its raw text.
    Partial,
    /// Nothing: history is disabled, or the filter is empty.
    Nothing,
}

fn value_size(v: &Param) -> usize {
    match v {
        Param::Text(s) | Param::Json(s) | Param::Decimal(s) => s.len(),
        Param::Bytes(b) => b.len(),
        _ => 0,
    }
}

/// The in-memory filter history (see the [module documentation](self)).
#[derive(Default)]
pub(crate) struct FilterHistory {
    by_connection: HashMap<ConnectionId, VecDeque<FilterHistoryEntry>>,
    order: VecDeque<ConnectionId>,
}

impl FilterHistory {
    pub(crate) fn record(
        &mut self,
        connection: ConnectionId,
        policy: &SafetyPolicy,
        prefs: &HistoryPreferences,
        query: &TableQuery,
    ) -> FilterRecorded {
        if !prefs.enabled
            || (query.filters.is_empty() && query.raw_where.is_none() && query.order_by.is_empty())
        {
            return FilterRecorded::Nothing;
        }
        let full = may_record(policy, prefs);
        let mut complete = true;
        let filters = query
            .filters
            .iter()
            .map(|f| {
                let keep = full
                    && !has_credential_word(&f.column)
                    && f.value
                        .as_ref()
                        .is_none_or(|v| value_size(v) <= MAX_KEPT_VALUE_BYTES);
                let mut f = f.clone();
                if !keep && f.value.is_some() {
                    f.value = None;
                    complete = false;
                }
                f
            })
            .collect();
        let raw_where = query.raw_where.as_ref().and_then(|raw| {
            if full && !has_credential_word(raw) && raw.len() <= MAX_KEPT_VALUE_BYTES {
                Some(raw.clone())
            } else {
                complete = false;
                None
            }
        });
        let entry = FilterHistoryEntry {
            table: query.table.clone(),
            filters,
            raw_where,
            order_by: query.order_by.clone(),
            complete,
            recorded_at: SystemTime::now(),
        };
        if !self.by_connection.contains_key(&connection) {
            if self.order.len() >= MAX_CONNECTIONS
                && let Some(old) = self.order.pop_front()
            {
                self.by_connection.remove(&old);
            }
            self.order.push_back(connection);
        }
        let list = self.by_connection.entry(connection).or_default();
        list.retain(|e| !e.same_filter(&entry));
        list.push_front(entry);
        list.truncate(MAX_FILTERS_PER_CONNECTION);
        if complete {
            FilterRecorded::Complete
        } else {
            FilterRecorded::Partial
        }
    }

    pub(crate) fn entries(
        &self,
        connection: ConnectionId,
        table: Option<&[String]>,
    ) -> Vec<FilterHistoryEntry> {
        self.by_connection
            .get(&connection)
            .map(|list| {
                list.iter()
                    .filter(|e| table.is_none_or(|t| e.table == t))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn clear(&mut self, connection: ConnectionId) {
        self.by_connection.remove(&connection);
        self.order.retain(|c| *c != connection);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::super::query::FilterOp;
    use super::*;
    use crate::connection_type::ConnectionType;

    fn query() -> TableQuery {
        TableQuery::new(vec!["t".into()])
            .filter(ColumnFilter::new(
                "name",
                FilterOp::Equal,
                Some(Param::Text("alice".into())),
            ))
            .filter(ColumnFilter::new(
                "password_hash",
                FilterOp::Equal,
                Some(Param::Text("h".into())),
            ))
            .with_raw_where("age > 3")
    }

    #[test]
    fn rules_follow_the_history_rules() {
        let prefs = HistoryPreferences::default();
        let mut h = FilterHistory::default();
        let id = ConnectionId::new_random();
        let dev = ConnectionType::development().policy();
        assert_eq!(
            h.record(id, &dev, &prefs, &query()),
            FilterRecorded::Partial
        );
        let e = &h.entries(id, None)[0];
        assert_eq!(e.filters[0].value, Some(Param::Text("alice".into())));
        assert_eq!(e.filters[1].value, None, "credential column");
        assert_eq!(e.raw_where.as_deref(), Some("age > 3"));
        // Production: the shape only.
        let prod = ConnectionType::production().policy();
        let p = ConnectionId::new_random();
        h.record(p, &prod, &prefs, &query());
        let e = &h.entries(p, None)[0];
        assert!(e.filters.iter().all(|f| f.value.is_none()));
        assert_eq!(e.raw_where, None);
        assert_eq!(e.to_query().filters.len(), 0);
        assert!(!format!("{e:?}").contains("alice"));
        // Disabled: nothing.
        let mut off = prefs;
        off.enabled = false;
        let d = ConnectionId::new_random();
        assert_eq!(h.record(d, &dev, &off, &query()), FilterRecorded::Nothing);
        assert!(h.entries(d, None).is_empty());
        // Deduplicated, newest first, bounded.
        for i in 0..(MAX_FILTERS_PER_CONNECTION + 5) {
            let q = TableQuery::new(vec!["t".into()]).with_raw_where(format!("a = {i}"));
            h.record(id, &dev, &prefs, &q);
            h.record(id, &dev, &prefs, &q);
        }
        let all = h.entries(id, Some(&["t".to_owned()]));
        assert_eq!(all.len(), MAX_FILTERS_PER_CONNECTION);
        assert_eq!(
            all[0].raw_where.as_deref(),
            Some(format!("a = {}", MAX_FILTERS_PER_CONNECTION + 4).as_str())
        );
        h.clear(id);
        assert!(h.entries(id, None).is_empty());
    }
}
