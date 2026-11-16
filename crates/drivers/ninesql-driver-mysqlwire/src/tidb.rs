//! TiDB read-only checks (P4-C, phase 4 review M1).
//!
//! TiDB has no session read-only mode: a read-only session is accepted only when the instance
//! refuses writes (`tidb_super_read_only` / `tidb_restricted_read_only`) **and** that refusal
//! applies to the session's account. TiDB exempts accounts holding
//! `RESTRICTED_REPLICA_WRITER_ADMIN` (typical replication users), directly or through an active
//! role, from both modes. So at connect the driver also:
//! - reads `SHOW GRANTS` (the current user with its active roles expanded, as TiDB shows it)
//!   and refuses an account holding `RESTRICTED_REPLICA_WRITER_ADMIN`, `SUPER` or
//!   `ALL PRIVILEGES ON *.*`; a line it cannot read fails the check ([`grants_refusal`]);
//! - sends [`WRITE_PROBE`], a write that cannot change anything whatever the account, which
//!   must fail with error 1836 ("Running in read-only mode") ([`probe_gated`]).
//!
//! After every statement the active roles are read back with the instance's mode
//! (`CURRENT_ROLE()`), and a change closes the session: `SET ROLE` is refused client-side as an
//! access-mode change, so this only catches a classifier gap.

use ninesql_driver_api::{DriverError, DriverResult};

use crate::actor::Rows;

/// A write that the read-only gate refuses with 1836 before anything else, and that cannot
/// change anything when the gate does not apply to the account:
/// - TiDB checks the read-only mode at the start of planning, after name resolution and before
///   privilege checks. A missing table fails earlier (name resolution: 1146, or 1142 for an
///   account without privileges on the name), so the target must exist for every account:
///   `information_schema.CHARACTER_SETS` always does.
/// - `information_schema` tables are memory tables that TiDB never writes: an account the gate
///   does not cover (`RESTRICTED_REPLICA_WRITER_ADMIN`) gets 1142, `root` on a writable
///   instance too. `WHERE FALSE` matches no row besides.
///
/// Tested on TiDB 8.5.8: 1836 for a `SELECT`-only account, an account with `ALL` on a database,
/// `SUPER` and `root`; 1142 for `RESTRICTED_REPLICA_WRITER_ADMIN` (granted or through an active
/// role) with the instance read-only, and for every account on a writable instance.
pub(crate) const WRITE_PROBE: &str = "DELETE FROM information_schema.CHARACTER_SETS WHERE FALSE";

/// TiDB's error for a write refused by the read-only mode.
const READ_ONLY_ERROR: &str = "(1836)";

/// Whether the probe's outcome proves the read-only gate applies to the session's account:
/// only error 1836 does (`statement_error` maps it to `ReadOnly` with the code appended by
/// `MyError::display`). Success or any other error does not.
pub(crate) fn probe_gated(outcome: &DriverResult<Rows>) -> bool {
    matches!(outcome, Err(DriverError::ReadOnly(m)) if m.ends_with(READ_ONLY_ERROR))
}

/// Privileges with which TiDB lets an account write on a read-only instance, or with which it
/// may (`SUPER` is not exempt on TiDB 8.5, refused anyway: it can lift the instance's mode).
const BYPASS_PRIVILEGES: &[&str] = &["RESTRICTED_REPLICA_WRITER_ADMIN", "SUPER"];

