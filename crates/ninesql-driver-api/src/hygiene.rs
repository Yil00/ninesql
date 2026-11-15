//! Log hygiene for statement text (I1).
//!
//! Drivers log statement text (init SQL included) only at `trace` level, only when
//! [`ConnectParams::may_log_statements`](crate::ConnectParams::may_log_statements) is `true`,
//! and never when [`text_may_hold_credentials`] is `true`: `ALTER ROLE bob PASSWORD 'x'`,
//! `CREATE USER u IDENTIFIED BY 'x'` or a `dblink_connect('postgresql://u:pw@h/db')` would
//! otherwise put a password in the log.
//!
//! The rule is the one `ninesql-core` applies before logging a statement or recording it in
//! the history, so the core and the drivers skip the same statements. It is lexical and fails
//! closed: a credential word anywhere (keyword, identifier, string literal, comment) is enough.

/// Words that mark a statement as handling a credential (lower case). Matched as whole words,
/// ignoring ASCII case; any character that is not alphanumeric separates words (`_` included,
/// so `aws_secret_access_key` holds `secret`).
pub const CREDENTIAL_WORDS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "passphrase",
    "identified",
    "secret",
    "secrets",
    "credential",
    "credentials",
    "encrypted",
    "unencrypted",
    "encrypt",
    "decrypt",
    "token",
    "apikey",
    "privatekey",
];

/// Whether `text` holds a URI with credentials, `scheme://user:password@…`: the pattern
/// `://[^\s/@]*:[^\s@]*@` (`dblink_connect('postgresql://u:pw@h/db')`, `CREATE SUBSCRIPTION …
/// CONNECTION '…'`, `postgres_fdw` options).
#[must_use]
pub fn has_uri_credentials(text: &str) -> bool {
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let after = &rest[at + 3..];
        // `[^\s/@]*` then `:`.
        let user_end = after
            .find(|c: char| c.is_whitespace() || c == '/' || c == '@' || c == ':')
            .unwrap_or(after.len());
        if after[user_end..].starts_with(':') {
            // `[^\s@]*` then `@`.
            let pass = &after[user_end + 1..];
            let pass_end = pass
                .find(|c: char| c.is_whitespace() || c == '@')
                .unwrap_or(pass.len());
            if pass[pass_end..].starts_with('@') {
                return true;
            }
        }
        rest = after;
    }
    false
}

/// Whether statement `text` may hold a credential: a [`CREDENTIAL_WORDS`] word anywhere, or a
/// URI with credentials ([`has_uri_credentials`]). Drivers never log such a text, even at
/// `trace` level with logging allowed (see the [module documentation](self)).
#[must_use]
pub fn text_may_hold_credentials(text: &str) -> bool {
    has_uri_credentials(text)
        || text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| (3..=16).contains(&w.len()))
            .any(|w| CREDENTIAL_WORDS.iter().any(|k| w.eq_ignore_ascii_case(k)))
}

/// Whether a driver may log statement `text`: logging is allowed for the session (`allowed`:
/// [`ConnectParams::may_log_statements`](crate::ConnectParams::may_log_statements) at connect)
/// and the text holds no credential ([`text_may_hold_credentials`]).
#[must_use]
pub fn may_log_statement_text(allowed: bool, text: &str) -> bool {
    allowed && !text_may_hold_credentials(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn credential_words_are_found_anywhere_ignoring_case() {
        for text in [
            "ALTER ROLE bob PASSWORD 'x'",
            "alter role bob with encrypted password 'x'",
            "CREATE USER bob IDENTIFIED BY 'x'",
            "ALTER USER bob IDENTIFIED WITH mysql_native_password BY 'x'",
            "SET PASSWORD FOR bob = 'x'",
            "CREATE USER MAPPING FOR bob SERVER s OPTIONS (user 'u', password 'x')",
            "CREATE SECRET s (TYPE s3, KEY_ID 'k', SECRET 'x')",
            "SELECT pgp_sym_encrypt('data', 'x')",
            "SELECT pgp_sym_decrypt(d, 'x')",
            "EXECUTE 'ALTER ROLE bob PASSWORD ' || quote_literal(p)",
            "SELECT 1 -- password: hunter2",
            "SELECT \"Password\" FROM t",
            "UPDATE s SET aws_secret_access_key = 'x'",
            "ALTER ROLE bob PASSWORD$$x$$",
            "SELECT dblink_connect('postgresql://u:pw@h/db')",
            "CREATE SUBSCRIPTION s CONNECTION 'postgres://repl:s3cr3t@primary:5432/app' PUBLICATION p",
            "SELECT * FROM t WHERE url = 'mysql://root:@localhost'",
            "SELECT 'https://u:p/q@host'",
            "SET @PWD = 'x'",
            "SELECT passwd, passphrase FROM t",
            "SELECT * FROM api WHERE apikey = 'x'",
            "INSERT INTO k (privatekey) VALUES ('x')",
            "SELECT token FROM s",
            "SELECT credentials, secrets FROM vault",
            "ALTER ROLE r UNENCRYPTED PASSWORD 'x'",
        ] {
            assert!(text_may_hold_credentials(text), "{text}");
            assert!(!may_log_statement_text(true, text), "{text}");
        }
        for text in [
            "SELECT * FROM numbers(3)",
            "DELETE FROM t WHERE id = 1",
            "SELECT passwordless FROM t",
            "CREATE TABLE tokens (id int)",
            "SELECT 'https://example.test/a@b'",
            "SELECT 'postgresql://host:5432/db'",
            "SELECT 'mailto:user@example.test'",
            "SELECT 'a://b c:d@e'",
            "",
        ] {
            assert!(!text_may_hold_credentials(text), "{text}");
            assert!(may_log_statement_text(true, text), "{text}");
            assert!(!may_log_statement_text(false, text), "{text}");
        }
    }

    proptest! {
        /// A credential word stays found whatever surrounds it, with any separator and case.
        #[test]
        fn a_credential_word_is_always_found(
            before in "[a-z ]{0,20}",
            after in "[a-z ]{0,20}",
            word in prop::sample::select(CREDENTIAL_WORDS.to_vec()),
            sep in prop::sample::select(vec![" ", "_", "'", "\"", "(", ".", "\n", "-", "$"]),
            upper in any::<bool>(),
        ) {
            let w = if upper { word.to_ascii_uppercase() } else { word.to_owned() };
            let text = format!("{before}{sep}{w}{sep}{after}");
            prop_assert!(text_may_hold_credentials(&text), "{text:?}");
        }

        #[test]
        fn never_panics(text in "\\PC{0,200}") {
            let _ = text_may_hold_credentials(&text);
        }
    }
}
