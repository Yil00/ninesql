//! One-shot transaction characteristics: `SET TRANSACTION …` with no scope keyword applies to
//! the *next* transaction only, and any transaction consumes it, a navigator read's included
//! (MySQL 8 reads `information_schema` through InnoDB). The navigator rolls back the
//! transaction its read started in manual-commit mode, and in auto-commit mode its read is a
//! transaction of its own: either way the user's pending characteristics would be lost, and
//! the user's next transaction would run read-write or at the session's isolation level
//! (security re-review L2-a).
//!
//! The connection task therefore tracks what the user left pending and, after a navigator read,
//! sets the same characteristics again from validated values ([`Pending::reissue_sql`]), never
//! from the user's text. Only the plain form is recognised (`SET TRANSACTION` followed by
//! `ISOLATION LEVEL …`, `READ ONLY` and / or `READ WRITE`, no comment); any other statement
//! that may leave characteristics pending (`SET @@transaction_read_only = 1`, a form with a
//! comment…) makes the state unknown, and navigator reads are then refused until the pending
//! characteristics are consumed or dropped (fails closed: nothing is browsed rather than the
//! user's settings lost).
//!
//! Engine behaviour this follows (checked on MySQL 8.4 and MariaDB 11.8): the pending
//! characteristics survive `SET autocommit`, `SET @user_var`, `USE` and table-less `SELECT`s;
//! `COMMIT`, `ROLLBACK` (even with no transaction), `START TRANSACTION` and any statement that
//! starts a transaction consume them; `SET SESSION TRANSACTION <x>` replaces only the pending
//! characteristic `x`; two `SET TRANSACTION` statements accumulate; an assignment to
//! `transaction_isolation` / `transaction_read_only` (or the `tx_…` aliases) in session scope
//! also replaces the next transaction's value.
//!
//! The tracker must never re-issue characteristics the server no longer has pending (the
//! user's next transaction could then run at a weaker level than the server would have used;
//! final review L-1). So, while characteristics are pending, the state becomes unknown after
//! any statement that may have overwritten or consumed them without the tracker knowing: an
//! assignment to one of those variables in any scope, case or quoting
//! ([`tx_var_assignment`]); any `Unknown` statement, succeeded or failed (`CALL`, `LOCK
//! TABLES`, `ANALYZE` / `OPTIMIZE` / `CHECK TABLE`, `FLUSH`, `CACHE INDEX`, `LOAD INDEX`…,
//! which commit implicitly or may start and end transactions), those words whatever the
//! classifier says, a session statement with a `SELECT`, and a read or write that failed
//! (it may have started its transaction first). With nothing pending, only a form that may
//! leave characteristics pending makes it unknown (a scope-less `@@transaction_isolation`
//! assignment, any assignment without its own `SESSION` / `LOCAL`); a plain read of such a
//! variable (`SET @a = @@tx_isolation`) does not.
//!
//! Residuals: a stored routine or prepared statement that runs `SET TRANSACTION` or assigns a
//! transaction variable is not seen when nothing is pending (the `CALL` / `EXECUTE` text does
//! not say so), and in auto-commit mode a table-less user `SELECT` is taken as consuming the
//! pending characteristics (a later navigator read may then consume them).

use ninesql_driver_api::{CodeToken, CodeTokenKind, LexicalRules, StatementClass, code_tokens};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    ReadOnly,
    ReadWrite,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Isolation {
    ReadUncommitted,
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

impl Isolation {
    fn sql(self) -> &'static str {
        match self {
            Self::ReadUncommitted => "READ UNCOMMITTED",
            Self::ReadCommitted => "READ COMMITTED",
            Self::RepeatableRead => "REPEATABLE READ",
            Self::Serializable => "SERIALIZABLE",
        }
    }
}

/// Transaction characteristics named by one statement (or pending).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Chars {
    pub(crate) access: Option<Access>,
    pub(crate) isolation: Option<Isolation>,
}

/// A recognised `SET [scope] TRANSACTION <characteristics>` statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SetTransaction {
    /// No scope: the next transaction only.
    Next(Chars),
    /// `SESSION` / `LOCAL`: replaces the pending characteristics it names.
    Session(Chars),
    /// `GLOBAL`: other sessions only.
    Global,
}

