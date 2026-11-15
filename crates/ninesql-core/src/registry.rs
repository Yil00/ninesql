//! Driver registry: engine id → driver (ADR-0003, ADR-0004).
//!
//! The UI never depends on a driver crate (docs/02-architecture.md): the application registers
//! the drivers it ships here, and the core looks them up by the connection's engine. The mock
//! driver of `ninesql-driver-api` (feature `mock`) is registered by tests only.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use ninesql_driver_api::{AuthKind, Capabilities, Dialect, Driver};

use crate::connection::{Auth, Connection, EngineId, TlsMode};
use crate::error::{ConnectionError, CoreError};

/// Engine id of the driver API for an engine id of the model (both are plain strings; the model
/// one is validated).
#[must_use]
pub fn api_engine(engine: &EngineId) -> ninesql_driver_api::EngineId {
    ninesql_driver_api::EngineId::new(engine.as_str())
}

/// Registered drivers, by engine id.
#[derive(Clone, Default)]
pub struct DriverRegistry {
    drivers: BTreeMap<String, Arc<dyn Driver>>,
}

impl fmt::Debug for DriverRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DriverRegistry")
            .field("engines", &self.drivers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl DriverRegistry {
    /// No driver.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a driver for every engine it serves, replacing any driver previously
    /// registered for one of them.
    pub fn register(&mut self, driver: Arc<dyn Driver>) {
        for engine in driver.engines() {
            if self
                .drivers
                .insert(engine.as_str().to_owned(), Arc::clone(&driver))
                .is_some()
            {
                tracing::warn!(engine = %engine, family = driver.family(), "driver replaced");
            }
        }
    }

    /// Engines with a driver, sorted.
    pub fn engines(&self) -> impl Iterator<Item = &str> {
        self.drivers.keys().map(String::as_str)
    }

    /// The driver of an engine.
    ///
    /// # Errors
    /// [`CoreError::NoDriver`].
    pub fn driver(&self, engine: &EngineId) -> Result<Arc<dyn Driver>, CoreError> {
        self.drivers
            .get(engine.as_str())
            .cloned()
            .ok_or_else(|| CoreError::NoDriver(engine.to_string()))
    }

    /// Capabilities of an engine before connect (no server version): what the connection
    /// dialog offers (I8).
    ///
    /// # Errors
    /// [`CoreError::NoDriver`], or the driver's error.
    pub fn capabilities(&self, engine: &EngineId) -> Result<Capabilities, CoreError> {
        Ok(self
            .driver(engine)?
            .capabilities(&api_engine(engine), None)?)
    }

    /// Dialect of an engine, usable without a session (highlighting, script splitting,
    /// statement classification).
    ///
    /// # Errors
    /// [`CoreError::NoDriver`], or the driver's error.
    pub fn dialect(&self, engine: &EngineId) -> Result<Arc<dyn Dialect>, CoreError> {
        Ok(self.driver(engine)?.dialect(&api_engine(engine))?)
    }

    /// Validates a connection against its driver: [`Connection::validate`], then every driver
    /// property against [`Capabilities::is_reserved_property`], so the keys the driver reserves
    /// on top of the shared rule (`extra_reserved_properties`) are refused **before** the
    /// connection is saved or imported, not only at connect time.
    ///
    /// Call it before [`crate::Workspace::create_connection`] /
    /// [`crate::Workspace::save_connections`] (connection dialog) and before persisting an
    /// imported connection (P2-C). The connect flow calls it again.
    ///
    /// # Errors
    /// [`CoreError::NoDriver`], [`CoreError::InvalidConnection`].
    pub fn validate_connection(&self, connection: &Connection) -> Result<(), CoreError> {
        let caps = self.capabilities(&connection.engine)?;
        connection.validate_for_driver(&caps)?;
        Ok(())
    }
}

impl Connection {
    /// [`Connection::validate`] plus the driver-specific rules, so a connection the driver
    /// would refuse at connect is refused when it is saved or imported (M2):
    /// - a property whose key `capabilities` reserves ([`Capabilities::is_reserved_property`],
    ///   i.e. the shared rule or one of the driver's `extra_reserved_properties`) is refused
    ///   with [`ConnectionError::ReservedProperty`];
    /// - a property off the driver's allowlist
    ///   ([`ConnectCapabilities::accepted_properties`](ninesql_driver_api::ConnectCapabilities::accepted_properties)),
    ///   an authentication method the driver does not list, and on network endpoints
    ///   `verify-ca` without a CA file (ADR-0019), a client certificate or an encrypted client
    ///   key the driver does not support, are refused with [`ConnectionError::InvalidField`].
    ///
    /// Values are never echoed.
    ///
    /// # Errors
    /// The first problem found.
    pub fn validate_for_driver(&self, capabilities: &Capabilities) -> Result<(), ConnectionError> {
        self.validate()?;
        let accepts = &capabilities.connect;
        for key in self.properties.keys() {
            if capabilities.is_reserved_property(key) {
                return Err(ConnectionError::ReservedProperty {
                    key: key.clone(),
                    use_instead: "the driver of this engine reserves it; use the typed connection settings",
                });
            }
            if !accepts.accepts_property(key) {
                return Err(ConnectionError::InvalidField {
                    field: format!("properties.{key}"),
                    reason: "is not a driver property the driver of this engine accepts",
                });
            }
        }
        let auth = match &self.auth {
            Auth::None => AuthKind::None,
            // A user name without a saved password connects with an empty password.
            Auth::Username { .. } | Auth::Password { .. } => AuthKind::Password,
            Auth::Token { .. } => AuthKind::Token,
        };
        if !accepts.auth.contains(&auth) {
            return Err(ConnectionError::InvalidField {
                field: "auth".to_owned(),
                reason: "uses an authentication method the driver of this engine does not support",
            });
        }
        if self.endpoint.is_network() && self.tls.mode != TlsMode::Disable {
            if self.tls.mode == TlsMode::VerifyCa
                && self.tls.ca_file.is_none()
                && accepts.verify_ca_requires_ca_file
            {
                return Err(ConnectionError::InvalidField {
                    field: "tls.ca_file".to_owned(),
                    reason: "is required by verify-ca (it trusts only that file); use verify-full to rely on the system trust store",
                });
            }
            if let Some(cert) = &self.tls.client_certificate {
                if !accepts.client_certificates {
                    return Err(ConnectionError::InvalidField {
                        field: "tls.client_certificate".to_owned(),
                        reason: "is not supported by the driver of this engine",
                    });
                }
                if cert.passphrase.is_some() && !accepts.encrypted_client_keys {
                    return Err(ConnectionError::InvalidField {
                        field: "tls.client_certificate.passphrase".to_owned(),
                        reason: "is set, but the driver of this engine does not support encrypted client keys; use an unencrypted key file readable only by you",
                    });
                }
            }
        }
        Ok(())
    }
}
