//! Driver registry: engine id → driver (ADR-0003, ADR-0004).
//!
//! The UI never depends on a driver crate (docs/02-architecture.md): the application registers
//! the drivers it ships here, and the core looks them up by the connection's engine. The mock
//! driver of `ninesql-driver-api` (feature `mock`) is registered by tests only.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use ninesql_driver_api::{Capabilities, Dialect, Driver};

use crate::connection::{Connection, EngineId};
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
    /// [`Connection::validate`] plus the driver-specific property rule: a property whose key
    /// `capabilities` reserves ([`Capabilities::is_reserved_property`], i.e. the shared rule or
    /// one of the driver's `extra_reserved_properties`) is refused with
    /// [`ConnectionError::ReservedProperty`]. The value is never echoed.
    ///
    /// # Errors
    /// The first problem found.
    pub fn validate_for_driver(&self, capabilities: &Capabilities) -> Result<(), ConnectionError> {
        self.validate()?;
        for key in self.properties.keys() {
            if capabilities.is_reserved_property(key) {
                return Err(ConnectionError::ReservedProperty {
                    key: key.clone(),
                    use_instead: "the driver of this engine reserves it; use the typed connection settings",
                });
            }
        }
        Ok(())
    }
}
