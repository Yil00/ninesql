//! Ranking of completion candidates against the typed prefix.
//!
//! Matching is ASCII case-insensitive. From best to worst: exact match, prefix match (shorter
//! candidates first), match on the start of a `_`-separated part (`id` → `user_id`), match on
//! the initials of the parts (`ui` → `user_id`), then a subsequence match (`usr` → `users`).
//! Anything else is not offered.

/// How a candidate matched the prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchQuality {
    /// Characters of the prefix appear in order.
    Subsequence,
    /// The prefix spells the initials of the `_`-separated parts.
    Initials,
    /// The prefix starts a `_`-separated part other than the first.
    PartPrefix,
    /// The candidate starts with the prefix.
    Prefix,
    /// Same text, ignoring case.
    Exact,
}

/// How `candidate` matches `prefix`, or `None`. An empty prefix matches everything as `Prefix`.
#[must_use]
pub fn match_quality(candidate: &str, prefix: &str) -> Option<MatchQuality> {
    let c = candidate.to_ascii_lowercase();
    let p = prefix.to_ascii_lowercase();
    if p.is_empty() {
        return Some(MatchQuality::Prefix);
    }
    if c == p {
        return Some(MatchQuality::Exact);
    }
    if c.starts_with(&p) {
        return Some(MatchQuality::Prefix);
    }
    let parts: Vec<&str> = c.split('_').filter(|s| !s.is_empty()).collect();
    if parts.iter().skip(1).any(|part| part.starts_with(&p)) {
        return Some(MatchQuality::PartPrefix);
    }
    if parts.len() > 1 {
        let initials: String = parts.iter().filter_map(|s| s.chars().next()).collect();
        if initials.starts_with(&p) {
            return Some(MatchQuality::Initials);
        }
    }
    let mut chars = c.chars();
    if p.chars().all(|pc| chars.any(|cc| cc == pc)) {
        return Some(MatchQuality::Subsequence);
    }
    None
}

/// The score of a match: quality first, then the context `boost` of the candidate's kind, then
/// shorter candidates.
#[must_use]
pub fn score(quality: MatchQuality, boost: u32, candidate_len: usize) -> u32 {
    let quality = match quality {
        MatchQuality::Exact => 5_000,
        MatchQuality::Prefix => 4_000,
        MatchQuality::PartPrefix => 3_000,
        MatchQuality::Initials => 2_000,
        MatchQuality::Subsequence => 1_000,
    };
    let len_penalty = u32::try_from(candidate_len.min(99)).unwrap_or(99);
    quality + boost * 100 + (99 - len_penalty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualities() {
        assert_eq!(match_quality("users", "USERS"), Some(MatchQuality::Exact));
        assert_eq!(match_quality("users", "us"), Some(MatchQuality::Prefix));
        assert_eq!(
            match_quality("user_id", "id"),
            Some(MatchQuality::PartPrefix)
        );
        assert_eq!(match_quality("user_id", "ui"), Some(MatchQuality::Initials));
        assert_eq!(
            match_quality("users", "usr"),
            Some(MatchQuality::Subsequence)
        );
        assert_eq!(match_quality("users", "x"), None);
        assert_eq!(match_quality("anything", ""), Some(MatchQuality::Prefix));
    }

    #[test]
    fn quality_beats_boost_and_length() {
        let exact = score(MatchQuality::Exact, 0, 30);
        let prefix = score(MatchQuality::Prefix, 9, 1);
        assert!(exact > prefix);
        assert!(score(MatchQuality::Prefix, 2, 10) > score(MatchQuality::Prefix, 1, 2));
        assert!(score(MatchQuality::Prefix, 1, 4) > score(MatchQuality::Prefix, 1, 9));
    }
}
