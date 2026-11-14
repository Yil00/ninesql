//! Tolerant model of DBeaver's `data-sources*.json` (docs/06-dbeaver-interop.md).
//!
//! Parsing is **total**: once the bytes are valid JSON with an object at the top, every key ends
//! up either in a typed field or in the `extra` map of the object it belongs to. A known key with
//! an unexpected type (a number where a string was expected…) stays in `extra` too, so the
//! mapping reports it as not imported instead of failing the file. Nothing here panics.
//!
//! Handler property values may be strings, integers or floats (`"22"`, `22`, `22.0`, verified in
//! the P0-E fixtures): they are kept as [`Scalar`].
//!
//! Secret hygiene (I1): plaintext passwords found in the file (legacy layouts, hand edits) are
//! moved into [`SecretString`] at once. Keys of `extra` that look like secrets
//! ([`ninesql_storage::hygiene::is_secret_like_key`]) have their string values zeroized and
//! replaced by `null`: the key is still reported, the value is gone. `Debug` of [`Extra`] and
//! [`Properties`] prints keys only.

use std::borrow::Cow;
use std::fmt;

use ninesql_secrets::SecretString;
use ninesql_storage::hygiene::is_secret_like_key;
use serde_json::{Map, Number, Value};
use zeroize::Zeroize;

/// Error of [`parse_data_sources`]. Carries a position, never content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// Not valid JSON (or nested too deep).
    #[error("invalid JSON at line {line}, column {column}")]
    InvalidJson {
        /// 1-based line.
        line: usize,
        /// 1-based column.
        column: usize,
    },
    /// Valid JSON whose top level is not an object.
    #[error("the top level is not a JSON object")]
    NotAnObject,
}

/// A scalar JSON value: DBeaver writes the same settings as strings, booleans or numbers.
#[derive(Clone, PartialEq)]
pub enum Scalar {
    /// A string.
    Text(String),
    /// A boolean.
    Bool(bool),
    /// A number (integer or float).
    Number(Number),
}

impl Scalar {
    fn from_value(value: Value) -> Result<Self, Value> {
        match value {
            Value::String(s) => Ok(Self::Text(s)),
            Value::Bool(b) => Ok(Self::Bool(b)),
            Value::Number(n) => Ok(Self::Number(n)),
            other => Err(other),
        }
    }

    /// The value as text (`22.0` stays `22.0`; use [`Scalar::as_u16`] for ports).
    #[must_use]
    pub fn as_text(&self) -> Cow<'_, str> {
        match self {
            Self::Text(s) => Cow::Borrowed(s),
            Self::Bool(b) => Cow::Owned(b.to_string()),
            Self::Number(n) => Cow::Owned(n.to_string()),
        }
    }

    /// The value as a boolean: `true` / `false`, or the strings `"true"` / `"false"` (any case).
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            Self::Text(s) if s.trim().eq_ignore_ascii_case("true") => Some(true),
            Self::Text(s) if s.trim().eq_ignore_ascii_case("false") => Some(false),
            _ => None,
        }
    }

    /// The value as a port-like integer: `22`, `"22"`, `22.0` and `"22.0"` all give 22.
    #[must_use]
    pub fn as_u16(&self) -> Option<u16> {
        match self {
            Self::Bool(_) => None,
            Self::Number(n) => n
                .as_u64()
                .and_then(|v| u16::try_from(v).ok())
                .or_else(|| n.as_f64().and_then(f64_to_u16)),
            Self::Text(s) => {
                let s = s.trim();
                s.parse::<u16>()
                    .ok()
                    .or_else(|| s.parse::<f64>().ok().and_then(f64_to_u16))
            }
        }
    }

    /// The value as a non-negative integer (periods in seconds).
    #[must_use]
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Bool(_) => None,
            Self::Number(n) => n.as_u64(),
            Self::Text(s) => s.trim().parse().ok(),
        }
    }
}

fn f64_to_u16(v: f64) -> Option<u16> {
    (v.fract() == 0.0 && (0.0..=f64::from(u16::MAX)).contains(&v)).then(|| {
        // Exact: integral and in range (checked above).
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let p = v as u16;
        p
    })
}