/// Parses the plain `SET [SESSION | LOCAL | GLOBAL] TRANSACTION <item> [, <item>]` form
/// (optional trailing `;`). `None` for anything else, a text with a block comment included:
/// an executable comment (`/*! … */`) may hold characteristics the server applies or not
/// depending on its version.
pub(crate) fn parse_set_transaction(text: &str) -> Option<SetTransaction> {
    if text.contains("/*") {
        return None;
    }
    let toks = code_tokens(text, LexicalRules::MYSQL);
    let mut end = toks.len();
    while end > 0 && toks[end - 1].kind == CodeTokenKind::Semicolon {
        end -= 1;
    }
    let toks = &toks[..end];
    let word = |i: usize| -> Option<String> {
        toks.get(i)
            .filter(|t| t.kind == CodeTokenKind::Word)
            .map(|t| t.text(text).to_ascii_uppercase())
    };
    if word(0)? != "SET" {
        return None;
    }
    let mut i = 1;
    let scope = match word(1)?.as_str() {
        "SESSION" | "LOCAL" => {
            i += 1;
            Some(true)
        }
        "GLOBAL" => {
            i += 1;
            None
        }
        _ => Some(false),
    };
    if word(i)? != "TRANSACTION" {
        return None;
    }
    i += 1;
    let mut chars = Chars::default();
    loop {
        match word(i)?.as_str() {
            "ISOLATION" => {
                if word(i + 1)? != "LEVEL" || chars.isolation.is_some() {
                    return None;
                }
                let (level, n) = match (word(i + 2)?.as_str(), word(i + 3).as_deref()) {
                    ("READ", Some("UNCOMMITTED")) => (Isolation::ReadUncommitted, 4),
                    ("READ", Some("COMMITTED")) => (Isolation::ReadCommitted, 4),
                    ("REPEATABLE", Some("READ")) => (Isolation::RepeatableRead, 4),
                    ("SERIALIZABLE", _) => (Isolation::Serializable, 3),
                    _ => return None,
                };
                chars.isolation = Some(level);
                i += n;
            }
            "READ" => {
                let access = match word(i + 1)?.as_str() {
                    "ONLY" => Access::ReadOnly,
                    "WRITE" => Access::ReadWrite,
                    _ => return None,
                };
                if chars.access.is_some() {
                    return None;
                }
                chars.access = Some(access);
                i += 2;
            }
            _ => return None,
        }
        match toks.get(i) {
            None => break,
            Some(t) if t.kind == CodeTokenKind::Other && t.text(text) == "," => i += 1,
            Some(_) => return None,
        }
    }
    Some(match scope {
        Some(false) => SetTransaction::Next(chars),
        Some(true) => SetTransaction::Session(chars),
        None => SetTransaction::Global,
    })
}

/// The system variables whose assignment changes the characteristics of the next
/// transaction. MySQL's `Sys_var_transaction_isolation::session_update` (and its
/// `transaction_read_only` counterpart) writes the session value *and* the next transaction's,
/// so even a session-scope assignment overwrites a pending one-shot characteristic; with `@@`
/// and no scope the assignment applies to the next transaction only.
const TX_VARIABLES: [&str; 4] = [
    "transaction_isolation",
    "transaction_read_only",
    "tx_isolation",
    "tx_read_only",
];

/// A token's text as a name: lower-cased, one pair of backticks or double or single quotes
/// removed (a string in a target position is not valid SQL, but an `ANSI_QUOTES` session reads
/// `"…"` as a name).
fn as_name(tok: &str) -> String {
    let b = tok.as_bytes();
    let inner = match (b.first(), b.last()) {
        (Some(q @ (b'`' | b'"' | b'\'')), Some(e)) if b.len() >= 2 && q == e => {
            &tok[1..tok.len() - 1]
        }
        _ => tok,
    };
    inner.to_ascii_lowercase()
}

/// How a statement assigns one of [`TX_VARIABLES`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TxVarAssignment {
    /// Every such assignment names `SESSION` / `LOCAL` in its own target (`SET SESSION
    /// transaction_isolation = …`, `SET @@session.tx_isolation = …`): the session value and
    /// the next transaction's are both replaced, nothing new is left pending.
    Session,
    /// Any other form: `@@` with no scope (next transaction only), no scope keyword in its own
    /// target (one may come from an earlier assignment of the list), `GLOBAL`, `PERSIST`,
    /// MariaDB `SET STATEMENT … FOR`, or a segment the scan cannot split.
    Other,
}

