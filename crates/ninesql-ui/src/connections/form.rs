//! View model of the connection dialog (F-CON-12, F-CON-06, F-CON-05, F-CON-08, F-CON-03):
//! the fields as the user edits them, the conversion to and from the core's [`Connection`],
//! and the notices shown next to them (I5, ADR-0016, ADR-0019).
//!
//! No field holds a secret. Credential fields are a [`SecretField`]: "save" or "ask every
//! time", plus the reference of a saved secret, shown as "Saved in keyring" / "Saved in vault"
//! / "Not saved", never the value. A newly typed secret is read from its masked input only when
//! the dialog saves or tests, handed over as a [`SecretString`], and the input is cleared.

use std::collections::BTreeSet;
use std::path::PathBuf;

use ninesql_core::driver_api::{AuthKind, ReadOnlyMechanism};
use ninesql_core::secrets::{SecretRef, SecretString, StoreKind};
use ninesql_core::{
    Auth, ClientCertificate, Connection, ConnectionTypeId, ConnectionTypes, Endpoint, EngineId,
    HostKeyPolicy, PasswordExchange, SecretSlot, SecretSource, SecurityWarning, SshAuth, SshHop,
    SshTunnel, TlsConfig, TlsMode,
};

use super::engines::EngineOffer;
use super::url::ParsedUrl;

/// Database authentication choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthChoice {
    /// No credentials.
    None,
    /// A user name, no password (`trust`, `peer`).
    UserOnly,
    /// User name and password.
    Password,
    /// A token, with an optional user name.
    Token,
}

impl AuthChoice {
    /// Label in the dialog.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "No authentication",
            Self::UserOnly => "User name only",
            Self::Password => "User and password",
            Self::Token => "Token",
        }
    }

    /// The driver API kind this choice needs (`UserOnly` connects as a password user with an
    /// empty password).
    #[must_use]
    pub fn kind(self) -> AuthKind {
        match self {
            Self::None => AuthKind::None,
            Self::UserOnly | Self::Password => AuthKind::Password,
            Self::Token => AuthKind::Token,
        }
    }

    /// The choices a driver accepts, in dialog order (I8).
    #[must_use]
    pub fn offered(auth: &BTreeSet<AuthKind>) -> Vec<Self> {
        [Self::Password, Self::UserOnly, Self::Token, Self::None]
            .into_iter()
            .filter(|c| auth.contains(&c.kind()))
            .collect()
    }
}

/// Where a credential comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretMode {
    /// Saved in the keyring or the vault.
    Save,
    /// Asked at every connect, kept in memory for the session only.
    Ask,
}

/// One credential field. Holds a reference, never a value (I1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretField {
    /// Save or ask.
    pub mode: SecretMode,
    /// The saved secret, when there is one.
    pub stored: Option<SecretRef>,
}

impl Default for SecretField {
    fn default() -> Self {
        Self {
            mode: SecretMode::Ask,
            stored: None,
        }
    }
}

impl SecretField {
    fn from_source(source: &SecretSource) -> Self {
        match source {
            SecretSource::Stored(r) => Self {
                mode: SecretMode::Save,
                stored: Some(r.clone()),
            },
            SecretSource::Prompt => Self::default(),
        }
    }

    /// What the dialog shows instead of the value: never the secret, not even masked.
    #[must_use]
    pub fn status(&self) -> &'static str {
        match (self.mode, &self.stored) {
            (SecretMode::Ask, _) => "Asked at every connect",
            (SecretMode::Save, Some(r)) if r.store == StoreKind::Keyring => "Saved in keyring",
            (SecretMode::Save, Some(_)) => "Saved in vault",
            (SecretMode::Save, None) => "Not saved",
        }
    }
}

/// SSH authentication choice of one hop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshAuthChoice {
    /// `ssh-agent`.
    Agent,
    /// Password.
    Password,
    /// Private key file (not RSA, ADR-0016).
    KeyFile,
}

impl SshAuthChoice {
    /// Label in the dialog.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Agent => "SSH agent",
            Self::Password => "Password",
            Self::KeyFile => "Key file",
        }
    }
}

/// Label of a host key policy.
#[must_use]
pub fn policy_label(policy: HostKeyPolicy) -> &'static str {
    match policy {
        HostKeyPolicy::Strict => "Strict (confirm unknown keys)",
        HostKeyPolicy::AcceptNew => "Accept new keys",
        HostKeyPolicy::Disabled => "Do not verify",
    }
}

/// One SSH hop as edited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HopForm {
    /// Host.
    pub host: String,
    /// Port, empty for 22.
    pub port: String,
    /// User name.
    pub username: String,
    /// Authentication.
    pub auth: SshAuthChoice,
    /// Private key path, for [`SshAuthChoice::KeyFile`].
    pub key_path: String,
    /// Password or key passphrase.
    pub secret: SecretField,
    /// Whether a key file has a passphrase.
    pub key_has_passphrase: bool,
    /// Host key policy (ADR-0016), strict by default.
    pub policy: HostKeyPolicy,
}

impl Default for HopForm {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: String::new(),
            username: String::new(),
            auth: SshAuthChoice::Agent,
            key_path: String::new(),
            secret: SecretField::default(),
            key_has_passphrase: false,
            policy: HostKeyPolicy::Strict,
        }
    }
}

impl HopForm {
    fn from_hop(hop: &SshHop) -> Self {
        let mut form = Self {
            host: hop.host.clone(),
            port: if hop.port == 22 {
                String::new()
            } else {
                hop.port.to_string()
            },
            username: hop.username.clone(),
            policy: hop.host_key_policy,
            ..Self::default()
        };
        match &hop.auth {
            SshAuth::Agent => form.auth = SshAuthChoice::Agent,
            SshAuth::Password { secret } => {
                form.auth = SshAuthChoice::Password;
                form.secret = SecretField::from_source(secret);
            }
            SshAuth::PublicKey {
                key_path,
                passphrase,
            } => {
                form.auth = SshAuthChoice::KeyFile;
                form.key_path = key_path.display().to_string();
                if let Some(p) = passphrase {
                    form.key_has_passphrase = true;
                    form.secret = SecretField::from_source(p);
                }
            }
        }
        form
    }

    /// Changes the authentication. The credential field is reset, so a saved SSH password is
    /// never reused as a key passphrase (or the other way round); the old secret is deleted
    /// when the connection is saved.
    pub fn set_auth(&mut self, choice: SshAuthChoice) {
        if self.auth != choice {
            self.auth = choice;
            self.secret = SecretField::default();
            self.key_has_passphrase = false;
        }
    }

    /// Ticks or unticks "key has a passphrase"; unticking forgets the saved passphrase.
    pub fn set_key_has_passphrase(&mut self, on: bool) {
        if self.key_has_passphrase != on {
            self.key_has_passphrase = on;
            self.secret = SecretField::default();
        }
    }