impl fmt::Debug for Scalar {
    /// Values can be sensitive (driver properties): `Debug` shows the kind only.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Text(_) => "Scalar::Text(..)",
            Self::Bool(_) => "Scalar::Bool(..)",
            Self::Number(_) => "Scalar::Number(..)",
        })
    }
}

/// Keys the parser did not consume, with their raw values. `Debug` prints the keys only.
///
/// Values under secret-like keys are zeroized and replaced by `null` when the map is built.
#[derive(Clone, Default, PartialEq)]
pub struct Extra(Map<String, Value>);

impl Extra {
    fn new(mut map: Map<String, Value>) -> Self {
        scrub_secret_like(&mut map);
        Self(map)
    }

    /// Unconsumed keys, sorted.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// The raw value of an unconsumed key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// Whether every key was consumed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Number of unconsumed keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Unconsumed keys with their values.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }
}

impl fmt::Debug for Extra {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.0.keys()).finish()
    }
}

impl Drop for Extra {
    fn drop(&mut self) {
        // Unknown keys may hold credentials of future DBeaver versions: wipe what we can.
        for value in self.0.values_mut() {
            scrub(value);
        }
    }
}

/// Scrubs the value of every secret-like key, at any depth (the key itself is kept so the
/// report can name it).
fn scrub_secret_like(map: &mut Map<String, Value>) {
    for (key, value) in map.iter_mut() {
        if is_secret_like_key(key) {
            scrub(value);
        } else {
            scrub_secret_like_value(value);
        }
    }
}