/// Reads the `SHOW GRANTS` lines of the current user (active roles expanded) and returns why a
/// read-only session must be refused, if it must. Fails closed: no line, an empty line or a
/// line that is not `GRANT <privileges> ON <level> TO …` or `GRANT <roles> TO …` is a refusal.
/// A role line is skipped: TiDB lists the privileges of the active roles on their own lines,
/// and a role that is not active cannot be enabled (`SET ROLE` is refused, and a change of
/// `CURRENT_ROLE()` closes the session).
pub(crate) fn grants_refusal(rows: &Rows) -> Option<String> {
    if rows.is_empty() {
        return Some("the account's privileges could not be read (SHOW GRANTS was empty)".into());
    }
    for row in rows {
        let Some(line) = row.first().and_then(Option::as_deref) else {
            return Some("the account's privileges could not be read".into());
        };
        match privileges_of(line) {
            Grant::Role => {}
            Grant::Unreadable => {
                return Some("the account's privileges could not be read".into());
            }
            Grant::Privileges { privileges, global } => {
                for p in privileges {
                    if BYPASS_PRIVILEGES.contains(&p.as_str()) {
                        return Some(format!("the account holds {p}"));
                    }
                    if global && (p == "ALL" || p == "ALL PRIVILEGES") {
                        return Some("the account holds ALL PRIVILEGES ON *.*".into());
                    }
                }
            }
        }
    }
    None
}

#[derive(Debug, PartialEq, Eq)]
enum Grant {
    /// `GRANT <privileges> ON <level> TO …`: the privileges, upper case, column lists removed,
    /// spaces collapsed; `global` when the level is `*.*`.
    Privileges {
        privileges: Vec<String>,
        global: bool,
    },
    /// `GRANT <roles> TO …`.
    Role,
    Unreadable,
}

/// A top-level piece of a grant line: a bare word, or anything else (quoted text, a
/// parenthesised list, punctuation) kept as written.
#[derive(Debug)]
struct Piece<'a> {
    text: &'a str,
    word: bool,
}

/// Splits a grant line into top-level pieces: quoted names (`'…'`, `"…"`, `` `…` ``, doubled
/// quotes inside) and parenthesised lists are one piece each; `,` is its own piece. `None`
/// when a quote or a parenthesis is not closed.
fn pieces(line: &str) -> Option<Vec<Piece<'_>>> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let start = i;
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if matches!(c, b'\'' | b'"' | b'`') {
            i += 1;
            loop {
                match b.get(i) {
                    None => return None,
                    Some(&q) if q == c => {
                        if b.get(i + 1) == Some(&c) {
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    }
                    Some(_) => i += 1,
                }
            }
            out.push(Piece {
                text: &line[start..i],
                word: false,
            });
        } else if c == b'(' {
            let mut depth = 0usize;
            let mut quote: Option<u8> = None;
            loop {
                let &ch = b.get(i)?;
                i += 1;
                match quote {
                    Some(q) if ch == q => quote = None,
                    Some(_) => {}
                    None if matches!(ch, b'\'' | b'"' | b'`') => quote = Some(ch),
                    None if ch == b'(' => depth += 1,
                    None if ch == b')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    None => {}
                }
            }
            out.push(Piece {
                text: &line[start..i],
                word: false,
            });
        } else if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$') {
                i += 1;
            }
            out.push(Piece {
                text: &line[start..i],
                word: true,
            });
        } else if c == b')' {
            return None;
        } else {
            // Punctuation (`,`, `*`, `.`, `@`) and any other byte, one character at a time.
            let len = line[i..].chars().next().map_or(1, char::len_utf8);
            i += len;
            out.push(Piece {
                text: &line[start..i],
                word: false,
            });
        }
    }
    Some(out)
}

fn is_word(p: &Piece<'_>, w: &str) -> bool {
    p.word && p.text.eq_ignore_ascii_case(w)
}

