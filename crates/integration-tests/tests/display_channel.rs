//! Cross-crate integration tests for the display-server
//! channel.
//!
//! These tests are the capstone for the Phase 2/3 stack that
//! landed in the weeks around the `display_connect`,
//! display-proto, display-server and toolkit slices. They
//! compose every layer end to end without any mock
//! transports:
//!
//!   * `kernel::sys::Kernel` — registers processes, allocates
//!     caps, hands out real socket fds via the
//!     `display_bind` / `display_connect` / `accept_socket`
//!     methods.
//!   * `kernel::sys::fd_read` / `fd_write` — actually move
//!     bytes through the kernel's `IpcTable::Socket` buffers.
//!   * `display_server::Server` — parses every framed request
//!     off the byte stream, auto-installs objects from the
//!     typed payload decoders, and journals what it saw.
//!   * `toolkit::Client` and `toolkit_free_client::FreeClient`
//!     — both sides of the Principle VII claim: the toolkit
//!     and the hand-rolled client produce byte-for-byte
//!     identical server journals.
//!
//! Each test builds a fresh kernel, a fresh display-server
//! process (with `DisplayServer` cap), a fresh client-app
//! process (with `DisplayClient`), binds the socket, drives
//! the client through a `display→registry→compositor→surface
//! →commit` walk, pumps the bytes kernel-side, and asserts
//! on the server's journal.

use abi::cap::{initial, Cap, CapSet};
use display_proto::{wire::MessageHeader, Interface, ObjectId, HEADER_SIZE};
use display_server::Server as DisplayServerState;
use kernel::fs::devfs::DevFs;
use kernel::fs::procfs::ProcFs;
use kernel::fs::tmpfs::TmpFs;
use kernel::sys::{Kernel, KernelError, RegisterArgs};
use toolkit::protocol::{Client as ToolkitClient, MemoryConnection};
use toolkit_free_client::FreeClient;

/// Build a kernel with the v1 default mounts (`/` tmpfs,
/// `/dev` devfs, `/proc` procfs) so process registration
/// and VFS paths work normally. The display-server tests
/// don't touch the VFS directly but the kernel glue layer
/// expects the mount table to exist.
fn make_kernel() -> Kernel {
    let mut k = Kernel::new();
    k.vfs
        .mount("/", Box::new(TmpFs::new()))
        .expect("root mount");
    k.vfs
        .mount("/dev", Box::new(DevFs::new()))
        .expect("devfs mount");
    k.vfs
        .mount("/proc", Box::new(ProcFs::with_static()))
        .expect("procfs mount");
    k
}

/// Register a display-server-like process with the
/// `DisplayServer` cap (plus `DisplayClient` so it can also
/// open ordinary display fds in tests that want it).
fn register_display_server(k: &mut Kernel) -> abi::ext::Pid {
    let caps = CapSet::from_caps(&[Cap::DisplayServer, Cap::DisplayClient]);
    let pid = k
        .register_process(RegisterArgs {
            name: "display-server",
            ppid: 0,
            caps,
            cwd: "/",
        })
        .unwrap();
    k.mark_ready(pid).unwrap();
    pid
}

/// Register an ordinary app with just `DisplayClient`.
fn register_app(k: &mut Kernel, name: &str) -> abi::ext::Pid {
    let pid = k
        .register_process(RegisterArgs {
            name,
            ppid: 0,
            caps: initial::ORDINARY_APP,
            cwd: "/",
        })
        .unwrap();
    k.mark_ready(pid).unwrap();
    pid
}

/// Push an entire Vec<u8> out through `fd_write` in one
/// call. v1 `IpcTable::send_on_socket` is write-all-or-
/// WouldBlock (the socket's rx buffer cap is much larger
/// than any single test payload), so a single call is
/// enough.
fn fd_write_all(
    k: &mut Kernel,
    pid: abi::ext::Pid,
    fd: u32,
    bytes: &[u8],
) -> Result<(), KernelError> {
    let n = k.fd_write(pid, fd, bytes)?;
    assert_eq!(n, bytes.len(), "short write from fd_write");
    Ok(())
}

/// Read every byte currently buffered on a socket fd.
/// Returns an empty vec on `WouldBlock`.
fn fd_read_available(k: &mut Kernel, pid: abi::ext::Pid, fd: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match k.fd_read(pid, fd, &mut buf) {
            Ok(0) => return out,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(KernelError::WouldBlock) => return out,
            Err(other) => panic!("fd_read: {other:?}"),
        }
    }
}

