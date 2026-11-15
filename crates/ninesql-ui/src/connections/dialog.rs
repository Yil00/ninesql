//! The connection dialog (F-CON-12 endpoint / URL, F-CON-06 TLS, F-CON-05 SSH, F-CON-08
//! properties, F-CON-03 type, F-CON-01 test): a GPUI view over [`ConnectionForm`].
//!
//! Only what the driver declares is offered (I8): engines from the registry, TLS modes from
//! `Capabilities::tls`, auth kinds and property keys from `ConnectCapabilities`. Weakened
//! settings are flagged as they are chosen (I5). Credentials show "Saved in keyring" /
//! "Saved in vault" / "Not saved" / "Asked at every connect", never a value; a new secret is typed
//! into a masked input and read only when saving or testing. "Test connection" goes through the
//! window's [`ConnectFlow`] on the driver runtime (I6).

use super::secret_input::{self, masked_input as masked, read_secret, secret_field};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ninesql_core::secrets::SecretString;
use ninesql_core::{ConnectionId, ConnectionTypeId, HostKeyPolicy, SecretSlot, TlsMode};

use super::connect_flow::{ConnectFlow, FlowFinished, FlowResult, Purpose};
use super::engines::{EngineOffer, tls_label};
use super::form::{
    AuthChoice, ConnectionForm, EndpointInput, HopForm, Notice, SecretMode, Severity,
    SshAuthChoice, notices, policy_label,
};
use super::hub::ConnectionHub;
use super::visuals::{hsla, text_on};

struct HopInputs {
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    key_path: Entity<InputState>,
    secret: Entity<InputState>,
}

struct PropertyInputs {
    key: Entity<InputState>,
    value: Entity<InputState>,
}

/// State of the last "Test connection".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestState {
    /// Not run.
    Idle,
    /// Running.
    Running,
    /// Finished.
    Done(FlowResult),
}

/// Emitted when the dialog saved a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionSaved(pub ConnectionId);

/// Emitted when the user cancels the dialog (its secret inputs are already cleared).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionDialogCancelled;

/// The connection dialog.
pub struct ConnectionDialog {
    hub: Entity<ConnectionHub>,
    flow: Entity<ConnectFlow>,
    form: ConnectionForm,
    name: Entity<InputState>,
    url: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    database: Entity<InputState>,
    file: Entity<InputState>,
    username: Entity<InputState>,
    secret: Entity<InputState>,
    ca_file: Entity<InputState>,
    client_cert: Entity<InputState>,
    client_key: Entity<InputState>,
    client_key_secret: Entity<InputState>,
    folder: Entity<InputState>,
    hops: Vec<HopInputs>,
    properties: Vec<PropertyInputs>,
    error: Option<String>,
    test: TestState,
    saving: bool,
    _subscription: Subscription,
}

impl EventEmitter<ConnectionSaved> for ConnectionDialog {}
impl EventEmitter<ConnectionDialogCancelled> for ConnectionDialog {}

fn input(window: &mut Window, cx: &mut App, value: &str, placeholder: &str) -> Entity<InputState> {
    let value = value.to_owned();
    let placeholder = placeholder.to_owned();
    cx.new(|cx| {
        InputState::new(window, cx)
            .default_value(value)
            .placeholder(placeholder)
    })
}

fn text(input: &Entity<InputState>, cx: &App) -> String {
    input.read(cx).value().to_string()
}

impl ConnectionDialog {
    /// A dialog over `form` (new, edit or duplicate).
    pub fn new(
        hub: Entity<ConnectionHub>,
        flow: Entity<ConnectFlow>,
        mut form: ConnectionForm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        if let Some(offer) = hub.read(cx).offer(&form.engine).cloned() {
            form.adapt_to(&offer);
        }
        let hops = form
            .hops
            .iter()
            .map(|h| Self::hop_inputs(h, window, cx))
            .collect();
        let properties = form
            .properties
            .iter()
            .map(|(k, v)| PropertyInputs {
                key: input(window, cx, k, "key"),
                value: input(window, cx, v, "value"),
            })
            .collect();
        let subscription = cx.subscribe(&flow, |this: &mut Self, _, event: &FlowFinished, cx| {
            if event.purpose == Purpose::Test && this.test == TestState::Running {
                this.test = TestState::Done(event.result.clone());
                cx.notify();
            }
        });
        Self {
            name: input(window, cx, &form.name, "Name shown in the navigator"),
            url: input(window, cx, "", "postgresql://user@host:5432/database"),
            host: input(window, cx, &form.host, "db.example.com"),
            port: input(window, cx, &form.port, "default"),
            database: input(window, cx, &form.database, "database"),
            file: input(window, cx, &form.file_path, "/path/to/database.db"),
            username: input(window, cx, &form.username, "user"),
            secret: masked(window, cx, "type to save a new value"),
            ca_file: input(window, cx, &form.ca_file, "/path/to/ca.pem"),
            client_cert: input(window, cx, &form.client_cert, "/path/to/client.crt"),
            client_key: input(window, cx, &form.client_key, "/path/to/client.key"),
            client_key_secret: masked(window, cx, "key passphrase"),
            folder: input(window, cx, &form.folder, "team/project"),
            hops,
            properties,
            hub,
            flow,
            form,
            error: None,
            test: TestState::Idle,
            saving: false,
            _subscription: subscription,
        }
    }

