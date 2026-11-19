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
//! characteristic `x`; two `SET TRANSACTION` statements accumulate.
//!
//! Residuals: a stored routine that runs `SET TRANSACTION` is not seen (the `CALL` text does not
//! say so), and in auto-commit mode a table-less user `SELECT` is taken as consuming the
//! pending characteristics (a later navigator read may then consume them).

use ninesql_driver_api::{CodeTokenKind, LexicalRules, StatementClass, code_tokens};

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

/// `true` when `text` may leave transaction characteristics pending in a form
/// [`parse_set_transaction`] does not read: the word `TRANSACTION` outside strings and quoted
/// names, or a system variable `@@transaction_…` / `@@tx_…` with no scope (MySQL applies
/// such an assignment to the next transaction only).
fn may_set_next(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("@@transaction_")
        || lower.contains("@@tx_")
        || code_tokens(text, LexicalRules::MYSQL)
            .iter()
            .any(|t| t.is_word(text, "TRANSACTION"))
}

/// What one user statement may do to the pending characteristics, computed from its text
/// before it is sent.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Effect {
    parsed: Option<SetTransaction>,
    may_set_next: bool,
}

impl Effect {
    pub(crate) fn of(text: &str) -> Self {
        let parsed = parse_set_transaction(text);
        Self {
            parsed,
            may_set_next: parsed.is_none() && may_set_next(text),
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
        match class {
            // `COMMIT` / `ROLLBACK` / `START TRANSACTION` consume them, and DDL commits
            // implicitly before it runs (whether or not it then fails).
            StatementClass::Transaction | StatementClass::Ddl | StatementClass::Destructive => {
                self.clear();
            }
            // In auto-commit mode a statement on a transactional table is a transaction of its
            // own. (In manual-commit mode such a statement leaves `in_trans` set, above.)
            StatementClass::Read | StatementClass::Write => {
                if ok && auto_commit {
                    self.clear();
                }
            }
            // Session statements, `ChangesAccessMode`, `Unknown` (`CALL`…).
            _ => {
                if ok && effect.may_set_next {
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