    /// Whether this hop has a credential field.
    #[must_use]
    pub fn has_secret(&self) -> bool {
        match self.auth {
            SshAuthChoice::Agent => false,
            SshAuthChoice::Password => true,
            SshAuthChoice::KeyFile => self.key_has_passphrase,
        }
    }
}

/// How the endpoint is entered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointInput {
    /// Host, port and database fields.
    Fields,
    /// A URL, parsed into the fields.
    Url,
}

/// The dialog's fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionForm {
    /// The connection being edited (keeps its id, origin, description, init SQL, enabled flag);
    /// `None` for a new one.
    pub existing: Option<Connection>,
    /// Display name.
    pub name: String,
    /// Engine id.
    pub engine: String,
    /// Fields or URL.
    pub endpoint_input: EndpointInput,
    /// Host.
    pub host: String,
    /// Port, empty for the engine's default.
    pub port: String,
    /// Database.
    pub database: String,
    /// Database file, for file engines.
    pub file_path: String,
    /// Authentication.
    pub auth: AuthChoice,
    /// User name.
    pub username: String,
    /// Password or token.
    pub auth_secret: SecretField,
    /// TLS mode.
    pub tls_mode: TlsMode,
    /// CA file (ADR-0019: the only trust anchor when set).
    pub ca_file: String,
    /// Client certificate file (mTLS).
    pub client_cert: String,
    /// Client key file (mTLS).
    pub client_key: String,
    /// Whether the client key has a passphrase.
    pub client_key_has_passphrase: bool,
    /// Client key passphrase.
    pub client_key_secret: SecretField,
    /// SSH tunnel on.
    pub ssh_enabled: bool,
    /// SSH hops, jump hosts first, the tunnel server last (the hop indices of `ninesql-ssh`).
    pub hops: Vec<HopForm>,
    /// Driver properties.
    pub properties: Vec<(String, String)>,
    /// Connection type (ADR-0007): asked explicitly, no silent default for a new connection.
    pub connection_type: Option<ConnectionTypeId>,
    /// Read-only override of the type's default.
    pub read_only: Option<bool>,
    /// Folder path in the navigator, `/`-separated.
    pub folder: String,
    /// SCRAM-only password exchange (`Connection::password_exchange` =
    /// `PasswordExchange::ScramOnly`). Offered only when the driver declares it in
    /// `ConnectCapabilities::password_exchanges` ([`EngineOffer::scram_only`]) and for password
    /// authentication.
    pub scram_only: bool,
}

impl ConnectionForm {
    /// An empty form for a new connection of `engine`.
    #[must_use]
    pub fn new(engine: &str) -> Self {
        Self {
            existing: None,
            name: String::new(),
            engine: engine.to_owned(),
            endpoint_input: EndpointInput::Fields,
            host: String::new(),
            port: String::new(),
            database: String::new(),
            file_path: String::new(),
            auth: AuthChoice::Password,
            username: String::new(),
            auth_secret: SecretField::default(),
            tls_mode: TlsMode::VerifyFull,
            ca_file: String::new(),
            client_cert: String::new(),
            client_key: String::new(),
            client_key_has_passphrase: false,
            client_key_secret: SecretField::default(),
            ssh_enabled: false,
            hops: Vec::new(),
            properties: Vec::new(),
            connection_type: None,
            read_only: None,
            folder: String::new(),
            scram_only: false,
        }
    }

    /// The form of an existing connection.
    #[must_use]
    pub fn from_connection(c: &Connection) -> Self {
        let mut form = Self::new(c.engine.as_str());
        form.existing = Some(c.clone());
        form.name = c.name.clone();
        match &c.endpoint {
            Endpoint::Tcp {
                host,
                port,
                database,
            } => {
                form.host = host.clone();
                form.port = port.map(|p| p.to_string()).unwrap_or_default();
                form.database = database.clone().unwrap_or_default();
            }
            Endpoint::UnixSocket { path, database } => {
                // No driver API endpoint for sockets yet: shown as the host field, the connect
                // flow reports it.
                form.host = path.display().to_string();
                form.database = database.clone().unwrap_or_default();
            }
            Endpoint::File { path } => form.file_path = path.display().to_string(),
        }
        match &c.auth {
            Auth::None => form.auth = AuthChoice::None,
            Auth::Username { username } => {
                form.auth = AuthChoice::UserOnly;
                form.username = username.clone();
            }
            Auth::Password { username, secret } => {
                form.auth = AuthChoice::Password;
                form.username = username.clone();
                form.auth_secret = SecretField::from_source(secret);
            }
            Auth::Token { username, secret } => {
                form.auth = AuthChoice::Token;
                form.username = username.clone().unwrap_or_default();
                form.auth_secret = SecretField::from_source(secret);
            }
        }
        form.tls_mode = c.tls.mode;
        form.ca_file = path_text(c.tls.ca_file.as_ref());
        if let Some(cc) = &c.tls.client_certificate {
            form.client_cert = cc.cert_file.display().to_string();
            form.client_key = cc.key_file.display().to_string();
            if let Some(p) = &cc.passphrase {
                form.client_key_has_passphrase = true;
                form.client_key_secret = SecretField::from_source(p);
            }
        }
        if let Some(ssh) = &c.ssh {
            form.ssh_enabled = true;
            form.hops = ssh.hops().map(HopForm::from_hop).collect();
        }
        form.properties = c
            .properties
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        form.scram_only = c.password_exchange == PasswordExchange::ScramOnly;
        form.connection_type = Some(c.connection_type.clone());
        form.read_only = c.read_only;
        form.folder = c.folder.join("/");
        form
    }

    /// A copy for "Duplicate": new id, name suffixed, saved credentials turned into "ask every
    /// time" (the secrets themselves are not copied: the dialog never reads them back).
    #[must_use]
    pub fn duplicate_of(c: &Connection) -> Self {
        let mut form = Self::from_connection(c);
        form.existing = None;
        form.name = format!("{} (copy)", c.name);
        for field in std::iter::once(&mut form.auth_secret)
            .chain(std::iter::once(&mut form.client_key_secret))
            .chain(form.hops.iter_mut().map(|h| &mut h.secret))
        {
            *field = SecretField::default();
        }
        form
    }