fn scrub_secret_like_value(value: &mut Value) {
    match value {
        Value::Object(m) => scrub_secret_like(m),
        Value::Array(items) => items.iter_mut().for_each(scrub_secret_like_value),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// Zeroizes every string inside `value`, then replaces it by `null`.
fn scrub(value: &mut Value) {
    match value {
        Value::String(s) => s.zeroize(),
        Value::Array(items) => items.iter_mut().for_each(scrub),
        Value::Object(map) => map.values_mut().for_each(scrub),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    *value = Value::Null;
}

/// Driver or handler properties: scalar values by key. `Debug` prints the keys only.
#[derive(Clone, Default, PartialEq)]
pub struct Properties {
    /// Scalar properties, sorted by key.
    pub entries: Vec<(String, Scalar)>,
    /// Keys whose value was not a scalar (object, array, null): not imported.
    pub non_scalar: Vec<String>,
}

impl Properties {
    fn from_map(map: Map<String, Value>) -> Self {
        let mut out = Self::default();
        for (key, value) in map {
            match Scalar::from_value(value) {
                Ok(s) => out.entries.push((key, s)),
                Err(mut v) => {
                    scrub(&mut v);
                    out.non_scalar.push(key);
                }
            }
        }
        out
    }

    /// The value of a key (exact match).
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Scalar> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Whether there is no property at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.non_scalar.is_empty()
    }
}

impl fmt::Debug for Properties {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Properties")
            .field(
                "keys",
                &self.entries.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .field("non_scalar", &self.non_scalar)
            .finish()
    }
}

impl Drop for Properties {
    fn drop(&mut self) {
        for (_, value) in &mut self.entries {
            if let Scalar::Text(s) = value {
                s.zeroize();
            }
        }
    }
}

/// Parsed `data-sources*.json`.
#[derive(Debug, Default)]
pub struct DataSources {
    /// `folders`, sorted by key.
    pub folders: Vec<FolderDef>,
    /// `connections`, sorted by id.
    pub connections: Vec<ConnectionDef>,
    /// `connection-types`, sorted by id.
    pub connection_types: Vec<ConnectionTypeDef>,
    /// Entries of `folders` / `connections` / `connection-types` that were not objects (keys).
    pub invalid_entries: Vec<String>,
    /// Other top-level keys (and the three above when they were not objects).
    pub extra: Extra,
}

/// One entry of `folders`. The key is a simple name (with `parent`, up to 25.x) or a full path
/// (`Team A/Production`, 26.x).
#[derive(Debug)]
pub struct FolderDef {
    /// Key in `folders`.
    pub key: String,
    /// `parent`: the parent folder (name or path).
    pub parent: Option<String>,
    /// `description`.
    pub description: Option<String>,
    /// Other keys.
    pub extra: Extra,
}

/// One entry of `connection-types`.
#[derive(Debug)]
pub struct ConnectionTypeDef {
    /// Key in `connection-types` (`dev`, `prod`, `test`, custom ids).
    pub id: String,
    /// `name`.
    pub name: Option<String>,
    /// `color`: `R,G,B` or a theme color key (up to 25.x for built-in types).
    pub color: Option<String>,
    /// `colorDark` (26.x).
    pub color_dark: Option<String>,
    /// `description`.
    pub description: Option<String>,
    /// `auto-commit`.
    pub auto_commit: Option<bool>,
    /// `confirm-execute`.
    pub confirm_execute: Option<bool>,
    /// `confirm-data-change`.
    pub confirm_data_change: Option<bool>,
    /// Other keys (`smart-commit`, `auto-close-*`, `security`…), classified by the mapping.
    pub extra: Extra,
}

/// One entry of `connections`.
#[derive(Debug)]
pub struct ConnectionDef {
    /// Key in `connections`: DBeaver's data source id (`postgres-jdbc-<hex>-<hex>`).
    pub id: String,
    /// `provider` (`postgresql`, `mysql`, `generic`…).
    pub provider: Option<String>,
    /// `driver` (`postgres-jdbc`, `mysql8`, `mariaDB`…).
    pub driver: Option<String>,
    /// `name`.
    pub name: Option<String>,
    /// `description`.
    pub description: Option<String>,
    /// `save-password`.
    pub save_password: Option<bool>,
    /// `read-only`.
    pub read_only: Option<bool>,
    /// `folder`: full folder path with `/` separators.
    pub folder: Option<String>,
    /// `configuration`.
    pub configuration: ConfigurationDef,
    /// Other keys.
    pub extra: Extra,
}

/// `configuration` of a connection. `Debug` hides the URL (it can carry `user:password@`).
#[derive(Default)]
pub struct ConfigurationDef {
    /// `host`.
    pub host: Option<String>,
    /// `port` (a string in every fixture; numbers accepted).
    pub port: Option<Scalar>,
    /// `server` (server / instance name of some engines).
    pub server: Option<String>,
    /// `database`.
    pub database: Option<String>,
    /// `url`: JDBC URL. Held as a secret (zeroized on drop, redacted): it can carry
    /// `user:password@` or a `password=` parameter.
    pub url: Option<SecretString>,
    /// `configurationType`: `MANUAL` or `URL`.
    pub configuration_type: Option<String>,
    /// `type`: connection type id.
    pub type_id: Option<String>,
    /// `auth-model`.
    pub auth_model: Option<String>,
    /// `user`, when stored in clear (legacy layouts).
    pub user: Option<String>,
    /// `password`, when stored in clear (legacy layouts, hand edits). A secret (I1).
    pub password: Option<SecretString>,
    /// `properties`: driver properties.
    pub properties: Properties,
    /// `provider-properties`.
    pub provider_properties: Properties,
    /// `bootstrap`.
    pub bootstrap: BootstrapDef,
    /// `handlers`, sorted by id.
    pub handlers: Vec<HandlerDef>,
    /// Handler entries that were not objects (ids).
    pub invalid_handlers: Vec<String>,
    /// Other keys.
    pub extra: Extra,
}

/// `configuration.bootstrap`. `Debug` shows the number of init queries, not their text.
#[derive(Default)]
pub struct BootstrapDef {
    /// `defaultSchema`.
    pub default_schema: Option<String>,
    /// `defaultCatalog` (key name to verify: not in the fixtures).
    pub default_catalog: Option<String>,
    /// `autocommit`.
    pub autocommit: Option<bool>,
    /// `query`: init queries (string items only; others are counted in `invalid_queries`).
    pub queries: Vec<String>,
    /// Number of `query` items that were not strings.
    pub invalid_queries: usize,
    /// Other keys.
    pub extra: Extra,
}

impl fmt::Debug for ConfigurationDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigurationDef")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("server", &self.server)
            .field("database", &self.database)
            .field("url", &self.url.as_ref().map(|_| "[HIDDEN]"))
            .field("configuration_type", &self.configuration_type)
            .field("type_id", &self.type_id)
            .field("auth_model", &self.auth_model)
            .field("user", &self.user)
            .field("password", &self.password)
            .field("properties", &self.properties)
            .field("provider_properties", &self.provider_properties)
            .field("bootstrap", &self.bootstrap)
            .field("handlers", &self.handlers)
            .field("invalid_handlers", &self.invalid_handlers)
            .field("extra", &self.extra)
            .finish()
    }
}

