//! Application-level facade over the protocol-layer
//! [`crate::protocol::Client`].
//!
//! [`App`] owns a [`Client`] plus the three globals every
//! v1 app needs (compositor, shm, xdg_shell). Its constructor
//! drives the `display.get_registry → registry.global* →
//! registry.bind` handshake in one call so every app in T121+
//! can skip the boilerplate and start creating surfaces.
//!
//! Higher layers (window wrapper T115, widgets T116,
//! layout T117, draw-buffer loop T119) will layer on top of
//! `App`; this slice stops at the bootstrap + dispatch cycle.

use display_proto::events::RegistryGlobal;
use display_proto::ids::ObjectId;
use display_proto::objects::Interface;
use display_proto::wire::WireError;

use crate::protocol::{Client, ClientError, ClientEventWithPayload, Connection};

/// Application-level handle to a connected display server.
///
/// Thin facade over the protocol-layer [`Client`]. Owns the
/// connection, allocates object ids, and drives the
/// `display.get_registry → registry.global* → registry.bind`
/// bootstrap sequence on construction. After
/// [`App::connect`] returns, bound global ids are queryable
/// via accessor methods; the client's event loop is driven
/// by calls to [`App::dispatch`].
pub struct App<C: Connection> {
    client: Client<C>,
    compositor: ObjectId,
    shm: ObjectId,
    xdg_shell: ObjectId,
}

impl<C: Connection> App<C> {
    /// Connect to a display server via `connection`, bind the
    /// three globals required by every v1 app
    /// (`pmd_compositor`, `pmd_shm`, `pmd_xdg_shell`), and
    /// return an `App` ready to create surfaces.
    ///
    /// The bootstrap sequence:
    ///
    /// 1. Construct a [`Client`] around `connection`.
    /// 2. Send `display.get_registry(new_id)` to allocate a
    ///    registry object.
    /// 3. Drain the connection's inbound side until no more
    ///    bytes arrive, parsing every event and recording
    ///    each `registry.global(name, interface, version)`
    ///    advertisement.
    /// 4. For each of `pmd_compositor`, `pmd_shm`,
    ///    `pmd_xdg_shell`, send `registry.bind(name,
    ///    interface, version, new_id)` — erroring out with
    ///    [`ClientError::MissingGlobal`] if any of the three
    ///    weren't advertised.
    ///
    /// Returns [`ClientError`] variants on wire-format,
    /// object-table, or handshake failures. Use
    /// [`App::dispatch`] afterwards to drive subsequent
    /// events.
    pub fn connect(connection: C) -> Result<Self, ClientError> {
        let mut client = Client::new(connection);
        let registry_id = client.get_registry()?;

        // Drain every byte the server has queued; the bind
        // sequence assumes the server has advertised its
        // globals synchronously. Bidirectional test
        // connections return the full advertisement in one
        // `recv` call; the default-empty `recv` path leaves
        // the globals map empty and falls through to the
        // missing-global check.
        let mut globals: Vec<RegistryGlobal> = Vec::new();
        let mut leftover: Vec<u8> = Vec::new();
        loop {
            let chunk = client.connection_mut().recv();
            if chunk.is_empty() && leftover.is_empty() {
                break;
            }
            leftover.extend_from_slice(&chunk);
            let (events, consumed) =
                client.push_received_with_payload(&leftover)?;
            for event in events {
                if event.interface == Interface::Registry
                    && event.object_id == registry_id
                    && event.opcode == 1
                /* global */
                {
                    let parsed = RegistryGlobal::decode(&event.payload)
                        .map_err(|_| ClientError::Wire(WireError::InvalidLength))?;
                    globals.push(parsed);
                }
            }
            leftover.drain(..consumed);
            if chunk.is_empty() && !leftover.is_empty() {
                // We have a partial trailing message and no
                // more bytes are coming — bail rather than
                // spin. The caller's recv contract is "empty
                // when nothing is ready".
                break;
            }
            if chunk.is_empty() {
                break;
            }
        }

        let compositor = Self::bind_required(
            &mut client,
            registry_id,
            &globals,
            Interface::Compositor,
        )?;
        let shm =
            Self::bind_required(&mut client, registry_id, &globals, Interface::Shm)?;
        let xdg_shell = Self::bind_required(
            &mut client,
            registry_id,
            &globals,
            Interface::XdgShell,
        )?;

        Ok(App {
            client,
            compositor,
            shm,
            xdg_shell,
        })
    }

    /// Look up a required global by interface name in the
    /// advertisements the registry handed us, bind it
    /// through `registry.bind`, and return the newly
    /// allocated client-side id. Errors with
    /// [`ClientError::MissingGlobal`] if the advertisement
    /// list has no entry for this interface.
    fn bind_required(
        client: &mut Client<C>,
        registry_id: ObjectId,
        globals: &[RegistryGlobal],
        target: Interface,
    ) -> Result<ObjectId, ClientError> {
        let name = target.name();
        let global = globals
            .iter()
            .find(|g| g.interface == name)
            .ok_or(ClientError::MissingGlobal(name))?;
        client.registry_bind(registry_id, global.name, target, global.version)
    }

    /// Accessor for the bound `pmd_compositor` global's
    /// object id.
    pub fn compositor(&self) -> ObjectId {
        self.compositor
    }

    /// Accessor for the bound `pmd_shm` global's object id.
    pub fn shm(&self) -> ObjectId {
        self.shm
    }

    /// Accessor for the bound `pmd_xdg_shell` global's
    /// object id. (Spec §10 calls the interface
    /// `pmd_xdg_wm_base`; the codebase renamed it to
    /// `pmd_xdg_shell` — see [`Interface::XdgShell`].)
    pub fn xdg_shell(&self) -> ObjectId {
        self.xdg_shell
    }

    /// Borrow the wrapped [`Client`] for callers that need
    /// direct protocol access (window wrapper, widgets,
    /// drawing). The mutable-borrow counterpart is
    /// [`App::client_mut`].
    pub fn client(&self) -> &Client<C> {
        &self.client
    }

    /// Mutably borrow the wrapped [`Client`].
    pub fn client_mut(&mut self) -> &mut Client<C> {
        &mut self.client
    }

    /// Run one event-dispatch cycle: flush any outbound
    /// bytes through the connection, drain whatever the
    /// server has queued, parse the byte stream into
    /// [`ClientEventWithPayload`]s, and return them.
    ///
    /// Outbound bytes are flushed by delegating to the
    /// connection's own `drain_outbound` (callers that wrap a
    /// real transport push those bytes onto the wire).
    /// Partial trailing messages in the inbound stream are
    /// silently dropped in this minimal cycle; a future slice
    /// that adds a blocking `App::run` loop will persist the
    /// leftover across calls.
    pub fn dispatch(
        &mut self,
    ) -> Result<Vec<ClientEventWithPayload>, ClientError> {
        let _ = self.client.drain_outbound();
        let chunk = self.client.connection_mut().recv();
        if chunk.is_empty() {
            return Ok(Vec::new());
        }
        let (events, _consumed) =
            self.client.push_received_with_payload(&chunk)?;
        Ok(events)
    }

    /// Consume the `App`, returning the underlying
    /// [`Client`]. Escape hatch for callers that need direct
    /// protocol access across an API boundary the facade
    /// would otherwise bottleneck.
    pub fn into_client(self) -> Client<C> {
        self.client
    }
}