    /// Fills the endpoint fields from a parsed URL. The engine must have a registered driver.
    ///
    /// # Errors
    /// A message for the user when no driver serves the URL's engine.
    pub fn apply_url(&mut self, url: &ParsedUrl, offers: &[EngineOffer]) -> Result<(), String> {
        let Some(offer) = offers.iter().find(|o| o.engine == url.engine) else {
            return Err(format!(
                "no driver for {} is available in this build",
                super::engines::display_name(&url.engine)
            ));
        };
        self.engine = offer.engine.clone();
        if let Some(file) = &url.file {
            self.file_path = file.display().to_string();
        }
        if let Some(h) = &url.host {
            self.host = h.clone();
        }
        self.port = url.port.map(|p| p.to_string()).unwrap_or_default();
        self.database = url.database.clone().unwrap_or_default();
        if let Some(u) = &url.username {
            self.username = u.clone();
            if self.auth == AuthChoice::None {
                self.auth = AuthChoice::Password;
            }
        }
        if let Some(mode) = url.tls_mode {
            self.tls_mode = mode;
        }
        if let Some(ca) = &url.ca_file {
            self.ca_file = ca.display().to_string();
        }
        for (k, v) in &url.properties {
            match self.properties.iter_mut().find(|(key, _)| key == k) {
                Some(entry) => entry.1 = v.clone(),
                None => self.properties.push((k.clone(), v.clone())),
            }
        }
        Ok(())
    }

    /// Changes the database authentication. The credential field is reset, so a saved password
    /// is never reused as a token (or the other way round); the old secret is deleted when the
    /// connection is saved.
    pub fn set_auth(&mut self, choice: AuthChoice) {
        if self.auth != choice {
            self.auth = choice;
            self.auth_secret = SecretField::default();
        }
    }

    /// Ticks or unticks "client key has a passphrase"; unticking forgets the saved passphrase.
    pub fn set_client_key_has_passphrase(&mut self, on: bool) {
        if self.client_key_has_passphrase != on {
            self.client_key_has_passphrase = on;
            self.client_key_secret = SecretField::default();
        }
    }

    /// Whether "Read-write" can be chosen: production types ignore a read-write override (the
    /// core keeps their read-only default), so the dialog does not offer it.
    #[must_use]
    pub fn read_write_allowed(&self, types: &ConnectionTypes) -> bool {
        self.connection_type
            .as_ref()
            .is_none_or(|t| !types.resolve(t).is_production())
    }

    /// Sets the connection type; a read-write override is dropped for a production type.
    pub fn set_connection_type(&mut self, id: ConnectionTypeId, types: &ConnectionTypes) {
        self.connection_type = Some(id);
        if self.read_only == Some(false) && !self.read_write_allowed(types) {
            self.read_only = None;
        }
    }

    /// Makes the form consistent with an engine's offer after an engine change: unsupported
    /// choices fall back to supported ones (I8). A downgraded TLS mode is never chosen
    /// silently: an unsupported mode moves to the strongest offered one.
    pub fn adapt_to(&mut self, offer: &EngineOffer) {
        self.engine = offer.engine.clone();
        let offered = AuthChoice::offered(&offer.auth);
        if !offered.contains(&self.auth)
            && let Some(first) = offered.first()
        {
            self.set_auth(*first);
        }
        if offer.offers_tls()
            && !offer.tls_modes.contains(&self.tls_mode)
            && let Some(strongest) = offer.tls_modes.last()
        {
            self.tls_mode = *strongest;
        }
        if offer.file_based {
            self.ssh_enabled = false;
        }
        if !offer.scram_only {
            self.scram_only = false;
        }
    }

    /// Every credential slot that is set to "save" without a saved secret yet: a value must be
    /// typed for it.
    #[must_use]
    pub fn slots_needing_value(&self, offer: &EngineOffer) -> Vec<SecretSlot> {
        self.secret_slots(offer)
            .into_iter()
            .filter(|(_, f)| f.mode == SecretMode::Save && f.stored.is_none())
            .map(|(s, _)| s)
            .collect()
    }

    /// Every credential field the form currently has, with its slot.
    #[must_use]
    pub fn secret_slots(&self, offer: &EngineOffer) -> Vec<(SecretSlot, &SecretField)> {
        let mut out = Vec::new();
        if matches!(self.auth, AuthChoice::Password | AuthChoice::Token) {
            out.push((SecretSlot::Auth, &self.auth_secret));
        }
        if self.ssh_enabled && !offer.file_based {
            let last = self.hops.len().saturating_sub(1);
            for (i, hop) in self.hops.iter().enumerate() {
                if hop.has_secret() {
                    let slot = if i == last {
                        SecretSlot::SshServer
                    } else {
                        SecretSlot::SshJump(i)
                    };
                    out.push((slot, &hop.secret));
                }
            }
        }
        if self.tls_section_applies(offer)
            && offer.client_certificates
            && !self.client_cert.trim().is_empty()
            && self.client_key_has_passphrase
        {
            out.push((SecretSlot::TlsClientKey, &self.client_key_secret));
        }
        out
    }

    /// Whether the "SCRAM only" toggle is shown: the driver enforces it and the connection
    /// authenticates with a password.
    #[must_use]
    pub fn scram_only_offered(&self, offer: &EngineOffer) -> bool {
        offer.scram_only && self.auth == AuthChoice::Password
    }

    fn scram_only_applies(&self, offer: &EngineOffer) -> bool {
        self.scram_only && self.scram_only_offered(offer)
    }

    fn tls_section_applies(&self, offer: &EngineOffer) -> bool {
        !offer.file_based && offer.offers_tls()
    }

