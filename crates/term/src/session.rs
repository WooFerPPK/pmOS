//! Display-protocol session for the `/usr/bin/term` app.
//!
//! A [`Session`] owns a [`toolkit::Client`] plus an embedded
//! [`Terminal`] and walks the client-side boot sequence every
//! terminal app needs:
//!
//!   1. `display.get_registry` — installs a registry object on
//!      the client side.
//!   2. Pumps `registry.global` events, records them in
//!      `known_globals`, and auto-binds the interfaces a
//!      terminal app cares about (compositor, shm). Unlike
//!      [`shell::Session`], a term session does NOT touch
//!      `pmd_shell_manager` — that's the desktop shell's
//!      privileged interface and requires `Cap::Shell`, which
//!      ordinary apps don't hold.
//!   3. `compositor.create_surface` — carved by the caller via
//!      [`Session::create_surface`] once the compositor is
//!      bound. In v1 a terminal is a single-window app, so
//!      the session tracks exactly one surface id.
//!   4. `surface.commit` — sent by the caller via
//!      [`Session::commit`] after each render pass. No pixel
//!      attach path yet; that lands when SHM buffer allocation
//!      is wired.
//!
//! Same transport-agnostic pattern as `shell::Session`: the
//! session takes a `toolkit::Connection`, tests use
//! `MemoryConnection`, and production will use a socket-fd
//! wrapper once the kernel's `display_connect` extension
//! syscall is bridged to userland.

use std::collections::{BTreeMap, HashMap};

use display_proto::{Interface, ObjectId, RegistryGlobal, RegistryGlobalRemove};
use toolkit::protocol::{Client, ClientError, Connection};

use crate::terminal::{Key, KeyFeedResult, Terminal};

/// Interfaces every terminal app wants bound as soon as the
/// display server advertises them. Ordered so the
/// deterministic `SessionStep::bound` output is predictable
/// for tests.
pub const INTERESTING_INTERFACES: &[Interface] =
    &[Interface::Compositor, Interface::Shm];

/// One registry global the server advertised. Mirrors
/// `shell::session::GlobalEntry` — kept as a local type so the
/// `term` crate stays decoupled from the `shell` crate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobalEntry {
    pub name: u32,
    pub interface: Option<Interface>,
    pub interface_name: String,
    pub version: u32,
    pub live: bool,
    pub bound_id: Option<ObjectId>,
}

/// One side-effect of calling [`Session::pump`].
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SessionStep {
    /// Globals discovered in this pump (added to
    /// `known_globals`).
    pub discovered: Vec<u32>,
    /// Globals bound in this pump (new_id allocated and
    /// `registry.bind` emitted on the wire).
    pub bound: Vec<Interface>,
    /// Globals removed in this pump.
    pub removed: Vec<u32>,
    /// Non-fatal protocol errors observed
    /// (`pmd_display.error` events).
    pub errors: Vec<ProtocolErrorNotice>,
}

impl SessionStep {
    pub fn is_empty(&self) -> bool {
        self.discovered.is_empty()
            && self.bound.is_empty()
            && self.removed.is_empty()
            && self.errors.is_empty()
    }
}

/// One `pmd_display.error` event as surfaced to the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolErrorNotice {
    pub object_id: ObjectId,
    pub code: u32,
    pub message: String,
}

/// Errors surfaced by [`Session`] methods.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionError {
    /// Forwarded from the underlying `toolkit::Client`.
    Client(ClientError),
    /// A registry.global / registry.global_remove /
    /// display.error event failed to decode.
    MalformedEvent(String),
    /// `start` was called twice.
    AlreadyStarted,
    /// A method that requires the registry was invoked
    /// before `start`.
    NotStarted,
    /// `create_surface` was called before the compositor
    /// global was bound.
    CompositorNotBound,
    /// `create_surface` was called a second time. v1
    /// terminals are single-surface apps.
    SurfaceAlreadyCreated,
    /// `commit` was called before `create_surface`.
    NoSurface,
    /// A buffer-pipeline method (`create_pool`, `attach`,
    /// ...) was called before the shm global was bound.
    ShmNotBound,
    /// `attach` was called with a buffer id the session's
    /// client does not know about.
    UnknownBuffer(ObjectId),
}

impl From<ClientError> for SessionError {
    fn from(e: ClientError) -> Self {
        SessionError::Client(e)
    }
}