    fn hop_inputs(h: &HopForm, window: &mut Window, cx: &mut App) -> HopInputs {
        HopInputs {
            host: input(window, cx, &h.host, "ssh.example.com"),
            port: input(window, cx, &h.port, "22"),
            user: input(window, cx, &h.username, "user"),
            key_path: input(window, cx, &h.key_path, "~/.ssh/id_ed25519"),
            secret: masked(window, cx, "type to save a new value"),
        }
    }

    /// The form (after reading the inputs).
    #[must_use]
    pub fn form(&self) -> &ConnectionForm {
        &self.form
    }

    /// Mutable form, for choices made outside the inputs (tests, buttons).
    pub fn form_mut(&mut self) -> &mut ConnectionForm {
        &mut self.form
    }

    /// The last error shown.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// State of "Test connection".
    #[must_use]
    pub fn test_state(&self) -> &TestState {
        &self.test
    }

    /// The input of a field, for tests: `name`, `host`, `port`, `database`, `file`, `username`,
    /// `secret`, `url`, `ca_file`, `folder`.
    #[must_use]
    pub fn input(&self, field: &str) -> Option<&Entity<InputState>> {
        Some(match field {
            "name" => &self.name,
            "url" => &self.url,
            "host" => &self.host,
            "port" => &self.port,
            "database" => &self.database,
            "file" => &self.file,
            "username" => &self.username,
            "secret" => &self.secret,
            "ca_file" => &self.ca_file,
            "folder" => &self.folder,
            _ => return None,
        })
    }

    fn offer(&self, cx: &App) -> Option<EngineOffer> {
        self.hub.read(cx).offer(&self.form.engine).cloned()
    }

    /// Copies the text inputs into the form.
    pub fn sync(&mut self, cx: &App) {
        let f = &mut self.form;
        f.name = text(&self.name, cx);
        f.host = text(&self.host, cx);
        f.port = text(&self.port, cx);
        f.database = text(&self.database, cx);
        f.file_path = text(&self.file, cx);
        f.username = text(&self.username, cx);
        f.ca_file = text(&self.ca_file, cx);
        f.client_cert = text(&self.client_cert, cx);
        f.client_key = text(&self.client_key, cx);
        f.folder = text(&self.folder, cx);
        for (h, i) in f.hops.iter_mut().zip(&self.hops) {
            h.host = text(&i.host, cx);
            h.port = text(&i.port, cx);
            h.username = text(&i.user, cx);
            h.key_path = text(&i.key_path, cx);
        }
        f.properties = self
            .properties
            .iter()
            .map(|p| (text(&p.key, cx), text(&p.value, cx)))
            .collect();
    }

    /// Secrets typed in this dialog for "save" fields. Read from the masked inputs only here.
    fn typed_secrets(&self, offer: &EngineOffer, cx: &App) -> Vec<(SecretSlot, SecretString)> {
        let mut out = Vec::new();
        for (slot, field) in self.form.secret_slots(offer) {
            if field.mode != SecretMode::Save {
                continue;
            }
            let input = match slot {
                SecretSlot::Auth => Some(&self.secret),
                SecretSlot::TlsClientKey => Some(&self.client_key_secret),
                SecretSlot::SshServer => self.hops.last().map(|h| &h.secret),
                SecretSlot::SshJump(i) => self.hops.get(i).map(|h| &h.secret),
            };
            if let Some(secret) = input.and_then(|i| read_secret(i, cx)) {
                out.push((slot, secret));
            }
        }
        out
    }

    /// Empties every secret input (save, cancel, close).
    pub fn clear_secret_inputs(&self, window: &mut Window, cx: &mut App) {
        let all = [&self.secret, &self.client_key_secret]
            .into_iter()
            .chain(self.hops.iter().map(|h| &h.secret));
        for input in all {
            secret_input::clear(input, window, cx);
        }
    }

    /// The masked inputs, for tests: database secret, client key passphrase, then each hop.
    #[must_use]
    pub fn secret_inputs(&self) -> Vec<Entity<InputState>> {
        [self.secret.clone(), self.client_key_secret.clone()]
            .into_iter()
            .chain(self.hops.iter().map(|h| h.secret.clone()))
            .collect()
    }

