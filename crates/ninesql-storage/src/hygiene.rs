//! Defence in depth for I1: find keys in a loaded file that look like they hold a secret value.
//!
//! NineSQL's own models have no such field, so a hit means a hand edit or a foreign tool put a
//! value there. The caller warns (naming the key path, never the value); such unknown keys are
//! dropped on the next save because models ignore unknown fields.

use serde_json::Value;

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

/// Whether a key name looks like it would hold a secret.
#[must_use]
pub fn is_secret_like_key(key: &str) -> bool {
    let norm: String = key
        .chars()
        .filter(|c| *c != '-' && *c != '_' && *c != '.')
        .flat_map(char::to_lowercase)
        .collect();
    SUSPICIOUS_EXACT.contains(&norm.as_str()) || SUSPICIOUS.iter().any(|s| norm.contains(s))
}

/// Paths (`connections[2].auth.password`) of keys that look secret and hold a non-empty string.
/// Values are never returned.
#[must_use]
pub fn find_secret_like_keys(value: &Value) -> Vec<String> {
    let mut found = Vec::new();
    walk(value, &mut String::new(), &mut found);
    found
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
                if matches!(child, Value::String(s) if !s.is_empty()) && is_secret_like_key(key) {
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
    }
}