    /// Builds the connection. `typed` holds the secrets typed in this dialog session (they
    /// replace a saved one or create it); `store` is where new secrets would go (`None`: no
    /// keyring or vault available, only "ask every time").
    ///
    /// # Errors
    /// The first field problem found, for the user. Driver-specific rules are checked
    /// afterwards with `DriverRegistry::validate_connection`.
    pub fn build(
        &self,
        offer: &EngineOffer,
        typed: Vec<(SecretSlot, SecretString)>,
        types: &ConnectionTypes,
        store: Option<StoreKind>,
    ) -> Result<BuiltConnection, FormError> {
        let engine = EngineId::new(&self.engine).map_err(|e| FormError::new("engine", e))?;
        let connection_type = self
            .connection_type
            .clone()
            .ok_or_else(|| FormError::new("type", "choose a connection type"))?;
        if types.get(&connection_type).is_none() {
            return Err(FormError::new("type", "unknown connection type"));
        }
        let endpoint = if offer.file_based {
            let path = self.file_path.trim();
            if path.is_empty() {
                return Err(FormError::new("file", "enter the database file"));
            }
            Endpoint::File {
                path: PathBuf::from(path),
            }
        } else {
            let host = self.host.trim();
            if host.is_empty() {
                return Err(FormError::new("host", "enter the host"));
            }
            Endpoint::Tcp {
                host: host.to_owned(),
                port: parse_port(&self.port, "port")?,
                database: non_empty(&self.database),
            }
        };

        let mut pending: Vec<(SecretSlot, SecretString)> = Vec::new();
        let mut typed = typed;
        let mut source_for = |slot: SecretSlot,
                              field: &SecretField,
                              what: &str|
         -> Result<SecretSource, FormError> {
            let new_value = typed
                .iter()
                .position(|(s, _)| *s == slot)
                .map(|i| typed.swap_remove(i).1);
            match field.mode {
                SecretMode::Ask => Ok(SecretSource::Prompt),
                SecretMode::Save => match (new_value, &field.stored) {
                    (Some(value), _) => {
                        if store.is_none() {
                            return Err(FormError::new(
                                "secret",
                                "no keyring or vault is available: choose \"ask every time\"",
                            ));
                        }
                        pending.push((slot, value));
                        // Pointed at a fresh reference when it is stored (the old secret, if
                        // any, is no longer referenced and lands in `to_delete`).
                        Ok(SecretSource::Prompt)
                    }
                    (None, Some(r)) => Ok(SecretSource::Stored(r.clone())),
                    (None, None) => Err(FormError::new(
                        "secret",
                        format!("type the {what} to save, or choose \"ask every time\""),
                    )),
                },
            }
        };

        if !AuthChoice::offered(&offer.auth).contains(&self.auth) {
            return Err(FormError::new(
                "auth",
                "this authentication is not supported by the driver of this engine",
            ));
        }
        let username = self.username.trim().to_owned();
        let auth = match self.auth {
            AuthChoice::None => Auth::None,
            AuthChoice::UserOnly => Auth::Username {
                username: required(&username, "user")?,
            },
            AuthChoice::Password => Auth::Password {
                username: required(&username, "user")?,
                secret: source_for(SecretSlot::Auth, &self.auth_secret, "password")?,
            },
            AuthChoice::Token => Auth::Token {
                username: non_empty(&username),
                secret: source_for(SecretSlot::Auth, &self.auth_secret, "token")?,
            },
        };

        let tls = if self.tls_section_applies(offer) {
            if !offer.tls_modes.contains(&self.tls_mode) {
                return Err(FormError::new(
                    "tls",
                    "this TLS mode is not offered by the driver of this engine",
                ));
            }
            let client_certificate = if offer.client_certificates
                && self.tls_mode != TlsMode::Disable
                && !self.client_cert.trim().is_empty()
            {
                let key = self.client_key.trim();
                if key.is_empty() {
                    return Err(FormError::new(
                        "client_key",
                        "enter the client key file of the certificate",
                    ));
                }
                let passphrase = if self.client_key_has_passphrase {
                    Some(source_for(
                        SecretSlot::TlsClientKey,
                        &self.client_key_secret,
                        "key passphrase",
                    )?)
                } else {
                    None
                };
                Some(ClientCertificate {
                    cert_file: PathBuf::from(self.client_cert.trim()),
                    key_file: PathBuf::from(key),
                    passphrase,
                })
            } else {
                None
            };
            TlsConfig {
                mode: self.tls_mode,
                ca_file: non_empty(&self.ca_file).map(PathBuf::from),
                client_certificate,
            }
        } else if offer.file_based {
            TlsConfig::default()
        } else {
            // A network engine whose driver offers no TLS: say so instead of keeping the
            // verify-full default the driver cannot honour (I8). Core (feat/p1-d-app-wiring)
            // refuses a TLS mode outside `Capabilities::tls` at save.
            TlsConfig {
                mode: TlsMode::Disable,
                ..TlsConfig::default()
            }
        };

        let ssh = if self.ssh_enabled && !offer.file_based {
            if self.hops.is_empty() {
                return Err(FormError::new("ssh", "add the SSH server"));
            }
            let last = self.hops.len() - 1;
            let mut hops = Vec::with_capacity(self.hops.len());
            for (i, h) in self.hops.iter().enumerate() {
                let slot = if i == last {
                    SecretSlot::SshServer
                } else {
                    SecretSlot::SshJump(i)
                };
                let auth = match h.auth {
                    SshAuthChoice::Agent => SshAuth::Agent,
                    SshAuthChoice::Password => SshAuth::Password {
                        secret: source_for(slot, &h.secret, "SSH password")?,
                    },
                    SshAuthChoice::KeyFile => {
                        let key = h.key_path.trim();
                        if key.is_empty() {
                            return Err(FormError::new("ssh.key", "enter the SSH key file"));
                        }
                        SshAuth::PublicKey {
                            key_path: PathBuf::from(key),
                            passphrase: if h.key_has_passphrase {
                                Some(source_for(slot, &h.secret, "key passphrase")?)
                            } else {
                                None
                            },
                        }
                    }
                };
                let host = h.host.trim();
                if host.is_empty() {
                    return Err(FormError::new("ssh.host", "enter the SSH host"));
                }
                let mut hop = SshHop::new(
                    host,
                    parse_port(&h.port, "ssh.port")?.unwrap_or(22),
                    required(h.username.trim(), "ssh.user")?,
                    auth,
                );
                hop.host_key_policy = h.policy;
                hops.push(hop);
            }
            let server = hops
                .pop()
                .ok_or_else(|| FormError::new("ssh", "add the SSH server"))?;
            Some(SshTunnel {
                server,
                jump_hosts: hops,
            })
        } else {
            None
        };

        let mut properties = std::collections::BTreeMap::new();
        if offer.offers_properties() {
            for (k, v) in &self.properties {
                let key = k.trim();
                if key.is_empty() {
                    continue;
                }
                if properties.insert(key.to_owned(), v.clone()).is_some() {
                    return Err(FormError::new(
                        "properties",
                        format!("the property `{key}` is set twice"),
                    ));
                }
            }
        }
        let folder: Vec<String> = self
            .folder
            .split('/')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();

        let mut connection = match &self.existing {
            Some(existing) => {
                let mut c = existing.clone();
                c.engine = engine;
                c.endpoint = endpoint;
                c.auth = auth;
                c.connection_type = connection_type;
                c
            }
            None => Connection::new(String::new(), engine, endpoint, auth, connection_type),
        };
        connection.name = self.name.trim().to_owned();
        if connection.name.is_empty() {
            return Err(FormError::new("name", "enter a name"));
        }
        connection.tls = tls;
        connection.ssh = ssh;
        connection.read_only = self.read_only;
        connection.folder = folder;
        connection.properties = properties;
        connection.password_exchange = if self.scram_only_applies(offer) {
            PasswordExchange::ScramOnly
        } else {
            PasswordExchange::Any
        };
        // Every saved secret of the previous version that the new one no longer references:
        // a value replaced, a field switched to "ask", or a credential field gone (auth kind
        // changed, SSH off, hop removed, passphrase unticked, client certificate cleared, TLS
        // disabled, engine changed). The hub deletes them after saving, unless still used.
        let kept: Vec<&SecretRef> = connection
            .secret_refs()
            .into_iter()
            .map(|(_, _, r)| r)
            .collect();
        let mut to_delete: Vec<SecretRef> = Vec::new();
        if let Some(existing) = &self.existing {
            for (_, _, r) in existing.secret_refs() {
                if !kept.contains(&r) && !to_delete.contains(r) {
                    to_delete.push(r.clone());
                }
            }
        }
        Ok(BuiltConnection {
            connection,
            to_store: pending,
            to_delete,
        })
    }
}