/// Drain one framed message from the front of `bytes`,
/// returning the message and the unconsumed tail. `None`
/// if `bytes` doesn't contain a full message yet.
fn split_first_message(bytes: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let header = MessageHeader::decode(bytes).ok()?;
    let msg_len = header.length as usize;
    if bytes.len() < msg_len {
        return None;
    }
    Some((bytes[..msg_len].to_vec(), bytes[msg_len..].to_vec()))
}

/// Given a byte buffer containing zero or more framed
/// messages, dispatch each one through the
/// display-server's server state and return the number of
/// messages dispatched.
fn pump_into_display_server(
    server: &mut DisplayServerState,
    server_client_id: display_server::ClientId,
    mut bytes: Vec<u8>,
) -> usize {
    let mut n = 0;
    while let Some((msg, rest)) = split_first_message(&bytes) {
        server
            .dispatch_request(server_client_id, &msg)
            .unwrap_or_else(|e| panic!("dispatch {n}: {e:?}"));
        n += 1;
        bytes = rest;
    }
    assert!(bytes.is_empty(), "leftover bytes: {bytes:?}");
    n
}

// ---- end-to-end tests ------------------------------------------

#[test]
fn free_client_bytes_reach_display_server_through_kernel_socket() {
    // Stand up a kernel. The display-server process binds
    // /run/display; the client process connects. The
    // hand-rolled FreeClient drives the protocol walk, its
    // bytes flow through fd_write → kernel socket →
    // fd_read on the server side, and the display-server
    // library's Server parses them and journals the result.
    //
    // NO toolkit in this test. No mocks. Every byte
    // traverses the real Kernel IPC path.
    let mut k = make_kernel();
    let ds_pid = register_display_server(&mut k);
    let listener_fd = k.display_bind(ds_pid).unwrap();

    let app_pid = register_app(&mut k, "app");
    let client_fd = k.display_connect(app_pid).unwrap();
    let server_fd = k.accept_socket(ds_pid, listener_fd).unwrap();

    // Build a full protocol walk as bytes.
    let mut fc = FreeClient::new();
    let registry = fc.get_registry().unwrap();
    let compositor = fc.registry_bind(registry, 1, "pmd_compositor", 1).unwrap();
    let surface = fc.compositor_create_surface(compositor).unwrap();
    fc.surface_commit(surface).unwrap();
    let wire_bytes = fc.drain_outbound();

    // Ship them through the kernel socket: client side
    // writes, server side reads.
    fd_write_all(&mut k, app_pid, client_fd, &wire_bytes).unwrap();
    let received = fd_read_available(&mut k, ds_pid, server_fd);
    assert_eq!(
        received, wire_bytes,
        "bytes must round-trip verbatim through the kernel socket"
    );

    // Stand up a display-server state machine and pump the
    // received bytes through it. The auto-install decoders
    // should bind every new_id the client sent.
    let mut server = DisplayServerState::new();
    let server_client_id = server.accept();
    let dispatched = pump_into_display_server(&mut server, server_client_id, received);
    assert_eq!(dispatched, 4);

    // Server's journal matches the client's request sequence.
    let server_client = server.client_mut(server_client_id).unwrap();
    let journal = server_client.drain_journal();
    let names: Vec<(Interface, &str)> = journal
        .iter()
        .map(|r| (r.interface, r.opcode_name))
        .collect();
    assert_eq!(
        names,
        vec![
            (Interface::Display, "get_registry"),
            (Interface::Registry, "bind"),
            (Interface::Compositor, "create_surface"),
            (Interface::Surface, "commit"),
        ]
    );

    // Every client-allocated object is bound in the
    // display-server's per-client table at the exact ID
    // the free client chose.
    assert_eq!(
        server_client.get(ObjectId::DISPLAY),
        Some(Interface::Display)
    );
    assert_eq!(server_client.get(registry), Some(Interface::Registry));
    assert_eq!(server_client.get(compositor), Some(Interface::Compositor));
    assert_eq!(server_client.get(surface), Some(Interface::Surface));
}

