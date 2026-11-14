//! Connection types: Development, Test, Production and user-defined (ADR-0007, I3).
//!
//! The built-in Production type, and any type flagged `production`, always confirms destructive
//! statements and never logs query text, whatever the file says ([`ConnectionType::policy`]).
//! A connection naming an unknown type is treated as Production (fail safe).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::ConnectionError;

/// Identifier of a connection type. DBeaver-compatible built-ins: `dev`, `test`, `prod`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ConnectionTypeId(String);

impl ConnectionTypeId {
    /// Built-in Development type id.
    pub const DEV: &'static str = "dev";
    /// Built-in Test type id.
    pub const TEST: &'static str = "test";
    /// Built-in Production type id.
    pub const PROD: &'static str = "prod";

    /// Validates an id: 1 to 64 bytes of `[a-z0-9_-]`, starting with a letter or digit.
    ///
    /// # Errors
    /// [`ConnectionError::InvalidConnectionTypeId`].
    pub fn new(id: impl Into<String>) -> Result<Self, ConnectionError> {
        let id = id.into();
        let mut bytes = id.bytes();
        let valid = id.len() <= 64
            && bytes
                .next()
                .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            && bytes
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
        if valid {
            Ok(Self(id))
        } else {
            Err(ConnectionError::InvalidConnectionTypeId)
        }
    }

    /// `dev`.
    #[must_use]
    pub fn dev() -> Self {
        Self(Self::DEV.to_owned())
    }

    /// `test`.
    #[must_use]
    pub fn test() -> Self {
        Self(Self::TEST.to_owned())
    }

    /// `prod`.
    #[must_use]
    pub fn prod() -> Self {
        Self(Self::PROD.to_owned())
    }

    /// The id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ConnectionTypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ConnectionTypeId {
    type Error = ConnectionError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ConnectionTypeId> for String {
    fn from(value: ConnectionTypeId) -> Self {
        value.0
    }
}

/// An sRGB color, serialized as `#RRGGBB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Color {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
}

impl Color {
    /// A color from its components.
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parses `#RRGGBB`.
    ///
    /// # Errors
    /// [`ConnectionError::InvalidColor`].
    pub fn parse(text: &str) -> Result<Self, ConnectionError> {
        let err = || ConnectionError::InvalidColor;
        let hex = text.strip_prefix('#').ok_or_else(err)?;
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(err());
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| err());
        Ok(Self::rgb(byte(0)?, byte(2)?, byte(4)?))
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }
}

impl TryFrom<String> for Color {
    type Error = ConnectionError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<Color> for String {
    fn from(value: Color) -> Self {
        value.to_string()
    }
}

/// A connection type and its safety rules (ADR-0007 §1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionType {
    /// Identifier referenced by connections.
    pub id: ConnectionTypeId,
    /// Display name.
    pub name: String,
    /// Color framing editor tabs and results.
    pub color: Color,
    /// Auto-commit default of new sessions.
    pub auto_commit: bool,
    /// Ask before destructive statements (`DROP`, `TRUNCATE`, `DELETE` without `WHERE`…).
    pub confirm_destructive: bool,
    /// Ask before every write.
    pub confirm_every_write: bool,
    /// Read-only default of connections of this type (enforced engine-side, I3).
    pub read_only: bool,
    /// Whether query text may be written to logs (at `trace` level).
    pub log_query_text: bool,
    /// Treat as production: forces the production rules of [`ConnectionType::policy`].
    #[serde(default)]
    pub production: bool,
}

/// Effective safety rules of a connection type, after enforcement of the production rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafetyPolicy {
    /// Auto-commit default.
    pub auto_commit: bool,
    /// Confirm destructive statements.
    pub confirm_destructive: bool,
    /// Confirm every write.
    pub confirm_every_write: bool,
    /// Read-only default.
    pub read_only: bool,
    /// Query text may be logged.
    pub log_query_text: bool,
    /// Production rules apply.
    pub production: bool,
}

impl ConnectionType {
    /// Built-in Development type (DBeaver color 255,255,255).
    #[must_use]
    pub fn development() -> Self {
        Self {
            id: ConnectionTypeId::dev(),
            name: "Development".to_owned(),
            color: Color::rgb(0xFF, 0xFF, 0xFF),
            auto_commit: true,
            confirm_destructive: false,
            confirm_every_write: false,
            read_only: false,
            log_query_text: true,
            production: false,
        }
    }

    /// Built-in Test type (DBeaver color 196,255,181).
    #[must_use]
    pub fn test() -> Self {
        Self {
            id: ConnectionTypeId::test(),
            name: "Test".to_owned(),
            color: Color::rgb(0xC4, 0xFF, 0xB5),
            auto_commit: true,
            confirm_destructive: false,
            confirm_every_write: false,
            read_only: false,
            log_query_text: true,
            production: false,
        }
    }

    /// Built-in Production type (DBeaver color 247,159,129): manual commit, confirmation of
    /// destructive statements, no query text in logs (ADR-0007 §3, §5, §6).
    #[must_use]
    pub fn production() -> Self {
        Self {
            id: ConnectionTypeId::prod(),
            name: "Production".to_owned(),
            color: Color::rgb(0xF7, 0x9F, 0x81),
            auto_commit: false,
            confirm_destructive: true,
            confirm_every_write: false,
            read_only: false,
            log_query_text: false,
            production: true,
        }
    }