/// The term app's display-protocol session plus embedded
/// terminal state. A caller owns one `Session` per open
/// terminal window.
pub struct Session<C: Connection> {
    client: Client<C>,
    terminal: Terminal,
    /// The registry id allocated by `start`. `None` before
    /// `start`.
    registry_id: Option<ObjectId>,
    /// Every registry global the server has advertised,
    /// keyed by name.
    known_globals: BTreeMap<u32, GlobalEntry>,
    /// Auto-bound object ids indexed by interface. HashMap
    /// because `Interface` derives `Hash` but not `Ord`.
    bound_by_interface: HashMap<Interface, ObjectId>,
    /// The surface id allocated by `create_surface`, if any.
    surface_id: Option<ObjectId>,
}

impl<C: Connection> Session<C> {
    /// Build a session wrapping `conn` and taking ownership
    /// of `terminal`. The terminal's banner is left intact
    /// so the caller can pre-seed scrollback before the
    /// display-protocol handshake.
    pub fn new(conn: C, terminal: Terminal) -> Self {
        Session {
            client: Client::new(conn),
            terminal,
            registry_id: None,
            known_globals: BTreeMap::new(),
            bound_by_interface: HashMap::new(),
            surface_id: None,
        }
    }

    /// Send `display.get_registry`, installing the
    /// client-side registry object and queueing the request
    /// bytes onto the connection.
    pub fn start(&mut self) -> Result<(), SessionError> {
        if self.registry_id.is_some() {
            return Err(SessionError::AlreadyStarted);
        }
        let id = self.client.get_registry()?;
        self.registry_id = Some(id);
        Ok(())
    }

    /// Borrow the embedded terminal.
    pub fn terminal(&self) -> &Terminal {
        &self.terminal
    }

    /// Mutable access to the embedded terminal.
    pub fn terminal_mut(&mut self) -> &mut Terminal {
        &mut self.terminal
    }

    /// Borrow the underlying `toolkit::Client` — used by
    /// tests to drain outbound bytes and introspect the
    /// object table.
    pub fn client(&self) -> &Client<C> {
        &self.client
    }

    pub fn client_mut(&mut self) -> &mut Client<C> {
        &mut self.client
    }

    /// Drain any outbound bytes the session has queued.
    pub fn drain_outbound(&mut self) -> Vec<u8> {
        self.client.drain_outbound()
    }

    /// Client-side registry id, if `start` has been called.
    pub fn registry_id(&self) -> Option<ObjectId> {
        self.registry_id
    }

    /// Surface id, if `create_surface` has been called.
    pub fn surface_id(&self) -> Option<ObjectId> {
        self.surface_id
    }

    /// True iff every interface in [`INTERESTING_INTERFACES`]
    /// has been bound AND the session has allocated a
    /// surface. The v1 term is single-surface and treats
    /// both facts as "ready to commit".
    pub fn is_ready(&self) -> bool {
        self.surface_id.is_some()
            && INTERESTING_INTERFACES
                .iter()
                .all(|iface| self.bound_by_interface.contains_key(iface))
    }

    /// True iff every interesting interface has been bound,
    /// regardless of whether a surface has been created yet.
    /// Used by callers that want to drive
    /// [`Session::create_surface`] themselves.
    pub fn interfaces_ready(&self) -> bool {
        INTERESTING_INTERFACES
            .iter()
            .all(|iface| self.bound_by_interface.contains_key(iface))
    }

    /// Lookup the bound object id for an interface.
    pub fn bound(&self, interface: Interface) -> Option<ObjectId> {
        self.bound_by_interface.get(&interface).copied()
    }

    /// Read-only access to the known-globals table.
    pub fn known_globals(&self) -> &BTreeMap<u32, GlobalEntry> {
        &self.known_globals
    }

    /// Send `compositor.create_surface`. Requires that the
    /// compositor global has been auto-bound (which happens
    /// automatically when the server advertises it via
    /// `registry.global`). Returns the allocated surface id.
    ///
    /// The v1 term is a single-surface app, so a second call
    /// returns [`SessionError::SurfaceAlreadyCreated`].
    pub fn create_surface(&mut self) -> Result<ObjectId, SessionError> {
        if self.surface_id.is_some() {
            return Err(SessionError::SurfaceAlreadyCreated);
        }
        let compositor_id = self
            .bound(Interface::Compositor)
            .ok_or(SessionError::CompositorNotBound)?;
        let surface_id = self.client.compositor_create_surface(compositor_id)?;
        self.surface_id = Some(surface_id);
        Ok(surface_id)
    }