/// A connection built from the form, with the secrets to store and the saved secrets that are
/// no longer referenced.
pub struct BuiltConnection {
    /// The connection (fields of `to_store` are still "ask every time" until stored).
    pub connection: Connection,
    /// Secrets typed in the dialog, to save in the store and point the fields at.
    pub to_store: Vec<(SecretSlot, SecretString)>,
    /// Saved secrets that the connection no longer uses, to delete after saving.
    pub to_delete: Vec<SecretRef>,
}

impl std::fmt::Debug for BuiltConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuiltConnection")
            .field("connection", &self.connection.id)
            .field(
                "to_store",
                &self.to_store.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            )
            .field("to_delete", &self.to_delete)
            .finish()
    }
}

/// A field problem, for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormError {
    /// Field id (`host`, `ssh.port`…).
    pub field: &'static str,
    /// What is wrong.
    pub message: String,
}

impl FormError {
    fn new(field: &'static str, message: impl ToString) -> Self {
        Self {
            field,
            message: message.to_string(),
        }
    }
}

impl std::fmt::Display for FormError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn path_text(p: Option<&PathBuf>) -> String {
    p.map(|p| p.display().to_string()).unwrap_or_default()
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

fn required(s: &str, field: &'static str) -> Result<String, FormError> {
    non_empty(s).ok_or_else(|| FormError::new(field, "enter the user name"))
}

fn parse_port(s: &str, field: &'static str) -> Result<Option<u16>, FormError> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    match t.parse::<u16>() {
        Ok(p) if p > 0 => Ok(Some(p)),
        _ => Err(FormError::new(
            field,
            "the port must be between 1 and 65535",
        )),
    }
}

/// How serious a notice is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Context.
    Info,
    /// A weakened setting, allowed but always visible (I5).
    Warning,
    /// Blocks saving.
    Error,
}

/// A notice shown in the dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// Severity.
    pub severity: Severity,
    /// Text.
    pub text: String,
}

impl Notice {
    fn warning(text: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            text: text.into(),
        }
    }
}

fn hop_name(c: &Connection, hop: usize) -> String {
    let host = c
        .ssh
        .as_ref()
        .and_then(|s| s.hops().nth(hop))
        .map_or("?", |h| h.host.as_str());
    format!("SSH hop {} ({host})", hop + 1)
}

/// Text of a model security warning in the dialog and on the connection's badge.
#[must_use]
pub fn security_warning_text(c: &Connection, w: SecurityWarning) -> String {
    match w {
        SecurityWarning::TlsDisabled => {
            "TLS is disabled: the traffic, including credentials, is readable on the network."
                .to_owned()
        }
        SecurityWarning::TlsCertificateNotVerified => {
            "Encrypt only: the server certificate is not verified, so a man in the middle can read and change the traffic."
                .to_owned()
        }
        SecurityWarning::TlsHostnameNotVerified => {
            "Verify CA: the host name is not checked; any server holding a certificate from this CA is accepted."
                .to_owned()
        }
        SecurityWarning::SshAcceptNewHostKeys { hop } => format!(
            "{}: unknown host keys are recorded without asking (trust on first use). A man in the middle on the first connection would go unnoticed; known keys are still verified and a changed key still blocks.",
            hop_name(c, hop)
        ),
        SecurityWarning::SshHostKeyNotVerified { hop } => format!(
            "{}: the host key is not verified. Anyone on the path can intercept the tunnel and the SSH credentials.",
            hop_name(c, hop)
        ),
        _ => w.to_string(),
    }
}

/// Notices for a built connection: weakened TLS and SSH settings (I5, ADR-0016), the
/// `verify-ca` CA-file rule (ADR-0019), SCRAM-only below `verify-ca`, the cleartext leg behind
/// an SSH tunnel (wave 2 review L4) and client-side-only read-only (ADR-0007 §2).
#[must_use]
pub fn notices(c: &Connection, offer: &EngineOffer, types: &ConnectionTypes) -> Vec<Notice> {
    let mut out: Vec<Notice> = c
        .security_warnings()
        .into_iter()
        .map(|w| Notice::warning(security_warning_text(c, w)))
        .collect();
    let network = c.endpoint.is_network() && offer.offers_tls();
    if network
        && c.tls.mode == TlsMode::VerifyCa
        && c.tls.ca_file.is_none()
        && offer.verify_ca_requires_ca_file
    {
        out.push(Notice {
            severity: Severity::Error,
            text: "Verify CA trusts only the CA file you give: choose the CA file, or use \"Verify CA and host name\" to rely on the system trust store.".to_owned(),
        });
    }
    if c.scram_without_verified_tls() {
        out.push(Notice::warning(
            "SCRAM only stops password-method downgrades, not a man in the middle: below Verify CA, an attacker who relays the exchange still gets in. Use \"Verify CA and host name\".",
        ));
    }
    if let (Some(_), Endpoint::Tcp { host, .. }) = (&c.ssh, &c.endpoint)
        && matches!(c.auth, Auth::Password { .. } | Auth::Token { .. })
        && c.tls.mode == TlsMode::Disable
        && !is_loopback(host)
    {
        out.push(Notice::warning(
            "The SSH tunnel protects only the way to the SSH server: from there to the database, the credentials cross the network in clear unless the database runs on the SSH server itself (host localhost). Enable TLS.",
        ));
    }
    if let Endpoint::File { path } = &c.endpoint
        && inside_dbeaver_directory(path)
    {
        // An early, lexical hint only (no file system access on the foreground thread, I6).
        // The decision is the core's: at connect it checks the canonical path against the
        // DBeaver roots and opens the file read-only (`SessionInfo::inside_dbeaver_workspace`,
        // shown on the session badge).
        out.push(Notice {
            severity: Severity::Info,
            text: "This file looks like it is inside a DBeaver directory: NineSQL never modifies DBeaver files (I4), so it will open it read-only.".to_owned(),
        });
    }
    let policy = c.policy(types);
    if policy.read_only && offer.capabilities.read_only == ReadOnlyMechanism::ClientSideOnly {
        out.push(Notice::warning(
            "This engine has no server-side read-only mode: NineSQL refuses the writes it recognizes, but a statement it cannot classify could still write. Use a read-only database account.",
        ));
    }
    if policy.production {
        out.push(Notice {
            severity: Severity::Info,
            text: "Production: destructive statements and init SQL need confirmation, and new sessions start in manual commit.".to_owned(),
        });
    }
    out
}