#[test]
fn toolkit_client_bytes_reach_display_server_through_kernel_socket() {
    // Same flow, but driven by `toolkit::Client` instead of
    // the hand-rolled free client. If this test agrees with
    // the one above byte-for-byte, Principle VII holds
    // across the full kernel path: the toolkit is a library
    // wrapper over the same wire the free client uses, and
    // the kernel doesn't care which one is on the other end.
    let mut k = make_kernel();
    let ds_pid = register_display_server(&mut k);
    let listener_fd = k.display_bind(ds_pid).unwrap();
    let app_pid = register_app(&mut k, "app");
    let client_fd = k.display_connect(app_pid).unwrap();
    let server_fd = k.accept_socket(ds_pid, listener_fd).unwrap();

    let mut tk = ToolkitClient::new(MemoryConnection::new());
    let registry = tk.get_registry().unwrap();
    let compositor = tk
        .registry_bind(registry, 1, Interface::Compositor, 1)
        .unwrap();
    let surface = tk.compositor_create_surface(compositor).unwrap();
    tk.surface_commit(surface).unwrap();
    let wire_bytes = tk.drain_outbound();

    fd_write_all(&mut k, app_pid, client_fd, &wire_bytes).unwrap();
    let received = fd_read_available(&mut k, ds_pid, server_fd);

    let mut server = DisplayServerState::new();
    let server_client_id = server.accept();
    let dispatched = pump_into_display_server(&mut server, server_client_id, received);
    assert_eq!(dispatched, 4);
    let journal = server.client_mut(server_client_id).unwrap().drain_journal();
    let names: Vec<(Interface, &str)> = journal
        .iter()
        .map(|r| (r.interface, r.opcode_name))
        .collect();
    assert_eq!(
        names,
        vec![
            (Interface::Display, "get_registry"),
            (Interface::Registry, "bind"),
            (Interface::Compositor, "create_surface"),
            (Interface::Surface, "commit"),
        ]
    );
}

#[test]
fn toolkit_and_free_client_produce_identical_bytes() {
    // Principle VII in its strongest form: run the same
    // protocol walk through both clients into their own
    // byte buffers and assert the two buffers are EXACTLY
    // equal. If they ever diverge, one side has drifted
    // from the shared display-proto contract and every
    // other test in this file will already be screaming —
    // but this direct byte-diff test makes the failure
    // easy to localise.
    let mut fc = FreeClient::new();
    let registry = fc.get_registry().unwrap();
    let compositor = fc.registry_bind(registry, 1, "pmd_compositor", 1).unwrap();
    let surface = fc.compositor_create_surface(compositor).unwrap();
    fc.surface_commit(surface).unwrap();
    let fc_bytes = fc.drain_outbound();

    let mut tk = ToolkitClient::new(MemoryConnection::new());
    // Same call sequence using the toolkit's typed helpers.
    let tk_registry = tk.get_registry().unwrap();
    let tk_compositor = tk
        .registry_bind(tk_registry, 1, Interface::Compositor, 1)
        .unwrap();
    let tk_surface = tk.compositor_create_surface(tk_compositor).unwrap();
    tk.surface_commit(tk_surface).unwrap();
    let tk_bytes = tk.drain_outbound();

    // Allocators on both sides hand out the same sequence
    // (3, 5, 7) so the ids referenced in each payload
    // match too.
    assert_eq!(registry.raw(), tk_registry.raw());
    assert_eq!(compositor.raw(), tk_compositor.raw());
    assert_eq!(surface.raw(), tk_surface.raw());

    assert_eq!(
        fc_bytes, tk_bytes,
        "FreeClient and ToolkitClient MUST produce byte-identical wire output"
    );
}

#[test]
fn display_server_rejects_malformed_client_bytes_without_tearing_down_the_kernel_socket() {
    // Send garbage over the kernel socket. The
    // display-server library reports an error, the kernel
    // socket stays open, and a follow-up valid request
    // still succeeds.
    let mut k = make_kernel();
    let ds_pid = register_display_server(&mut k);
    let listener_fd = k.display_bind(ds_pid).unwrap();
    let app_pid = register_app(&mut k, "app");
    let client_fd = k.display_connect(app_pid).unwrap();
    let server_fd = k.accept_socket(ds_pid, listener_fd).unwrap();

    // Craft a header with a bogus opcode on the display
    // interface. The server's dispatcher rejects it as
    // UnknownOpcode before the decoder would even run.
    let bogus = MessageHeader::try_new(ObjectId::DISPLAY, 0xff, 0, 0).unwrap();
    let mut buf = vec![0u8; HEADER_SIZE];
    bogus.encode(&mut buf).unwrap();
    fd_write_all(&mut k, app_pid, client_fd, &buf).unwrap();

    let bytes = fd_read_available(&mut k, ds_pid, server_fd);
    assert_eq!(bytes, buf);

    let mut server = DisplayServerState::new();
    let server_client_id = server.accept();
    // Dispatch this one message directly (don't use the
    // pump helper, which asserts success).
    let err = server
        .dispatch_request(server_client_id, &bytes)
        .unwrap_err();
    match err {
        display_server::ServerError::Client(display_server::ClientError::UnknownOpcode {
            interface,
            opcode,
        }) => {
            assert_eq!(interface, Interface::Display);
            assert_eq!(opcode, 0xff);
        }
        other => panic!("expected UnknownOpcode, got {other:?}"),
    }

    // Kernel socket still works: send a valid get_registry
    // and it flows through unaffected.
    let mut fc = FreeClient::new();
    fc.get_registry().unwrap();
    let wire_bytes = fc.drain_outbound();
    fd_write_all(&mut k, app_pid, client_fd, &wire_bytes).unwrap();
    let follow_up = fd_read_available(&mut k, ds_pid, server_fd);
    assert_eq!(follow_up, wire_bytes);
}