    /// "Cancel": clears the secret inputs, tells the window and closes the dialog.
    pub fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_secret_inputs(window, cx);
        cx.emit(ConnectionDialogCancelled);
        window.close_dialog(cx);
    }

    /// Changes the database authentication: the credential field and its input are reset
    /// (a saved password is never reused as a token).
    pub fn set_auth(&mut self, choice: AuthChoice, window: &mut Window, cx: &mut Context<Self>) {
        if self.form.auth != choice {
            self.form.set_auth(choice);
            secret_input::clear(&self.secret, window, cx);
            cx.notify();
        }
    }

    /// Changes the authentication of SSH hop `index`: its credential field and input are reset.
    pub fn set_hop_auth(
        &mut self,
        index: usize,
        choice: SshAuthChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(h) = self.form.hops.get_mut(index)
            && h.auth != choice
        {
            h.set_auth(choice);
            if let Some(i) = self.hops.get(index) {
                secret_input::clear(&i.secret, window, cx);
            }
            cx.notify();
        }
    }

    /// Notices for the current fields, plus the build or validation error.
    #[must_use]
    pub fn current_notices(&self, cx: &App) -> Vec<Notice> {
        let Some(offer) = self.offer(cx) else {
            return vec![Notice {
                severity: Severity::Error,
                text: "No driver is available for this engine.".to_owned(),
            }];
        };
        let hub = self.hub.read(cx);
        // Built without typed secrets: notices only read settings.
        let mut probe = self.form.clone();
        for field in std::iter::once(&mut probe.auth_secret)
            .chain(std::iter::once(&mut probe.client_key_secret))
            .chain(probe.hops.iter_mut().map(|h| &mut h.secret))
        {
            if field.stored.is_none() {
                field.mode = SecretMode::Ask;
            }
        }
        match probe.build(&offer, Vec::new(), hub.types(), hub.store_for_new_secrets()) {
            Ok(built) => {
                let mut out = notices(&built.connection, &offer, hub.types(), self.form.scram_only);
                if let Some(existing) = &self.form.existing
                    && hub.is_open(existing.id)
                {
                    out.push(Notice {
                        severity: Severity::Info,
                        text: "This connection is open: its sessions keep their current settings until you reconnect.".to_owned(),
                    });
                }
                if let Err(e) = hub.validate(&built.connection)
                    && !out.iter().any(|n| n.severity == Severity::Error)
                {
                    out.push(Notice {
                        severity: Severity::Error,
                        text: e,
                    });
                }
                for k in built.connection.properties.keys() {
                    if !offer.accepts_property(k) {
                        out.push(Notice {
                            severity: Severity::Error,
                            text: format!(
                                "The property `{k}` is not accepted by this driver{}.",
                                accepted_hint(&offer)
                            ),
                        });
                    }
                }
                out
            }
            Err(_) => Vec::new(),
        }
    }

    /// Changes the engine; settings the driver does not support fall back (I8).
    pub fn set_engine(&mut self, engine: &str, cx: &mut Context<Self>) {
        if let Some(offer) = self.hub.read(cx).offer(engine).cloned() {
            self.form.adapt_to(&offer);
            cx.notify();
        }
    }

    /// Applies the URL input to the fields (F-CON-12). A URL with a password is refused.
    pub fn apply_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let url = text(&self.url, cx);
        let offers = self.hub.read(cx).offers().to_vec();
        let result = super::url::parse(&url)
            .map_err(|e| e.to_string())
            .and_then(|parsed| {
                self.sync(cx);
                self.form.apply_url(&parsed, &offers)
            });
        // The URL is never kept: a refused URL may hold a password.
        self.url.update(cx, |s, cx| s.set_value("", window, cx));
        match result {
            Ok(()) => {
                self.error = None;
                self.form.endpoint_input = EndpointInput::Fields;
                if let Some(offer) = self.offer(cx) {
                    self.form.adapt_to(&offer);
                }
                let f = self.form.clone();
                for (input, value) in [
                    (&self.host, &f.host),
                    (&self.port, &f.port),
                    (&self.database, &f.database),
                    (&self.file, &f.file_path),
                    (&self.username, &f.username),
                    (&self.ca_file, &f.ca_file),
                ] {
                    let value = value.clone();
                    input.update(cx, |s, cx| s.set_value(value, window, cx));
                }
                let missing: Vec<(String, String)> = f
                    .properties
                    .iter()
                    .skip(self.properties.len())
                    .cloned()
                    .collect();
                for (i, p) in self.properties.iter().enumerate() {
                    if let Some((_, v)) = f.properties.get(i) {
                        let v = v.clone();
                        p.value.update(cx, |s, cx| s.set_value(v, window, cx));
                    }
                }
                for (k, v) in missing {
                    self.properties.push(PropertyInputs {
                        key: input(window, cx, &k, "key"),
                        value: input(window, cx, &v, "value"),
                    });
                }
            }
            Err(e) => self.error = Some(e),
        }
        cx.notify();
    }

    /// Adds an SSH hop: the first one is the SSH server; each further one is inserted before
    /// it, as a jump host.
    pub fn add_hop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let hop = HopForm::default();
        let inputs = Self::hop_inputs(&hop, window, cx);
        let at = self.form.hops.len().saturating_sub(1);
        if self.form.hops.is_empty() {
            self.form.hops.push(hop);
            self.hops.push(inputs);
        } else {
            self.form.hops.insert(at, hop);
            self.hops.insert(at, inputs);
        }
        self.form.ssh_enabled = true;
        cx.notify();
    }

    /// Removes the SSH hop at `index` (its typed secret is cleared; a saved one is deleted when
    /// the connection is saved).
    pub fn remove_hop(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index < self.form.hops.len() {
            self.form.hops.remove(index);
            let inputs = self.hops.remove(index);
            secret_input::clear(&inputs.secret, window, cx);
            if self.form.hops.is_empty() {
                self.form.ssh_enabled = false;
            }
            cx.notify();
        }
    }

    /// Adds an empty property row.
    pub fn add_property(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.properties.push(PropertyInputs {
            key: input(window, cx, "", "key"),
            value: input(window, cx, "", "value"),
        });
        self.form.properties.push((String::new(), String::new()));
        cx.notify();
    }

    /// "Test connection": builds the connection (with the secrets typed here) and opens it
    /// through the connect flow, which closes it again and reports.
    pub fn test_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync(cx);
        let Some(offer) = self.offer(cx) else {
            return;
        };
        let typed = self.typed_secrets(&offer, cx);
        let hub = self.hub.read(cx);
        let built = match self
            .form
            .build(&offer, typed, hub.types(), hub.store_for_new_secrets())
        {
            Ok(b) => b,
            Err(e) => {
                self.error = Some(e.message);
                cx.notify();
                return;
            }
        };
        if let Err(e) = hub.validate(&built.connection) {
            self.error = Some(e);
            cx.notify();
            return;
        }
        self.error = None;
        self.test = TestState::Running;
        let super::form::BuiltConnection {
            connection,
            to_store,
            ..
        } = built;
        self.flow.update(cx, |flow, cx| {
            flow.start(connection, Purpose::Test, to_store, window, cx);
        });
        cx.notify();
    }

    /// Validates with the core's `validate_connection` and saves (secrets to the keyring or
    /// vault, connections.json on the runtime's blocking pool). Emits [`ConnectionSaved`].
    pub fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        self.sync(cx);
        let Some(offer) = self.offer(cx) else {
            return;
        };
        let typed = self.typed_secrets(&offer, cx);
        let hub = self.hub.clone();
        let built = {
            let h = hub.read(cx);
            self.form
                .build(&offer, typed, h.types(), h.store_for_new_secrets())
        };
        let built = match built {
            Ok(b) => b,
            Err(e) => {
                self.error = Some(e.message);
                cx.notify();
                return;
            }
        };
        let task = hub.update(cx, |hub, cx| hub.save(built, cx));
        self.saving = true;
        self.error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |dialog, window, cx| {
                dialog.saving = false;
                match result {
                    Ok(id) => {
                        dialog.clear_secret_inputs(window, cx);
                        cx.emit(ConnectionSaved(id));
                    }
                    Err(e) => dialog.error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn choice_row<T: Copy + PartialEq + 'static>(
        &self,
        id: &'static str,
        options: Vec<(T, SharedString, bool)>,
        selected: T,
        on_pick: fn(&mut Self, T, &mut Window, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) -> Div {
        h_flex()
            .gap_1()
            .flex_wrap()
            .children(
                options
                    .into_iter()
                    .enumerate()
                    .map(|(i, (value, label, enabled))| {
                        Button::new((id, i))
                            .small()
                            .outline()
                            .label(label)
                            .selected(value == selected)
                            .disabled(!enabled)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                on_pick(this, value, window, cx);
                                cx.notify();
                            }))
                    }),
            )
    }

    fn secret_block(
        &self,
        id: &'static str,
        mode: SecretMode,
        status: &'static str,
        input: &Entity<InputState>,
        on_pick: fn(&mut Self, SecretMode, &mut Window, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) -> Div {
        let store = self.hub.read(cx).store_for_new_secrets();
        let save_label: SharedString = match store {
            Some(k) => format!("Save in {k}").into(),
            None => "Save (no keyring or vault available)".into(),
        };
        v_flex()
            .gap_1()
            .child(self.choice_row(
                id,
                vec![
                    (SecretMode::Ask, "Ask every time".into(), true),
                    (SecretMode::Save, save_label, store.is_some()),
                ],
                mode,
                on_pick,
                cx,
            ))
            .child(
                h_flex()
                    .gap_2()
                    .when(mode == SecretMode::Save, |el| {
                        el.child(div().flex_1().child(secret_field(input)))
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!("{id}-status")))
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(status),
                    ),
            )
    }

    fn render_ssh(&self, offer: &EngineOffer, cx: &mut Context<Self>) -> Div {
        let muted = cx.theme().muted_foreground;
        let warn = cx.theme().warning;
        let last = self.form.hops.len().saturating_sub(1);
        let mut section = v_flex().gap_2().child(
            h_flex()
                .gap_2()
                .child(
                    Button::new("ssh-add-hop")
                        .small()
                        .label(if self.form.hops.is_empty() {
                            "Use an SSH tunnel"
                        } else {
                            "Add jump host"
                        })
                        .on_click(cx.listener(|this, _, window, cx| this.add_hop(window, cx))),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child("RSA key files are not supported: use an Ed25519 or ECDSA key, or ssh-agent (ADR-0016)."),
                ),
        );
        if offer.file_based {
            return v_flex();
        }
        for (i, (hop, inputs)) in self.form.hops.iter().zip(&self.hops).enumerate() {
            let title = if i == last {
                "SSH server".to_owned()
            } else {
                format!("Jump host {}", i + 1)
            };
            let policies = [
                HostKeyPolicy::Strict,
                HostKeyPolicy::AcceptNew,
                HostKeyPolicy::Disabled,
            ];
            let mut block = v_flex()
                .id(("ssh-hop", i))
                .gap_1()
                .p_2()
                .border_1()
                .border_color(cx.theme().border)
                .rounded_md()
                .child(
                    h_flex()
                        .justify_between()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(title),
                        )
                        .child(
                            Button::new(("ssh-remove", i))
                                .ghost()
                                .xsmall()
                                .label("Remove")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.remove_hop(i, window, cx)
                                })),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(&inputs.host)))
                        .child(div().w(px(80.)).child(Input::new(&inputs.port)))
                        .child(div().w(px(140.)).child(Input::new(&inputs.user))),
                )
                .child(
                    h_flex().gap_1().children(
                        [
                            SshAuthChoice::Agent,
                            SshAuthChoice::Password,
                            SshAuthChoice::KeyFile,
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(j, choice)| {
                            Button::new(SharedString::from(format!("ssh-auth-{i}-{j}")))
                                .small()
                                .outline()
                                .label(choice.label())
                                .selected(hop.auth == choice)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.set_hop_auth(i, choice, window, cx);
                                }))
                        }),
                    ),
                );
            if hop.auth == SshAuthChoice::KeyFile {
                block = block.child(Input::new(&inputs.key_path)).child(
                    Button::new(("ssh-key-pass", i))
                        .small()
                        .outline()
                        .label("Key has a passphrase")
                        .selected(hop.key_has_passphrase)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if let Some(h) = this.form.hops.get_mut(i) {
                                let on = !h.key_has_passphrase;
                                h.set_key_has_passphrase(on);
                            }
                            if let Some(inputs) = this.hops.get(i) {
                                secret_input::clear(&inputs.secret, window, cx);
                            }
                            cx.notify();
                        })),
                );
            }
            if hop.has_secret() {
                let store = self.hub.read(cx).store_for_new_secrets();
                block = block.child(
                    h_flex()
                        .gap_1()
                        .children(
                            [SecretMode::Ask, SecretMode::Save]
                                .into_iter()
                                .enumerate()
                                .map(|(j, mode)| {
                                    Button::new(SharedString::from(format!("ssh-secret-{i}-{j}")))
                                        .small()
                                        .outline()
                                        .label(match mode {
                                            SecretMode::Ask => "Ask every time",
                                            SecretMode::Save => "Save",
                                        })
                                        .selected(hop.secret.mode == mode)
                                        .disabled(mode == SecretMode::Save && store.is_none())
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            if let Some(h) = this.form.hops.get_mut(i) {
                                                h.secret.mode = mode;
                                            }
                                            if mode == SecretMode::Ask
                                                && let Some(inputs) = this.hops.get(i)
                                            {
                                                secret_input::clear(&inputs.secret, window, cx);
                                            }
                                            cx.notify();
                                        }))
                                }),
                        )
                        .when(hop.secret.mode == SecretMode::Save, |el| {
                            el.child(div().flex_1().child(secret_field(&inputs.secret)))
                        })
                        .child(div().text_xs().text_color(muted).child(hop.secret.status())),
                );
            }
            block = block
                .child(div().text_xs().text_color(muted).child("Host key policy"))
                .child(h_flex().gap_1().children(policies.into_iter().enumerate().map(
                    |(j, policy)| {
                        Button::new(SharedString::from(format!("ssh-policy-{i}-{j}")))
                            .small()
                            .outline()
                            .label(policy_label(policy))
                            .selected(hop.policy == policy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(h) = this.form.hops.get_mut(i) {
                                    h.policy = policy;
                                }
                                cx.notify();
                            }))
                    },
                )))
                .when(hop.policy == HostKeyPolicy::AcceptNew, |el| {
                    el.child(div().text_xs().text_color(warn).child(
                        "Accept new keys: the first connection to this host trusts whatever key it presents (a man in the middle at that moment would not be noticed). Known keys are still verified; a host known only under an RSA key still asks.",
                    ))
                })
                .when(hop.policy == HostKeyPolicy::Disabled, |el| {
                    el.child(div().text_xs().text_color(warn).child(
                        "Do not verify: anyone on the path can impersonate this SSH server and capture the tunnel and its credentials.",
                    ))
                });
            section = section.child(block);
        }
        section
    }
}