impl fmt::Debug for BootstrapDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BootstrapDef")
            .field("default_schema", &self.default_schema)
            .field("default_catalog", &self.default_catalog)
            .field("autocommit", &self.autocommit)
            .field("query_count", &self.queries.len())
            .field("invalid_queries", &self.invalid_queries)
            .field("extra", &self.extra)
            .finish()
    }
}

/// One entry of `configuration.handlers` (SSH tunnel, TLS, proxy…).
#[derive(Debug)]
pub struct HandlerDef {
    /// Handler id (`ssh_tunnel`, `postgre_ssl`, `mysql_ssl`…).
    pub id: String,
    /// `type`: `TUNNEL`, `CONFIG`, `PROXY`…
    pub kind: Option<String>,
    /// `enabled`.
    pub enabled: Option<bool>,
    /// `save-password`.
    pub save_password: Option<bool>,
    /// `user`, when stored in clear (before DBeaver moves it to credentials-config.json).
    pub user: Option<String>,
    /// `password`, when stored in clear. A secret (I1).
    pub password: Option<SecretString>,
    /// `properties`.
    pub properties: Properties,
    /// Other keys.
    pub extra: Extra,
}

/// Parses the bytes of a `data-sources*.json` file.
///
/// # Errors
/// [`ParseError`] when the bytes are not JSON or the top level is not an object. Everything else
/// is tolerated (see the module documentation).
pub fn parse_data_sources(bytes: &[u8]) -> Result<DataSources, ParseError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| ParseError::InvalidJson {
        line: e.line(),
        column: e.column(),
    })?;
    let Value::Object(map) = value else {
        return Err(ParseError::NotAnObject);
    };
    let mut top = Fields::new(map);
    let mut out = DataSources::default();
    if let Some(folders) = top.object("folders") {
        for (key, value) in folders {
            match value {
                Value::Object(m) => out.folders.push(folder(key, m)),
                mut other => {
                    scrub(&mut other);
                    out.invalid_entries.push(format!("folders.{key}"));
                }
            }
        }
    }
    if let Some(connections) = top.object("connections") {
        for (key, value) in connections {
            match value {
                Value::Object(m) => out.connections.push(connection(key, m)),
                mut other => {
                    scrub(&mut other);
                    out.invalid_entries.push(format!("connections.{key}"));
                }
            }
        }
    }
    if let Some(types) = top.object("connection-types") {
        for (key, value) in types {
            match value {
                Value::Object(m) => out.connection_types.push(connection_type(key, m)),
                mut other => {
                    scrub(&mut other);
                    out.invalid_entries.push(format!("connection-types.{key}"));
                }
            }
        }
    }
    out.extra = top.finish();
    Ok(out)
}

fn folder(key: String, map: Map<String, Value>) -> FolderDef {
    let mut f = Fields::new(map);
    FolderDef {
        key,
        parent: f.string("parent"),
        description: f.string("description"),
        extra: f.finish(),
    }
}

fn connection_type(id: String, map: Map<String, Value>) -> ConnectionTypeDef {
    let mut f = Fields::new(map);
    ConnectionTypeDef {
        id,
        name: f.string("name"),
        color: f.string("color"),
        color_dark: f.string("colorDark"),
        description: f.string("description"),
        auto_commit: f.bool("auto-commit"),
        confirm_execute: f.bool("confirm-execute"),
        confirm_data_change: f.bool("confirm-data-change"),
        extra: f.finish(),
    }
}