#[test]
fn multiple_apps_share_the_same_display_server_with_independent_journals() {
    // Two apps connect to the same display server. Each
    // app's byte stream is dispatched into its own
    // server-side Client state machine; the two journals
    // never bleed into each other.
    let mut k = make_kernel();
    let ds_pid = register_display_server(&mut k);
    let listener_fd = k.display_bind(ds_pid).unwrap();

    let app_a = register_app(&mut k, "a");
    let fd_a = k.display_connect(app_a).unwrap();
    let server_fd_a = k.accept_socket(ds_pid, listener_fd).unwrap();

    let app_b = register_app(&mut k, "b");
    let fd_b = k.display_connect(app_b).unwrap();
    let server_fd_b = k.accept_socket(ds_pid, listener_fd).unwrap();

    let mut server = DisplayServerState::new();
    let ds_a = server.accept();
    let ds_b = server.accept();

    // App A walks display → registry.
    let mut fc_a = FreeClient::new();
    let _ = fc_a.get_registry().unwrap();
    fd_write_all(&mut k, app_a, fd_a, &fc_a.drain_outbound()).unwrap();
    let bytes_a = fd_read_available(&mut k, ds_pid, server_fd_a);
    pump_into_display_server(&mut server, ds_a, bytes_a);

    // App B walks display → registry.bind(shm).
    let mut fc_b = FreeClient::new();
    let reg_b = fc_b.get_registry().unwrap();
    let _ = fc_b.registry_bind(reg_b, 2, "pmd_shm", 1).unwrap();
    fd_write_all(&mut k, app_b, fd_b, &fc_b.drain_outbound()).unwrap();
    let bytes_b = fd_read_available(&mut k, ds_pid, server_fd_b);
    pump_into_display_server(&mut server, ds_b, bytes_b);

    // A's journal has exactly one entry (get_registry),
    // B's has two (get_registry + bind). The object tables
    // are independent: B has a Shm object at id 5, A does not.
    let a_journal = server.client_mut(ds_a).unwrap().drain_journal();
    let b_journal = server.client_mut(ds_b).unwrap().drain_journal();
    assert_eq!(a_journal.len(), 1);
    assert_eq!(a_journal[0].opcode_name, "get_registry");
    assert_eq!(b_journal.len(), 2);
    assert_eq!(b_journal[0].opcode_name, "get_registry");
    assert_eq!(b_journal[1].opcode_name, "bind");

    assert_eq!(
        server.client(ds_b).unwrap().get(ObjectId::new(5)),
        Some(Interface::Shm)
    );
    assert_eq!(server.client(ds_a).unwrap().get(ObjectId::new(5)), None);
}

#[test]
fn multiple_writes_are_streamed_on_the_same_socket() {
    // Send the four-message walk as four separate
    // fd_write calls, not one batched write. The kernel
    // socket should accumulate the bytes and deliver them
    // all on the next fd_read. This pins down the
    // streaming semantics.
    let mut k = make_kernel();
    let ds_pid = register_display_server(&mut k);
    let listener_fd = k.display_bind(ds_pid).unwrap();
    let app_pid = register_app(&mut k, "app");
    let client_fd = k.display_connect(app_pid).unwrap();
    let server_fd = k.accept_socket(ds_pid, listener_fd).unwrap();

    // Build four requests into separate buffers by draining
    // the FreeClient between each send.
    let mut fc = FreeClient::new();

    let registry = fc.get_registry().unwrap();
    fd_write_all(&mut k, app_pid, client_fd, &fc.drain_outbound()).unwrap();

    let compositor = fc.registry_bind(registry, 1, "pmd_compositor", 1).unwrap();
    fd_write_all(&mut k, app_pid, client_fd, &fc.drain_outbound()).unwrap();

    let surface = fc.compositor_create_surface(compositor).unwrap();
    fd_write_all(&mut k, app_pid, client_fd, &fc.drain_outbound()).unwrap();

    fc.surface_commit(surface).unwrap();
    fd_write_all(&mut k, app_pid, client_fd, &fc.drain_outbound()).unwrap();

    // All four are now on the kernel socket. One read
    // drains everything.
    let all = fd_read_available(&mut k, ds_pid, server_fd);

    let mut server = DisplayServerState::new();
    let server_client_id = server.accept();
    let dispatched = pump_into_display_server(&mut server, server_client_id, all);
    assert_eq!(dispatched, 4);
}