fn privileges_of(line: &str) -> Grant {
    let Some(pieces) = pieces(line) else {
        return Grant::Unreadable;
    };
    if !pieces.first().is_some_and(|p| is_word(p, "GRANT")) {
        return Grant::Unreadable;
    }
    let Some(end) = pieces
        .iter()
        .skip(1)
        .position(|p| is_word(p, "ON") || is_word(p, "TO"))
        .map(|i| i + 1)
    else {
        return Grant::Unreadable;
    };
    if is_word(&pieces[end], "TO") {
        // `GRANT 'r'@'%' TO …`: roles are quoted names, never privilege words.
        return if pieces[1..end].iter().all(|p| !p.word) && end > 1 {
            Grant::Role
        } else {
            Grant::Unreadable
        };
    }
    // `ON <level> TO`: the level is the pieces up to `TO`.
    let Some(to) = pieces
        .iter()
        .skip(end + 1)
        .position(|p| is_word(p, "TO"))
        .map(|i| i + end + 1)
    else {
        return Grant::Unreadable;
    };
    let level: String = pieces[end + 1..to].iter().map(|p| p.text).collect();
    let mut privileges = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for p in &pieces[1..end] {
        if p.text == "," {
            if current.is_empty() {
                return Grant::Unreadable;
            }
            privileges.push(current.join(" ").to_ascii_uppercase());
            current.clear();
        } else if p.word {
            current.push(p.text);
        } else if p.text.starts_with('(') && !current.is_empty() {
            // A column list (`SELECT (a, b)`): not part of the privilege's name.
        } else {
            return Grant::Unreadable;
        }
    }
    if current.is_empty() {
        return Grant::Unreadable;
    }
    privileges.push(current.join(" ").to_ascii_uppercase());
    Grant::Privileges {
        privileges,
        global: level == "*.*",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn rows(lines: &[&str]) -> Rows {
        lines.iter().map(|l| vec![Some((*l).to_owned())]).collect()
    }

    #[test]
    fn grant_lines_parse() {
        assert_eq!(
            privileges_of("GRANT USAGE ON *.* TO 'nine'@'%'"),
            Grant::Privileges {
                privileges: vec!["USAGE".into()],
                global: true
            }
        );
        assert_eq!(
            privileges_of("GRANT ALL PRIVILEGES ON `nine`.* TO 'nine'@'%'"),
            Grant::Privileges {
                privileges: vec!["ALL PRIVILEGES".into()],
                global: false
            }
        );
        assert_eq!(
            privileges_of(
                "GRANT Select (`a ON b`, c), insert ON `x ON y`.`t` TO 'u'@'%' WITH GRANT OPTION"
            ),
            Grant::Privileges {
                privileges: vec!["SELECT".into(), "INSERT".into()],
                global: false
            }
        );
        assert_eq!(
            privileges_of("GRANT BACKUP_ADMIN,RESTRICTED_REPLICA_WRITER_ADMIN ON *.* TO 'r'@'%'"),
            Grant::Privileges {
                privileges: vec![
                    "BACKUP_ADMIN".into(),
                    "RESTRICTED_REPLICA_WRITER_ADMIN".into()
                ],
                global: true
            }
        );
        assert_eq!(
            privileges_of("GRANT 'p_r'@'%', 'o ON x'@'%' TO 'p_ru'@'%'"),
            Grant::Role
        );
        for bad in [
            "",
            "REVOKE SELECT ON *.* FROM 'u'@'%'",
            "GRANT SELECT ON *.*",
            "GRANT ON *.* TO 'u'@'%'",
            "GRANT SELECT,, INSERT ON *.* TO 'u'@'%'",
            "GRANT 'unterminated TO 'u'@'%'",
            "GRANT SELECT (a ON *.* TO 'u'@'%'",
            "GRANT SELECT) ON *.* TO 'u'@'%'",
            "GRANT TO 'u'@'%'",
            "GRANT r TO 'u'@'%'",
        ] {
            assert_eq!(privileges_of(bad), Grant::Unreadable, "{bad}");
        }
    }

    #[test]
    fn bypass_privileges_refuse_a_read_only_session() {
        // Plain accounts, as TiDB 8.5.8 shows them.
        assert_eq!(
            grants_refusal(&rows(&[
                "GRANT USAGE ON *.* TO 'nine'@'%'",
                "GRANT ALL PRIVILEGES ON `nine`.* TO 'nine'@'%'",
            ])),
            None
        );
        assert_eq!(
            grants_refusal(&rows(&[
                "GRANT SELECT ON *.* TO 'ro'@'%'",
                "GRANT 'reader'@'%' TO 'ro'@'%'",
            ])),
            None
        );
        // The account itself, or an active role expanded by SHOW GRANTS.
        for lines in [
            &[
                "GRANT USAGE ON *.* TO 'p'@'%'",
                "GRANT RESTRICTED_REPLICA_WRITER_ADMIN ON *.* TO 'p'@'%'",
            ][..],
            &[
                "GRANT 'p_r'@'%' TO 'p'@'%'",
                "GRANT restricted_replica_writer_admin ON *.* TO 'p'@'%'",
            ],
            &["GRANT SUPER ON *.* TO 'p'@'%'"],
            &["GRANT ALL PRIVILEGES ON *.* TO 'root'@'%' WITH GRANT OPTION"],
            &["GRANT ALL ON *.* TO 'root'@'%'"],
            &["GRANT SELECT, Super ON *.* TO 'p'@'%'"],
        ] {
            assert!(grants_refusal(&rows(lines)).is_some(), "{lines:?}");
        }
        // Fails closed.
        assert!(grants_refusal(&Vec::new()).is_some());
        assert!(grants_refusal(&vec![vec![None]]).is_some());
        assert!(grants_refusal(&vec![Vec::new()]).is_some());
        assert!(
            grants_refusal(&rows(&[
                "GRANT USAGE ON *.* TO 'nine'@'%'",
                "something else",
            ]))
            .is_some()
        );
    }

    #[test]
    fn only_error_1836_proves_the_gate() {
        let gated: DriverResult<Rows> = Err(DriverError::ReadOnly(
            "Running in read-only mode (1836)".into(),
        ));
        assert!(probe_gated(&gated));
        for not in [
            Ok(Vec::new()),
            Err(DriverError::ReadOnly(
                "Cannot execute statement in a READ ONLY transaction. (1792)".into(),
            )),
            Err(DriverError::Query {
                code: Some("1142".into()),
                message: "DELETE command denied".into(),
                position: None,
            }),
            Err(DriverError::Closed("gone".into())),
        ] {
            assert!(!probe_gated(&not), "{not:?}");
        }
    }

    #[test]
    fn pieces_never_panic() {
        for s in [
            "(",
            ")",
            "((",
            "'",
            "`a``b`",
            "GRANT (",
            "GRANT ()",
            "é",
            "GRANT é ON",
            "(')')",
        ] {
            let _ = privileges_of(s);
        }
    }
}

/// The probe and the role read-back against the `tidb` service of `dev/compose.yaml`, below the
/// public API (`tests/tidb.rs` covers the connect refusals). Switches the dev instance to
/// `tidb_super_read_only` for a moment and always back; the integration tests of `tests/tidb.rs`
/// run in another test binary, which cargo never runs at the same time. Skipped when TiDB is
/// unreachable, unless `NINESQL_REQUIRE_ENGINES` lists `tidb`.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod engine {
    use std::time::Duration;

    use futures_util::FutureExt as _;
    use ninesql_driver_api::{
        Auth, ConnectParams, DriverError, Endpoint, EngineId, ExecOptions, Secret, Session,
        Statement, StatementClass, TlsMode,
    };

    use super::{WRITE_PROBE, probe_gated};
    use crate::{MySession, MySqlWireDriver};

    const ROOT_PASSWORD: &str = "nine_dev_only_root";
    const USER_PASSWORD: &str = "ninesql_m1_dev_only";

    /// Host, port and database of the dev service.
    type Base = (String, u16, String);

    fn dev_params() -> Option<Base> {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../dev/engines.json");
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let e = doc["engines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == "tidb")
            .unwrap();
        let host = e["host"].as_str().unwrap();
        let port = u16::try_from(e["port"].as_u64().unwrap()).unwrap();
        let addr = format!("{host}:{port}");
        let reachable = addr
            .parse()
            .ok()
            .and_then(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_secs(2)).ok())
            .is_some();
        if !reachable {
            let required = std::env::var("NINESQL_REQUIRE_ENGINES")
                .unwrap_or_default()
                .split(',')
                .any(|s| s.trim() == "tidb");
            assert!(!required, "tidb is required but unreachable at {addr}");
            return None;
        }
        Some((
            host.to_owned(),
            port,
            e["database"].as_str().unwrap().to_owned(),
        ))
    }

    fn as_user(base: &Base, user: &str, password: &str) -> ConnectParams {
        let mut p = ConnectParams::new(EngineId::TIDB, Endpoint::tcp(&base.0, base.1));
        p.database = Some(base.2.clone());
        p.tls.mode = TlsMode::Disable;
        p.application_name = "ninesql-mysqlwire-tidb-probe".into();
        p.connect_timeout = Duration::from_secs(30);
        p.auth = Auth::password(user, Some(Secret::new(password.to_owned())));
        p
    }

    async fn drain(
        s: &mut MySession,
        sql: &str,
        class: Option<StatementClass>,
    ) -> Result<Vec<String>, DriverError> {
        let st = Statement::sql(sql);
        let mut stream = match class {
            Some(c) => s.dispatch(st, ExecOptions::default(), c).await?,
            None => s.execute(st, ExecOptions::default()).await?,
        };
        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            if let ninesql_driver_api::ResultItem::Rows(b) = item? {
                use ninesql_driver_api::arrow_array::Array as _;
                use ninesql_driver_api::arrow_array::cast::AsArray;
                let c = b.column(0);
                if let Some(a) = c.as_string_opt::<i32>() {
                    out.extend((0..a.len()).map(|i| a.value(i).to_owned()));
                }
            }
        }
        Ok(out)
    }

    async fn set_read_only(base: &Base, on: bool) {
        let driver = MySqlWireDriver::new();
        let mut root = driver
            .open(as_user(base, "root", ROOT_PASSWORD))
            .await
            .unwrap();
        let v = u8::from(on);
        drain(
            &mut root,
            &format!("SET GLOBAL tidb_super_read_only = {v}"),
            None,
        )
        .await
        .unwrap();
        for _ in 0..100 {
            let got = drain(
                &mut root,
                "SELECT CAST(@@GLOBAL.tidb_super_read_only AS CHAR)",
                None,
            )
            .await
            .unwrap();
            if got.first().map(String::as_str) == Some(&v.to_string()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Box::new(root).close().await.unwrap();
        // Propagation is asynchronous: give other sessions a moment too.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn probe_is_refused_only_for_accounts_the_gate_covers() {
        let Some(base) = dev_params() else { return };
        let driver = MySqlWireDriver::new();
        set_read_only(&base, false).await;
        let mut root = driver
            .open(as_user(&base, "root", ROOT_PASSWORD))
            .await
            .unwrap();
        for sql in [
            "DROP USER IF EXISTS ninesql_m1p_repl, ninesql_m1p_plain, ninesql_m1p_dormant",
            "DROP ROLE IF EXISTS ninesql_m1p_writer",
            "CREATE ROLE ninesql_m1p_writer",
            "GRANT RESTRICTED_REPLICA_WRITER_ADMIN ON *.* TO ninesql_m1p_writer",
            &format!("CREATE USER ninesql_m1p_repl IDENTIFIED BY '{USER_PASSWORD}'"),
            &format!("CREATE USER ninesql_m1p_plain IDENTIFIED BY '{USER_PASSWORD}'"),
            &format!("CREATE USER ninesql_m1p_dormant IDENTIFIED BY '{USER_PASSWORD}'"),
            "GRANT SELECT ON nine.* TO ninesql_m1p_repl, ninesql_m1p_plain, ninesql_m1p_dormant",
            "GRANT RESTRICTED_REPLICA_WRITER_ADMIN ON *.* TO ninesql_m1p_repl",
            "GRANT ninesql_m1p_writer TO ninesql_m1p_dormant",
        ] {
            drain(&mut root, sql, None).await.unwrap();
        }
        let body = async {
            // Writable instance: the probe is refused by privileges for everyone.
            let plain = driver
                .open(as_user(&base, "ninesql_m1p_plain", USER_PASSWORD))
                .await
                .unwrap();
            assert!(!probe_gated(&plain.run(WRITE_PROBE).await));
            assert!(!probe_gated(&root.run(WRITE_PROBE).await));
            Box::new(plain).close().await.unwrap();

            set_read_only(&base, true).await;
            // The gate covers a plain account and root (`ALL PRIVILEGES`, `SUPER`)...
            let plain = driver
                .open(as_user(&base, "ninesql_m1p_plain", USER_PASSWORD))
                .await
                .unwrap();
            assert!(probe_gated(&plain.run(WRITE_PROBE).await));
            Box::new(plain).close().await.unwrap();
            let root2 = driver
                .open(as_user(&base, "root", ROOT_PASSWORD))
                .await
                .unwrap();
            assert!(probe_gated(&root2.run(WRITE_PROBE).await));
            Box::new(root2).close().await.unwrap();
            // ...not RESTRICTED_REPLICA_WRITER_ADMIN, for which the probe alone refuses.
            let repl = driver
                .open(as_user(&base, "ninesql_m1p_repl", USER_PASSWORD))
                .await
                .unwrap();
            let r = repl.run(WRITE_PROBE).await;
            assert!(!probe_gated(&r), "{r:?}");
            Box::new(repl).close().await.unwrap();

            // A dormant role holding the privilege: the session is accepted read-only; `SET
            // ROLE` is refused client-side, and engine-side too when sent past the classifier
            // (forged class: TiDB's read-only mode refuses `SET ROLE` with 1836).
            let mut p = as_user(&base, "ninesql_m1p_dormant", USER_PASSWORD);
            p.read_only = true;
            let mut dormant = driver.open(p).await.unwrap();
            assert!(dormant.is_read_only());
            for class in [None, Some(StatementClass::Session)] {
                let r = drain(&mut dormant, "SET ROLE ALL", class).await;
                assert!(
                    matches!(r, Err(DriverError::ReadOnly(_))),
                    "{class:?}: {r:?}"
                );
            }
            let role = drain(&mut dormant, "SELECT CURRENT_ROLE()", None)
                .await
                .unwrap();
            assert_eq!(role, ["NONE"]);
            // The read-back after every statement: a change of the active roles (simulated
            // here, since TiDB refuses every way to make one) closes the session.
            *dormant.link.shared.tidb_role.lock().unwrap() = Some("`other`@`%`".into());
            let r = drain(&mut dormant, "SELECT 1", None).await;
            assert!(matches!(r, Err(DriverError::ReadOnly(_))), "{r:?}");
            assert!(matches!(
                drain(&mut dormant, "SELECT 1", None).await,
                Err(DriverError::Closed(_))
            ));
            Box::new(dormant).close().await.unwrap();
        };
        let outcome = std::panic::AssertUnwindSafe(body).catch_unwind().await;
        set_read_only(&base, false).await;
        let mut root = driver
            .open(as_user(&base, "root", ROOT_PASSWORD))
            .await
            .unwrap();
        for sql in [
            "DROP USER IF EXISTS ninesql_m1p_repl, ninesql_m1p_plain, ninesql_m1p_dormant",
            "DROP ROLE IF EXISTS ninesql_m1p_writer",
        ] {
            drain(&mut root, sql, None).await.unwrap();
        }
        Box::new(root).close().await.unwrap();
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }
}
