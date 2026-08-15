//! Desktop-shell protocol session state machine.
//!
//! A [`Session`] owns a [`toolkit::Client`] and walks the
//! client-side boot sequence the desktop shell needs:
//!
//!   1. `display.get_registry` — installs a registry
//!      object on the client side.
//!   2. Receives `registry.global(name, interface,
//!      version)` events from the server, records them in
//!      `known_globals`, and auto-binds the interfaces the
//!      shell cares about (compositor, shm, ...).
//!   3. Flags readiness once the required globals are all
//!      bound so higher layers can start drawing a
//!      wallpaper or launching apps.
//!
//! The session is transport-agnostic — it takes a
//! `toolkit::Connection`, which means tests use
//! `MemoryConnection` (a `Vec<u8>` queue) and production
//! uses a socket-fd wrapper (a follow-up once the
//! kernel's `display_connect` extension syscall is
//! bridged to userland via WASI).

use std::collections::{BTreeMap, HashMap};

use display_proto::{
    Interface, ObjectId, RegistryGlobal, RegistryGlobalRemove, ShellWindowCreated,
    ShellWindowDestroyed, ShellWindowFocused, ShellWindowTitleChanged,
};
use toolkit::protocol::{Client, ClientError, Connection};

/// Interfaces the shell wants bound as soon as the display
/// server advertises them. Ordered so the deterministic
/// `SessionStep::GlobalsBound` output is predictable for
/// tests.
///
/// `ShellManager` is in this set because the desktop shell
/// needs the window-list / focus / close API as soon as
/// the server advertises it. Ordinary apps with only the
/// `DisplayClient` cap don't get to bind it — the server's
/// bind dispatch enforces the capability check.
pub const INTERESTING_INTERFACES: &[Interface] = &[
    Interface::Compositor,
    Interface::Shm,
    Interface::ShellManager,
];

/// One window the shell knows about. Populated by
/// `pmd_shell_manager.window_*` events after
/// [`Session::subscribe_windows`] has been called.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    pub window_id: u32,
    pub title: String,
    pub app_id: String,
    pub focused: bool,
}

/// One registry global the server advertised. Either
/// still-announced (`live == true`) or subsequently
/// removed (`live == false`). The shell keeps removed
/// entries around so `lookup` can distinguish "never
/// seen" from "removed".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobalEntry {
    /// The server-chosen u32 name.
    pub name: u32,
    /// Decoded interface, if the wire string maps to one
    /// we understand via [`Interface::from_name`].
    pub interface: Option<Interface>,
    /// Raw interface name as the server sent it. Kept so
    /// diagnostics and future extension tables can see
    /// unknown interfaces.
    pub interface_name: String,
    /// Protocol version the server advertised.
    pub version: u32,
    /// Cleared to false when a subsequent `global_remove`
    /// event references this name.
    pub live: bool,
    /// Set once the shell sends `registry.bind` for this
    /// global. Carries the client-side new_id the bind
    /// used.
    pub bound_id: Option<ObjectId>,
}

/// One side-effect of calling [`Session::pump`]. Returned
/// so callers can log or assert on what changed without
/// re-reading internal state.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SessionStep {
    /// Globals discovered in this pump (added to
    /// `known_globals`).
    pub discovered: Vec<u32>,
    /// Globals bound in this pump (new_id allocated +
    /// `registry.bind` emitted on the wire).
    pub bound: Vec<Interface>,
    /// Globals removed in this pump (`registry.global_remove`
    /// received).
    pub removed: Vec<u32>,
    /// Non-fatal protocol errors observed
    /// (`pmd_display.error` events).
    pub errors: Vec<ProtocolErrorNotice>,
    /// Windows the shell discovered during this pump
    /// (from `pmd_shell_manager.window_created`).
    pub windows_created: Vec<u32>,
    /// Windows removed during this pump (from
    /// `window_destroyed`).
    pub windows_destroyed: Vec<u32>,
    /// New focus target during this pump, if any
    /// (from `window_focused`).
    pub focus_changed_to: Option<u32>,
    /// Windows whose title changed during this pump.
    pub title_changes: Vec<u32>,
}

impl SessionStep {
    pub fn is_empty(&self) -> bool {
        self.discovered.is_empty()
            && self.bound.is_empty()
            && self.removed.is_empty()
            && self.errors.is_empty()
            && self.windows_created.is_empty()
            && self.windows_destroyed.is_empty()
            && self.focus_changed_to.is_none()
            && self.title_changes.is_empty()
    }
}

/// One `pmd_display.error` event as surfaced to the
/// caller. The raw `message` string is kept as-is — the
/// shell decides whether to log, display, or ignore it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolErrorNotice {
    pub object_id: ObjectId,
    pub code: u32,
    pub message: String,
}