fn connection(id: String, map: Map<String, Value>) -> ConnectionDef {
    let mut f = Fields::new(map);
    ConnectionDef {
        id,
        provider: f.string("provider"),
        driver: f.string("driver"),
        name: f.string("name"),
        description: f.string("description"),
        save_password: f.bool("save-password"),
        read_only: f.bool("read-only"),
        folder: f.string("folder"),
        configuration: f
            .object("configuration")
            .map(configuration)
            .unwrap_or_default(),
        extra: f.finish(),
    }
}

fn configuration(map: Map<String, Value>) -> ConfigurationDef {
    let mut f = Fields::new(map);
    let mut handlers = Vec::new();
    let mut invalid_handlers = Vec::new();
    if let Some(hs) = f.object("handlers") {
        for (id, value) in hs {
            match value {
                Value::Object(m) => handlers.push(handler(id, m)),
                mut other => {
                    scrub(&mut other);
                    invalid_handlers.push(id);
                }
            }
        }
    }
    ConfigurationDef {
        host: f.string("host"),
        port: f.scalar("port"),
        server: f.string("server"),
        database: f.string("database"),
        url: f.secret("url"),
        configuration_type: f.string("configurationType"),
        type_id: f.string("type"),
        auth_model: f.string("auth-model"),
        user: f.string("user"),
        password: f.secret("password"),
        properties: f.properties("properties"),
        provider_properties: f.properties("provider-properties"),
        bootstrap: f.object("bootstrap").map(bootstrap).unwrap_or_default(),
        handlers,
        invalid_handlers,
        extra: f.finish(),
    }
}

fn bootstrap(map: Map<String, Value>) -> BootstrapDef {
    let mut f = Fields::new(map);
    let mut queries = Vec::new();
    let mut invalid_queries = 0;
    match f.take("query") {
        Some(Value::Array(items)) => {
            for item in items {
                match item {
                    Value::String(s) => queries.push(s),
                    mut other => {
                        scrub(&mut other);
                        invalid_queries += 1;
                    }
                }
            }
        }
        // A single string is accepted as one query.
        Some(Value::String(s)) => queries.push(s),
        Some(other) => f.put_back("query", other),
        None => {}
    }
    BootstrapDef {
        default_schema: f.string("defaultSchema"),
        default_catalog: f.string("defaultCatalog"),
        autocommit: f.bool("autocommit"),
        queries,
        invalid_queries,
        extra: f.finish(),
    }
}

fn handler(id: String, map: Map<String, Value>) -> HandlerDef {
    let mut f = Fields::new(map);
    HandlerDef {
        id,
        kind: f.string("type"),
        enabled: f.bool("enabled"),
        save_password: f.bool("save-password"),
        user: f.string("user"),
        password: f.secret("password"),
        properties: f.properties("properties"),
        extra: f.finish(),
    }
}

/// Takes known keys out of a JSON object; whatever is left (or has the wrong type) becomes
/// [`Extra`].
struct Fields {
    map: Map<String, Value>,
}

impl Fields {
    fn new(map: Map<String, Value>) -> Self {
        Self { map }
    }

    fn take(&mut self, key: &str) -> Option<Value> {
        self.map.remove(key)
    }

    fn put_back(&mut self, key: &str, value: Value) {
        self.map.insert(key.to_owned(), value);
    }

    fn string(&mut self, key: &str) -> Option<String> {
        match self.take(key)? {
            Value::String(s) => Some(s),
            Value::Null => None,
            other => {
                self.put_back(key, other);
                None
            }
        }
    }

    fn bool(&mut self, key: &str) -> Option<bool> {
        let value = self.take(key)?;
        if value.is_null() {
            return None;
        }
        match Scalar::from_value(value) {
            Ok(s) => match s.as_bool() {
                Some(b) => Some(b),
                None => {
                    self.put_back(key, scalar_to_value(s));
                    None
                }
            },
            Err(other) => {
                self.put_back(key, other);
                None
            }
        }
    }

    fn scalar(&mut self, key: &str) -> Option<Scalar> {
        match self.take(key)? {
            Value::Null => None,
            value => match Scalar::from_value(value) {
                Ok(s) => Some(s),
                Err(other) => {
                    self.put_back(key, other);
                    None
                }
            },
        }
    }

