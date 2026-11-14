//! Defence in depth for I1: find keys in a loaded file that look like they hold a secret value.
//!
//! NineSQL's own models have no such field, so a hit means a hand edit or a foreign tool put a
//! value there. The caller warns (naming the key path, never the value); such unknown keys are
//! dropped on the next save because models ignore unknown fields.

use serde_json::Value;
use zeroize::Zeroize;

/// Key fragments (lower case, `-` / `_` removed) that suggest a secret value.
const SUSPICIOUS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "secret",
    "token",
    "apikey",
    "accesskey",
    "privatekey",
    "credential",
];
/// Keys that suggest a secret only when they match exactly.
const SUSPICIOUS_EXACT: &[&str] = &["pwd", "pass", "pin"];

/// Known driver option keys that match the fragments above but never hold a secret (normalized).
/// Checked before the heuristics so they are not reported, stripped or refused **by key**; their
/// value must still have the expected shape ([`benign_value_shape_ok`]), otherwise the entry is
/// treated as secret-like ([`is_secret_like_entry`]).
pub const BENIGN_SECRET_LIKE_KEYS: &[&str] = &[
    // MySQL Connector/J: charset used to encode the password.
    "passwordcharacterencoding",
    // PostgreSQL: password hashing method (server setting, also passed as an option).
    "passwordencryption",
];

/// Values accepted for `password_encryption` (PostgreSQL), lower case.
const PASSWORD_ENCRYPTION_VALUES: &[&str] = &["scram-sha-256", "md5", "on", "off", "true", "false"];

/// Whether `value` has the shape expected for the benign key `normalized_key` (one of
/// [`BENIGN_SECRET_LIKE_KEYS`]). A misplaced password in such a key fails the check.
///
/// - `passwordcharacterencoding`: a known charset name ([`is_known_charset`]): `UTF-8`, `utf8mb4`,
///   `ISO-8859-1`, `Cp1252`, `windows-1252`, `latin1`, `Shift_JIS`…
/// - `passwordencryption`: one of `scram-sha-256`, `md5`, `on`, `off`, `true`, `false`.
///
/// Any other key returns `false`.
#[must_use]
pub fn benign_value_shape_ok(normalized_key: &str, value: &str) -> bool {
    match normalized_key {
        "passwordcharacterencoding" => is_known_charset(value),
        "passwordencryption" => PASSWORD_ENCRYPTION_VALUES
            .iter()
            .any(|v| v.eq_ignore_ascii_case(value)),
        _ => false,
    }
}

/// Charset names accepted as they are (case-insensitive).
const CHARSET_NAMES: &[&str] = &[
    "utf-8",
    "utf8",
    "utf8mb3",
    "utf8mb4",
    "utf-16",
    "utf16",
    "utf-16le",
    "utf-16be",
    "utf16le",
    "utf16be",
    "utf-32",
    "utf32",
    "utf-32le",
    "utf-32be",
    "us-ascii",
    "ascii",
    "big5",
    "big5-hkscs",
    "gbk",
    "gb2312",
    "gb18030",
    "shift_jis",
    "shift-jis",
    "sjis",
    "ms932",
    "windows-31j",
    "euc-jp",
    "euc_jp",
    "eucjpms",
    "euc-kr",
    "euc_kr",
    "euckr",
    "euc-cn",
    "euc-tw",
    "koi8-r",
    "koi8-u",
    "koi8r",
    "koi8u",
    "tis-620",
    "tis620",
    "iso-2022-jp",
];

/// Charset families: a prefix followed by 1 to 5 digits (`cp1252`, `windows-1251`, `latin1`,
/// `iso-8859-15`, `ISO8859_1`).
const CHARSET_NUMBERED_PREFIXES: &[&str] = &[
    "iso-8859-",
    "iso8859-",
    "iso8859_",
    "iso_8859-",
    "cp",
    "windows-",
    "latin",
    "ibm",
];

/// Whether `value` is a known character set name (Java, MySQL / MariaDB and IANA spellings,
/// case-insensitive). A pasted password such as `Hunter2` is not.
#[must_use]
pub fn is_known_charset(value: &str) -> bool {
    let v = value.to_ascii_lowercase();
    CHARSET_NAMES.contains(&v.as_str())
        || CHARSET_NUMBERED_PREFIXES.iter().any(|p| {
            v.strip_prefix(p).is_some_and(|n| {
                (1..=5).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit())
            })
        })
}

/// Normalizes a key: lower case, without `-`, `_`, `.` and spaces.
#[must_use]
pub fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|c| !matches!(c, '-' | '_' | '.' | ' '))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether a key name looks like it would hold a secret.
#[must_use]
pub fn is_secret_like_key(key: &str) -> bool {
    let norm = normalize_key(key);
    if BENIGN_SECRET_LIKE_KEYS.contains(&norm.as_str()) {
        return false;
    }
    SUSPICIOUS_EXACT.contains(&norm.as_str()) || SUSPICIOUS.iter().any(|s| norm.contains(s))
}

/// Whether a key / string value pair looks like it holds a secret: the key is secret-like
/// ([`is_secret_like_key`]), or it is a benign key ([`BENIGN_SECRET_LIKE_KEYS`]) whose value does
/// not have the expected shape ([`benign_value_shape_ok`]). Empty values never count.
#[must_use]
pub fn is_secret_like_entry(key: &str, value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let norm = normalize_key(key);
    if BENIGN_SECRET_LIKE_KEYS.contains(&norm.as_str()) {
        return !benign_value_shape_ok(&norm, value);
    }
    is_secret_like_key(key)
}