/// Errors surfaced by [`Session`] methods. Most are thin
/// wrappers over the underlying [`ClientError`] + decoder
/// failures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionError {
    /// Forwarded from the underlying `toolkit::Client`.
    Client(ClientError),
    /// A registry.global or registry.global_remove event
    /// did not decode cleanly.
    MalformedEvent(String),
    /// `start` was called more than once — the shell has
    /// only one registry.
    AlreadyStarted,
    /// `start` has not been called yet and a method that
    /// requires the registry was invoked.
    NotStarted,
    /// A `pmd_shell_manager.*` request was attempted but
    /// the shell-manager global has not been bound — the
    /// server hasn't advertised it yet, or the shell
    /// doesn't hold the `Shell` capability.
    ShellManagerNotBound,
}

impl From<ClientError> for SessionError {
    fn from(e: ClientError) -> Self {
        SessionError::Client(e)
    }
}

pub struct Session<C: Connection> {
    client: Client<C>,
    /// The registry object id once `start` has sent
    /// display.get_registry. `None` before start.
    registry_id: Option<ObjectId>,
    /// Every registry global the server has advertised,
    /// keyed by name. Removed entries stay in the map
    /// with `live == false`.
    known_globals: BTreeMap<u32, GlobalEntry>,
    /// Bound objects indexed by interface, for quick
    /// lookup from higher layers. `HashMap` because
    /// [`Interface`] derives `Hash` but not `Ord`.
    bound_by_interface: HashMap<Interface, ObjectId>,
    /// Set to true once the shell has sent
    /// `shell_manager.subscribe_windows`. Until then the
    /// server won't emit window_* events to this client.
    windows_subscribed: bool,
    /// Currently-known windows, keyed by window_id. The
    /// shell populates this from window_created /
    /// window_destroyed / window_focused /
    /// window_title_changed events.
    windows: BTreeMap<u32, WindowInfo>,
    /// The window_id of the currently-focused window, if
    /// any.
    focused_window: Option<u32>,
}

impl<C: Connection> Session<C> {
    /// Construct a session over `conn`. The caller ships
    /// the returned session bytes via its chosen transport
    /// (a real socket fd, an in-memory loopback, etc.).
    pub fn new(conn: C) -> Self {
        Session {
            client: Client::new(conn),
            registry_id: None,
            known_globals: BTreeMap::new(),
            bound_by_interface: HashMap::new(),
            windows_subscribed: false,
            windows: BTreeMap::new(),
            focused_window: None,
        }
    }

    /// Send `display.get_registry`, installing the
    /// client-side registry object and queueing the
    /// request bytes onto the connection.
    pub fn start(&mut self) -> Result<(), SessionError> {
        if self.registry_id.is_some() {
            return Err(SessionError::AlreadyStarted);
        }
        let id = self.client.get_registry()?;
        self.registry_id = Some(id);
        Ok(())
    }

    /// Borrow the underlying `toolkit::Client`. Exposed so
    /// higher layers and tests can introspect the
    /// connection, drain outbound bytes, or install
    /// additional objects.
    pub fn client(&self) -> &Client<C> {
        &self.client
    }

    pub fn client_mut(&mut self) -> &mut Client<C> {
        &mut self.client
    }

    /// Drain any outbound bytes the session has queued.
    /// Equivalent to `self.client_mut().drain_outbound()`;
    /// exposed here as a convenience.
    pub fn drain_outbound(&mut self) -> Vec<u8> {
        self.client.drain_outbound()
    }

    /// The client-side registry id, if `start` has been
    /// called.
    pub fn registry_id(&self) -> Option<ObjectId> {
        self.registry_id
    }

    /// True iff every interface in [`INTERESTING_INTERFACES`]
    /// has a bound object.
    pub fn is_ready(&self) -> bool {
        INTERESTING_INTERFACES
            .iter()
            .all(|iface| self.bound_by_interface.contains_key(iface))
    }

    /// Lookup the bound object id for an interface.
    pub fn bound(&self, interface: Interface) -> Option<ObjectId> {
        self.bound_by_interface.get(&interface).copied()
    }

    /// Reverse lookup: given an object id, which bound
    /// interface holds it (if any)? Used by the
    /// display.error handler to drop stale bindings when
    /// the server rejects a bind.
    fn interface_for_id(&self, id: ObjectId) -> Option<Interface> {
        for (iface, bound_id) in &self.bound_by_interface {
            if *bound_id == id {
                return Some(*iface);
            }
        }
        None
    }

    /// Read-only access to the known-globals table.
    pub fn known_globals(&self) -> &BTreeMap<u32, GlobalEntry> {
        &self.known_globals
    }