fn accepted_hint(offer: &EngineOffer) -> String {
    match &offer.accepted_properties {
        Some(keys) if !keys.is_empty() => format!("; accepted: {}", keys.join(", ")),
        Some(_) => "; it accepts none".to_owned(),
        None => String::new(),
    }
}

fn section_title(title: &'static str) -> Div {
    div()
        .pt_2()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .child(title)
}

fn field(label: &'static str, el: impl IntoElement) -> Div {
    v_flex()
        .gap_1()
        .child(div().text_xs().child(label))
        .child(el)
}

impl Render for ConnectionDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync(cx);
        let hub = self.hub.read(cx);
        let offers = hub.offers().to_vec();
        let types = hub.types().all().to_vec();
        let Some(offer) = self.offer(cx) else {
            return v_flex()
                .p_2()
                .child("No database driver is available in this build.")
                .into_any_element();
        };
        let (muted, success, danger_raw, warning_raw, background) = {
            let t = cx.theme();
            (
                t.muted_foreground,
                t.success,
                t.danger,
                t.warning,
                t.background,
            )
        };
        let notices = self.current_notices(cx);

        let engine_row = h_flex()
            .gap_1()
            .flex_wrap()
            .children(offers.iter().enumerate().map(|(i, o)| {
                let engine = o.engine.clone();
                Button::new(("engine", i))
                    .small()
                    .outline()
                    .label(o.name.clone())
                    .selected(o.engine == self.form.engine)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_engine(&engine, cx)))
            }));

        let endpoint = if offer.file_based {
            v_flex().child(field("Database file", Input::new(&self.file)))
        } else {
            let mode_row = self.choice_row(
                "endpoint-mode",
                vec![
                    (EndpointInput::Fields, "Host and port".into(), true),
                    (EndpointInput::Url, "URL".into(), true),
                ],
                self.form.endpoint_input,
                |this, v, _, _| this.form.endpoint_input = v,
                cx,
            );
            let port_label = match offer.default_port {
                Some(p) => format!("Port (default {p})"),
                None => "Port".to_owned(),
            };
            v_flex()
                .gap_2()
                .child(mode_row)
                .when(self.form.endpoint_input == EndpointInput::Url, |el| {
                    el.child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(Input::new(&self.url)))
                            .child(
                                Button::new("url-apply")
                                    .small()
                                    .label("Fill fields")
                                    .on_click(cx.listener(|this, _, window, cx| this.apply_url(window, cx))),
                            ),
                    )
                    .child(div().text_xs().text_color(muted).child(
                        "The URL only fills the fields and is not kept. Do not put a password in it.",
                    ))
                })
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().child(field("Host", Input::new(&self.host))))
                        .child(
                            div()
                                .w(px(140.))
                                .child(v_flex().gap_1().child(div().text_xs().child(port_label)).child(Input::new(&self.port))),
                        ),
                )
                .child(field("Database", Input::new(&self.database)))
        };

        let auth_choices: Vec<(AuthChoice, SharedString, bool)> = AuthChoice::offered(&offer.auth)
            .into_iter()
            .map(|c| (c, c.label().into(), true))
            .collect();
        let auth = v_flex()
            .gap_2()
            .when(auth_choices.len() > 1, |el| {
                el.child(self.choice_row(
                    "auth",
                    auth_choices,
                    self.form.auth,
                    |this, v, window, cx| this.set_auth(v, window, cx),
                    cx,
                ))
            })
            .when(
                matches!(
                    self.form.auth,
                    AuthChoice::UserOnly | AuthChoice::Password | AuthChoice::Token
                ),
                |el| el.child(field("User", Input::new(&self.username))),
            )
            .when(
                matches!(self.form.auth, AuthChoice::Password | AuthChoice::Token),
                |el| {
                    el.child(self.secret_block(
                        "auth-secret",
                        self.form.auth_secret.mode,
                        self.form.auth_secret.status(),
                        &self.secret,
                        |this, m, window, cx| {
                            this.form.auth_secret.mode = m;
                            if m == SecretMode::Ask {
                                secret_input::clear(&this.secret, window, cx);
                            }
                        },
                        cx,
                    ))
                },
            );

        let tls = if offer.file_based {
            None
        } else if !offer.offers_tls() {
            Some(
                v_flex().child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child("The driver of this engine does not offer TLS."),
                ),
            )
        } else {
            let modes: Vec<(TlsMode, SharedString, bool)> = offer
                .tls_modes
                .iter()
                .rev()
                .map(|m| (*m, tls_label(*m).into(), true))
                .collect();
            let needs_ca =
                self.form.tls_mode == TlsMode::VerifyCa && offer.verify_ca_requires_ca_file;
            Some(
                v_flex()
                    .gap_2()
                    .child(self.choice_row(
                        "tls",
                        modes,
                        self.form.tls_mode,
                        |this, v, _, _| this.form.tls_mode = v,
                        cx,
                    ))
                    .when(
                        matches!(self.form.tls_mode, TlsMode::VerifyCa | TlsMode::VerifyFull),
                        |el| {
                            el.child(field(
                                if needs_ca {
                                    "CA file (required: the only trusted authority)"
                                } else {
                                    "CA file (optional: replaces the system trust store)"
                                },
                                Input::new(&self.ca_file),
                            ))
                        },
                    )
                    .when(
                        offer.client_certificates && self.form.tls_mode != TlsMode::Disable,
                        |el| {
                            el.child(
                                h_flex()
                                    .gap_2()
                                    .child(div().flex_1().child(field(
                                        "Client certificate (optional)",
                                        Input::new(&self.client_cert),
                                    )))
                                    .child(
                                        div().flex_1().child(field(
                                            "Client key",
                                            Input::new(&self.client_key),
                                        )),
                                    ),
                            )
                            .when(offer.encrypted_client_keys, |el| {
                                el.child(
                                    Button::new("client-key-pass")
                                        .small()
                                        .outline()
                                        .label("Key has a passphrase")
                                        .selected(self.form.client_key_has_passphrase)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            let on = !this.form.client_key_has_passphrase;
                                            this.form.set_client_key_has_passphrase(on);
                                            secret_input::clear(
                                                &this.client_key_secret,
                                                window,
                                                cx,
                                            );
                                            cx.notify();
                                        })),
                                )
                                .when(
                                    self.form.client_key_has_passphrase,
                                    |el| {
                                        el.child(self.secret_block(
                                            "client-key-secret",
                                            self.form.client_key_secret.mode,
                                            self.form.client_key_secret.status(),
                                            &self.client_key_secret,
                                            |this, m, window, cx| {
                                                this.form.client_key_secret.mode = m;
                                                if m == SecretMode::Ask {
                                                    secret_input::clear(
                                                        &this.client_key_secret,
                                                        window,
                                                        cx,
                                                    );
                                                }
                                            },
                                            cx,
                                        ))
                                    },
                                )
                            })
                        },
                    ),
            )
        };

        let properties = if offer.offers_properties() {
            v_flex()
                .gap_1()
                .children(self.properties.iter().enumerate().map(|(i, p)| {
                    h_flex()
                        .id(("property", i))
                        .gap_2()
                        .child(div().w(px(200.)).child(Input::new(&p.key)))
                        .child(div().flex_1().child(Input::new(&p.value)))
                }))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("property-add")
                                .small()
                                .label("Add property")
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.add_property(window, cx)
                                    }),
                                ),
                        )
                        .child(div().text_xs().text_color(muted).child(
                            match &offer.accepted_properties {
                                Some(keys) => format!("Accepted: {}", keys.join(", ")),
                                None => "Any non-reserved key; secrets never go here.".to_owned(),
                            },
                        )),
                )
        } else {
            v_flex().child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("The driver of this engine accepts no driver properties."),
            )
        };

        let type_row = h_flex()
            .gap_1()
            .flex_wrap()
            .children(types.iter().enumerate().map(|(i, t)| {
                let id = t.id.clone();
                let color = hsla(t.color);
                let selected = self.form.connection_type.as_ref() == Some(&t.id);
                Button::new(("type", i))
                    .small()
                    .outline()
                    .selected(selected)
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                div()
                                    .size(px(10.))
                                    .rounded_full()
                                    .bg(color)
                                    .border_1()
                                    .border_color(text_on(color)),
                            )
                            .child(t.name.clone()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let types = this.hub.read(cx).types().clone();
                        this.form.set_connection_type(id.clone(), &types);
                        cx.notify();
                    }))
            }));
        let read_write_allowed = self.form.read_write_allowed(self.hub.read(cx).types());
        let read_only_row = v_flex()
            .gap_1()
            .child(self.choice_row(
                "read-only",
                vec![
                    (None, "Type default".into(), true),
                    (Some(true), "Read-only".into(), true),
                    (Some(false), "Read-write".into(), read_write_allowed),
                ],
                self.form.read_only,
                |this, v, _, _| this.form.read_only = v,
                cx,
            ))
            .when(!read_write_allowed, |el| {
                el.child(div().id("read-write-hint").text_xs().text_color(muted).child(
                    "Read-write is not available for a production type: its read-only setting comes from the type.",
                ))
            });
        let init_sql = self.form.existing.as_ref().map_or(0, |c| c.init_sql.len());

        let test_status: Option<(SharedString, Hsla)> = match &self.test {
            TestState::Idle => None,
            TestState::Running => Some(("Testing…".into(), muted)),
            TestState::Done(FlowResult::TestOk {
                server_version,
                warnings,
            }) => Some((
                format!(
                    "Connected{}{}",
                    server_version
                        .as_ref()
                        .map(|v| format!(" to {v}"))
                        .unwrap_or_default(),
                    if warnings.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " ({} security warnings: {})",
                            warnings.len(),
                            warnings.join(" ")
                        )
                    }
                )
                .into(),
                success,
            )),
            TestState::Done(FlowResult::Failed(e)) => {
                Some((format!("Failed: {e}").into(), danger_raw))
            }
            TestState::Done(FlowResult::Cancelled) => Some(("Cancelled".into(), muted)),
            TestState::Done(FlowResult::Opened { .. }) => None,
        };
        let has_error = notices.iter().any(|n| n.severity == Severity::Error);
        let danger = crate::appearance::ensure_contrast(
            danger_raw,
            &[background],
            crate::appearance::MIN_TEXT_CONTRAST,
        );
        let warning = crate::appearance::ensure_contrast(
            warning_raw,
            &[background],
            crate::appearance::MIN_TEXT_CONTRAST,
        );

        let body = v_flex()
            .id("connection-dialog-body")
            .gap_2()
            .max_h(px(560.))
            .overflow_y_scroll()
            .pr_2()
            .child(field("Name", Input::new(&self.name)))
            .child(section_title("Engine"))
            .child(engine_row)
            .child(section_title("Endpoint"))
            .child(endpoint)
            .child(section_title("Authentication"))
            .child(auth)
            .when_some(tls, |el, tls| el.child(section_title("TLS")).child(tls))
            .when(!offer.file_based, |el| el.child(section_title("SSH tunnel")).child(self.render_ssh(&offer, cx)))
            .child(section_title("Driver properties"))
            .child(properties)
            .child(section_title("Connection type"))
            .child(type_row)
            .child(read_only_row)
            .child(field("Folder", Input::new(&self.folder)))
            .when(init_sql > 0, |el| {
                el.child(div().text_xs().text_color(muted).child(format!(
                    "{init_sql} init SQL statement(s), kept as imported; they are confirmed at connect when the type asks for it."
                )))
            })
            .children(notices.iter().enumerate().map(|(i, n)| {
                let color = match n.severity {
                    Severity::Error => danger,
                    Severity::Warning => warning,
                    Severity::Info => muted,
                };
                div().id(("notice", i)).text_xs().text_color(color).child(n.text.clone())
            }))
            .when_some(self.error.clone(), |el, e| {
                el.child(div().id("dialog-error").text_sm().text_color(danger).child(e))
            });

        let footer = h_flex()
            .gap_2()
            .justify_between()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("connection-test")
                            .label("Test connection")
                            .disabled(self.test == TestState::Running || has_error)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.test_connection(window, cx)),
                            ),
                    )
                    .when_some(test_status, |el, (status, color)| {
                        el.child(
                            div()
                                .id("test-status")
                                .text_xs()
                                .text_color(color)
                                .child(status),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("connection-cancel")
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, window, cx| this.cancel(window, cx))),
                    )
                    .child(
                        Button::new("connection-save")
                            .primary()
                            .label("Save")
                            .disabled(self.saving || has_error)
                            .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                    ),
            );
        v_flex()
            .key_context("ConnectionDialog")
            .gap_2()
            .child(body)
            .child(footer)
            .into_any_element()
    }
}

/// Default form for a new connection: the first offered engine and no type chosen yet
/// (ADR-0007: the type is asked explicitly).
#[must_use]
pub fn new_form(hub: &ConnectionHub) -> Option<ConnectionForm> {
    let offer = hub.offers().first()?;
    let mut form = ConnectionForm::new(&offer.engine);
    form.adapt_to(offer);
    Some(form)
}

/// Used by tests and the palette: whether a type id is the production one.
#[must_use]
pub fn is_production_type(hub: &ConnectionHub, id: &ConnectionTypeId) -> bool {
    hub.types().resolve(id).is_production()
}