/// Whether a path lies, by its components, inside a DBeaver data directory (`DBeaverData`,
/// `workspace6`, the legacy `.dbeaver4` / `.dbeaver`). Lexical only: no file system access.
#[must_use]
pub fn inside_dbeaver_directory(path: &std::path::Path) -> bool {
    path.components().any(|c| {
        let name = c.as_os_str();
        name == "DBeaverData" || name == "workspace6" || name == ".dbeaver4" || name == ".dbeaver"
    })
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use std::sync::Arc;

    use ninesql_core::DriverRegistry;
    use ninesql_core::secrets::SecretId;

    use super::super::engines::offers;
    use super::*;

    fn offer(engine: &str) -> EngineOffer {
        let mut r = DriverRegistry::new();
        r.register(Arc::new(ninesql_driver_pgwire::PgWireDriver::new()));
        r.register(Arc::new(ninesql_driver_mysqlwire::MySqlWireDriver::new()));
        r.register(Arc::new(ninesql_driver_sqlite::SqliteDriver::new()));
        offers(&r).into_iter().find(|o| o.engine == engine).unwrap()
    }

    fn pg_form() -> ConnectionForm {
        let mut f = ConnectionForm::new("postgresql");
        f.name = "Sales".into();
        f.host = "db.example.test".into();
        f.username = "app".into();
        f.connection_type = Some(ConnectionTypeId::dev());
        f
    }

    fn secret(s: &str) -> SecretString {
        SecretString::from(s.to_owned())
    }

    #[test]
    fn a_new_connection_needs_an_explicit_type() {
        let mut f = pg_form();
        f.connection_type = None;
        let err = f
            .build(
                &offer("postgresql"),
                vec![],
                &ConnectionTypes::default(),
                None,
            )
            .unwrap_err();
        assert_eq!(err.field, "type");
    }

    #[test]
    fn ask_every_time_builds_a_prompt_without_any_store() {
        let built = pg_form()
            .build(
                &offer("postgresql"),
                vec![],
                &ConnectionTypes::default(),
                None,
            )
            .unwrap();
        assert!(matches!(
            built.connection.auth,
            Auth::Password {
                secret: SecretSource::Prompt,
                ..
            }
        ));
        assert!(built.to_store.is_empty());
        assert_eq!(built.connection.tls.mode, TlsMode::VerifyFull, "I5 default");
    }

    #[test]
    fn saving_a_password_needs_a_store_and_never_reaches_the_model() {
        let mut f = pg_form();
        f.auth_secret.mode = SecretMode::Save;
        let o = offer("postgresql");
        let types = ConnectionTypes::default();
        let err = f
            .build(
                &o,
                vec![(SecretSlot::Auth, secret("hunter2"))],
                &types,
                None,
            )
            .unwrap_err();
        assert!(err.message.contains("ask every time"));
        let built = f
            .build(
                &o,
                vec![(SecretSlot::Auth, secret("hunter2"))],
                &types,
                Some(StoreKind::Vault),
            )
            .unwrap();
        assert_eq!(built.to_store.len(), 1);
        let json = serde_json::to_string(&built.connection).unwrap();
        assert!(!json.contains("hunter2"), "I1");
        assert!(!format!("{built:?}").contains("hunter2"));
        // Nothing typed and nothing saved: the user must type it or choose "ask".
        assert!(f.build(&o, vec![], &types, Some(StoreKind::Vault)).is_err());
    }

    #[test]
    fn secret_status_never_shows_a_value() {
        let r = SecretRef::new(StoreKind::Keyring, SecretId::new("x:auth").unwrap());
        let field = SecretField {
            mode: SecretMode::Save,
            stored: Some(r),
        };
        assert_eq!(field.status(), "Saved in keyring");
        let vault = SecretField {
            mode: SecretMode::Save,
            stored: Some(SecretRef::new(
                StoreKind::Vault,
                SecretId::new("x:auth").unwrap(),
            )),
        };
        assert_eq!(vault.status(), "Saved in vault");
        assert_eq!(
            SecretField {
                mode: SecretMode::Save,
                stored: None
            }
            .status(),
            "Not saved"
        );
        assert_eq!(SecretField::default().status(), "Asked at every connect");
    }

    fn vault_ref(id: &str) -> SecretRef {
        SecretRef::new(StoreKind::Vault, SecretId::new(id).unwrap())
    }

    /// A saved PostgreSQL connection with every credential slot saved: database password, a
    /// password jump host, a key file with passphrase on the SSH server, an encrypted client
    /// key.
    fn saved_with_every_secret() -> Connection {
        let mut f = pg_form();
        f.client_cert = "/etc/ninesql/client.crt".into();
        f.client_key = "/etc/ninesql/client.key".into();
        f.client_key_has_passphrase = true;
        f.ssh_enabled = true;
        f.hops = vec![
            HopForm {
                host: "jump.example.test".into(),
                username: "me".into(),
                auth: SshAuthChoice::Password,
                ..HopForm::default()
            },
            HopForm {
                host: "bastion.example.test".into(),
                username: "me".into(),
                auth: SshAuthChoice::KeyFile,
                key_path: "/home/me/.ssh/id_ed25519".into(),
                key_has_passphrase: true,
                ..HopForm::default()
            },
        ];
        let mut c = f
            .build(
                &offer("postgresql"),
                vec![],
                &ConnectionTypes::default(),
                Some(StoreKind::Vault),
            )
            .unwrap()
            .connection;
        for (slot, id) in [
            (SecretSlot::Auth, "c:auth:1"),
            (SecretSlot::SshJump(0), "c:ssh-jump-0:1"),
            (SecretSlot::SshServer, "c:ssh:1"),
            (SecretSlot::TlsClientKey, "c:tls-client-key:1"),
        ] {
            c.set_secret_ref(slot, vault_ref(id)).unwrap();
        }
        assert_eq!(c.secret_refs().len(), 4);
        c
    }

    fn deleted_after(edit: impl FnOnce(&mut ConnectionForm), engine: &str) -> Vec<String> {
        let mut f = ConnectionForm::from_connection(&saved_with_every_secret());
        edit(&mut f);
        let built = f
            .build(
                &offer(engine),
                vec![],
                &ConnectionTypes::default(),
                Some(StoreKind::Vault),
            )
            .unwrap();
        let mut ids: Vec<String> = built
            .to_delete
            .iter()
            .map(|r| r.id.as_str().to_owned())
            .collect();
        ids.sort();
        ids
    }

    #[test]
    fn unchanged_edit_deletes_nothing() {
        assert!(deleted_after(|_| {}, "postgresql").is_empty());
    }

    #[test]
    fn switching_to_ask_deletes_the_saved_secret() {
        let deleted = deleted_after(|f| f.auth_secret.mode = SecretMode::Ask, "postgresql");
        assert_eq!(deleted, ["c:auth:1"]);
    }

    #[test]
    fn a_replaced_value_deletes_the_old_secret() {
        let mut f = ConnectionForm::from_connection(&saved_with_every_secret());
        f.auth_secret.mode = SecretMode::Save;
        let built = f
            .build(
                &offer("postgresql"),
                vec![(SecretSlot::Auth, secret("new-value"))],
                &ConnectionTypes::default(),
                Some(StoreKind::Vault),
            )
            .unwrap();
        assert_eq!(built.to_store.len(), 1);
        assert_eq!(built.to_delete, [vault_ref("c:auth:1")]);
    }

    #[test]
    fn credential_fields_that_go_away_orphan_nothing() {
        // M2 of the P1-C review: every way a credential field can disappear.
        assert_eq!(
            deleted_after(|f| f.set_auth(AuthChoice::UserOnly), "postgresql"),
            ["c:auth:1"]
        );
        assert_eq!(
            deleted_after(|f| f.ssh_enabled = false, "postgresql"),
            ["c:ssh-jump-0:1", "c:ssh:1"]
        );
        assert_eq!(
            deleted_after(
                |f| {
                    f.hops.remove(0);
                },
                "postgresql"
            ),
            ["c:ssh-jump-0:1"]
        );
        assert_eq!(
            deleted_after(|f| f.hops[1].set_key_has_passphrase(false), "postgresql"),
            ["c:ssh:1"]
        );
        assert_eq!(
            deleted_after(|f| f.set_client_key_has_passphrase(false), "postgresql"),
            ["c:tls-client-key:1"]
        );
        assert_eq!(
            deleted_after(|f| f.client_cert.clear(), "postgresql"),
            ["c:tls-client-key:1"]
        );
        assert_eq!(
            deleted_after(|f| f.tls_mode = TlsMode::Disable, "postgresql"),
            ["c:tls-client-key:1"]
        );
        // SQLite: a file engine, no SSH and no TLS; only the database password could stay, and
        // the sqlite driver takes none.
        let o = offer("sqlite");
        let deleted = deleted_after(
            |f| {
                f.adapt_to(&o);
                f.file_path = "/tmp/x.db".into();
            },
            "sqlite",
        );
        assert!(deleted.contains(&"c:ssh:1".to_owned()));
        assert!(deleted.contains(&"c:ssh-jump-0:1".to_owned()));
        assert!(deleted.contains(&"c:tls-client-key:1".to_owned()));
    }

    #[test]
    fn a_new_hop_never_inherits_a_moved_hops_secret() {
        // M1: removing jump 0 moves jump 1 down; the moved hop keeps its own reference and the
        // new hop starts without one.
        let mut c = saved_with_every_secret();
        let ssh = c.ssh.as_mut().unwrap();
        ssh.jump_hosts.push(SshHop::new(
            "jump2.example.test",
            22,
            "me",
            SshAuth::Password {
                secret: SecretSource::Stored(vault_ref("c:ssh-jump-1:1")),
            },
        ));
        let mut f = ConnectionForm::from_connection(&c);
        f.hops.remove(0);
        f.hops.insert(1, HopForm::default());
        assert_eq!(f.hops[0].secret.stored, Some(vault_ref("c:ssh-jump-1:1")));
        assert_eq!(f.hops[1].secret.stored, None);
    }

    #[test]
    fn changing_auth_kind_resets_the_secret_field() {
        // L6: a saved password is never reused as a token or a key passphrase.
        let mut f = ConnectionForm::from_connection(&saved_with_every_secret());
        f.set_auth(AuthChoice::Token);
        assert_eq!(f.auth_secret, SecretField::default());
        f.set_auth(AuthChoice::Token);
        let mut hop = f.hops[0].clone();
        assert!(hop.secret.stored.is_some());
        hop.set_auth(SshAuthChoice::KeyFile);
        assert_eq!(hop.secret, SecretField::default());
        assert!(!hop.key_has_passphrase);
        let mut same = f.hops[0].clone();
        same.set_auth(SshAuthChoice::Password);
        assert!(same.secret.stored.is_some(), "no change, no reset");
    }

    #[test]
    fn read_write_is_not_offered_for_production() {
        let types = ConnectionTypes::default();
        let mut f = pg_form();
        assert!(f.read_write_allowed(&types));
        f.read_only = Some(false);
        f.set_connection_type(ConnectionTypeId::prod(), &types);
        assert!(!f.read_write_allowed(&types));
        assert_eq!(f.read_only, None, "the ignored override is dropped");
        f.read_only = Some(true);
        f.set_connection_type(ConnectionTypeId::dev(), &types);
        assert_eq!(f.read_only, Some(true));
    }

    #[test]
    fn scram_only_round_trips_and_is_offered_only_for_passwords() {
        let o = offer("postgresql");
        let types = ConnectionTypes::default();
        let mut f = pg_form();
        assert!(f.scram_only_offered(&o));
        assert!(!f.scram_only_offered(&offer("mysql")));
        f.scram_only = true;
        let c = f.build(&o, vec![], &types, None).unwrap().connection;
        assert_eq!(c.password_exchange, PasswordExchange::ScramOnly);
        assert!(!c.scram_without_verified_tls(), "verify-full by default");
        assert!(ConnectionForm::from_connection(&c).scram_only);
        f.set_auth(AuthChoice::UserOnly);
        assert!(!f.scram_only_offered(&o));
        let c = f.build(&o, vec![], &types, None).unwrap().connection;
        assert_eq!(c.password_exchange, PasswordExchange::Any);
    }

    #[test]
    fn dbeaver_directories_are_flagged() {
        assert!(inside_dbeaver_directory(std::path::Path::new(
            "/home/me/.local/share/DBeaverData/workspace6/General/db.sqlite"
        )));
        assert!(inside_dbeaver_directory(std::path::Path::new(
            "/home/me/.dbeaver4/x.db"
        )));
        assert!(!inside_dbeaver_directory(std::path::Path::new(
            "/home/me/data/dbeaver-notes.db"
        )));
        let mut f = ConnectionForm::new("sqlite");
        f.name = "x".into();
        f.connection_type = Some(ConnectionTypeId::dev());
        f.file_path = "/home/me/.local/share/DBeaverData/workspace6/db.sqlite".into();
        let o = offer("sqlite");
        f.adapt_to(&o);
        let types = ConnectionTypes::default();
        let built = f.build(&o, vec![], &types, None).unwrap();
        let n = notices(&built.connection, &o, &types);
        assert!(n.iter().any(|n| n.text.contains("DBeaver directory")));
    }

    #[test]
    fn round_trip_through_the_model() {
        let mut f = pg_form();
        f.port = "6543".into();
        f.database = "sales".into();
        f.tls_mode = TlsMode::VerifyCa;
        f.ca_file = "/etc/ssl/private-ca.pem".into();
        f.ssh_enabled = true;
        f.hops = vec![
            HopForm {
                host: "jump.example.test".into(),
                username: "me".into(),
                ..HopForm::default()
            },
            HopForm {
                host: "bastion.example.test".into(),
                port: "2222".into(),
                username: "me".into(),
                auth: SshAuthChoice::KeyFile,
                key_path: "/home/me/.ssh/id_ed25519".into(),
                policy: HostKeyPolicy::AcceptNew,
                ..HopForm::default()
            },
        ];
        f.properties = vec![("application_name".into(), "nine".into())];
        f.folder = " team / sales ".into();
        f.read_only = Some(true);
        let built = f
            .build(
                &offer("postgresql"),
                vec![],
                &ConnectionTypes::default(),
                None,
            )
            .unwrap();
        let c = &built.connection;
        let ssh = c.ssh.as_ref().unwrap();
        assert_eq!(ssh.jump_hosts.len(), 1);
        assert_eq!(ssh.server.port, 2222);
        assert_eq!(ssh.server.host_key_policy, HostKeyPolicy::AcceptNew);
        assert_eq!(c.folder, ["team", "sales"]);
        let mut back = ConnectionForm::from_connection(c);
        back.existing = None;
        f.folder = "team/sales".into();
        assert_eq!(back, f);
    }

    #[test]
    fn file_engines_take_a_file_and_no_tls_or_ssh() {
        let mut f = ConnectionForm::new("sqlite");
        let o = offer("sqlite");
        f.adapt_to(&o);
        assert_eq!(f.auth, AuthChoice::None, "the only auth sqlite declares");
        f.name = "Local".into();
        f.connection_type = Some(ConnectionTypeId::dev());
        f.ssh_enabled = true;
        assert_eq!(
            f.build(&o, vec![], &ConnectionTypes::default(), None)
                .unwrap_err()
                .field,
            "file"
        );
        f.file_path = "/tmp/nine.db".into();
        let c = f
            .build(&o, vec![], &ConnectionTypes::default(), None)
            .unwrap()
            .connection;
        assert!(matches!(c.endpoint, Endpoint::File { .. }));
        assert!(c.ssh.is_none());
        assert!(c.security_warnings().is_empty());
    }

    #[test]
    fn notices_flag_weak_settings() {
        let o = offer("postgresql");
        let types = ConnectionTypes::default();
        let mut f = pg_form();
        f.tls_mode = TlsMode::VerifyCa;
        let c = f.build(&o, vec![], &types, None).unwrap().connection;
        let n = notices(&c, &o, &types);
        assert!(
            n.iter()
                .any(|n| n.severity == Severity::Error && n.text.contains("CA file"))
        );
        assert!(
            n.iter()
                .any(|n| n.text.contains("host name is not checked"))
        );

        f.tls_mode = TlsMode::Require;
        f.scram_only = true;
        let c = f.build(&o, vec![], &types, None).unwrap().connection;
        assert_eq!(c.password_exchange, PasswordExchange::ScramOnly);
        let n = notices(&c, &o, &types);
        assert!(n.iter().any(|n| n.text.contains("SCRAM only")));
        // Not offered by mysqlwire: the policy is not saved, so no SCRAM notice there.
        let mut m = f.clone();
        m.engine = "mysql".into();
        let my = offer("mysql");
        let c_my = m.build(&my, vec![], &types, None).unwrap().connection;
        assert_eq!(c_my.password_exchange, PasswordExchange::Any);
        assert!(
            !notices(&c_my, &my, &types)
                .iter()
                .any(|n| n.text.contains("SCRAM"))
        );
        f.scram_only = false;

        f.tls_mode = TlsMode::Disable;
        f.ssh_enabled = true;
        f.hops = vec![HopForm {
            host: "bastion".into(),
            username: "me".into(),
            policy: HostKeyPolicy::AcceptNew,
            ..HopForm::default()
        }];
        let c = f.build(&o, vec![], &types, None).unwrap().connection;
        let n = notices(&c, &o, &types);
        assert!(n.iter().any(|n| n.text.contains("trust on first use")));
        assert!(n.iter().any(|n| n.text.contains("in clear")));
        f.connection_type = Some(ConnectionTypeId::prod());
        let c = f.build(&o, vec![], &types, None).unwrap().connection;
        assert!(
            notices(&c, &o, &types)
                .iter()
                .any(|n| n.text.starts_with("Production"))
        );
    }

    #[test]
    fn engine_change_keeps_only_offered_choices() {
        let mut f = pg_form();
        f.auth = AuthChoice::Token;
        f.scram_only = true;
        f.adapt_to(&offer("mysql"));
        assert_eq!(f.auth, AuthChoice::Password);
        assert!(!f.scram_only);
        assert_eq!(f.tls_mode, TlsMode::VerifyFull);
    }

    #[test]
    fn duplicate_forgets_saved_secrets() {
        let mut f = pg_form();
        f.auth_secret.mode = SecretMode::Save;
        let mut c = f
            .build(
                &offer("postgresql"),
                vec![(SecretSlot::Auth, secret("x"))],
                &ConnectionTypes::default(),
                Some(StoreKind::Vault),
            )
            .unwrap()
            .connection;
        c.set_secret_ref(
            SecretSlot::Auth,
            SecretRef::new(StoreKind::Vault, SecretId::new("a:auth").unwrap()),
        )
        .unwrap();
        let d = ConnectionForm::duplicate_of(&c);
        assert!(d.existing.is_none());
        assert_eq!(d.name, "Sales (copy)");
        assert_eq!(d.auth_secret, SecretField::default());
    }

    #[test]
    fn url_fills_the_form() {
        let mut r = DriverRegistry::new();
        r.register(Arc::new(ninesql_driver_mysqlwire::MySqlWireDriver::new()));
        let offers = offers(&r);
        let mut f = ConnectionForm::new("mysql");
        let url = super::super::url::parse("mariadb://me@db:3307/shop?ssl-mode=REQUIRED").unwrap();
        f.apply_url(&url, &offers).unwrap();
        assert_eq!(
            (
                f.engine.as_str(),
                f.host.as_str(),
                f.port.as_str(),
                f.username.as_str()
            ),
            ("mariadb", "db", "3307", "me")
        );
        assert_eq!(f.tls_mode, TlsMode::Require);
        let pg = super::super::url::parse("postgresql://db").unwrap();
        assert!(
            f.apply_url(&pg, &offers)
                .unwrap_err()
                .contains("PostgreSQL")
        );
    }
}