    /// A string moved straight into a [`SecretString`]; any other type is scrubbed and kept as
    /// a `null` in `extra` so it is still reported.
    fn secret(&mut self, key: &str) -> Option<SecretString> {
        match self.take(key)? {
            Value::String(s) => Some(crate::credentials::into_secret(s)),
            Value::Null => None,
            mut other => {
                scrub(&mut other);
                self.put_back(key, Value::Null);
                None
            }
        }
    }

    fn object(&mut self, key: &str) -> Option<Map<String, Value>> {
        match self.take(key)? {
            Value::Object(m) => Some(m),
            Value::Null => None,
            other => {
                self.put_back(key, other);
                None
            }
        }
    }

    fn properties(&mut self, key: &str) -> Properties {
        self.object(key)
            .map(Properties::from_map)
            .unwrap_or_default()
    }

    fn finish(self) -> Extra {
        Extra::new(self.map)
    }
}

fn scalar_to_value(s: Scalar) -> Value {
    match s {
        Scalar::Text(t) => Value::String(t),
        Scalar::Bool(b) => Value::Bool(b),
        Scalar::Number(n) => Value::Number(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_ports() {
        let n = |v: Value| match Scalar::from_value(v) {
            Ok(s) => s.as_u16(),
            Err(_) => None,
        };
        assert_eq!(n(Value::from("22")), Some(22));
        assert_eq!(n(Value::from(22)), Some(22));
        assert_eq!(n(Value::from(22.0)), Some(22));
        assert_eq!(n(Value::from("22.0")), Some(22));
        assert_eq!(n(Value::from(22.5)), None);
        assert_eq!(n(Value::from(-1)), None);
        assert_eq!(n(Value::from(70000)), None);
        assert_eq!(n(Value::from("x")), None);
        assert_eq!(n(Value::from(true)), None);
        assert_eq!(n(Value::from(f64::MAX)), None);
    }

    #[test]
    fn wrong_types_go_to_extra() {
        let ds = parse_data_sources(
            br#"{"connections":{"c1":{"name":5,"read-only":"maybe","configuration":
               {"host":["x"],"port":{"a":1},"password":7}}},"folders":[],"x":1}"#,
        )
        .unwrap_or_default();
        assert_eq!(ds.connections.len(), 1);
        let c = &ds.connections[0];
        assert!(c.name.is_none());
        assert_eq!(c.extra.keys().collect::<Vec<_>>(), ["name", "read-only"]);
        let keys: Vec<_> = c.configuration.extra.keys().collect();
        assert_eq!(keys, ["host", "password", "port"]);
        assert_eq!(c.configuration.extra.get("password"), Some(&Value::Null));
        assert_eq!(ds.extra.keys().collect::<Vec<_>>(), ["folders", "x"]);
    }

    #[test]
    fn secret_like_extra_values_are_scrubbed() {
        let ds = parse_data_sources(
            br#"{"connections":{"c1":{"configuration":{"auth-token":"fixture-x","other":"v"}}}}"#,
        )
        .unwrap_or_default();
        let extra = &ds.connections[0].configuration.extra;
        assert_eq!(extra.get("auth-token"), Some(&Value::Null));
        assert_eq!(extra.get("other"), Some(&Value::from("v")));
        assert_eq!(format!("{extra:?}"), r#"{"auth-token", "other"}"#);
    }

    #[test]
    fn nested_secret_like_values_are_scrubbed() {
        let ds = parse_data_sources(
            br#"{"connections":{"c1":{"future":{"a":[{"apiToken":"fixture-n"}],"b":"kept"}}}}"#,
        )
        .unwrap_or_default();
        let future = ds.connections[0].extra.get("future").cloned();
        assert_eq!(
            future,
            Some(serde_json::json!({"a":[{"apiToken":null}],"b":"kept"}))
        );
    }

    #[test]
    fn errors_carry_no_content() {
        let err = parse_data_sources(b"{\"a\": fixture-secret}").err();
        let text = err.map(|e| e.to_string()).unwrap_or_default();
        assert!(text.starts_with("invalid JSON at line 1"));
        assert!(!text.contains("fixture"));
        assert_eq!(
            parse_data_sources(b"[1]").err(),
            Some(ParseError::NotAnObject)
        );
    }
}