    /// Read-only access to the known-windows table.
    /// Populated once `subscribe_windows` has been called
    /// and the server has emitted `window_created` events.
    pub fn windows(&self) -> &BTreeMap<u32, WindowInfo> {
        &self.windows
    }

    /// Currently-focused window id, if any.
    pub fn focused_window(&self) -> Option<u32> {
        self.focused_window
    }

    /// True iff the shell has sent
    /// `shell_manager.subscribe_windows`.
    pub fn windows_subscribed(&self) -> bool {
        self.windows_subscribed
    }

    /// Send `pmd_shell_manager.subscribe_windows()`. The
    /// shell-manager global must already be bound (this
    /// happens automatically once the server advertises it
    /// via registry.global). Returns
    /// `Err(SessionError::ShellManagerNotBound)` otherwise.
    ///
    /// The server starts emitting `window_*` events on the
    /// shell-manager object after this. Subsequent calls
    /// are idempotent — the second call returns Ok without
    /// re-sending.
    pub fn subscribe_windows(&mut self) -> Result<(), SessionError> {
        if self.windows_subscribed {
            return Ok(());
        }
        let shell_manager_id = self
            .bound(Interface::ShellManager)
            .ok_or(SessionError::ShellManagerNotBound)?;
        // No payload — opcode 1 is subscribe_windows.
        self.client.send_request(shell_manager_id, 1, &[])?;
        self.windows_subscribed = true;
        Ok(())
    }

    /// Send `pmd_shell_manager.focus_window(window_id)`.
    /// Errors if the shell-manager global isn't bound.
    /// Does NOT pre-validate that `window_id` is in the
    /// known-windows table — the server is the authority.
    pub fn focus_window(&mut self, window_id: u32) -> Result<(), SessionError> {
        let shell_manager_id = self
            .bound(Interface::ShellManager)
            .ok_or(SessionError::ShellManagerNotBound)?;
        let payload = window_id.to_le_bytes();
        self.client.send_request(shell_manager_id, 2, &payload)?;
        Ok(())
    }

    /// Send `pmd_shell_manager.close_window(window_id)`.
    pub fn close_window(&mut self, window_id: u32) -> Result<(), SessionError> {
        let shell_manager_id = self
            .bound(Interface::ShellManager)
            .ok_or(SessionError::ShellManagerNotBound)?;
        let payload = window_id.to_le_bytes();
        self.client.send_request(shell_manager_id, 3, &payload)?;
        Ok(())
    }

    /// Send `pmd_shell_manager.minimize_window(window_id)`.
    pub fn minimize_window(&mut self, window_id: u32) -> Result<(), SessionError> {
        let shell_manager_id = self
            .bound(Interface::ShellManager)
            .ok_or(SessionError::ShellManagerNotBound)?;
        let payload = window_id.to_le_bytes();
        self.client.send_request(shell_manager_id, 4, &payload)?;
        Ok(())
    }

    /// Send `pmd_shell_manager.toggle_maximized_window(window_id)`.
    pub fn toggle_maximized_window(&mut self, window_id: u32) -> Result<(), SessionError> {
        let shell_manager_id = self
            .bound(Interface::ShellManager)
            .ok_or(SessionError::ShellManagerNotBound)?;
        self.client
            .send_request(shell_manager_id, 7, &window_id.to_le_bytes())?;
        Ok(())
    }

    /// Feed a chunk of incoming bytes from the display
    /// server. Returns a [`SessionStep`] summarising the
    /// events the session reacted to — plus a count of
    /// bytes consumed. The caller drops `consumed` bytes
    /// from its buffer and calls `pump` again when more
    /// arrive.
    pub fn pump(&mut self, input: &[u8]) -> Result<(SessionStep, usize), SessionError> {
        let registry_id = self.registry_id.ok_or(SessionError::NotStarted)?;
        let (events, consumed) = self.client.push_received_with_payload(input)?;

        let mut step = SessionStep::default();
        for event in events {
            self.handle_event(registry_id, &event, &mut step)?;
        }
        Ok((step, consumed))
    }