fn is_secret_like_child(key: &str, child: &Value) -> bool {
    matches!(child, Value::String(s) if is_secret_like_entry(key, s))
}

/// Overwrites every string of a JSON value with zeroes before it is dropped (best effort: copies
/// made earlier by the parser or the allocator are out of reach).
pub fn zeroize_value(value: &mut Value) {
    match value {
        Value::String(s) => s.zeroize(),
        Value::Array(items) => items.iter_mut().for_each(zeroize_value),
        Value::Object(map) => map.values_mut().for_each(zeroize_value),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Paths (`connections[2].auth.password`) of entries that look secret
/// ([`is_secret_like_entry`]) and hold a non-empty string. Values are never returned.
#[must_use]
pub fn find_secret_like_keys(value: &Value) -> Vec<String> {
    let mut found = Vec::new();
    walk(value, &mut String::new(), &mut found);
    found
}

/// Removes every key that [`find_secret_like_keys`] would report, in place, zeroizing the removed
/// values. Returns the number of removed keys.
pub fn strip_secret_like_keys(value: &mut Value) -> usize {
    match value {
        Value::Object(map) => {
            let mut removed = 0;
            map.retain(|key, child| {
                if is_secret_like_child(key, child) {
                    zeroize_value(child);
                    removed += 1;
                    false
                } else {
                    true
                }
            });
            removed + map.values_mut().map(strip_secret_like_keys).sum::<usize>()
        }
        Value::Array(items) => items.iter_mut().map(strip_secret_like_keys).sum(),
        _ => 0,
    }
}

fn walk(value: &Value, path: &mut String, found: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let len = path.len();
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(key);
                if is_secret_like_child(key, child) {
                    found.push(path.clone());
                }
                walk(child, path, found);
                path.truncate(len);
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                let len = path.len();
                path.push_str(&format!("[{i}]"));
                walk(child, path, found);
                path.truncate(len);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn benign_keys_are_not_secret_like() {
        for key in ["passwordCharacterEncoding", "password_encryption"] {
            assert!(!is_secret_like_key(key), "{key}");
        }
        for key in [
            "password",
            "PASSWORD",
            "sslpassword",
            "clientSecret",
            "api-token",
        ] {
            assert!(is_secret_like_key(key), "{key}");
        }
    }

    #[test]
    fn detects_secret_like_string_values_only() {
        let v = json!({
            "connections": [
                { "auth": { "method": "password", "secret": { "store": "keyring", "id": "x" } } },
                { "auth": { "Password": "hunter2" }, "properties": { "ssl-key-passphrase": "x" } }
            ],
            "token": "",
            "pwd": "p",
            "compass": "north"
        });
        let found = find_secret_like_keys(&v);
        assert_eq!(
            found,
            vec![
                "connections[1].auth.Password",
                "connections[1].properties.ssl-key-passphrase",
                "pwd",
            ]
        );
        let mut stripped = v.clone();
        assert_eq!(strip_secret_like_keys(&mut stripped), 3);
        assert!(find_secret_like_keys(&stripped).is_empty());
        assert_eq!(stripped["compass"], "north");
        assert_eq!(stripped["connections"][0]["auth"]["method"], "password");
    }

    #[test]
    fn benign_keys_need_a_benign_value_shape() {
        for (key, value) in [
            ("passwordCharacterEncoding", "UTF-8"),
            ("passwordCharacterEncoding", "utf8mb4"),
            ("passwordCharacterEncoding", "ISO-8859-1"),
            ("passwordCharacterEncoding", "Cp1252"),
            ("passwordCharacterEncoding", "windows-1251"),
            ("passwordCharacterEncoding", "latin1"),
            ("passwordCharacterEncoding", "ISO8859_15"),
            ("passwordCharacterEncoding", "Shift_JIS"),
            ("passwordCharacterEncoding", "UTF-16LE"),
            ("password_encryption", "scram-sha-256"),
            ("password_encryption", "MD5"),
        ] {
            assert!(!is_secret_like_entry(key, value), "{key}={value}");
        }
        for (key, value) in [
            ("passwordCharacterEncoding", "Hunter2 secret!"),
            ("passwordCharacterEncoding", "12345678"),
            // Security review L4: identifier-like passwords are not charsets.
            ("passwordCharacterEncoding", "Hunter2"),
            ("passwordCharacterEncoding", "cpHunter2"),
            ("passwordCharacterEncoding", "latin123456"),
            ("passwordCharacterEncoding", "utf-8x"),
            ("passwordCharacterEncoding", &"a".repeat(41)),
            ("password_encryption", "Hunter2"),
            ("password", "x"),
        ] {
            assert!(is_secret_like_entry(key, value), "{key}={value}");
        }
        assert!(!is_secret_like_entry("password", ""));
        let mut v =
            json!({ "passwordCharacterEncoding": "p@ss w0rd", "password_encryption": "md5" });
        assert_eq!(find_secret_like_keys(&v), ["passwordCharacterEncoding"]);
        assert_eq!(strip_secret_like_keys(&mut v), 1);
        assert_eq!(v, json!({ "password_encryption": "md5" }));
    }
}
