//! Driver registry: engine id → driver (ADR-0003, ADR-0004).
//!
//! The UI never depends on a driver crate (docs/02-architecture.md): the application registers
//! the drivers it ships here, and the core looks them up by the connection's engine. The mock
//! driver of `ninesql-driver-api` (feature `mock`) is registered by tests only.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use ninesql_driver_api::{self as api, AuthKind, Capabilities, Dialect, Driver};

use crate::connection::{Auth, Connection, EngineId, TlsMode};
use crate::error::{ConnectionError, CoreError};

/// Engine id of the driver API for an engine id of the model (both are plain strings; the model
/// one is validated).
#[must_use]
pub fn api_engine(engine: &EngineId) -> ninesql_driver_api::EngineId {
    ninesql_driver_api::EngineId::new(engine.as_str())
}

/// The driver API TLS mode of a model TLS mode.
pub(crate) const fn api_tls_mode(mode: TlsMode) -> api::TlsMode {
    match mode {
        TlsMode::Disable => api::TlsMode::Disable,
        TlsMode::Require => api::TlsMode::Require,
        TlsMode::VerifyCa => api::TlsMode::VerifyCa,
        TlsMode::VerifyFull => api::TlsMode::VerifyFull,
    }
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

    /// **The** validation entry point for saving and importing a connection: the model rules
    /// ([`Connection::validate`]) and every driver rule ([`Connection::validate_for_driver`]:
    /// reserved and unknown properties, authentication kind, password-exchange policy, TLS mode
    /// against [`Capabilities::tls`], `verify-ca` CA file, client certificates), so what the
    /// driver would refuse at connect is refused **before** the connection is saved or
    /// imported.
    ///
    /// - P1-C (connection dialog): call it on every change to show the error next to the
    ///   field, and save through [`crate::Workspace::create_connection_checked`] /
    ///   [`crate::Workspace::save_connections_checked`], which call it for every connection.
    /// - P2-C (DBeaver import): call it on each converted connection before persisting it; a
    ///   connection it refuses is reported (and may be saved disabled), never saved as is.
    /// - The connect flow calls it again ([`crate::SessionManager::open`]).
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
    ///   an authentication method the driver does not list, a password-exchange policy the
    ///   driver does not declare
    ///   ([`ConnectCapabilities::password_exchanges`](ninesql_driver_api::ConnectCapabilities::password_exchanges)),
    ///   and on network endpoints a TLS mode outside [`Capabilities::tls`] (with an empty set,
    ///   only `disable` is accepted: the driver has no TLS, and a connection saved as
    ///   `verify-full` must not look encrypted when it is not, I5 / I8), `verify-ca` without a
    ///   CA file (ADR-0019), a client certificate or an encrypted client key the driver does
    ///   not support, are refused with [`ConnectionError::InvalidField`].
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
        if !accepts.accepts_password_exchange(self.password_exchange.to_api()) {
            return Err(ConnectionError::InvalidField {
                field: "password_exchange".to_owned(),
                reason: "is a policy the driver of this engine cannot enforce; use `any`",
            });
        }
        if self.endpoint.is_network()
            && self.tls.mode != TlsMode::Disable
            && !capabilities.tls.contains(&api_tls_mode(self.tls.mode))
        {
            return Err(ConnectionError::InvalidField {
                field: "tls.mode".to_owned(),
                reason: if capabilities.tls.is_empty() {
                    "must be `disable`: the driver of this engine does not support TLS"
                } else {
                    "is a TLS mode the driver of this engine does not support"
                },
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