    fn handle_event(
        &mut self,
        registry_id: ObjectId,
        event: &toolkit::protocol::ClientEventWithPayload,
        step: &mut SessionStep,
    ) -> Result<(), SessionError> {
        match (event.interface, event.opcode) {
            (Interface::Registry, 1 /* global */) => {
                if event.object_id != registry_id {
                    // Stray global on a non-registry object.
                    // The protocol allows servers to emit
                    // events on any object the client has
                    // bound, but a global is registry-only.
                    return Ok(());
                }
                let parsed = RegistryGlobal::decode(&event.payload)
                    .map_err(|e| SessionError::MalformedEvent(format!("{e:?}")))?;
                let interface = Interface::from_name(&parsed.interface);
                let entry = GlobalEntry {
                    name: parsed.name,
                    interface,
                    interface_name: parsed.interface,
                    version: parsed.version,
                    live: true,
                    bound_id: None,
                };
                self.known_globals.insert(entry.name, entry);
                step.discovered.push(parsed.name);
                if let Some(i) = interface {
                    if INTERESTING_INTERFACES.contains(&i)
                        && !self.bound_by_interface.contains_key(&i)
                    {
                        let bound_id = self.client.registry_bind(
                            registry_id,
                            parsed.name,
                            i,
                            parsed.version,
                        )?;
                        self.bound_by_interface.insert(i, bound_id);
                        step.bound.push(i);
                        if let Some(mut_entry) = self.known_globals.get_mut(&parsed.name) {
                            mut_entry.bound_id = Some(bound_id);
                        }
                    }
                }
                Ok(())
            }
            (Interface::Registry, 2 /* global_remove */) => {
                if event.object_id != registry_id {
                    return Ok(());
                }
                let parsed = RegistryGlobalRemove::decode(&event.payload)
                    .map_err(|e| SessionError::MalformedEvent(format!("{e:?}")))?;
                if let Some(entry) = self.known_globals.get_mut(&parsed.name) {
                    entry.live = false;
                }
                step.removed.push(parsed.name);
                Ok(())
            }
            (Interface::Display, 1 /* error */) => {
                let parsed = display_proto::DisplayError::decode(&event.payload)
                    .map_err(|e| SessionError::MalformedEvent(format!("{e:?}")))?;
                // If the error's object_id matches an
                // interface we thought we'd bound (because
                // the auto-bind happens on registry.global
                // BEFORE the server's reply), drop the
                // stale binding. The known_globals entry
                // is also marked unbound. This is how the
                // shell observes a server-side cap-gate
                // rejection.
                if let Some(iface) = self.interface_for_id(parsed.object_id) {
                    self.bound_by_interface.remove(&iface);
                    for entry in self.known_globals.values_mut() {
                        if entry.bound_id == Some(parsed.object_id) {
                            entry.bound_id = None;
                        }
                    }
                }
                step.errors.push(ProtocolErrorNotice {
                    object_id: parsed.object_id,
                    code: parsed.code,
                    message: parsed.message,
                });
                Ok(())
            }
            (Interface::ShellManager, 1 /* window_created */) => {
                let parsed = ShellWindowCreated::decode(&event.payload)
                    .map_err(|e| SessionError::MalformedEvent(format!("{e:?}")))?;
                self.windows.insert(
                    parsed.window_id,
                    WindowInfo {
                        window_id: parsed.window_id,
                        title: parsed.title,
                        app_id: parsed.app_id,
                        focused: false,
                    },
                );
                step.windows_created.push(parsed.window_id);
                Ok(())
            }
            (Interface::ShellManager, 2 /* window_destroyed */) => {
                let parsed = ShellWindowDestroyed::decode(&event.payload)
                    .map_err(|e| SessionError::MalformedEvent(format!("{e:?}")))?;
                self.windows.remove(&parsed.window_id);
                if self.focused_window == Some(parsed.window_id) {
                    self.focused_window = None;
                }
                step.windows_destroyed.push(parsed.window_id);
                Ok(())
            }
            (Interface::ShellManager, 3 /* window_focused */) => {
                let parsed = ShellWindowFocused::decode(&event.payload)
                    .map_err(|e| SessionError::MalformedEvent(format!("{e:?}")))?;
                // Clear the focused flag on the previous
                // focused window, set it on the new one.
                if let Some(prev) = self.focused_window {
                    if let Some(w) = self.windows.get_mut(&prev) {
                        w.focused = false;
                    }
                }
                if let Some(w) = self.windows.get_mut(&parsed.window_id) {
                    w.focused = true;
                }
                self.focused_window = Some(parsed.window_id);
                step.focus_changed_to = Some(parsed.window_id);
                Ok(())
            }
            (Interface::ShellManager, 4 /* window_title_changed */) => {
                let parsed = ShellWindowTitleChanged::decode(&event.payload)
                    .map_err(|e| SessionError::MalformedEvent(format!("{e:?}")))?;
                if let Some(w) = self.windows.get_mut(&parsed.window_id) {
                    w.title = parsed.new_title;
                    step.title_changes.push(parsed.window_id);
                }
                Ok(())
            }
            // Every other (interface, opcode) pair is
            // accepted but ignored in the v1 skeleton.
            // Surface/xdg events land when that extension
            // set is wired.
            _ => Ok(()),
        }
    }
}