/// Scans a `SET` statement (an executable comment's content included, comments otherwise
/// skipped) for an assignment to one of [`TX_VARIABLES`], in any scope, case or quoting. Only
/// assignment *targets* count, so `SET @a = @@tx_isolation` (a read) does not, nor does a user
/// variable of the same name (`SET @tx_isolation = 1`). A segment of the list with no `=` /
/// `:=` is a target as a whole (fails closed).
pub(crate) fn tx_var_assignment(text: &str) -> Option<TxVarAssignment> {
    let toks = code_tokens(text, LexicalRules::MYSQL);
    if !toks.first().is_some_and(|t| t.is_word(text, "SET")) {
        return None;
    }
    // The assignment list, split at top-level commas.
    let mut segments: Vec<&[CodeToken]> = Vec::new();
    let mut depth = 0usize;
    let mut start = 1;
    for (i, t) in toks.iter().enumerate().skip(1) {
        match t.kind {
            CodeTokenKind::OpenParen => depth += 1,
            CodeTokenKind::CloseParen => depth = depth.saturating_sub(1),
            CodeTokenKind::Other if depth == 0 && t.text(text) == "," => {
                segments.push(&toks[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    segments.push(&toks[start..]);
    let mut found = None;
    for seg in segments {
        // The target: the tokens before the first top-level `=` (`:=` lexes as `:` and `=`).
        let mut depth = 0usize;
        let mut end = seg.len();
        for (i, t) in seg.iter().enumerate() {
            match t.kind {
                CodeTokenKind::OpenParen => depth += 1,
                CodeTokenKind::CloseParen => depth = depth.saturating_sub(1),
                CodeTokenKind::Other if depth == 0 && t.text(text) == "=" => {
                    end = i;
                    break;
                }
                _ => {}
            }
        }
        let target = &seg[..end];
        let at = |i: usize| {
            target
                .get(i)
                .is_some_and(|t| t.kind == CodeTokenKind::Other && t.text(text) == "@")
        };
        let (mut session, mut other_scope, mut assigns) = (false, false, false);
        for (i, t) in target.iter().enumerate() {
            if !matches!(
                t.kind,
                CodeTokenKind::Word | CodeTokenKind::QuotedIdent | CodeTokenKind::Str
            ) {
                continue;
            }
            match as_name(t.text(text)).as_str() {
                "session" | "local" => session = true,
                "global" | "persist" | "persist_only" | "statement" => other_scope = true,
                n if TX_VARIABLES.contains(&n) => {
                    // `@name` (a single `@`) is a user variable.
                    let user_variable = i >= 1 && at(i - 1) && !(i >= 2 && at(i - 2));
                    assigns |= !user_variable;
                }
                _ => {}
            }
        }
        if assigns {
            found = Some(
                if session && !other_scope && found != Some(TxVarAssignment::Other) {
                    TxVarAssignment::Session
                } else {
                    TxVarAssignment::Other
                },
            );
        }
    }
    found
}

/// First words of statements that commit implicitly, or may start, commit or end a
/// transaction, although the classifier does not read them as DDL or transaction control:
/// `LOCK TABLES`, `UNLOCK TABLES`, `ANALYZE` / `OPTIMIZE` / `CHECK` / `REPAIR TABLE`, `FLUSH`,
/// `CACHE INDEX`, `LOAD INDEX` and `CALL`.
const IMPLICIT_COMMIT_WORDS: [&str; 10] = [
    "LOCK", "UNLOCK", "ANALYZE", "OPTIMIZE", "CHECK", "REPAIR", "FLUSH", "CACHE", "LOAD", "CALL",
];

/// What one user statement may do to the pending characteristics, computed from its text
/// before it is sent.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Effect {
    parsed: Option<SetTransaction>,
    /// Not parsed, and may leave characteristics pending in a form the driver cannot read: the
    /// word `TRANSACTION` outside strings and quoted names (`SET TRANSACTION /* c */ …`), or
    /// an assignment to a transaction variable that is not explicitly session-scope.
    may_set_next: bool,
    /// Not parsed, and assigns a transaction variable in some scope ([`tx_var_assignment`]).
    assigns_tx_var: bool,
    /// Its first word is in [`IMPLICIT_COMMIT_WORDS`].
    implicit_commit: bool,
    /// Has a `SELECT`: a subquery (`SET @a = (SELECT … FROM t)`) may read a transactional
    /// table, which in auto-commit mode is a transaction of its own.
    has_select: bool,
}

impl Effect {
    pub(crate) fn of(text: &str) -> Self {
        let parsed = parse_set_transaction(text);
        let toks = code_tokens(text, LexicalRules::MYSQL);
        let assignment = parsed.is_none().then(|| tx_var_assignment(text)).flatten();
        Self {
            parsed,
            may_set_next: parsed.is_none()
                && (assignment == Some(TxVarAssignment::Other)
                    || toks.iter().any(|t| t.is_word(text, "TRANSACTION"))),
            assigns_tx_var: assignment.is_some(),
            implicit_commit: toks
                .first()
                .is_some_and(|t| IMPLICIT_COMMIT_WORDS.iter().any(|w| t.is_word(text, w))),
            has_select: toks.iter().any(|t| t.is_word(text, "SELECT")),
        }
    }
}

/// The characteristics the user left pending for the next transaction, as far as the driver
/// can tell.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Pending {
    chars: Chars,
    /// A statement may have left characteristics pending that the driver cannot restore.
    unknown: bool,
}

impl Pending {
    /// Navigator reads must be refused: characteristics may be pending that a read would
    /// consume and the driver could not set again.
    pub(crate) fn blocks_navigator(&self) -> bool {
        self.unknown
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Drops a pending access mode (the driver's `SET SESSION TRANSACTION READ ONLY`
    /// re-assert in a read-only session replaces it).
    pub(crate) fn clear_access(&mut self) {
        self.chars.access = None;
    }

    fn remove(&mut self, named: Chars) {
        if named.access.is_some() {
            self.chars.access = None;
        }
        if named.isolation.is_some() {
            self.chars.isolation = None;
        }
    }

    /// The statement that sets the pending characteristics again, from validated values;
    /// `None` when nothing is pending. Never `READ WRITE` in a read-only session.
    pub(crate) fn reissue_sql(&self, read_only: bool) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(level) = self.chars.isolation {
            parts.push(format!("ISOLATION LEVEL {}", level.sql()));
        }
        match self.chars.access {
            Some(Access::ReadOnly) => parts.push("READ ONLY".to_owned()),
            Some(Access::ReadWrite) if !read_only => parts.push("READ WRITE".to_owned()),
            _ => {}
        }
        if parts.is_empty() {
            return None;
        }
        Some(format!("SET TRANSACTION {}", parts.join(", ")))
    }

    /// After a user statement: `ok` when the server answered without an error, `in_trans`
    /// and `auto_commit` from the server's (fresh) status flags.
    pub(crate) fn after_user_statement(
        &mut self,
        effect: Effect,
        class: StatementClass,
        ok: bool,
        in_trans: bool,
        auto_commit: bool,
    ) {
        if in_trans {
            // A transaction is open: any pending characteristics went to it (and `SET
            // TRANSACTION` fails inside one).
            self.clear();
            return;
        }
        if let Some(parsed) = effect.parsed {
            if ok {
                match parsed {
                    SetTransaction::Next(c) => {
                        if c.access.is_some() {
                            self.chars.access = c.access;
                        }
                        if c.isolation.is_some() {
                            self.chars.isolation = c.isolation;
                        }
                    }
                    SetTransaction::Session(c) => self.remove(c),
                    SetTransaction::Global => {}
                }
            }
            return;
        }
        let pending = self.chars != Chars::default();
        // An assignment to a transaction variable, whether or not it succeeded (a list of
        // assignments may be applied in part): a scope-less `@@` form leaves characteristics
        // pending, and any form, a session-scope one included, overwrites the ones the driver
        // tracked, so re-issuing those would re-apply stale (possibly weaker) settings.
        // Whatever the class of an assignment; the word `TRANSACTION` alone says nothing in
        // transaction control and DDL, which consume them anyway (below): `START
        // TRANSACTION`, a table named `transaction`.
        let consumes = matches!(
            class,
            StatementClass::Transaction | StatementClass::Ddl | StatementClass::Destructive
        );
        if (effect.assigns_tx_var && (pending || effect.may_set_next))
            || (effect.may_set_next && !consumes)
        {
            self.unknown = true;
            return;
        }
        match class {
            // `COMMIT` / `ROLLBACK` / `START TRANSACTION` consume them, and DDL commits
            // implicitly before it runs (whether or not it then fails).
            StatementClass::Transaction | StatementClass::Ddl | StatementClass::Destructive => {
                self.clear();
            }
            // In auto-commit mode a statement on a transactional table is a transaction of its
            // own. (In manual-commit mode such a statement leaves `in_trans` set, above.) A
            // statement that failed may have started its transaction, consuming them, before
            // it failed, or had it rolled back whole (a deadlock): the driver cannot tell.
            StatementClass::Read | StatementClass::Write => {
                if !ok && pending {
                    self.unknown = true;
                } else if ok && auto_commit {
                    self.clear();
                }
            }
            // Session statements, `ChangesAccessMode`, `Unknown` (`CALL`, `LOCK TABLES`,
            // `ANALYZE TABLE`, `FLUSH`…), succeeded or failed. Several of these commit
            // implicitly, a `CALL` may start, commit or fail inside a transaction, and a `SET`
            // with a subquery reads a table: with characteristics pending, the driver cannot
            // tell whether they were consumed.
            _ => {
                if pending
                    && (class == StatementClass::Unknown
                        || effect.implicit_commit
                        || effect.has_select)
                {
                    self.unknown = true;
                }
            }
        }
    }

    /// After a driver statement (transaction control from the core, session settings).
    pub(crate) fn after_driver_statement(&mut self, sql: &str, ok: bool) {
        if !ok {
            return;
        }
        match sql {
            "START TRANSACTION" | "COMMIT" | "ROLLBACK" => self.clear(),
            _ => {
                if let Some(SetTransaction::Session(c)) = parse_set_transaction(sql) {
                    self.remove(c);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn next(access: Option<Access>, isolation: Option<Isolation>) -> Option<SetTransaction> {
        Some(SetTransaction::Next(Chars { access, isolation }))
    }

    #[test]
    fn parses_the_plain_forms() {
        assert_eq!(
            parse_set_transaction("SET TRANSACTION READ ONLY"),
            next(Some(Access::ReadOnly), None)
        );
        assert_eq!(
            parse_set_transaction("set transaction isolation level read committed, read write;"),
            next(Some(Access::ReadWrite), Some(Isolation::ReadCommitted))
        );
        assert_eq!(
            parse_set_transaction("SET TRANSACTION READ ONLY ,ISOLATION LEVEL SERIALIZABLE"),
            next(Some(Access::ReadOnly), Some(Isolation::Serializable))
        );
        assert_eq!(
            parse_set_transaction("SET  TRANSACTION\n ISOLATION LEVEL REPEATABLE READ -- x"),
            next(None, Some(Isolation::RepeatableRead))
        );
        assert_eq!(
            parse_set_transaction("SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED"),
            next(None, Some(Isolation::ReadUncommitted))
        );
        assert_eq!(
            parse_set_transaction("SET SESSION TRANSACTION READ WRITE"),
            Some(SetTransaction::Session(Chars {
                access: Some(Access::ReadWrite),
                isolation: None
            }))
        );
        assert_eq!(
            parse_set_transaction("SET LOCAL TRANSACTION ISOLATION LEVEL SERIALIZABLE"),
            Some(SetTransaction::Session(Chars {
                access: None,
                isolation: Some(Isolation::Serializable)
            }))
        );
        assert_eq!(
            parse_set_transaction("SET GLOBAL TRANSACTION READ ONLY"),
            Some(SetTransaction::Global)
        );
    }

    #[test]
    fn refuses_everything_else() {
        for text in [
            "SET TRANSACTION",
            "SET TRANSACTION READ ONLY, READ WRITE",
            "SET TRANSACTION READ ONLY,",
            "SET TRANSACTION READ ONLY READ WRITE",
            "SET TRANSACTION ISOLATION LEVEL SNAPSHOT",
            "SET TRANSACTION ISOLATION LEVEL READ COMMITTED, ISOLATION LEVEL SERIALIZABLE",
            "SET TRANSACTION /*!80000 ISOLATION LEVEL SERIALIZABLE, */ READ ONLY",
            "SET TRANSACTION /* c */ READ ONLY",
            "SET @@transaction_read_only = 1",
            "SET transaction_isolation = 'READ-COMMITTED'",
            "SET TRANSACTION READ ONLY; SET TRANSACTION READ WRITE",
            "SET PERSIST TRANSACTION READ ONLY",
            "START TRANSACTION READ ONLY",
            "SET `TRANSACTION` READ ONLY",
            "",
        ] {
            assert_eq!(parse_set_transaction(text), None, "{text}");
        }
    }

    #[test]
    fn spots_other_forms() {
        assert!(Effect::of("SET @@transaction_read_only = 1").may_set_next);
        assert!(Effect::of("set @@TX_ISOLATION = 'SERIALIZABLE'").may_set_next);
        assert!(Effect::of("SET TRANSACTION /* c */ READ ONLY").may_set_next);
        assert!(!Effect::of("SET TRANSACTION READ ONLY").may_set_next);
        assert!(!Effect::of("SET @@SESSION.transaction_isolation = 'READ-COMMITTED'").may_set_next);
        assert!(!Effect::of("SET @a = 'TRANSACTION'").may_set_next);
        assert!(!Effect::of("SET @a = @@tx_isolation").may_set_next);
        assert!(Effect::of("SET @@`tx_read_only` = 1").may_set_next);
        assert!(!Effect::of("SET sql_mode = ''").may_set_next);
    }

    fn user(p: &mut Pending, text: &str, class: StatementClass, in_trans: bool, ac: bool) {
        p.after_user_statement(Effect::of(text), class, true, in_trans, ac);
    }

    #[test]
    fn tracks_and_reissues() {
        let mut p = Pending::default();
        assert_eq!(p.reissue_sql(false), None);
        user(
            &mut p,
            "SET TRANSACTION READ ONLY",
            StatementClass::Session,
            false,
            false,
        );
        assert_eq!(
            p.reissue_sql(false).as_deref(),
            Some("SET TRANSACTION READ ONLY")
        );
        user(
            &mut p,
            "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            StatementClass::Session,
            false,
            false,
        );
        assert_eq!(
            p.reissue_sql(false).as_deref(),
            Some("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE, READ ONLY")
        );
        // Kept across statements that do not consume them.
        user(&mut p, "SELECT 1", StatementClass::Read, false, false);
        user(&mut p, "SET @a = 1", StatementClass::Session, false, false);
        assert!(p.reissue_sql(false).is_some() && !p.blocks_navigator());
        // `SET SESSION TRANSACTION <x>` replaces only `x`.
        user(
            &mut p,
            "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED",
            StatementClass::Session,
            false,
            false,
        );
        assert_eq!(
            p.reissue_sql(false).as_deref(),
            Some("SET TRANSACTION READ ONLY")
        );
        // A statement that opens a transaction consumes them.
        user(&mut p, "SELECT * FROM t", StatementClass::Read, true, false);
        assert_eq!(p.reissue_sql(false), None);

        user(
            &mut p,
            "SET TRANSACTION READ WRITE",
            StatementClass::ChangesAccessMode,
            false,
            true,
        );
        assert_eq!(
            p.reissue_sql(true),
            None,
            "never READ WRITE in a read-only session"
        );
        // Auto-commit: a read is a transaction of its own.
        user(&mut p, "SELECT * FROM t", StatementClass::Read, false, true);
        assert_eq!(p.reissue_sql(false), None);

        user(
            &mut p,
            "SET TRANSACTION READ ONLY",
            StatementClass::Session,
            false,
            true,
        );
        user(&mut p, "ROLLBACK", StatementClass::Transaction, false, true);
        assert_eq!(p.reissue_sql(false), None);

        // A failed statement changes nothing.
        p.after_user_statement(
            Effect::of("SET TRANSACTION READ ONLY"),
            StatementClass::Session,
            false,
            false,
            false,
        );
        assert_eq!(p.reissue_sql(false), None);
    }

    #[test]
    fn unknown_forms_block_the_navigator_until_consumed() {
        let mut p = Pending::default();
        user(
            &mut p,
            "SET @@transaction_read_only = 1",
            StatementClass::Unknown,
            false,
            false,
        );
        assert!(p.blocks_navigator());
        user(&mut p, "SELECT 1", StatementClass::Read, false, false);
        assert!(p.blocks_navigator());
        user(&mut p, "COMMIT", StatementClass::Transaction, false, false);
        assert!(!p.blocks_navigator());
        user(
            &mut p,
            "SET @@tx_isolation = 'SERIALIZABLE'",
            StatementClass::Unknown,
            false,
            false,
        );
        p.after_driver_statement("ROLLBACK", true);
        assert!(!p.blocks_navigator());
    }

    #[test]
    fn finds_transaction_variable_assignments() {
        use TxVarAssignment::{Other, Session};
        for (text, expected) in [
            (
                "SET SESSION transaction_isolation = 'SERIALIZABLE'",
                Some(Session),
            ),
            ("set local TX_ISOLATION='SERIALIZABLE'", Some(Session)),
            ("SET @@SESSION.tx_isolation = 'SERIALIZABLE'", Some(Session)),
            ("SET @@local.transaction_read_only = 0", Some(Session)),
            // Backticks, any case (C).
            ("SET @@`session`.`tx_isolation` = 'x'", Some(Session)),
            ("SET SESSION `Transaction_Isolation` = 'x'", Some(Session)),
            ("SET @@`SESSION` . `TX_READ_ONLY` := 0", Some(Session)),
            // No scope of its own, `@@` with no scope, other scopes.
            ("SET transaction_isolation = 'SERIALIZABLE'", Some(Other)),
            ("SET `tx_read_only` = 0", Some(Other)),
            (
                "SET @@transaction_isolation = 'READ-UNCOMMITTED'",
                Some(Other),
            ),
            ("SET @@`tx_isolation` = 'READ-UNCOMMITTED'", Some(Other)),
            ("SET GLOBAL transaction_isolation = 'x'", Some(Other)),
            ("SET PERSIST transaction_read_only = 1", Some(Other)),
            ("SET STATEMENT tx_isolation = 'x' FOR SELECT 1", Some(Other)),
            (
                "SET SESSION sql_mode = '', transaction_isolation = 'x'",
                Some(Other),
            ),
            (
                "SET SESSION tx_isolation = 'x', @@tx_read_only = 0",
                Some(Other),
            ),
            (
                "SET SESSION tx_isolation = 'x', SESSION tx_read_only = 0",
                Some(Session),
            ),
            (
                "SET /*!80000 SESSION transaction_isolation = 'SERIALIZABLE' */",
                Some(Session),
            ),
            ("SET @a = 1, tx_isolation = (SELECT 'x')", Some(Other)),
            ("SET tx_isolation", Some(Other)),
            // Reads, user variables, strings, comments, other statements.
            ("SET @a = @@tx_isolation", None),
            ("SET @a = @@SESSION.transaction_isolation, @b = 1", None),
            ("SET @tx_isolation = 1", None),
            ("SET @`tx_isolation` = 1", None),
            ("SET @a = 'tx_isolation'", None),
            ("SET sql_mode = '' -- tx_isolation = 1", None),
            ("SET @a = (SELECT tx_isolation FROM t)", None),
            ("SELECT @@tx_isolation", None),
            ("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE", None),
        ] {
            assert_eq!(tx_var_assignment(text), expected, "{text}");
        }
    }

    fn pending_read_uncommitted() -> Pending {
        let mut p = Pending::default();
        user(
            &mut p,
            "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
            StatementClass::Session,
            false,
            false,
        );
        assert!(p.reissue_sql(false).is_some());
        p
    }

    #[test]
    fn a_session_assignment_overwrites_pending_characteristics() {
        // A: the server's next-transaction level is now SERIALIZABLE; re-issuing READ
        // UNCOMMITTED would weaken it.
        for text in [
            "SET SESSION transaction_isolation = 'SERIALIZABLE'",
            "SET transaction_isolation='SERIALIZABLE'",
            "SET @@SESSION.tx_isolation = 'SERIALIZABLE'",
            "SET @@local.tx_isolation = 'SERIALIZABLE'",
            "SET SESSION transaction_read_only = 1",
            "SET @@session.tx_read_only = 1",
            // C: backticks.
            "SET @@`session`.`transaction_isolation` = 'SERIALIZABLE'",
            "SET SESSION `TX_ISOLATION` = 'SERIALIZABLE'",
        ] {
            for class in [
                StatementClass::ChangesAccessMode,
                StatementClass::Session,
                StatementClass::Unknown,
            ] {
                for ok in [true, false] {
                    let mut p = pending_read_uncommitted();
                    p.after_user_statement(Effect::of(text), class, ok, false, false);
                    assert!(p.blocks_navigator(), "{text} {class:?} {ok}");
                }
            }
        }
        // With nothing pending, a session-scope assignment leaves nothing pending...
        let mut p = Pending::default();
        user(
            &mut p,
            "SET SESSION transaction_isolation = 'SERIALIZABLE'",
            StatementClass::ChangesAccessMode,
            false,
            false,
        );
        assert!(!p.blocks_navigator() && p.reissue_sql(false).is_none());
        // ...but a next-transaction form does (in any quoting).
        for text in [
            "SET @@`transaction_isolation` = 'READ-UNCOMMITTED'",
            "SET tx_isolation = 'READ-UNCOMMITTED'",
        ] {
            let mut p = Pending::default();
            user(
                &mut p,
                text,
                StatementClass::ChangesAccessMode,
                false,
                false,
            );
            assert!(p.blocks_navigator(), "{text}");
        }
        // I-2: reading the variable is not an assignment.
        let mut p = pending_read_uncommitted();
        user(
            &mut p,
            "SET @a = @@tx_isolation",
            StatementClass::Session,
            false,
            false,
        );
        assert!(!p.blocks_navigator() && p.reissue_sql(false).is_some());
    }

    #[test]
    fn implicit_commits_make_pending_characteristics_unknown() {
        // B: these commit implicitly or may start and end a transaction.
        for text in [
            "LOCK TABLES t READ",
            "UNLOCK TABLES",
            "ANALYZE TABLE t",
            "OPTIMIZE TABLE t",
            "CHECK TABLE t",
            "REPAIR TABLE t",
            "FLUSH TABLES",
            "CACHE INDEX t IN hot",
            "LOAD INDEX INTO CACHE t",
            "CALL p()",
            "DO 1",
        ] {
            for ok in [true, false] {
                for ac in [true, false] {
                    // As the classifier reads them, and whatever it would say.
                    for class in [StatementClass::Unknown, StatementClass::Session] {
                        let mut p = pending_read_uncommitted();
                        p.after_user_statement(Effect::of(text), class, ok, false, ac);
                        let expected = class == StatementClass::Unknown || text != "DO 1";
                        assert_eq!(p.blocks_navigator(), expected, "{text} {ok} {ac} {class:?}");
                    }
                }
            }
        }
        // A read or write that failed may have started (and ended) its transaction.
        for class in [StatementClass::Read, StatementClass::Write] {
            for ac in [true, false] {
                let mut p = pending_read_uncommitted();
                p.after_user_statement(
                    Effect::of("INSERT INTO t VALUES (1)"),
                    class,
                    false,
                    false,
                    ac,
                );
                assert!(p.blocks_navigator());
            }
        }
        // A `SET` with a subquery reads a table.
        let mut p = pending_read_uncommitted();
        user(
            &mut p,
            "SET @a = (SELECT x FROM t)",
            StatementClass::Session,
            false,
            true,
        );
        assert!(p.blocks_navigator());
        // Nothing pending: nothing to lose, the navigator stays available.
        let mut p = Pending::default();
        user(
            &mut p,
            "LOCK TABLES t READ",
            StatementClass::Unknown,
            false,
            true,
        );
        p.after_user_statement(
            Effect::of("CALL p()"),
            StatementClass::Unknown,
            false,
            false,
            true,
        );
        assert!(!p.blocks_navigator());
        // Still fine: harmless session statements keep the pending characteristics.
        let mut p = pending_read_uncommitted();
        for text in ["SET @a = 1", "USE nine", "SET autocommit = 0"] {
            user(&mut p, text, StatementClass::Session, false, false);
        }
        assert!(!p.blocks_navigator() && p.reissue_sql(false).is_some());
        // DDL and transaction control consume them, a `transaction` name included.
        user(
            &mut p,
            "CREATE TABLE transaction (id INT)",
            StatementClass::Ddl,
            false,
            false,
        );
        assert!(!p.blocks_navigator() && p.reissue_sql(false).is_none());
    }

    #[test]
    fn driver_statements() {
        let mut p = Pending::default();
        user(
            &mut p,
            "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE, READ ONLY",
            StatementClass::Session,
            false,
            false,
        );
        p.after_driver_statement("SET SESSION autocommit = 0", true);
        p.after_driver_statement("COMMIT", false);
        assert!(p.reissue_sql(false).is_some());
        p.after_driver_statement("SET SESSION TRANSACTION READ WRITE", true);
        assert_eq!(
            p.reissue_sql(false).as_deref(),
            Some("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        );
        p.after_driver_statement("START TRANSACTION", true);
        assert_eq!(p.reissue_sql(false), None);
    }
}