    /// The three built-in types.
    #[must_use]
    pub fn builtins() -> [Self; 3] {
        [Self::development(), Self::test(), Self::production()]
    }

    /// Whether production rules apply (the built-in `prod` type always counts).
    #[must_use]
    pub fn is_production(&self) -> bool {
        self.production || self.id.as_str() == ConnectionTypeId::PROD
    }

    /// Effective rules. Production types always confirm destructive statements and never log
    /// query text (ADR-0007 §6); the user can only make them stricter.
    #[must_use]
    pub fn policy(&self) -> SafetyPolicy {
        let production = self.is_production();
        SafetyPolicy {
            auto_commit: self.auto_commit,
            confirm_destructive: self.confirm_destructive || production,
            confirm_every_write: self.confirm_every_write,
            read_only: self.read_only,
            log_query_text: self.log_query_text && !production,
            production,
        }
    }
}

/// The connection types of a project: built-ins (possibly customized) plus user-defined ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionTypes {
    types: Vec<ConnectionType>,
}

impl Default for ConnectionTypes {
    fn default() -> Self {
        Self {
            types: ConnectionType::builtins().into(),
        }
    }
}

impl ConnectionTypes {
    /// Builds the set from saved types; missing built-ins are added, duplicates keep the first.
    #[must_use]
    pub fn from_saved(saved: Vec<ConnectionType>) -> Self {
        let mut types: Vec<ConnectionType> = Vec::with_capacity(saved.len() + 3);
        for t in saved {
            if types.iter().any(|x| x.id == t.id) {
                tracing::warn!(id = %t.id, "duplicate connection type ignored");
                continue;
            }
            types.push(t);
        }
        for builtin in ConnectionType::builtins() {
            if !types.iter().any(|t| t.id == builtin.id) {
                types.push(builtin);
            }
        }
        Self { types }
    }

    /// All types.
    #[must_use]
    pub fn all(&self) -> &[ConnectionType] {
        &self.types
    }

    /// The type with that id, if defined.
    #[must_use]
    pub fn get(&self, id: &ConnectionTypeId) -> Option<&ConnectionType> {
        self.types.iter().find(|t| &t.id == id)
    }

    /// The type to apply for `id`: the defined one, or Production when `id` is unknown (fail
    /// safe, I3).
    #[must_use]
    pub fn resolve(&self, id: &ConnectionTypeId) -> ConnectionType {
        self.get(id).cloned().unwrap_or_else(|| {
            tracing::warn!(id = %id, "unknown connection type, applying production rules");
            let mut prod = self
                .get(&ConnectionTypeId::prod())
                .cloned()
                .unwrap_or_else(ConnectionType::production);
            prod.production = true;
            prod
        })
    }

    /// Adds or replaces a type.
    pub fn upsert(&mut self, connection_type: ConnectionType) {
        match self.types.iter_mut().find(|t| t.id == connection_type.id) {
            Some(slot) => *slot = connection_type,
            None => self.types.push(connection_type),
        }
    }

    /// Removes a user-defined type. Built-ins cannot be removed; returns whether it was removed.
    pub fn remove(&mut self, id: &ConnectionTypeId) -> bool {
        if ConnectionType::builtins().iter().any(|b| &b.id == id) {
            return false;
        }
        let before = self.types.len();
        self.types.retain(|t| &t.id != id);
        before != self.types.len()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;

    #[test]
    fn color_parse_and_display() {
        assert_eq!(
            Color::parse("#f79f81").unwrap(),
            Color::rgb(0xF7, 0x9F, 0x81)
        );
        assert_eq!(Color::rgb(1, 2, 255).to_string(), "#0102FF");
        for bad in ["f79f81", "#f79f8", "#f79f8g", "#ffffffff", "", "#ÿÿÿ"] {
            assert!(Color::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn type_id_rules() {
        assert!(ConnectionTypeId::new("prod").is_ok());
        assert!(ConnectionTypeId::new("staging-eu_1").is_ok());
        for bad in ["", "Prod", "-x", "a b", "é"] {
            assert!(ConnectionTypeId::new(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn production_rules_cannot_be_relaxed() {
        let mut prod = ConnectionType::production();
        prod.confirm_destructive = false;
        prod.log_query_text = true;
        prod.production = false;
        let policy = prod.policy();
        assert!(policy.production);
        assert!(policy.confirm_destructive);
        assert!(!policy.log_query_text);

        let mut custom = ConnectionType::development();
        custom.id = ConnectionTypeId::new("live").unwrap();
        custom.production = true;
        custom.log_query_text = true;
        assert!(!custom.policy().log_query_text);
        assert!(custom.policy().confirm_destructive);
    }

    #[test]
    fn unknown_type_resolves_to_production() {
        let types = ConnectionTypes::default();
        let t = types.resolve(&ConnectionTypeId::new("gone").unwrap());
        assert!(t.policy().production);
        assert!(!t.policy().log_query_text);
    }

    #[test]
    fn builtins_are_always_present_and_not_removable() {
        let mut custom = ConnectionType::test();
        custom.id = ConnectionTypeId::new("staging").unwrap();
        custom.name = "Staging".into();
        let mut types = ConnectionTypes::from_saved(vec![custom.clone()]);
        assert_eq!(types.all().len(), 4);
        assert!(!types.remove(&ConnectionTypeId::prod()));
        assert!(types.remove(&custom.id));
        assert_eq!(types.all().len(), 3);
    }
}