    /// Send `surface.commit` on the session's surface. If
    /// [`Session::attach`] was called earlier in the frame,
    /// the commit promotes the attached buffer to the
    /// surface's current content; otherwise the commit is a
    /// no-op tick (still valid protocol).
    pub fn commit(&mut self) -> Result<(), SessionError> {
        let surface_id = self.surface_id.ok_or(SessionError::NoSurface)?;
        self.client.surface_commit(surface_id)?;
        Ok(())
    }

    /// Send `shm.create_pool(size)`. Requires the shm
    /// global to be bound. Returns the allocated pool id
    /// so the caller can hand it to
    /// [`Session::create_buffer`].
    pub fn create_pool(&mut self, size: u32) -> Result<ObjectId, SessionError> {
        let shm_id = self.bound(Interface::Shm).ok_or(SessionError::ShmNotBound)?;
        let pool_id = self.client.shm_create_pool(shm_id, size)?;
        Ok(pool_id)
    }

    /// Send `shm_pool.create_buffer(offset, width, height,
    /// stride, format)` against an already-allocated pool.
    /// Returns the allocated buffer id.
    ///
    /// `format` is one of the `display_proto::buffer_format`
    /// constants — `ARGB8888` or `XRGB8888` in v1.
    pub fn create_buffer(
        &mut self,
        pool_id: ObjectId,
        offset: u32,
        width: u32,
        height: u32,
        stride: u32,
        format: u32,
    ) -> Result<ObjectId, SessionError> {
        let buffer_id = self
            .client
            .shm_pool_create_buffer(pool_id, offset, width, height, stride, format)?;
        Ok(buffer_id)
    }

    /// Send `surface.attach(buffer_id, x, y)`. Requires
    /// that the surface has been created and that
    /// `buffer_id` is in the session's object table.
    pub fn attach(
        &mut self,
        buffer_id: ObjectId,
        x: i32,
        y: i32,
    ) -> Result<(), SessionError> {
        let surface_id = self.surface_id.ok_or(SessionError::NoSurface)?;
        if self.client.get(buffer_id) != Some(Interface::Buffer) {
            return Err(SessionError::UnknownBuffer(buffer_id));
        }
        self.client.surface_attach(surface_id, buffer_id, x, y)?;
        Ok(())
    }

    /// Send `surface.damage(x, y, width, height)`.
    pub fn damage(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<(), SessionError> {
        let surface_id = self.surface_id.ok_or(SessionError::NoSurface)?;
        self.client.surface_damage(surface_id, x, y, width, height)?;
        Ok(())
    }

    /// Convenience wrapper: attach the given buffer at
    /// origin, mark the entire buffer region as damaged,
    /// and commit the surface. Equivalent to calling
    /// [`Session::attach`] then [`Session::damage`] then
    /// [`Session::commit`] with matching arguments — just
    /// less typing at the call site and guaranteed to
    /// short-circuit on the first error.
    pub fn present(
        &mut self,
        buffer_id: ObjectId,
        width: u32,
        height: u32,
    ) -> Result<(), SessionError> {
        self.attach(buffer_id, 0, 0)?;
        self.damage(0, 0, width as i32, height as i32)?;
        self.commit()?;
        Ok(())
    }

    /// Feed a key into the embedded terminal. Returns the
    /// [`KeyFeedResult`] unchanged so callers can inspect
    /// committed lines or repaint.
    ///
    /// Future slice: when the display server emits
    /// `pmd_input.key_press` events on the session's
    /// surface, `pump` will route them through this method
    /// automatically and the caller won't need to call
    /// `feed_key` directly.
    pub fn feed_key(&mut self, key: Key) -> KeyFeedResult {
        self.terminal.feed_key(key)
    }

    /// Feed a chunk of incoming bytes from the display
    /// server. Returns a [`SessionStep`] summarising the
    /// events reacted to, plus how many bytes of `input`
    /// were consumed.
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
                        let bound_id = self
                            .client
                            .registry_bind(registry_id, parsed.name, i, parsed.version)?;
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
                // On a cap-rejected bind the object_id on
                // the error matches one of our
                // optimistically-bound interfaces. Drop the
                // stale binding so `is_ready` reflects the
                // server's view.
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
            // Every other (interface, opcode) pair is
            // accepted but ignored in the v1 skeleton.
            _ => Ok(()),
        }
    }

    fn interface_for_id(&self, id: ObjectId) -> Option<Interface> {
        for (iface, bound_id) in &self.bound_by_interface {
            if *bound_id == id {
                return Some(*iface);
            }
        }
        None
    }
}
