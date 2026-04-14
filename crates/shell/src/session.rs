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

use display_proto::{Interface, ObjectId, RegistryGlobal, RegistryGlobalRemove};
use toolkit::protocol::{Client, ClientError, Connection};

/// Interfaces the shell wants bound as soon as the display
/// server advertises them. Ordered so the deterministic
/// `SessionStep::GlobalsBound` output is predictable for
/// tests.
pub const INTERESTING_INTERFACES: &[Interface] =
    &[Interface::Compositor, Interface::Shm];

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
}

impl SessionStep {
    pub fn is_empty(&self) -> bool {
        self.discovered.is_empty()
            && self.bound.is_empty()
            && self.removed.is_empty()
            && self.errors.is_empty()
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

    /// Read-only access to the known-globals table.
    pub fn known_globals(&self) -> &BTreeMap<u32, GlobalEntry> {
        &self.known_globals
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
                step.errors.push(ProtocolErrorNotice {
                    object_id: parsed.object_id,
                    code: parsed.code,
                    message: parsed.message,
                });
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
