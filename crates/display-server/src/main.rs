//! PMos display server binary — protocol-aware compositor (T110).
//!
//! Listens on `/run/display`, accepts clients, dispatches Wayland-
//! inspired protocol messages via `display_server::Server`, composites
//! attached buffers into the server's framebuffer, and presents the
//! composed pixels to `/dev/fb0` on each client turn. SIGTERM-driven
//! exit closes the long-running-server arc.
//!
//! Dual-mode reception: the v1 demo client `display-client-demo`
//! writes raw 16-byte RGBA payloads that pre-date the protocol. If
//! a received chunk decodes as a valid [`MessageHeader`] AND the
//! header's `length` matches the chunk size, the chunk is routed
//! through `Server::dispatch_request` and the resulting compositor
//! state is presented. Otherwise, the chunk is forwarded verbatim
//! to `/dev/fb0` (the legacy raw-blit path). This preserves the
//! pre-T110 Playwright `real-kernel.spec.ts` assertions while
//! adding the full protocol path toolkit clients use.
//!
//! Up to 64 clients are multiplexed through WASI `poll_oneoff` with
//! exact full-duplex interests. Each input-first turn has bounded
//! accept, watch, client-read, request, event-drain, write, and
//! disconnect quanta; locally buffered work uses a zero-deadline poll
//! so readiness changes on other clients are never hidden.
//!
//! Flow:
//!   1. `display_bind()`                        — claim `/run/display`.
//!   2. `path_open("/dev/fb0")`                  — open framebuffer.
//!   3. In the outer loop, drain input first, accept a bounded client
//!      batch, and service ready client streams in rotating order.
//!   4. Dispatch bounded complete requests, return ordered events,
//!      and present `server.framebuffer().pixels()` to `/dev/fb0`.
//!      The exact frozen legacy fixture is relayed as a raw blit.
//!   5. Park on listener/input/watch/signal plus exact client READ and
//!      pending-output WRITE interests only when no local work remains.
//!   6. On SIGTERM, drain immediately ready transport, then use a bounded
//!      transport-only grace poll to catch late kernel routing. Exit at
//!      quiescence after the grace deadline or the fixed total turn cap,
//!      print `display-server fb blit ok`, and fall off the end of `main`.
//!
//! Exit codes:
//!
//!   * 0  = success (the bounded post-SIGTERM transport drain completed)
//!   * 10 = `display_bind` failed
//!   * 12 = `ipc_accept` returned an unexpected negative rc
//!   * 14 = `fd_read` returned a non-EAGAIN error or read 0 bytes
//!   * 15 = `path_open("/dev/fb0")` failed
//!   * 16 = framebuffer `fd_write` failed or short-wrote
//!   * 18 = `fd_read` poll exhausted for a connected client
//!   * 19 = `fd_close` on the client fd returned a non-zero errno

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn path_open(
        dirfd: i32,
        dirflags: i32,
        path_ptr: *const u8,
        path_len: i32,
        oflags: i32,
        fs_rights_base: i64,
        fs_rights_inheriting: i64,
        fdflags: i32,
        fd_out_ptr: *mut u32,
    ) -> i32;
    fn fd_write(fd: i32, iovs_ptr: *const Ciovec, iovs_len: i32, nwritten_ptr: *mut u32) -> i32;
    fn fd_read(fd: i32, iovs_ptr: *const Iovec, iovs_len: i32, nread_ptr: *mut u32) -> i32;
    fn fd_close(fd: i32) -> i32;
    fn poll_oneoff(
        subscriptions: *const u8,
        events: *mut u8,
        nsubscriptions: u32,
        nevents: *mut u32,
    ) -> i32;
}

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "pmos_ext")]
extern "C" {
    fn display_bind() -> i32;
    fn ipc_accept_nonblock(listener_fd: i32) -> i32;
    fn ipc_peer_caps(fd: i32, caps_out: *mut u64) -> i32;
    fn ipc_peer_pid(fd: i32, pid_out: *mut i32) -> i32;
    fn fs_watch(path_ptr: *const u8, path_len: i32, mask: u32, flags: u32) -> i32;
}

#[cfg(target_arch = "wasm32")]
#[repr(C)]
struct Ciovec {
    buf: *const u8,
    buf_len: u32,
}

#[cfg(target_arch = "wasm32")]
#[repr(C)]
struct Iovec {
    buf: *mut u8,
    buf_len: u32,
}

#[cfg(target_arch = "wasm32")]
const SIGNAL_FD: i32 = abi::fd::SIGNAL as i32;

#[cfg(target_arch = "wasm32")]
unsafe fn poll_sigterm() -> bool {
    let mut buf = [0u8; 8];
    let iov = Iovec {
        buf: buf.as_mut_ptr(),
        buf_len: buf.len() as u32,
    };
    let mut nread: u32 = 0;
    let rc = fd_read(SIGNAL_FD, &iov, 1, &mut nread);
    if rc != 0 {
        return false;
    }
    let n = nread as usize;
    let mut i = 0;
    while i + 2 <= n {
        let signum = u16::from_le_bytes([buf[i], buf[i + 1]]);
        if signum == 15 {
            return true;
        }
        i += 2;
    }
    false
}

// Initial protocol-vs-legacy classification lives in the lib so
// native tests can exercise fragmented first reads without a WASI
// transport; this binary consumes `classify_initial_stream`.

/// If `chunk` is a `pmd_display.get_registry` request, push
/// the v1 globals catalog onto the client's pending events
/// queue so the toolkit's App::connect bind handshake can
/// complete on the next recv. The advertise lives in the
/// binary (not the library's `Server::dispatch_request`)
/// because integration tests under `crates/integration-tests/`
/// drive Server directly and emit globals manually — keeping
/// auto-advertise here keeps the binary's production path
/// self-contained without changing those tests' assumptions.
#[cfg(target_arch = "wasm32")]
fn advertise_globals_for_get_registry(
    server: &mut display_server::Server,
    client_id: display_server::ClientId,
    chunk: &[u8],
) {
    use display_server::HEADER_SIZE;
    let Ok(header) = display_server::MessageHeader::decode(chunk) else {
        return;
    };
    if header.object_id != display_server::ObjectId::DISPLAY || header.opcode != 2 {
        return;
    }
    let payload_end = header.length as usize;
    if payload_end < HEADER_SIZE || payload_end > chunk.len() {
        return;
    }
    let payload = &chunk[HEADER_SIZE..payload_end];
    if let Ok(req) = display_proto::requests::DisplayGetRegistry::decode(payload) {
        server.advertise_globals_to(client_id, req.new_id);
    }
}

#[cfg(target_arch = "wasm32")]
unsafe fn open_dev(path: &[u8]) -> Option<u32> {
    let mut fd: u32 = 0;
    let rc = path_open(0, 0, path.as_ptr(), path.len() as i32, 0, 0, 0, 0, &mut fd);
    if rc != 0 {
        return None;
    }
    Some(fd)
}

#[cfg(target_arch = "wasm32")]
struct PreferenceWatch {
    parent_fd: i32,
    file_fd: Option<i32>,
}

#[cfg(target_arch = "wasm32")]
impl PreferenceWatch {
    unsafe fn new() -> Result<Self, i32> {
        let parent_fd = fs_watch(
            b"/etc".as_ptr(),
            4,
            abi::ext::WATCH_CREATE | abi::ext::WATCH_DELETE,
            0,
        );
        if parent_fd < 0 {
            return Err(-parent_fd);
        }
        let file_fd = match Self::open_file() {
            Ok(fd) => fd,
            Err(errno) => {
                let _ = fd_close(parent_fd);
                return Err(errno);
            }
        };
        Ok(Self { parent_fd, file_fd })
    }

    unsafe fn open_file() -> Result<Option<i32>, i32> {
        let path = preferences::DEFAULT_PATH.as_bytes();
        let fd = fs_watch(path.as_ptr(), path.len() as i32, abi::ext::WATCH_MODIFY, 0);
        if fd >= 0 {
            Ok(Some(fd))
        } else if -fd == abi::errno::ENOENT {
            Ok(None)
        } else {
            Err(-fd)
        }
    }

    fn wait_fds(&self) -> Vec<i32> {
        let mut fds = vec![self.parent_fd];
        if let Some(fd) = self.file_fd {
            fds.push(fd);
        }
        fds
    }

    unsafe fn drain(&mut self) -> Result<bool, i32> {
        let parent_changed = drain_watch_fd(self.parent_fd)?;
        let file_changed = match self.file_fd {
            Some(fd) => drain_watch_fd(fd)?,
            None => false,
        };
        if parent_changed {
            // Release the stale-inode slot before registering the replacement,
            // so refresh never needs transient extra watch capacity.
            if let Some(fd) = self.file_fd.take() {
                let _ = fd_close(fd);
            }
            self.file_fd = Self::open_file()?;
        }
        Ok(parent_changed || file_changed)
    }
}

#[cfg(target_arch = "wasm32")]
impl Drop for PreferenceWatch {
    fn drop(&mut self) {
        unsafe {
            let _ = fd_close(self.parent_fd);
            if let Some(fd) = self.file_fd {
                let _ = fd_close(fd);
            }
        }
    }
}

#[cfg(any(target_arch = "wasm32", test))]
const ACCEPTS_PER_TURN: usize = 2;
#[cfg(any(target_arch = "wasm32", test))]
const WATCH_READS_PER_TURN: usize = 2;
#[cfg(any(target_arch = "wasm32", test))]
const REQUESTS_PER_CLIENT_PER_TURN: usize = 4;
#[cfg(any(target_arch = "wasm32", test))]
const REQUESTS_PER_TURN: usize = 32;
#[cfg(any(target_arch = "wasm32", test))]
const CLIENT_READ_ATTEMPTS_PER_TURN: usize = 4;
#[cfg(any(target_arch = "wasm32", test))]
const CLIENT_WRITE_ATTEMPTS_PER_TURN: usize = 4;
#[cfg(any(target_arch = "wasm32", test))]
const CLIENT_EVENT_BYTES_PER_TURN: usize = display_server::MAX_CONN_OUTBOUND_BYTES;
#[cfg(any(target_arch = "wasm32", test))]
const CLIENT_DISCONNECTS_PER_TURN: usize = 1;
/// A graceful shutdown may spend one turn retiring each admitted client, but
/// a peer that keeps the listener or a stream ready cannot postpone exit.
#[cfg(any(target_arch = "wasm32", test))]
const SHUTDOWN_DRAIN_TURNS: usize = display_server::MAX_SERVER_CLIENTS;
/// Give already-completed peer sends time to become visible to this worker.
/// The wait set contains only the listener and connected client transport.
#[cfg(any(target_arch = "wasm32", test))]
const SHUTDOWN_QUIESCENCE_GRACE_MS: u64 = 100;
/// A fence is a real `/dev/fb0` write. Cap it globally, even though at most two
/// authenticated shell connections can have a one-shot marker queued.
#[cfg(any(target_arch = "wasm32", test))]
const PRESENT_FENCE_WRITES_PER_TURN: usize = 1;
#[cfg(any(target_arch = "wasm32", test))]
const CLIENT_SOCKET_WRITE_QUANTUM_BYTES: usize = 32 * 1024;
#[cfg(any(target_arch = "wasm32", test))]
const MAX_NON_INPUT_TRANSPORT_SYSCALLS_PER_TURN: usize = ACCEPTS_PER_TURN * 3
    + WATCH_READS_PER_TURN * 2
    + CLIENT_READ_ATTEMPTS_PER_TURN
    + CLIENT_WRITE_ATTEMPTS_PER_TURN
    + CLIENT_DISCONNECTS_PER_TURN
    + PRESENT_FENCE_WRITES_PER_TURN;
#[cfg(any(target_arch = "wasm32", test))]
const _: () = assert!(MAX_NON_INPUT_TRANSPORT_SYSCALLS_PER_TURN <= 20);
#[cfg(any(target_arch = "wasm32", test))]
const _: () = assert!(
    REQUESTS_PER_TURN * (display_server::MAX_TOPLEVEL_METADATA_BYTES as usize + 21) + 14
        <= CLIENT_SOCKET_WRITE_QUANTUM_BYTES
);

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct RequestDrain {
    consumed: usize,
    dispatched: usize,
    complete_remaining: bool,
    stopped_early: bool,
}

#[cfg(any(target_arch = "wasm32", test))]
fn has_complete_protocol_request(pending: &[u8]) -> bool {
    if pending.len() < display_server::HEADER_SIZE {
        return false;
    }
    let len = u16::from_le_bytes([pending[6], pending[7]]) as usize;
    len < display_server::HEADER_SIZE || len <= pending.len()
}

#[cfg(any(target_arch = "wasm32", test))]
fn drain_protocol_requests_bounded<D>(
    pending: &mut Vec<u8>,
    limit: usize,
    mut dispatch: D,
) -> RequestDrain
where
    D: FnMut(&[u8]) -> bool,
{
    let mut consumed = 0usize;
    let mut dispatched = 0usize;
    let mut stopped_early = false;
    while dispatched < limit {
        let remaining = &pending[consumed..];
        if remaining.len() < display_server::HEADER_SIZE {
            break;
        }
        let len = u16::from_le_bytes([remaining[6], remaining[7]]) as usize;
        if len < display_server::HEADER_SIZE {
            // Preserve the existing fail-closed framing behaviour: malformed
            // input is discarded, but it cannot consume an unbounded turn.
            consumed = pending.len();
            break;
        }
        if len > remaining.len() {
            break;
        }
        let keep_dispatching = dispatch(&remaining[..len]);
        consumed += len;
        dispatched += 1;
        if !keep_dispatching {
            stopped_early = true;
            break;
        }
    }
    if consumed > 0 {
        pending.drain(..consumed);
    }
    RequestDrain {
        consumed,
        dispatched,
        complete_remaining: has_complete_protocol_request(pending),
        stopped_early,
    }
}

#[cfg(any(target_arch = "wasm32", test))]
fn rotating_indices(len: usize, cursor: usize) -> Vec<usize> {
    if len == 0 {
        return Vec::new();
    }
    let start = cursor % len;
    (0..len).map(|offset| (start + offset) % len).collect()
}

#[cfg(any(target_arch = "wasm32", test))]
fn priority_first<T: Copy, F>(order: &[T], mut is_priority: F) -> Vec<T>
where
    F: FnMut(T) -> bool,
{
    let mut prioritized = Vec::with_capacity(order.len());
    prioritized.extend(order.iter().copied().filter(|item| is_priority(*item)));
    prioritized.extend(order.iter().copied().filter(|item| !is_priority(*item)));
    prioritized
}

#[cfg(any(target_arch = "wasm32", test))]
const fn pending_event_can_make_local_progress(
    next_event_bytes: Option<usize>,
    outbound_remaining: usize,
) -> bool {
    matches!(next_event_bytes, Some(bytes) if bytes <= outbound_remaining)
}

#[cfg(any(target_arch = "wasm32", test))]
const fn should_park_after_request_round(local_pending_work: bool) -> bool {
    !local_pending_work
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
enum ShutdownTurnDecision {
    #[default]
    Continue,
    WaitForDeadline(u64),
    Finish,
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct ShutdownDrain {
    requested: bool,
    listener_drained: bool,
    turns_remaining: usize,
    deadline_ms: u64,
}

#[cfg(any(target_arch = "wasm32", test))]
impl ShutdownDrain {
    fn request(&mut self, now_ms: u64) {
        if !self.requested {
            self.requested = true;
            self.listener_drained = false;
            self.turns_remaining = SHUTDOWN_DRAIN_TURNS;
            self.deadline_ms = now_ms.saturating_add(SHUTDOWN_QUIESCENCE_GRACE_MS);
        }
    }

    fn observe_accept_result(&mut self, result: Result<usize, i32>) {
        if self.requested {
            self.listener_drained = matches!(result, Err(rc) if rc == -abi::errno::EAGAIN);
        }
    }

    const fn requested(self) -> bool {
        self.requested
    }

    fn finish_turn(
        &mut self,
        now_ms: u64,
        local_pending_work: bool,
        client_transport_ready: bool,
    ) -> ShutdownTurnDecision {
        if !self.requested {
            return ShutdownTurnDecision::Continue;
        }
        self.turns_remaining = self.turns_remaining.saturating_sub(1);
        if self.turns_remaining == 0 {
            return ShutdownTurnDecision::Finish;
        }
        if !self.listener_drained || local_pending_work || client_transport_ready {
            return ShutdownTurnDecision::Continue;
        }
        if now_ms >= self.deadline_ms {
            ShutdownTurnDecision::Finish
        } else {
            ShutdownTurnDecision::WaitForDeadline(self.deadline_ms - now_ms)
        }
    }
}

#[cfg(any(target_arch = "wasm32", test))]
const fn client_transport_can_make_local_progress(
    read_ready: bool,
    write_ready: bool,
    outbound_pending: bool,
    disconnect_pending: bool,
) -> bool {
    read_ready || disconnect_pending || (write_ready && outbound_pending)
}

#[cfg(any(target_arch = "wasm32", test))]
fn drain_accepts_bounded<A, H>(mut accept: A, mut handle: H) -> Result<usize, i32>
where
    A: FnMut() -> i32,
    H: FnMut(i32),
{
    let mut accepted = 0;
    for _ in 0..ACCEPTS_PER_TURN {
        let fd = accept();
        if fd < 0 {
            return Err(fd);
        }
        handle(fd);
        accepted += 1;
    }
    Ok(accepted)
}

#[cfg(any(target_arch = "wasm32", test))]
fn drain_watch_records_bounded<R>(mut read: R) -> Result<bool, i32>
where
    R: FnMut(&mut [u8]) -> Result<usize, i32>,
{
    let mut changed = false;
    for _ in 0..WATCH_READS_PER_TURN {
        let mut records = [0u8; abi::ext::FS_WATCH_EVENT_SIZE * 16];
        match read(&mut records) {
            Err(errno) if errno == abi::errno::EAGAIN => return Ok(changed),
            Err(errno) => return Err(errno),
            Ok(0) => return Ok(changed),
            Ok(nread) if nread % abi::ext::FS_WATCH_EVENT_SIZE == 0 => changed = true,
            Ok(_) => return Err(abi::errno::EIO),
        }
    }
    Ok(changed)
}

#[cfg(target_arch = "wasm32")]
unsafe fn drain_watch_fd(fd: i32) -> Result<bool, i32> {
    drain_watch_records_bounded(|records| {
        let iov = Iovec {
            buf: records.as_mut_ptr(),
            buf_len: records.len() as u32,
        };
        let mut nread = 0u32;
        let errno = unsafe { fd_read(fd, &iov, 1, &mut nread) };
        if errno != 0 {
            Err(errno)
        } else {
            Ok(nread as usize)
        }
    })
}

#[cfg(any(target_arch = "wasm32", test))]
fn decode_ready_interests(
    interests: &[(i32, u8)],
    events: &[u8],
    nevents: usize,
    allow_clock: bool,
) -> Result<Vec<(i32, u8)>, i32> {
    if nevents == 0
        || nevents > interests.len() + usize::from(allow_clock)
        || events.len() < nevents.saturating_mul(abi::wasi::poll::EVENT_SIZE)
    {
        return Err(abi::errno::EIO);
    }
    let mut ready = Vec::with_capacity(nevents);
    for event_index in 0..nevents {
        let base = event_index * abi::wasi::poll::EVENT_SIZE;
        let event_errno = u16::from_le_bytes([
            events[base + abi::wasi::poll::EVENT_OFF_ERROR],
            events[base + abi::wasi::poll::EVENT_OFF_ERROR + 1],
        ]) as i32;
        if event_errno != 0 {
            return Err(event_errno);
        }
        let userdata = u64::from_le_bytes(
            events[base + abi::wasi::poll::EVENT_OFF_USERDATA
                ..base + abi::wasi::poll::EVENT_OFF_USERDATA + 8]
                .try_into()
                .map_err(|_| abi::errno::EIO)?,
        );
        let event_type = events[base + abi::wasi::poll::EVENT_OFF_TYPE];
        if allow_clock && userdata == 0 && event_type == abi::wasi::eventtype::CLOCK {
            continue;
        }
        let Some(interest_index) = userdata
            .checked_sub(1)
            .and_then(|value| usize::try_from(value).ok())
        else {
            return Err(abi::errno::EIO);
        };
        let Some(interest) = interests.get(interest_index).copied() else {
            return Err(abi::errno::EIO);
        };
        if event_type != interest.1 {
            return Err(abi::errno::EIO);
        }
        ready.push(interest);
    }
    Ok(ready)
}

#[cfg(target_arch = "wasm32")]
unsafe fn poll_for_activity(
    interests: &[(i32, u8)],
    timeout_ns: Option<u64>,
) -> Result<Vec<(i32, u8)>, i32> {
    let subscription_count = interests.len() + usize::from(timeout_ns.is_some());
    if interests.is_empty() || subscription_count > abi::wasi::poll::MAX_SUBSCRIPTIONS {
        return Err(abi::errno::EINVAL);
    }
    let mut subscriptions = vec![0u8; subscription_count * abi::wasi::poll::SUBSCRIPTION_SIZE];
    for (index, (fd, event_type)) in interests.iter().copied().enumerate() {
        let base = index * abi::wasi::poll::SUBSCRIPTION_SIZE;
        subscriptions[base..base + 8].copy_from_slice(&(index as u64 + 1).to_le_bytes());
        subscriptions[base + abi::wasi::poll::SUB_OFF_TAG] = event_type;
        subscriptions
            [base + abi::wasi::poll::SUB_FDRW_OFF_FD..base + abi::wasi::poll::SUB_FDRW_OFF_FD + 4]
            .copy_from_slice(&(fd as u32).to_le_bytes());
    }
    if let Some(timeout_ns) = timeout_ns {
        let base = interests.len() * abi::wasi::poll::SUBSCRIPTION_SIZE;
        subscriptions[base + abi::wasi::poll::SUB_OFF_TAG] = abi::wasi::eventtype::CLOCK;
        subscriptions[base + abi::wasi::poll::SUB_CLOCK_OFF_ID
            ..base + abi::wasi::poll::SUB_CLOCK_OFF_ID + 4]
            .copy_from_slice(&abi::wasi::CLOCKID_MONOTONIC.to_le_bytes());
        subscriptions[base + abi::wasi::poll::SUB_CLOCK_OFF_TIMEOUT
            ..base + abi::wasi::poll::SUB_CLOCK_OFF_TIMEOUT + 8]
            .copy_from_slice(&timeout_ns.to_le_bytes());
        // A zero relative timeout is an exact readiness snapshot; a positive
        // one is either the restore deadline or bounded shutdown grace wakeup.
        // Userdata zero is reserved for this synthetic clock and filtered.
    }
    let mut events = vec![0u8; subscription_count * abi::wasi::poll::EVENT_SIZE];
    let mut nevents = 0u32;
    let errno = poll_oneoff(
        subscriptions.as_ptr(),
        events.as_mut_ptr(),
        subscription_count as u32,
        &mut nevents,
    );
    if errno != 0 {
        return Err(errno);
    }
    decode_ready_interests(interests, &events, nevents as usize, timeout_ns.is_some())
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct PollReadiness {
    listener_read_ready: bool,
}

#[cfg(any(target_arch = "wasm32", test))]
fn summarize_poll_readiness(listener: i32, ready: &[(i32, u8)]) -> PollReadiness {
    PollReadiness {
        listener_read_ready: ready.contains(&(listener, abi::wasi::eventtype::FD_READ)),
    }
}

#[cfg(any(target_arch = "wasm32", test))]
fn build_wait_interests(
    listener: i32,
    signal: i32,
    mouse: Option<i32>,
    keyboard: Option<i32>,
    watch_fds: &[i32],
    clients: &[(i32, bool)],
    transport_only: bool,
) -> Result<Vec<(i32, u8)>, i32> {
    let mut interests = Vec::with_capacity(4 + watch_fds.len() + clients.len() * 2);
    interests.push((listener, abi::wasi::eventtype::FD_READ));
    if !transport_only {
        interests.push((signal, abi::wasi::eventtype::FD_READ));
        if let Some(fd) = mouse {
            interests.push((fd, abi::wasi::eventtype::FD_READ));
        }
        if let Some(fd) = keyboard {
            interests.push((fd, abi::wasi::eventtype::FD_READ));
        }
        interests.extend(
            watch_fds
                .iter()
                .copied()
                .map(|fd| (fd, abi::wasi::eventtype::FD_READ)),
        );
    }
    for (fd, outbound_pending) in clients.iter().copied() {
        interests.push((fd, abi::wasi::eventtype::FD_READ));
        if outbound_pending {
            interests.push((fd, abi::wasi::eventtype::FD_WRITE));
        }
    }
    if interests.len() > abi::wasi::poll::MAX_SUBSCRIPTIONS {
        return Err(abi::errno::EINVAL);
    }
    Ok(interests)
}

#[cfg(target_arch = "wasm32")]
struct VfsKeymapPreferenceSource;

#[cfg(target_arch = "wasm32")]
impl display_server::protocol::keymap::KeymapPreferenceSource for VfsKeymapPreferenceSource {
    fn read(
        &mut self,
    ) -> Result<Option<Vec<u8>>, display_server::protocol::keymap::KeymapPreferenceReadError> {
        use display_server::protocol::keymap::KeymapPreferenceReadError;
        use std::io::{ErrorKind, Read};

        const MAX_PREFERENCE_BYTES: u64 = 64 * 1024;
        let file = match std::fs::File::open(preferences::DEFAULT_PATH) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(KeymapPreferenceReadError::Unavailable),
        };
        let mut bytes = Vec::new();
        if file
            .take(MAX_PREFERENCE_BYTES + 1)
            .read_to_end(&mut bytes)
            .is_err()
            || bytes.len() as u64 > MAX_PREFERENCE_BYTES
        {
            return Err(KeymapPreferenceReadError::Unavailable);
        }
        Ok(Some(bytes))
    }
}

#[cfg(target_arch = "wasm32")]
struct SystemKeymapPreferenceClock {
    started: std::time::Instant,
}

#[cfg(target_arch = "wasm32")]
impl SystemKeymapPreferenceClock {
    fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl display_server::protocol::keymap::KeymapPreferenceClock for SystemKeymapPreferenceClock {
    fn monotonic_ms(&mut self) -> u64 {
        self.started.elapsed().as_millis().min(u64::MAX as u128) as u64
    }
}

/// Drain every available input event from the kernel's
/// `/dev/input/{mouse,kbd}` rings and inject each into
/// `server`. Returns true when at least one event was
/// processed. Routing an event is not itself framebuffer
/// damage: direct pointer-driven scene changes advance the
/// server's recomposition serial, while client reactions are
/// presented after their later surface commits.
#[cfg(target_arch = "wasm32")]
unsafe fn drain_input_events(
    server: &mut display_server::Server,
    mouse_fd: i32,
    kbd_fd: i32,
) -> bool {
    let mut any = false;
    // Mouse events are 20 bytes each; read up to 32 events (640
    // bytes) per drain to keep the buffer small while not
    // capping common sequences.
    let mut mouse_buf = [0u8; 640];
    let iov = Iovec {
        buf: mouse_buf.as_mut_ptr(),
        buf_len: mouse_buf.len() as u32,
    };
    let mut nread: u32 = 0;
    let rc = fd_read(mouse_fd, &iov, 1, &mut nread);
    if rc == 0 && nread > 0 {
        let was_dragging = server.is_dragging();
        let n = display_server::drain_mouse_events_into(&mouse_buf[..nread as usize], server);
        if n > 0 {
            any = true;
        }
        if was_dragging && !server.is_dragging() {
            println!("display-server: drag completed");
        }
    }
    // Keyboard events are 8 bytes; read up to 64 (512 bytes).
    let mut kbd_buf = [0u8; 512];
    let iov = Iovec {
        buf: kbd_buf.as_mut_ptr(),
        buf_len: kbd_buf.len() as u32,
    };
    let mut nread: u32 = 0;
    let rc = fd_read(kbd_fd, &iov, 1, &mut nread);
    if rc == 0 && nread > 0 {
        let n = display_server::drain_kbd_events_into(&kbd_buf[..nread as usize], server);
        if n > 0 {
            any = true;
        }
    }
    any
}

/// FB driver op code for set-mode (matches OP_SET_MODE in
/// `web/src/drivers/fb.ts`).
#[cfg(target_arch = "wasm32")]
const FB_OP_SET_MODE: u8 = 0x01;
/// FB driver op code for blit (matches OP_BLIT in
/// `web/src/drivers/fb.ts`).
#[cfg(target_arch = "wasm32")]
#[allow(dead_code)]
const FB_OP_BLIT: u8 = 0x02;
/// Begin a chunked blit (allocates accumulator). Payload:
/// `(width: u32 LE, height: u32 LE)`.
#[cfg(target_arch = "wasm32")]
const FB_OP_BLIT_BEGIN: u8 = 0x03;
/// Append pixels to the chunked blit. Payload:
/// `(offset: u32 LE) || pixel_bytes`.
#[cfg(target_arch = "wasm32")]
const FB_OP_BLIT_CHUNK: u8 = 0x04;
/// Finalize the chunked blit — driver posts `fb:blit` to main.
#[cfg(target_arch = "wasm32")]
const FB_OP_BLIT_END: u8 = 0x05;
/// Paint one independently-decodable framebuffer rectangle.
/// Payload: `(x, y, width, height: u32 LE) || RGBA8`.
#[cfg(target_arch = "wasm32")]
const FB_OP_PATCH: u8 = 0x06;
/// Paint one RLE-compressed framebuffer rectangle. Payload geometry matches
/// `FB_OP_PATCH`; each following run is `(count: u32 LE, rgba: [u8; 4])`.
#[cfg(any(target_arch = "wasm32", test))]
const FB_OP_PATCH_RLE: u8 = 0x07;
/// Paint multiple disjoint rectangles atomically. Payload starts with
/// `(rect_count: u8, palette_count_minus_one: u8)`, then one shared RGBA8
/// palette, followed by each rectangle's 16-byte geometry and row-major
/// `(count: u16, palette_index: u8)` runs.
#[cfg(any(target_arch = "wasm32", test))]
const FB_OP_PATCH_PALETTE_RLE_BATCH: u8 = 0x08;
/// Emit a generic presentation-ordering fence without changing pixels.
/// Payload: `(serial: u32 LE)`. Desktop policy remains in the authenticated
/// display protocol request that asks the server to schedule this marker.
#[cfg(any(target_arch = "wasm32", test))]
const FB_OP_PRESENT_FENCE: u8 = 0x09;

#[cfg(any(target_arch = "wasm32", test))]
const FB_MAX_COMMAND_BYTES: usize = 32 * 1024;
#[cfg(any(target_arch = "wasm32", test))]
const FB_PATCH_COMMAND_HEADER_BYTES: usize = 1 + 4 * 4;
#[cfg(any(target_arch = "wasm32", test))]
const FB_RLE_RUN_BYTES: usize = 4 + 4;
#[cfg(any(target_arch = "wasm32", test))]
const FB_PALETTE_BATCH_HEADER_BYTES: usize = 1 + 2;
#[cfg(any(target_arch = "wasm32", test))]
const FB_PALETTE_BATCH_RECT_HEADER_BYTES: usize = 4 * 4;
#[cfg(any(target_arch = "wasm32", test))]
const FB_PALETTE_BATCH_RUN_BYTES: usize = 2 + 1;
#[cfg(any(target_arch = "wasm32", test))]
const FB_PALETTE_BATCH_MAX_RECTS: usize = 8;
#[cfg(any(target_arch = "wasm32", test))]
const FB_PALETTE_BATCH_MAX_COLORS: usize = 256;

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct DamageRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

/// Return the smallest pixel-aligned rectangle containing every
/// byte difference between two equally-sized RGBA framebuffers.
/// Geometry or length changes conservatively invalidate the whole
/// output. Equal scanlines use slice comparison so the common
/// mostly-static desktop case avoids per-pixel work.
#[cfg(test)]
fn changed_bounds(previous: &[u8], current: &[u8], width: u32, height: u32) -> Option<DamageRect> {
    let expected = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    if expected == 0 {
        return None;
    }
    if previous.len() != expected || current.len() != expected {
        return Some(DamageRect {
            x: 0,
            y: 0,
            width,
            height,
        });
    }

    let row_bytes = width as usize * 4;
    let mut min_x = width;
    let mut min_y = height;
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    let mut any = false;

    for y in 0..height {
        let start = y as usize * row_bytes;
        let previous_row = &previous[start..start + row_bytes];
        let current_row = &current[start..start + row_bytes];
        let Some((left, right)) = changed_row_bounds(previous_row, current_row, width) else {
            continue;
        };

        any = true;
        min_x = min_x.min(left);
        min_y = min_y.min(y);
        max_x = max_x.max(right);
        max_y = max_y.max(y + 1);
    }

    if any {
        Some(DamageRect {
            x: min_x,
            y: min_y,
            width: max_x - min_x,
            height: max_y - min_y,
        })
    } else {
        None
    }
}

#[cfg(any(target_arch = "wasm32", test))]
fn rows_equal_u64(previous: &[u8], current: &[u8]) -> bool {
    if previous.len() != current.len() {
        return false;
    }
    let mut offset = 0usize;
    while offset + 8 <= previous.len() {
        let left = u64::from_ne_bytes(
            previous[offset..offset + 8]
                .try_into()
                .expect("exact word slice"),
        );
        let right = u64::from_ne_bytes(
            current[offset..offset + 8]
                .try_into()
                .expect("exact word slice"),
        );
        if left != right {
            return false;
        }
        offset += 8;
    }
    previous[offset..] == current[offset..]
}

#[cfg(any(target_arch = "wasm32", test))]
fn changed_row_bounds(previous: &[u8], current: &[u8], width: u32) -> Option<(u32, u32)> {
    if rows_equal_u64(previous, current) {
        return None;
    }
    let mut left = 0u32;
    while left < width {
        let offset = left as usize * 4;
        if previous[offset..offset + 4] != current[offset..offset + 4] {
            break;
        }
        left += 1;
    }
    let mut right = width;
    while right > left {
        let offset = (right - 1) as usize * 4;
        if previous[offset..offset + 4] != current[offset..offset + 4] {
            break;
        }
        right -= 1;
    }
    Some((left, right))
}

/// Return vertically contiguous changed-row bands, keeping unchanged gaps out
/// of transport. Adversarial fragmentation is bounded: once `max_bands` would
/// be exceeded, the result coalesces to one conservative bounding rectangle.
#[cfg(any(target_arch = "wasm32", test))]
fn changed_bands(
    previous: &[u8],
    current: &[u8],
    width: u32,
    height: u32,
    max_bands: usize,
) -> Vec<DamageRect> {
    let Some(expected) = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
    else {
        return Vec::new();
    };
    if expected == 0 {
        return Vec::new();
    }
    if previous.len() != expected || current.len() != expected {
        return vec![DamageRect {
            x: 0,
            y: 0,
            width,
            height,
        }];
    }

    let row_bytes = width as usize * 4;
    let mut bands = Vec::with_capacity(max_bands.min(8));
    let mut current_band: Option<DamageRect> = None;
    let mut global: Option<DamageRect> = None;
    let mut fragmented = max_bands == 0;
    for y in 0..height {
        let start = y as usize * row_bytes;
        let Some((left, right)) = changed_row_bounds(
            &previous[start..start + row_bytes],
            &current[start..start + row_bytes],
            width,
        ) else {
            if let Some(band) = current_band.take() {
                if bands.len() < max_bands {
                    bands.push(band);
                } else {
                    fragmented = true;
                }
            }
            continue;
        };

        global = Some(match global {
            Some(bounds) => {
                let x = bounds.x.min(left);
                let right = (bounds.x + bounds.width).max(right);
                DamageRect {
                    x,
                    y: bounds.y,
                    width: right - x,
                    height: y + 1 - bounds.y,
                }
            }
            None => DamageRect {
                x: left,
                y,
                width: right - left,
                height: 1,
            },
        });

        current_band = Some(match current_band {
            Some(band) => {
                let x = band.x.min(left);
                let right = (band.x + band.width).max(right);
                DamageRect {
                    x,
                    y: band.y,
                    width: right - x,
                    height: y + 1 - band.y,
                }
            }
            None => DamageRect {
                x: left,
                y,
                width: right - left,
                height: 1,
            },
        });
    }
    if let Some(band) = current_band {
        if bands.len() < max_bands {
            bands.push(band);
        } else {
            fragmented = true;
        }
    }
    if fragmented {
        global.into_iter().collect()
    } else {
        bands
    }
}

/// Compare only server-validated output-space candidates. Full-scene
/// recomposition still ran before this function; the candidates merely bound
/// the equality proof against the last presented shadow.
#[cfg(any(target_arch = "wasm32", test))]
fn changed_bands_bounded(
    previous: &[u8],
    current: &[u8],
    width: u32,
    height: u32,
    candidates: &[display_server::OutputDamageRect],
    max_bands: usize,
) -> Vec<DamageRect> {
    let Some(expected) = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
    else {
        return Vec::new();
    };
    if expected == 0 {
        return Vec::new();
    }
    if previous.len() != expected || current.len() != expected {
        return vec![DamageRect {
            x: 0,
            y: 0,
            width,
            height,
        }];
    }
    if candidates.is_empty() {
        return Vec::new();
    }

    let framebuffer_row_bytes = width as usize * 4;
    let mut bands = Vec::with_capacity(max_bands.min(8));
    let mut global: Option<DamageRect> = None;
    let mut fragmented = max_bands == 0;
    for candidate in candidates {
        let x = candidate.x.min(width);
        let y = candidate.y.min(height);
        let right = candidate.x.saturating_add(candidate.width).min(width);
        let bottom = candidate.y.saturating_add(candidate.height).min(height);
        if right <= x || bottom <= y {
            continue;
        }
        let candidate_width = right - x;
        let x_bytes = x as usize * 4;
        let candidate_row_bytes = candidate_width as usize * 4;
        let mut current_band: Option<DamageRect> = None;
        for row in y..bottom {
            let start = row as usize * framebuffer_row_bytes + x_bytes;
            let Some((local_left, local_right)) = changed_row_bounds(
                &previous[start..start + candidate_row_bytes],
                &current[start..start + candidate_row_bytes],
                candidate_width,
            ) else {
                if let Some(band) = current_band.take() {
                    if bands.len() < max_bands {
                        bands.push(band);
                    } else {
                        fragmented = true;
                    }
                }
                continue;
            };
            let left = x + local_left;
            let right = x + local_right;
            global = Some(match global {
                Some(bounds) => {
                    let union_x = bounds.x.min(left);
                    let union_right = (bounds.x + bounds.width).max(right);
                    DamageRect {
                        x: union_x,
                        y: bounds.y.min(row),
                        width: union_right - union_x,
                        height: (bounds.y + bounds.height).max(row + 1) - bounds.y.min(row),
                    }
                }
                None => DamageRect {
                    x: left,
                    y: row,
                    width: right - left,
                    height: 1,
                },
            });
            current_band = Some(match current_band {
                Some(band) => {
                    let union_x = band.x.min(left);
                    let union_right = (band.x + band.width).max(right);
                    DamageRect {
                        x: union_x,
                        y: band.y,
                        width: union_right - union_x,
                        height: row + 1 - band.y,
                    }
                }
                None => DamageRect {
                    x: left,
                    y: row,
                    width: right - left,
                    height: 1,
                },
            });
        }
        if let Some(band) = current_band {
            if bands.len() < max_bands {
                bands.push(band);
            } else {
                fragmented = true;
            }
        }
    }
    if fragmented {
        global.into_iter().collect()
    } else {
        bands
    }
}

/// Encode one damage rectangle as bounded pixel runs. Returns `None` unless
/// the complete command both fits the syscall heap and is strictly smaller
/// than the equivalent raw patch, allowing the presenter to fall back to its
/// existing raw-strip path without partial output.
#[cfg(any(target_arch = "wasm32", test))]
fn encode_rle_patch(
    pixels: &[u8],
    framebuffer_width: u32,
    framebuffer_height: u32,
    damage: DamageRect,
    max_command_bytes: usize,
) -> Option<Vec<u8>> {
    if damage.width == 0 || damage.height == 0 {
        return None;
    }
    let right = damage.x.checked_add(damage.width)?;
    let bottom = damage.y.checked_add(damage.height)?;
    if right > framebuffer_width || bottom > framebuffer_height {
        return None;
    }
    let framebuffer_bytes = (framebuffer_width as usize)
        .checked_mul(framebuffer_height as usize)?
        .checked_mul(4)?;
    if pixels.len() != framebuffer_bytes {
        return None;
    }
    let damage_pixels = (damage.width as usize).checked_mul(damage.height as usize)?;
    let raw_command_bytes =
        FB_PATCH_COMMAND_HEADER_BYTES.checked_add(damage_pixels.checked_mul(4)?)?;
    if max_command_bytes < FB_PATCH_COMMAND_HEADER_BYTES + FB_RLE_RUN_BYTES {
        return None;
    }

    let mut command = Vec::with_capacity(max_command_bytes.min(raw_command_bytes));
    command.push(FB_OP_PATCH_RLE);
    command.extend_from_slice(&damage.x.to_le_bytes());
    command.extend_from_slice(&damage.y.to_le_bytes());
    command.extend_from_slice(&damage.width.to_le_bytes());
    command.extend_from_slice(&damage.height.to_le_bytes());

    let framebuffer_row_bytes = (framebuffer_width as usize).checked_mul(4)?;
    let mut run_color: Option<[u8; 4]> = None;
    let mut run_count = 0u32;
    let flush_run = |command: &mut Vec<u8>, color: [u8; 4], count: u32| -> Option<()> {
        let next_len = command.len().checked_add(FB_RLE_RUN_BYTES)?;
        if next_len > max_command_bytes || next_len >= raw_command_bytes {
            return None;
        }
        command.extend_from_slice(&count.to_le_bytes());
        command.extend_from_slice(&color);
        Some(())
    };

    for y in damage.y as usize..bottom as usize {
        for x in damage.x as usize..right as usize {
            let offset = y * framebuffer_row_bytes + x * 4;
            let color: [u8; 4] = pixels[offset..offset + 4].try_into().ok()?;
            match run_color {
                Some(current) if current == color && run_count < u32::MAX => {
                    run_count += 1;
                }
                Some(current) => {
                    flush_run(&mut command, current, run_count)?;
                    run_color = Some(color);
                    run_count = 1;
                }
                None => {
                    run_color = Some(color);
                    run_count = 1;
                }
            }
        }
    }
    flush_run(&mut command, run_color?, run_count)?;
    Some(command)
}

/// Encode disjoint damage rectangles into one independently-decodable command.
/// A shared palette and compact indexed runs keep common desktop chrome under
/// the one-write syscall limit while preserving one atomic presentation. Any
/// geometry, palette, or encoded-size overflow returns `None` so the caller can
/// use the existing per-band RLE/raw paths unchanged.
#[cfg(any(target_arch = "wasm32", test))]
fn encode_palette_rle_patch_batch(
    pixels: &[u8],
    framebuffer_width: u32,
    framebuffer_height: u32,
    damages: &[DamageRect],
    max_command_bytes: usize,
) -> Option<Vec<u8>> {
    use std::collections::BTreeMap;

    if damages.len() < 2 || damages.len() > FB_PALETTE_BATCH_MAX_RECTS {
        return None;
    }
    let framebuffer_bytes = (framebuffer_width as usize)
        .checked_mul(framebuffer_height as usize)?
        .checked_mul(4)?;
    if pixels.len() != framebuffer_bytes {
        return None;
    }

    let mut encoded_len = FB_PALETTE_BATCH_HEADER_BYTES.checked_add(
        damages
            .len()
            .checked_mul(FB_PALETTE_BATCH_RECT_HEADER_BYTES)?,
    )?;
    if encoded_len > max_command_bytes {
        return None;
    }

    let framebuffer_row_bytes = (framebuffer_width as usize).checked_mul(4)?;
    let mut palette = Vec::<[u8; 4]>::new();
    let mut palette_indexes = BTreeMap::<[u8; 4], u8>::new();
    let mut encoded_runs = Vec::<Vec<(u16, u8)>>::with_capacity(damages.len());

    for damage in damages {
        if damage.width == 0 || damage.height == 0 {
            return None;
        }
        let right = damage.x.checked_add(damage.width)?;
        let bottom = damage.y.checked_add(damage.height)?;
        if right > framebuffer_width || bottom > framebuffer_height {
            return None;
        }

        let mut runs = Vec::<(u16, u8)>::new();
        let mut current_index = None;
        let mut current_count = 0u16;
        let flush_run = |runs: &mut Vec<(u16, u8)>,
                         encoded_len: &mut usize,
                         index: u8,
                         count: u16|
         -> Option<()> {
            *encoded_len = encoded_len.checked_add(FB_PALETTE_BATCH_RUN_BYTES)?;
            if *encoded_len > max_command_bytes {
                return None;
            }
            runs.push((count, index));
            Some(())
        };

        for y in damage.y as usize..bottom as usize {
            for x in damage.x as usize..right as usize {
                let offset = y * framebuffer_row_bytes + x * 4;
                let color: [u8; 4] = pixels[offset..offset + 4].try_into().ok()?;
                let index = match palette_indexes.get(&color).copied() {
                    Some(index) => index,
                    None => {
                        if palette.len() == FB_PALETTE_BATCH_MAX_COLORS {
                            return None;
                        }
                        let index = palette.len() as u8;
                        palette.push(color);
                        palette_indexes.insert(color, index);
                        encoded_len = encoded_len.checked_add(4)?;
                        if encoded_len > max_command_bytes {
                            return None;
                        }
                        index
                    }
                };

                match current_index {
                    Some(current) if current == index && current_count < u16::MAX => {
                        current_count += 1;
                    }
                    Some(current) => {
                        flush_run(&mut runs, &mut encoded_len, current, current_count)?;
                        current_index = Some(index);
                        current_count = 1;
                    }
                    None => {
                        current_index = Some(index);
                        current_count = 1;
                    }
                }
            }
        }
        flush_run(&mut runs, &mut encoded_len, current_index?, current_count)?;
        encoded_runs.push(runs);
    }

    let mut command = Vec::with_capacity(encoded_len);
    command.push(FB_OP_PATCH_PALETTE_RLE_BATCH);
    command.push(damages.len() as u8);
    command.push((palette.len() - 1) as u8);
    for color in palette {
        command.extend_from_slice(&color);
    }
    for (damage, runs) in damages.iter().zip(encoded_runs) {
        command.extend_from_slice(&damage.x.to_le_bytes());
        command.extend_from_slice(&damage.y.to_le_bytes());
        command.extend_from_slice(&damage.width.to_le_bytes());
        command.extend_from_slice(&damage.height.to_le_bytes());
        for (count, index) in runs {
            command.extend_from_slice(&count.to_le_bytes());
            command.push(index);
        }
    }
    debug_assert_eq!(command.len(), encoded_len);
    Some(command)
}

#[cfg(any(target_arch = "wasm32", test))]
fn advance_presented_damage(
    presented: &mut [u8],
    pixels: &[u8],
    framebuffer_width: u32,
    damage: DamageRect,
) -> bool {
    if presented.len() != pixels.len() {
        return false;
    }
    let Some(bottom) = damage.y.checked_add(damage.height) else {
        return false;
    };
    let Some(row_bytes) = (damage.width as usize).checked_mul(4) else {
        return false;
    };
    let Some(framebuffer_row_bytes) = (framebuffer_width as usize).checked_mul(4) else {
        return false;
    };
    let Some(damage_x_bytes) = (damage.x as usize).checked_mul(4) else {
        return false;
    };
    for y in damage.y as usize..bottom as usize {
        let Some(start) = y
            .checked_mul(framebuffer_row_bytes)
            .and_then(|row| row.checked_add(damage_x_bytes))
        else {
            return false;
        };
        let Some(end) = start.checked_add(row_bytes) else {
            return false;
        };
        let (Some(source), Some(destination)) =
            (pixels.get(start..end), presented.get_mut(start..end))
        else {
            return false;
        };
        destination.copy_from_slice(source);
    }
    true
}

/// Send `OP_SET_MODE(width, height)` to the framebuffer
/// driver. Called once at startup so the host-side renderer
/// knows the framebuffer's pixel dimensions before the first
/// blit lands. Payload layout (matching
/// `crates/kernel/src/dev/mod.rs::framebuffer_write` +
/// `web/src/drivers/fb.ts::handleSetMode`):
///
///   ```text
///   [op:u8 = OP_SET_MODE] [width:u32 LE] [height:u32 LE]
///   ```
#[cfg(target_arch = "wasm32")]
unsafe fn fb_set_mode(fb_fd: i32, width: u32, height: u32) -> bool {
    let mut buf = [0u8; 9];
    buf[0] = FB_OP_SET_MODE;
    buf[1..5].copy_from_slice(&width.to_le_bytes());
    buf[5..9].copy_from_slice(&height.to_le_bytes());
    let iov = Ciovec {
        buf: buf.as_ptr(),
        buf_len: buf.len() as u32,
    };
    let mut written: u32 = 0;
    let rc = fd_write(fb_fd, &iov, 1, &mut written);
    rc == 0
}

#[cfg(any(target_arch = "wasm32", test))]
fn encode_present_fence(serial: u32) -> [u8; 5] {
    let mut command = [0u8; 5];
    command[0] = FB_OP_PRESENT_FENCE;
    command[1..].copy_from_slice(&serial.to_le_bytes());
    command
}

/// Present the server's composed framebuffer to `/dev/fb0`
/// using the chunked-blit op sequence. The SAB ring's per-
/// syscall heap window is 32 KiB; a full frame (e.g. 800×600
/// = 1.9 MiB) doesn't fit in one fd_write, so the binary
/// splits the frame into a `BEGIN` + N × `CHUNK` + `END`
/// sequence. The FB driver's TS side accumulates the chunks
/// into a single `fb:blit` postMessage, indistinguishable
/// from a non-chunked blit on the receiving side.
///
/// Returns `true` if every chunk's fd_write succeeded.
#[cfg(target_arch = "wasm32")]
unsafe fn present_framebuffer(
    server: &display_server::Server,
    fb_fd: i32,
    presented: &mut Vec<u8>,
) -> bool {
    /// Per-fd_write payload cap. Must fit alongside the
    /// 4-byte (`offset: u32`) chunk header in the SAB ring's
    /// 32 KiB heap window. 24 KiB leaves comfortable headroom
    /// for the request slot's other fields.
    const CHUNK_BYTES: usize = 24 * 1024;
    let fb = server.framebuffer();
    let width = fb.width();
    let height = fb.height();
    let pixels = fb.pixels();
    const MAX_PRESENT_BANDS: usize = 8;
    let damages = match server.presentation_damage() {
        display_server::PresentationDamage::Full => {
            changed_bands(presented, pixels, width, height, MAX_PRESENT_BANDS)
        }
        display_server::PresentationDamage::Bounded(candidates) => changed_bands_bounded(
            presented,
            pixels,
            width,
            height,
            candidates,
            MAX_PRESENT_BANDS,
        ),
    };
    if damages.is_empty() {
        return true;
    }
    if presented.len() == pixels.len() {
        if let Some(command) =
            encode_palette_rle_patch_batch(pixels, width, height, &damages, FB_MAX_COMMAND_BYTES)
        {
            let iov = Ciovec {
                buf: command.as_ptr(),
                buf_len: command.len() as u32,
            };
            let mut written: u32 = 0;
            let rc = fd_write(fb_fd, &iov, 1, &mut written);
            if rc != 0 || written as usize != command.len() {
                return false;
            }
            for damage in damages {
                if !advance_presented_damage(presented, pixels, width, damage) {
                    return false;
                }
            }
            return true;
        }
    }
    let full_pixels = width as u64 * height as u64;
    let allow_full_for_large_damage = damages.len() == 1;

    for damage in damages {
        let damage_pixels = damage.width as u64 * damage.height as u64;
        let require_full = presented.len() != pixels.len()
            || (allow_full_for_large_damage && damage_pixels.saturating_mul(2) >= full_pixels);

        if presented.len() == pixels.len() {
            if let Some(command) =
                encode_rle_patch(pixels, width, height, damage, FB_MAX_COMMAND_BYTES)
            {
                let iov = Ciovec {
                    buf: command.as_ptr(),
                    buf_len: command.len() as u32,
                };
                let mut written: u32 = 0;
                let rc = fd_write(fb_fd, &iov, 1, &mut written);
                if rc != 0 || written as usize != command.len() {
                    return false;
                }
                if !advance_presented_damage(presented, pixels, width, damage) {
                    return false;
                }
                continue;
            }
        }

        if !require_full {
            // The complete fd_write, including opcode and geometry, must
            // fit the kernel host's 32 KiB heap scratch window.
            const MAX_PATCH_RGBA_BYTES: usize =
                FB_MAX_COMMAND_BYTES - FB_PATCH_COMMAND_HEADER_BYTES;
            let row_bytes = damage.width as usize * 4;
            if row_bytes == 0 || row_bytes > MAX_PATCH_RGBA_BYTES {
                return false;
            }
            let rows_per_patch = (MAX_PATCH_RGBA_BYTES / row_bytes).max(1);
            let framebuffer_row_bytes = width as usize * 4;
            let mut row = 0usize;
            while row < damage.height as usize {
                let strip_height = core::cmp::min(rows_per_patch, damage.height as usize - row);
                let strip_y = damage.y as usize + row;
                let mut command =
                    Vec::with_capacity(FB_PATCH_COMMAND_HEADER_BYTES + row_bytes * strip_height);
                command.push(FB_OP_PATCH);
                command.extend_from_slice(&damage.x.to_le_bytes());
                command.extend_from_slice(&(strip_y as u32).to_le_bytes());
                command.extend_from_slice(&damage.width.to_le_bytes());
                command.extend_from_slice(&(strip_height as u32).to_le_bytes());
                for source_y in strip_y..strip_y + strip_height {
                    let source_start = source_y * framebuffer_row_bytes + damage.x as usize * 4;
                    command.extend_from_slice(&pixels[source_start..source_start + row_bytes]);
                }
                let iov = Ciovec {
                    buf: command.as_ptr(),
                    buf_len: command.len() as u32,
                };
                let mut written: u32 = 0;
                let rc = fd_write(fb_fd, &iov, 1, &mut written);
                if rc != 0 || written as usize != command.len() {
                    return false;
                }
                row += strip_height;
            }

            // Advance only this successfully-presented band in the shadow.
            if !advance_presented_damage(presented, pixels, width, damage) {
                return false;
            }
            continue;
        }

        // BEGIN: payload (width, height).
        {
            let mut hdr = [0u8; 9];
            hdr[0] = FB_OP_BLIT_BEGIN;
            hdr[1..5].copy_from_slice(&width.to_le_bytes());
            hdr[5..9].copy_from_slice(&height.to_le_bytes());
            let iov = Ciovec {
                buf: hdr.as_ptr(),
                buf_len: hdr.len() as u32,
            };
            let mut written: u32 = 0;
            let rc = fd_write(fb_fd, &iov, 1, &mut written);
            if rc != 0 || written as usize != hdr.len() {
                return false;
            }
        }

        // CHUNK: walk the pixels, sending CHUNK_BYTES at a time.
        let mut offset: usize = 0;
        while offset < pixels.len() {
            let end = core::cmp::min(offset + CHUNK_BYTES, pixels.len());
            let slice = &pixels[offset..end];
            let mut buf: Vec<u8> = Vec::with_capacity(5 + slice.len());
            buf.push(FB_OP_BLIT_CHUNK);
            buf.extend_from_slice(&(offset as u32).to_le_bytes());
            buf.extend_from_slice(slice);
            let iov = Ciovec {
                buf: buf.as_ptr(),
                buf_len: buf.len() as u32,
            };
            let mut written: u32 = 0;
            let rc = fd_write(fb_fd, &iov, 1, &mut written);
            if rc != 0 || written as usize != buf.len() {
                return false;
            }
            offset = end;
        }

        // END: payload empty. Driver posts the assembled blit to main.
        {
            let hdr = [FB_OP_BLIT_END];
            let iov = Ciovec {
                buf: hdr.as_ptr(),
                buf_len: hdr.len() as u32,
            };
            let mut written: u32 = 0;
            let rc = fd_write(fb_fd, &iov, 1, &mut written);
            if rc != 0 || written as usize != hdr.len() {
                return false;
            }
        }
        presented.clear();
        presented.extend_from_slice(pixels);
        return true;
    }
    true
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Debug, Default)]
struct PresentationSchedule {
    frame_dirty: bool,
    scene_dirty: bool,
}

#[cfg(any(target_arch = "wasm32", test))]
impl PresentationSchedule {
    #[cfg(test)]
    fn mark_frame_dirty(&mut self) {
        self.frame_dirty = true;
    }

    fn mark_scene_if(&mut self, changed: bool) {
        self.scene_dirty |= changed;
    }

    fn take_scene_ready(&mut self, deferred: bool) -> bool {
        if deferred || !self.scene_dirty {
            return false;
        }
        self.frame_dirty = false;
        self.scene_dirty = false;
        true
    }

    fn take_ready(&mut self, deferred: bool) -> bool {
        if deferred || (!self.frame_dirty && !self.scene_dirty) {
            return false;
        }
        self.frame_dirty = false;
        self.scene_dirty = false;
        true
    }

    fn present_fence_ready(&self, deferred: bool) -> bool {
        !deferred && !self.frame_dirty && !self.scene_dirty
    }
}

/// Cross the frame-callback lifecycle boundary only after the framebuffer
/// command sequence has succeeded. `present_framebuffer` reports success for
/// an unchanged scene as an equality proof against its last presented shadow,
/// so that path completes callbacks without manufacturing a write. Deferred
/// focus transactions remain ineligible.
#[cfg(any(target_arch = "wasm32", test))]
fn complete_successful_frame_presentation(
    server: &mut display_server::Server,
    present_succeeded: bool,
    deferred: bool,
    callback_data: u32,
    remaining: &mut usize,
) -> Result<usize, ()> {
    if !present_succeeded {
        return Err(());
    }
    if deferred {
        return Ok(0);
    }
    server.clear_presentation_damage();
    server.mark_frame_callbacks_presented(callback_data);
    Ok(server.complete_presented_frame_callbacks(remaining))
}

#[cfg(any(target_arch = "wasm32", test))]
fn frame_callback_lifecycle_requires_local_turn(
    server: &display_server::Server,
    completed_after_transport: usize,
) -> bool {
    completed_after_transport > 0 || server.ready_frame_callback_lifecycle_can_progress()
}

#[cfg(test)]
fn take_ready_present_fence(
    server: &mut display_server::Server,
    presentation: &PresentationSchedule,
) -> Option<u32> {
    let deferred = server.presentation_deferred();
    take_ready_present_fence_with_deferred(server, presentation, deferred)
}

#[cfg(any(target_arch = "wasm32", test))]
fn take_ready_present_fence_with_deferred(
    server: &mut display_server::Server,
    presentation: &PresentationSchedule,
    deferred: bool,
) -> Option<u32> {
    presentation
        .present_fence_ready(deferred)
        .then(|| server.take_pending_present_fence())
        .flatten()
}

#[cfg(any(target_arch = "wasm32", test))]
fn drain_ready_present_fences_bounded<E>(
    server: &mut display_server::Server,
    presentation: &PresentationSchedule,
    deferred: bool,
    writes_remaining: &mut usize,
    mut emit: E,
) -> bool
where
    E: FnMut(u32) -> bool,
{
    while *writes_remaining > 0 {
        let Some(serial) = take_ready_present_fence_with_deferred(server, presentation, deferred)
        else {
            break;
        };
        if !emit(serial) {
            return false;
        }
        *writes_remaining -= 1;
    }
    true
}

/// Forward every authenticated shell readiness request whose preceding scene
/// work has reached a successful framebuffer presentation (or an equality
/// proof against the last successfully presented pixels). The framebuffer
/// command is deliberately generic; the browser interprets its serial only as
/// an in-order presentation fence.
#[cfg(target_arch = "wasm32")]
unsafe fn emit_ready_present_fences(
    server: &mut display_server::Server,
    presentation: &PresentationSchedule,
    fb_fd: i32,
    writes_remaining: &mut usize,
) -> bool {
    let deferred = server.presentation_deferred();
    drain_ready_present_fences_bounded(server, presentation, deferred, writes_remaining, |serial| {
        let command = encode_present_fence(serial);
        let iov = Ciovec {
            buf: command.as_ptr(),
            buf_len: command.len() as u32,
        };
        let mut written = 0u32;
        let rc = fd_write(fb_fd, &iov, 1, &mut written);
        rc == 0 && written as usize == command.len()
    })
}

#[cfg(target_arch = "wasm32")]
fn main() {
    println!("display-server starting");

    const EAGAIN: i32 = 6;
    const EINTR: i32 = 27;

    unsafe {
        let listener = display_bind();
        if listener < 0 {
            std::process::exit(10);
        }

        const FB_PATH: &[u8] = b"/dev/fb0";
        let mut fb_fd: u32 = 0;
        let rc = path_open(
            0,
            0,
            FB_PATH.as_ptr(),
            FB_PATH.len() as i32,
            0,
            0,
            0,
            0,
            &mut fb_fd,
        );
        if rc != 0 {
            std::process::exit(15);
        }

        // T133 finishing touch: input device fds. Open
        // `/dev/input/{mouse,kbd}` so the server can drain
        // pointer + keyboard events between accept iterations
        // and inject them through Server::inject_*. If either
        // device is missing (older kernel build) the open fails
        // and we fall back to "no input" mode — the protocol
        // path keeps working without input.
        // Note: the v1 devfs is FLAT — the input nodes live
        // at /dev/input_mouse and /dev/input_kbd (not under
        // a /dev/input/ subdirectory). See crates/kernel/
        // src/fs/devfs.rs.
        // T133: open /dev/input_mouse + /dev/input_kbd so the
        // server can drain pointer + keyboard events between
        // accept iterations and inject them through
        // Server::inject_*. When the open fails (older kernel
        // build, devfs missing the node) the server falls back
        // to "no input" mode silently.
        // Note: the v1 devfs is FLAT — the input nodes live at
        // /dev/input_mouse / /dev/input_kbd (not under a
        // /dev/input/ subdirectory). See crates/kernel/src/fs/
        // devfs.rs.
        let mouse_fd = open_dev(b"/dev/input_mouse")
            .map(|fd| fd as i32)
            .unwrap_or(-1);
        let kbd_fd = open_dev(b"/dev/input_kbd")
            .map(|fd| fd as i32)
            .unwrap_or(-1);

        // Compositor + protocol state. The library owns the
        // framebuffer + every client's surface tree.
        let mut keymap_preferences = display_server::protocol::keymap::KeymapPreferenceRuntime::new(
            VfsKeymapPreferenceSource,
            SystemKeymapPreferenceClock::new(),
        );
        let mut keymap_watch = match PreferenceWatch::new() {
            Ok(watch) => watch,
            Err(_) => std::process::exit(17),
        };
        let mut server = display_server::Server::new();
        let restore_clock = std::time::Instant::now();
        let _ = server.set_keyboard_layout(keymap_preferences.current());
        let mut presented_pixels = Vec::new();
        let mut i: u32 = 0;

        // Tell the host-side framebuffer driver about the
        // composed framebuffer's pixel dimensions BEFORE the
        // first present. The FB driver's TS side
        // (`web/src/drivers/fb.ts`) sizes its `OffscreenCanvas`
        // off the SET_MODE width/height; subsequent BLIT ops
        // must match these dimensions exactly.
        let fb_w = server.framebuffer().width();
        let fb_h = server.framebuffer().height();
        if !fb_set_mode(fb_fd as i32, fb_w, fb_h) {
            std::process::exit(16);
        }

        #[derive(Copy, Clone, PartialEq, Eq)]
        enum StreamMode {
            Undecided,
            Protocol,
        }

        // Connected clients we are multiplexing across. `pending`
        // is both the initial classification buffer and, once the
        // mode is locked to Protocol, the framed-message reassembly
        // buffer. Classification state is transport state; it never
        // depends on the client's diagnostic request journal.
        struct Conn {
            server_fd: i32,
            client_id: display_server::ClientId,
            pending: Vec<u8>,
            outbound: display_server::OutboundQueue,
            stream_mode: StreamMode,
            served: bool,
            read_ready: bool,
            write_ready: bool,
            disconnect_pending: bool,
        }
        let mut conns: Vec<Conn> = Vec::new();
        let poll_client_readiness = |conns: &mut [Conn],
                                     watch_fds: &[i32],
                                     timeout_ms: Option<u64>,
                                     transport_only: bool|
         -> Result<PollReadiness, i32> {
            let clients = conns
                .iter()
                .map(|conn| (conn.server_fd, !conn.outbound.is_empty()))
                .collect::<Vec<_>>();
            let interests = build_wait_interests(
                listener,
                SIGNAL_FD,
                (mouse_fd >= 0).then_some(mouse_fd),
                (kbd_fd >= 0).then_some(kbd_fd),
                watch_fds,
                &clients,
                transport_only,
            )?;
            let timeout_ns = timeout_ms.map(|ms| ms.saturating_mul(1_000_000));
            let ready = poll_for_activity(&interests, timeout_ns)?;
            let readiness = summarize_poll_readiness(listener, &ready);
            for (fd, event_type) in ready {
                let Some(conn) = conns.iter_mut().find(|conn| conn.server_fd == fd) else {
                    continue;
                };
                match event_type {
                    abi::wasi::eventtype::FD_READ => conn.read_ready = true,
                    abi::wasi::eventtype::FD_WRITE => conn.write_ready = true,
                    _ => {}
                }
            }
            Ok(readiness)
        };
        let mut request_cursor = 0usize;
        let mut transport_cursor = 0usize;
        let mut presentation = PresentationSchedule::default();
        let mut shutdown_drain = ShutdownDrain::default();
        let mut input_priority_used = false;
        let mut recv_buf = [0u8; 32 * 1024];
        let read_iov = Iovec {
            buf: recv_buf.as_mut_ptr(),
            buf_len: recv_buf.len() as u32,
        };

        'outer: loop {
            if poll_sigterm() {
                shutdown_drain
                    .request(restore_clock.elapsed().as_millis().min(u64::MAX as u128) as u64);
            }
            let mut present_fence_writes_remaining = PRESENT_FENCE_WRITES_PER_TURN;
            let mut frame_callback_lifecycle_remaining =
                display_server::MAX_FRAME_CALLBACK_COMPLETIONS_PER_TURN;

            // Logical state may advance many times in one turn (coalesced
            // pointer motion, protocol commits, or simultaneous disconnects),
            // but the complete framebuffer is rebuilt at most once before the
            // turn presents. Direct Server users remain immediate by default.
            server.begin_recomposition_batch();
            server.advance_monotonic_time(
                restore_clock.elapsed().as_millis().min(u64::MAX as u128) as u64,
            );
            let input_recomposition_serial = server.recomposition_serial();

            // Drain input events at the top of every iteration.
            // Pointer motion + button events feed the active-drag
            // machinery and the click-to-focus path; geometry-
            // dirtying events trigger a re-present after the
            // poll cycle.
            if !shutdown_drain.requested() && (mouse_fd >= 0 || kbd_fd >= 0) {
                let _ = drain_input_events(&mut server, mouse_fd, kbd_fd);
            }
            if server.recomposition_serial() != input_recomposition_serial && !input_priority_used {
                presentation.mark_scene_if(server.finish_recomposition_batch());
                let deferred = server.presentation_deferred();
                if presentation.take_ready(deferred) {
                    let present_succeeded =
                        present_framebuffer(&server, fb_fd as i32, &mut presented_pixels);
                    if complete_successful_frame_presentation(
                        &mut server,
                        present_succeeded,
                        deferred,
                        restore_clock.elapsed().as_millis() as u32,
                        &mut frame_callback_lifecycle_remaining,
                    )
                    .is_err()
                    {
                        std::process::exit(16);
                    }
                }
                if !emit_ready_present_fences(
                    &mut server,
                    &presentation,
                    fb_fd as i32,
                    &mut present_fence_writes_remaining,
                ) {
                    std::process::exit(16);
                }
                // Input-to-pixel never waits behind client transport work.
                // Pending protocol events remain ordered and are serviced on
                // the next input-first turn.
                input_priority_used = true;
                match poll_client_readiness(&mut conns, &keymap_watch.wait_fds(), Some(0), false) {
                    Ok(_) => {}
                    Err(errno) if errno == EINTR => {}
                    Err(_) => std::process::exit(18),
                }
                continue 'outer;
            }

            let keymap_changed = match keymap_watch.drain() {
                Ok(changed) => changed,
                Err(_) => std::process::exit(17),
            };
            if keymap_changed && keymap_preferences.refresh() {
                let layout = keymap_preferences.current();
                if server.set_keyboard_layout(layout).unwrap_or(false) {
                    println!("display-server: keymap changed {}", layout.as_str());
                }
            }

            // Admit a bounded batch. A still-readable listener remains in the
            // wait set for the next turn, while existing clients/input/present
            // cannot be starved by a perpetually replenished backlog.
            let accept_result = drain_accepts_bounded(
                || ipc_accept_nonblock(listener),
                |new_fd| {
                    // Derive authorization from credentials captured
                    // by the kernel on this IPC connection. Never
                    // accept capability claims in display-protocol
                    // bytes: an ordinary toolkit client controls all
                    // of those bytes. If credential lookup fails,
                    // fail this connection closed.
                    let mut caps_bits = 0u64;
                    if ipc_peer_caps(new_fd, &mut caps_bits) != 0 {
                        let _ = fd_close(new_fd);
                        return;
                    }
                    let mut peer_pid = 0i32;
                    if ipc_peer_pid(new_fd, &mut peer_pid) != 0 || peer_pid <= 0 {
                        let _ = fd_close(new_fd);
                        return;
                    }
                    let caps = abi::cap::CapSet(caps_bits);
                    let Ok(client_id) = server.try_accept_with_credentials(caps, peer_pid as u32)
                    else {
                        let _ = fd_close(new_fd);
                        return;
                    };
                    conns.push(Conn {
                        server_fd: new_fd,
                        client_id,
                        pending: Vec::new(),
                        outbound: display_server::OutboundQueue::new(),
                        stream_mode: StreamMode::Undecided,
                        served: false,
                        // An accepted socket may already contain the client's
                        // greeting; arm one read without waiting for a second
                        // readiness edge.
                        read_ready: true,
                        write_ready: false,
                        disconnect_pending: false,
                    });
                },
            );
            match accept_result {
                Ok(_) => {}
                Err(rc) if rc == -EAGAIN => {}
                Err(rc) if rc == -EINTR => {
                    if poll_sigterm() {
                        shutdown_drain.request(
                            restore_clock.elapsed().as_millis().min(u64::MAX as u128) as u64,
                        );
                    }
                }
                Err(_) => std::process::exit(12),
            }
            shutdown_drain.observe_accept_result(accept_result);

            // Service clients in rotating order. A client with already-framed
            // local work is never allowed to append another 32 KiB read until
            // that work is consumed. Per-client and global request quanta put
            // a hard ceiling on expensive valid requests such as
            // `surface.commit`, while the next outer turn begins with input.
            let visit_order = if conns.is_empty() {
                Vec::new()
            } else {
                rotating_indices(conns.len(), request_cursor)
                    .into_iter()
                    .map(|idx| conns[idx].client_id)
                    .collect::<Vec<_>>()
            };
            let mut requests_remaining = REQUESTS_PER_TURN;
            let mut client_reads_remaining = CLIENT_READ_ATTEMPTS_PER_TURN;
            let mut client_writes_remaining = CLIENT_WRITE_ATTEMPTS_PER_TURN;
            let mut disconnects_remaining = CLIENT_DISCONNECTS_PER_TURN;
            let mut local_pending_work = false;
            server.drain_ready_frame_callback_lifecycle(&mut frame_callback_lifecycle_remaining);
            let mut request_next_hint = request_cursor;
            for client_id in visit_order {
                if requests_remaining == 0 {
                    local_pending_work = true;
                    break;
                }
                let Some(idx) = conns.iter().position(|conn| conn.client_id == client_id) else {
                    continue;
                };
                let server_fd = conns[idx].server_fd;
                if conns[idx].disconnect_pending {
                    if disconnects_remaining == 0 {
                        local_pending_work = true;
                        continue;
                    }
                    let _ = fd_close(server_fd);
                    let generation = server.scene_generation();
                    let _ = server.disconnect(client_id);
                    presentation.mark_scene_if(server.scene_generation() != generation);
                    conns.swap_remove(idx);
                    disconnects_remaining -= 1;
                    local_pending_work = true;
                    request_next_hint = idx;
                    continue;
                }
                let buffered_complete = conns[idx].stream_mode == StreamMode::Protocol
                    && has_complete_protocol_request(&conns[idx].pending);

                if !buffered_complete && conns[idx].read_ready && client_reads_remaining == 0 {
                    local_pending_work = true;
                    continue;
                }
                if !buffered_complete && conns[idx].read_ready {
                    client_reads_remaining -= 1;
                    request_next_hint = idx.saturating_add(1);
                    conns[idx].read_ready = false;
                    let mut nread: u32 = 0;
                    let rc = fd_read(server_fd, &read_iov, 1, &mut nread);
                    if rc == 0 && nread > 0 {
                        conns[idx]
                            .pending
                            .extend_from_slice(&recv_buf[..nread as usize]);
                        if conns[idx].stream_mode == StreamMode::Undecided {
                            match display_server::classify_initial_stream(&conns[idx].pending) {
                                display_server::InitialStreamClassification::NeedMore => continue,
                                display_server::InitialStreamClassification::Protocol => {
                                    conns[idx].stream_mode = StreamMode::Protocol;
                                }
                                display_server::InitialStreamClassification::LegacyRawBlit => {
                                    let fb_iov = Ciovec {
                                        buf: conns[idx].pending.as_ptr(),
                                        buf_len: conns[idx].pending.len() as u32,
                                    };
                                    let mut fb_written: u32 = 0;
                                    let rc = fd_write(fb_fd as i32, &fb_iov, 1, &mut fb_written);
                                    if rc != 0 || fb_written != conns[idx].pending.len() as u32 {
                                        std::process::exit(16);
                                    }
                                    println!("display-server served client {}", i);
                                    i += 1;
                                    conns[idx].served = true;
                                    conns[idx].disconnect_pending = true;
                                }
                                display_server::InitialStreamClassification::Invalid => {
                                    conns[idx].disconnect_pending = true;
                                }
                            }
                        }
                    } else if rc != EAGAIN {
                        conns[idx].disconnect_pending = true;
                    }
                }

                if conns[idx].disconnect_pending {
                    if disconnects_remaining == 0 {
                        local_pending_work = true;
                        continue;
                    }
                    let _ = fd_close(server_fd);
                    let generation = server.scene_generation();
                    let _ = server.disconnect(client_id);
                    presentation.mark_scene_if(server.scene_generation() != generation);
                    conns.swap_remove(idx);
                    disconnects_remaining -= 1;
                    local_pending_work = true;
                    request_next_hint = idx;
                    continue;
                }

                let mut scene_mutated_this_client = false;
                if conns[idx].stream_mode == StreamMode::Protocol {
                    let generation = server.scene_generation();
                    let limit = REQUESTS_PER_CLIENT_PER_TURN.min(requests_remaining);
                    let drain = drain_protocol_requests_bounded(
                        &mut conns[idx].pending,
                        limit,
                        |message| {
                            let request_serial = server.recomposition_serial();
                            let _ = server.dispatch_request(client_id, message);
                            advertise_globals_for_get_registry(&mut server, client_id, message);
                            scene_mutated_this_client =
                                server.recomposition_serial() != request_serial;
                            !scene_mutated_this_client
                        },
                    );
                    debug_assert_eq!(drain.stopped_early, scene_mutated_this_client);
                    requests_remaining -= drain.dispatched;
                    if drain.dispatched > 0 {
                        request_next_hint = idx.saturating_add(1);
                    }
                    local_pending_work |= drain.complete_remaining || scene_mutated_this_client;
                    if drain.consumed > 0 && !conns[idx].served {
                        println!("display-server served client {}", i);
                        i += 1;
                        conns[idx].served = true;
                    }
                    presentation.mark_scene_if(server.scene_generation() != generation);
                }

                let deferred = server.presentation_deferred();
                if presentation.take_scene_ready(deferred) {
                    let present_succeeded =
                        present_framebuffer(&server, fb_fd as i32, &mut presented_pixels);
                    if complete_successful_frame_presentation(
                        &mut server,
                        present_succeeded,
                        deferred,
                        restore_clock.elapsed().as_millis() as u32,
                        &mut frame_callback_lifecycle_remaining,
                    )
                    .is_err()
                    {
                        std::process::exit(16);
                    }
                }
                if !emit_ready_present_fences(
                    &mut server,
                    &presentation,
                    fb_fd as i32,
                    &mut present_fence_writes_remaining,
                ) {
                    std::process::exit(16);
                }
                local_pending_work |= server.pending_present_fence_count() > 0
                    && presentation.present_fence_ready(server.presentation_deferred());
                if scene_mutated_this_client {
                    break;
                }
            }
            if requests_remaining == 0 {
                local_pending_work = true;
            }
            if conns.is_empty() {
                request_cursor = 0;
            } else {
                request_cursor = request_next_hint % conns.len();
            }

            // After the read pass, flush every client's
            // pending events. Events can land on clients OTHER
            // than the one we just dispatched a message for —
            // pmd_shell_manager broadcasts events to every
            // subscribed shell when ANY app's toplevel
            // mutates, so the broadcast pass below has to
            // reach the shell's fd even if the only fd we
            // read this tick was the app's.
            //
            // Partial-write handling: the kernel's
            // `send_on_socket` accepts only as many bytes as
            // fit in the peer's rx_buf right now (cap 64 KiB).
            // A burst of pointer events on a busy shell can
            // saturate the rx_buf and turn a 1 KiB event
            // batch into a 64 KiB-aligned partial write. The
            // unwritten tail used to be dropped on the floor,
            // which silently broke event delivery after ~30s
            // of activity. Now: drain server-side events into
            // a per-conn `outbound` queue + flush as much as
            // the kernel will take, leaving the rest queued
            // for next tick.
            let rotating_transport_order = if conns.is_empty() {
                Vec::new()
            } else {
                rotating_indices(conns.len(), transport_cursor)
                    .into_iter()
                    .map(|idx| conns[idx].client_id)
                    .collect::<Vec<_>>()
            };
            // Every authenticated shell receives a reserved write opportunity
            // before ordinary connections. Admission caps this class at two,
            // leaving at least two of the four rotating write slots for apps.
            let transport_order = priority_first(&rotating_transport_order, |client_id| {
                server
                    .client(client_id)
                    .is_some_and(|client| client.capabilities.contains(abi::cap::Cap::Shell))
            });
            let mut event_bytes_remaining = CLIENT_EVENT_BYTES_PER_TURN;
            let mut transport_next_hint = transport_cursor;
            for client_id in transport_order {
                let Some(idx) = conns.iter().position(|conn| conn.client_id == client_id) else {
                    continue;
                };
                let server_fd = conns[idx].server_fd;
                if server.client_event_queue_overflowed(client_id) {
                    conns[idx].disconnect_pending = true;
                }

                if conns[idx].disconnect_pending {
                    if disconnects_remaining == 0 {
                        local_pending_work = true;
                        continue;
                    }
                    let _ = fd_close(server_fd);
                    let generation = server.scene_generation();
                    let _ = server.disconnect(client_id);
                    presentation.mark_scene_if(server.scene_generation() != generation);
                    conns.swap_remove(idx);
                    disconnects_remaining -= 1;
                    local_pending_work = true;
                    transport_next_hint = idx;
                    continue;
                }

                if server.client_pending_event_bytes(client_id).unwrap_or(0) > 0 {
                    let drain_budget =
                        event_bytes_remaining.min(conns[idx].outbound.remaining_capacity());
                    if drain_budget > 0 {
                        if let Some(events) =
                            server.drain_client_events_bounded(client_id, drain_budget)
                        {
                            if !events.is_empty() {
                                event_bytes_remaining -= events.len();
                                transport_next_hint = idx.saturating_add(1);
                                if conns[idx].outbound.append(&events).is_err() {
                                    // `drain_budget` was computed from this exact
                                    // queue immediately before the append; failure
                                    // is an internal accounting violation, not a
                                    // slow-peer condition.
                                    std::process::exit(18);
                                } else {
                                    // Newly generated output gets one optimistic
                                    // write attempt without waiting for another
                                    // readiness edge.
                                    conns[idx].write_ready = true;
                                }
                            }
                        }
                    }
                }

                if conns[idx].disconnect_pending {
                    if disconnects_remaining == 0 {
                        local_pending_work = true;
                        continue;
                    }
                    let _ = fd_close(server_fd);
                    let generation = server.scene_generation();
                    let _ = server.disconnect(client_id);
                    presentation.mark_scene_if(server.scene_generation() != generation);
                    conns.swap_remove(idx);
                    disconnects_remaining -= 1;
                    local_pending_work = true;
                    transport_next_hint = idx;
                    continue;
                }

                if conns[idx].outbound.is_empty() || !conns[idx].write_ready {
                    local_pending_work |= pending_event_can_make_local_progress(
                        server.client_next_pending_event_bytes(client_id),
                        conns[idx].outbound.remaining_capacity(),
                    );
                    continue;
                }
                if client_writes_remaining == 0 {
                    local_pending_work = true;
                    continue;
                }

                client_writes_remaining -= 1;
                conns[idx].write_ready = false;
                transport_next_hint = idx.saturating_add(1);
                let before_len = conns[idx].outbound.len();
                let ev_iov = Ciovec {
                    buf: conns[idx].outbound.as_slice().as_ptr(),
                    buf_len: before_len as u32,
                };
                let mut written: u32 = 0;
                let rc = fd_write(server_fd, &ev_iov, 1, &mut written);
                if rc == 0 && written > 0 {
                    conns[idx].outbound.consume(written as usize);
                } else if rc != EAGAIN {
                    // A zero-progress success or non-EAGAIN transport error
                    // cannot preserve forward progress.
                    conns[idx].disconnect_pending = true;
                }

                if conns[idx].disconnect_pending {
                    if disconnects_remaining == 0 {
                        local_pending_work = true;
                        continue;
                    }
                    let _ = fd_close(server_fd);
                    let generation = server.scene_generation();
                    let _ = server.disconnect(client_id);
                    presentation.mark_scene_if(server.scene_generation() != generation);
                    conns.swap_remove(idx);
                    disconnects_remaining -= 1;
                    local_pending_work = true;
                    transport_next_hint = idx;
                } else {
                    local_pending_work |= pending_event_can_make_local_progress(
                        server.client_next_pending_event_bytes(client_id),
                        conns[idx].outbound.remaining_capacity(),
                    );
                }
            }
            if conns.is_empty() {
                transport_cursor = 0;
            } else {
                transport_cursor = transport_next_hint % conns.len();
            }

            local_pending_work |= server.advance_monotonic_time(
                restore_clock.elapsed().as_millis().min(u64::MAX as u128) as u64,
            );
            presentation.mark_scene_if(server.finish_recomposition_batch());

            let deferred = server.presentation_deferred();
            let mut completed_after_transport = 0usize;
            if presentation.take_ready(deferred) {
                let present_succeeded =
                    present_framebuffer(&server, fb_fd as i32, &mut presented_pixels);
                match complete_successful_frame_presentation(
                    &mut server,
                    present_succeeded,
                    deferred,
                    restore_clock.elapsed().as_millis() as u32,
                    &mut frame_callback_lifecycle_remaining,
                ) {
                    Ok(completed) => completed_after_transport += completed,
                    Err(()) => std::process::exit(16),
                }
            }
            completed_after_transport += server
                .drain_ready_frame_callback_lifecycle(&mut frame_callback_lifecycle_remaining);
            if !emit_ready_present_fences(
                &mut server,
                &presentation,
                fb_fd as i32,
                &mut present_fence_writes_remaining,
            ) {
                std::process::exit(16);
            }
            local_pending_work |= server.pending_present_fence_count() > 0
                && presentation.present_fence_ready(server.presentation_deferred());
            local_pending_work |=
                frame_callback_lifecycle_requires_local_turn(&server, completed_after_transport);
            input_priority_used = false;

            if shutdown_drain.requested() || !should_park_after_request_round(local_pending_work) {
                // Buffered local work must not blind the server to readiness
                // changes on other clients. Refresh the exact fd set with a
                // zero-deadline poll before starting the next input-first turn.
                // Shutdown uses the same snapshot to drain bytes queued before
                // SIGTERM without waiting on idle clients or trusting hostile
                // transport readiness beyond its fixed total turn cap.
                let transport_only = shutdown_drain.requested();
                let readiness = match poll_client_readiness(
                    &mut conns,
                    &keymap_watch.wait_fds(),
                    Some(0),
                    transport_only,
                ) {
                    Ok(readiness) => Some(readiness),
                    Err(errno) if errno == EINTR => None,
                    Err(_) => std::process::exit(18),
                };
                let client_transport_ready = conns.iter().any(|conn| {
                    client_transport_can_make_local_progress(
                        conn.read_ready,
                        conn.write_ready,
                        !conn.outbound.is_empty(),
                        conn.disconnect_pending,
                    )
                });
                let transport_ready = readiness.is_none()
                    || readiness.is_some_and(|snapshot| snapshot.listener_read_ready)
                    || client_transport_ready;
                if shutdown_drain.requested() {
                    let now_ms = restore_clock.elapsed().as_millis().min(u64::MAX as u128) as u64;
                    match shutdown_drain.finish_turn(now_ms, local_pending_work, transport_ready) {
                        ShutdownTurnDecision::Continue => {}
                        ShutdownTurnDecision::Finish => break 'outer,
                        ShutdownTurnDecision::WaitForDeadline(timeout_ms) => {
                            match poll_client_readiness(&mut conns, &[], Some(timeout_ms), true) {
                                Ok(_) => {}
                                Err(errno) if errno == EINTR => {}
                                Err(_) => std::process::exit(18),
                            }
                        }
                    }
                }
                continue 'outer;
            }

            // Every immediately available accept, input, client request,
            // outbound event, scene, and framebuffer task is drained above.
            // Park on the exact remaining readiness set. Backpressured clients
            // keep both READ and WRITE interests, preventing full-duplex
            // deadlock while preserving the server's 64-client maximum.
            let restore_timeout_ms = server.restore_poll_timeout_ms();
            match poll_client_readiness(
                &mut conns,
                &keymap_watch.wait_fds(),
                restore_timeout_ms,
                false,
            ) {
                Ok(_) => {}
                Err(errno) if errno == EINTR => {}
                Err(_) => std::process::exit(18),
            }
        }

        for c in &conns {
            let _ = fd_close(c.server_fd);
        }
    }

    println!("display-server fb blit ok");
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    // Native stub so `cargo test --workspace` + `cargo build
    // --workspace` link the bin target. The WASM target is the
    // only one the slice exercises.
}

#[cfg(test)]
mod tests {
    use super::{
        advance_presented_damage, build_wait_interests, changed_bands, changed_bands_bounded,
        changed_bounds, client_transport_can_make_local_progress, decode_ready_interests,
        drain_accepts_bounded, drain_protocol_requests_bounded, drain_ready_present_fences_bounded,
        drain_watch_records_bounded, encode_palette_rle_patch_batch, encode_present_fence,
        encode_rle_patch, frame_callback_lifecycle_requires_local_turn,
        pending_event_can_make_local_progress, priority_first, rotating_indices, rows_equal_u64,
        should_park_after_request_round, summarize_poll_readiness, take_ready_present_fence,
        take_ready_present_fence_with_deferred, DamageRect, PresentationSchedule, ShutdownDrain,
        ShutdownTurnDecision, ACCEPTS_PER_TURN, CLIENT_DISCONNECTS_PER_TURN,
        CLIENT_EVENT_BYTES_PER_TURN, CLIENT_READ_ATTEMPTS_PER_TURN,
        CLIENT_SOCKET_WRITE_QUANTUM_BYTES, CLIENT_WRITE_ATTEMPTS_PER_TURN, FB_MAX_COMMAND_BYTES,
        FB_OP_PATCH_PALETTE_RLE_BATCH, FB_OP_PATCH_RLE, FB_OP_PRESENT_FENCE,
        MAX_NON_INPUT_TRANSPORT_SYSCALLS_PER_TURN, PRESENT_FENCE_WRITES_PER_TURN,
        REQUESTS_PER_CLIENT_PER_TURN, REQUESTS_PER_TURN, SHUTDOWN_DRAIN_TURNS,
        SHUTDOWN_QUIESCENCE_GRACE_MS, WATCH_READS_PER_TURN,
    };
    use display_proto::{MessageHeader, ObjectId, HEADER_SIZE};

    fn protocol_request(object_id: ObjectId, opcode: u16, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0u8; HEADER_SIZE + payload.len()];
        MessageHeader::try_new(object_id, opcode, payload.len(), 0)
            .unwrap()
            .encode(&mut bytes[..HEADER_SIZE])
            .unwrap();
        bytes[HEADER_SIZE..].copy_from_slice(payload);
        bytes
    }

    fn registry_bind_payload(name: u32, interface: &str, new_id: ObjectId) -> Vec<u8> {
        let interface = interface.as_bytes();
        let padding = (4 - interface.len() % 4) % 4;
        let mut payload = Vec::new();
        payload.extend_from_slice(&name.to_le_bytes());
        payload.extend_from_slice(&(interface.len() as u32).to_le_bytes());
        payload.extend_from_slice(interface);
        payload.extend(core::iter::repeat_n(0, padding));
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&new_id.raw().to_le_bytes());
        payload
    }

    fn poll_event(userdata: u64, event_type: u8) -> Vec<u8> {
        let mut event = vec![0u8; abi::wasi::poll::EVENT_SIZE];
        event[abi::wasi::poll::EVENT_OFF_USERDATA..abi::wasi::poll::EVENT_OFF_USERDATA + 8]
            .copy_from_slice(&userdata.to_le_bytes());
        event[abi::wasi::poll::EVENT_OFF_TYPE] = event_type;
        event
    }

    #[test]
    fn zero_deadline_poll_filters_clock_and_preserves_new_client_readiness() {
        let interests = [
            (40, abi::wasi::eventtype::FD_READ),
            (40, abi::wasi::eventtype::FD_WRITE),
            (41, abi::wasi::eventtype::FD_READ),
        ];
        let events = [
            poll_event(0, abi::wasi::eventtype::CLOCK),
            poll_event(2, abi::wasi::eventtype::FD_WRITE),
            poll_event(3, abi::wasi::eventtype::FD_READ),
        ]
        .concat();

        assert_eq!(
            decode_ready_interests(&interests, &events, 3, true),
            Ok(vec![
                (40, abi::wasi::eventtype::FD_WRITE),
                (41, abi::wasi::eventtype::FD_READ),
            ])
        );
        assert_eq!(
            decode_ready_interests(
                &interests,
                &poll_event(0, abi::wasi::eventtype::CLOCK),
                1,
                false,
            ),
            Err(abi::errno::EIO)
        );
    }

    #[test]
    fn all_non_input_transport_phases_fit_the_twenty_call_turn_bound() {
        assert_eq!(
            CLIENT_EVENT_BYTES_PER_TURN,
            display_server::MAX_CONN_OUTBOUND_BYTES,
        );
        assert_eq!(
            MAX_NON_INPUT_TRANSPORT_SYSCALLS_PER_TURN,
            ACCEPTS_PER_TURN * 3
                + WATCH_READS_PER_TURN * 2
                + CLIENT_READ_ATTEMPTS_PER_TURN
                + CLIENT_WRITE_ATTEMPTS_PER_TURN
                + CLIENT_DISCONNECTS_PER_TURN
                + PRESENT_FENCE_WRITES_PER_TURN,
        );
        assert_eq!(MAX_NON_INPUT_TRANSPORT_SYSCALLS_PER_TURN, 20);
        assert_eq!(CLIENT_SOCKET_WRITE_QUANTUM_BYTES, 32 * 1024);
    }

    #[test]
    fn authenticated_shells_receive_reserved_transport_order_without_starving_apps() {
        let order = [10, 11, 12, 13, 14, 15];
        let prioritized = priority_first(&order, |id| matches!(id, 11 | 14));
        assert_eq!(prioritized, [11, 14, 10, 12, 13, 15]);
        assert_eq!(
            &prioritized[..CLIENT_WRITE_ATTEMPTS_PER_TURN],
            &[11, 14, 10, 12],
        );
    }

    #[test]
    fn capacity_blocked_pending_event_parks_until_a_complete_frame_can_move() {
        let next_event = Some(14);

        assert!(!pending_event_can_make_local_progress(next_event, 0));
        assert!(!pending_event_can_make_local_progress(next_event, 13));
        assert!(pending_event_can_make_local_progress(next_event, 14));
        assert!(!pending_event_can_make_local_progress(None, usize::MAX));
        assert!(should_park_after_request_round(
            pending_event_can_make_local_progress(next_event, 0),
        ));

        // A partial socket write can free exactly enough queue capacity for
        // the next complete event, which is real local work for the next turn.
        let outbound_capacity = 32;
        let before_partial_write = 24;
        let bytes_written = 6;
        let remaining_after_write = outbound_capacity - (before_partial_write - bytes_written);
        assert!(pending_event_can_make_local_progress(
            next_event,
            remaining_after_write,
        ));
    }

    #[test]
    fn perpetual_accept_source_stops_at_budget_before_existing_phases() {
        let mut next_fd = 100;
        let mut phases = Vec::new();
        let accepted = drain_accepts_bounded(
            || {
                let fd = next_fd;
                next_fd += 1;
                fd
            },
            |fd| phases.push(format!("accept-{fd}")),
        )
        .unwrap();
        phases.extend(
            ["input", "signal", "client-read", "client-write", "present"].map(str::to_string),
        );

        assert_eq!(accepted, ACCEPTS_PER_TURN);
        assert_eq!(phases[ACCEPTS_PER_TURN], "input");
        assert_eq!(phases.last().unwrap(), "present");
    }

    #[test]
    fn finite_accept_backlog_drains_before_eagain() {
        let mut backlog = std::collections::VecDeque::from([20, 21, 22]);
        let mut accepted = Vec::new();
        let result = drain_accepts_bounded(
            || backlog.pop_front().unwrap_or(-abi::errno::EAGAIN),
            |fd| accepted.push(fd),
        );
        assert_eq!(result, Ok(ACCEPTS_PER_TURN));
        assert_eq!(accepted, [20, 21]);
        assert_eq!(backlog, [22]);
        let result = drain_accepts_bounded(
            || backlog.pop_front().unwrap_or(-abi::errno::EAGAIN),
            |fd| accepted.push(fd),
        );
        assert_eq!(result, Err(-abi::errno::EAGAIN));
        assert_eq!(accepted, [20, 21, 22]);
        assert!(backlog.is_empty());
    }

    #[test]
    fn sigterm_drain_requires_accept_and_client_transport_quiescence() {
        let mut shutdown = ShutdownDrain::default();
        shutdown.request(10);
        assert!(shutdown.requested());
        let mut backlog = std::collections::VecDeque::from([20, 21]);
        let mut clients = Vec::new();

        let first = drain_accepts_bounded(
            || backlog.pop_front().unwrap_or(-abi::errno::EAGAIN),
            |fd| clients.push((fd, true)),
        );
        shutdown.observe_accept_result(first);
        assert_eq!(first, Ok(ACCEPTS_PER_TURN));
        assert_eq!(
            shutdown.finish_turn(10, false, false),
            ShutdownTurnDecision::Continue,
        );

        let second = drain_accepts_bounded(
            || backlog.pop_front().unwrap_or(-abi::errno::EAGAIN),
            |fd| clients.push((fd, true)),
        );
        shutdown.observe_accept_result(second);
        assert_eq!(second, Err(-abi::errno::EAGAIN));
        assert!(clients.iter().any(|(_, read_ready)| *read_ready));
        assert_eq!(
            shutdown.finish_turn(10, false, true),
            ShutdownTurnDecision::Continue,
        );

        for (_, read_ready) in &mut clients {
            *read_ready = false;
        }
        assert_eq!(
            shutdown.finish_turn(10, false, false),
            ShutdownTurnDecision::WaitForDeadline(SHUTDOWN_QUIESCENCE_GRACE_MS),
        );
        assert_eq!(
            shutdown.finish_turn(109, false, false),
            ShutdownTurnDecision::WaitForDeadline(1),
        );
        assert_eq!(
            shutdown.finish_turn(110, false, false),
            ShutdownTurnDecision::Finish,
        );
    }

    #[test]
    fn sigterm_drain_does_not_wait_on_idle_or_backpressured_clients() {
        let mut shutdown = ShutdownDrain::default();
        shutdown.request(500);
        shutdown.observe_accept_result(Err(-abi::errno::EAGAIN));

        assert!(!client_transport_can_make_local_progress(
            false, false, false, false,
        ));
        assert!(!client_transport_can_make_local_progress(
            false, true, false, false,
        ));
        assert!(!client_transport_can_make_local_progress(
            false, false, true, false,
        ));
        assert_eq!(
            shutdown.finish_turn(500, false, false),
            ShutdownTurnDecision::WaitForDeadline(SHUTDOWN_QUIESCENCE_GRACE_MS),
        );

        assert!(client_transport_can_make_local_progress(
            true, false, false, false,
        ));
        assert!(client_transport_can_make_local_progress(
            false, false, false, true,
        ));
        assert!(client_transport_can_make_local_progress(
            false, true, true, false,
        ));
        assert_eq!(
            shutdown.finish_turn(500, true, false),
            ShutdownTurnDecision::Continue,
        );
        assert_eq!(
            shutdown.finish_turn(500, false, true),
            ShutdownTurnDecision::Continue,
        );

        shutdown.observe_accept_result(Err(-abi::errno::EINTR));
        assert_eq!(
            shutdown.finish_turn(500, false, false),
            ShutdownTurnDecision::Continue,
        );
    }

    #[test]
    fn sigterm_grace_rechecks_early_wakes_and_late_client_readiness() {
        let mut shutdown = ShutdownDrain::default();
        shutdown.request(1_000);
        shutdown.observe_accept_result(Err(-abi::errno::EAGAIN));

        assert_eq!(
            shutdown.finish_turn(1_000, false, false),
            ShutdownTurnDecision::WaitForDeadline(100),
        );
        assert_eq!(
            shutdown.finish_turn(1_025, false, false),
            ShutdownTurnDecision::WaitForDeadline(75),
            "an early unrelated wake cannot satisfy the grace deadline",
        );
        assert_eq!(
            shutdown.finish_turn(1_075, false, true),
            ShutdownTurnDecision::Continue,
            "client bytes becoming ready during grace must be drained",
        );
        assert_eq!(
            shutdown.finish_turn(1_099, false, false),
            ShutdownTurnDecision::WaitForDeadline(1),
        );
        assert_eq!(
            shutdown.finish_turn(1_100, false, false),
            ShutdownTurnDecision::Finish,
        );

        shutdown.request(2_000);
        assert_eq!(
            shutdown.finish_turn(1_100, false, false),
            ShutdownTurnDecision::Finish,
            "a repeated signal cannot move the original deadline",
        );
    }

    #[test]
    fn sigterm_listener_wake_at_deadline_forces_another_accept_turn() {
        let mut shutdown = ShutdownDrain::default();
        shutdown.request(0);
        shutdown.observe_accept_result(Err(-abi::errno::EAGAIN));

        let readiness = summarize_poll_readiness(
            10,
            &[
                (10, abi::wasi::eventtype::FD_READ),
                (20, abi::wasi::eventtype::FD_READ),
            ],
        );
        assert!(readiness.listener_read_ready);
        assert_eq!(
            shutdown.finish_turn(100, false, readiness.listener_read_ready),
            ShutdownTurnDecision::Continue,
            "a connect after EAGAIN must survive the deadline boundary",
        );

        let unrelated = summarize_poll_readiness(
            10,
            &[
                (4, abi::wasi::eventtype::FD_READ),
                (20, abi::wasi::eventtype::FD_READ),
            ],
        );
        assert!(!unrelated.listener_read_ready);
        shutdown.observe_accept_result(Err(-abi::errno::EAGAIN));
        assert_eq!(
            shutdown.finish_turn(100, false, unrelated.listener_read_ready),
            ShutdownTurnDecision::Finish,
        );
    }

    #[test]
    fn sigterm_drain_has_a_total_bound_under_perpetual_transport_readiness() {
        let mut shutdown = ShutdownDrain::default();
        shutdown.request(0);

        for turn in 1..SHUTDOWN_DRAIN_TURNS {
            shutdown.observe_accept_result(Ok(ACCEPTS_PER_TURN));
            assert!(client_transport_can_make_local_progress(
                true, false, false, false,
            ));
            assert!(
                shutdown.finish_turn(0, true, true) == ShutdownTurnDecision::Continue,
                "shutdown ended before the total drain bound on turn {turn}",
            );
        }
        shutdown.observe_accept_result(Ok(ACCEPTS_PER_TURN));
        assert_eq!(
            shutdown.finish_turn(0, true, true),
            ShutdownTurnDecision::Finish,
        );

        // Repeated signals cannot replenish the exhausted grace budget.
        shutdown.request(1_000);
        assert_eq!(
            shutdown.finish_turn(0, true, true),
            ShutdownTurnDecision::Finish,
        );
    }

    #[test]
    fn watch_drain_is_bounded_under_refill_and_drains_finite_queue() {
        let mut perpetual_reads = 0;
        assert_eq!(
            drain_watch_records_bounded(|_| {
                perpetual_reads += 1;
                Ok(abi::ext::FS_WATCH_EVENT_SIZE)
            }),
            Ok(true)
        );
        assert_eq!(perpetual_reads, WATCH_READS_PER_TURN);

        let mut remaining = 3;
        let mut finite_reads = 0;
        assert_eq!(
            drain_watch_records_bounded(|_| {
                finite_reads += 1;
                if remaining == 0 {
                    Err(abi::errno::EAGAIN)
                } else {
                    remaining -= 1;
                    Ok(abi::ext::FS_WATCH_EVENT_SIZE)
                }
            }),
            Ok(true)
        );
        assert_eq!(finite_reads, WATCH_READS_PER_TURN);
        assert_eq!(remaining, 3 - WATCH_READS_PER_TURN);
        assert_eq!(
            drain_watch_records_bounded(|_| {
                finite_reads += 1;
                if remaining == 0 {
                    Err(abi::errno::EAGAIN)
                } else {
                    remaining -= 1;
                    Ok(abi::ext::FS_WATCH_EVENT_SIZE)
                }
            }),
            Ok(true)
        );
        assert_eq!(finite_reads, 4);
        assert_eq!(remaining, 0);
    }

    #[test]
    fn sixty_four_full_duplex_clients_fit_one_bounded_wait_set() {
        let clients = (0..64).map(|index| (100 + index, true)).collect::<Vec<_>>();
        let interests = build_wait_interests(10, 4, Some(11), Some(12), &[13, 14], &clients, false)
            .expect("listener/input/watch plus 64 read+write clients fit cap 256");
        assert_eq!(interests.len(), 6 + 64 * 2);
        for (fd, _) in &clients {
            assert!(interests.contains(&(*fd, abi::wasi::eventtype::FD_READ)));
            assert!(interests.contains(&(*fd, abi::wasi::eventtype::FD_WRITE)));
        }
        assert!(interests.len() <= abi::wasi::poll::MAX_SUBSCRIPTIONS);

        let shutdown_interests =
            build_wait_interests(10, 4, Some(11), Some(12), &[13, 14], &clients, true)
                .expect("shutdown listener and client transport fit the wait set");
        assert_eq!(shutdown_interests.len(), 1 + 64 * 2);
        assert!(shutdown_interests.contains(&(10, abi::wasi::eventtype::FD_READ)));
        for excluded in [4, 11, 12, 13, 14] {
            assert!(!shutdown_interests.iter().any(|(fd, _)| *fd == excluded));
        }
    }

    #[test]
    fn protocol_request_quantum_retains_exact_complete_and_fragmented_suffix() {
        let request = protocol_request(ObjectId::DISPLAY, 2, &3u32.to_le_bytes());
        let mut pending = request.repeat(10);
        pending.extend_from_slice(&request[..5]);
        let mut seen = 0;

        let first =
            drain_protocol_requests_bounded(&mut pending, REQUESTS_PER_CLIENT_PER_TURN, |_| {
                seen += 1;
                true
            });
        assert_eq!(first.dispatched, REQUESTS_PER_CLIENT_PER_TURN);
        assert_eq!(first.consumed, request.len() * REQUESTS_PER_CLIENT_PER_TURN);
        assert!(first.complete_remaining);
        assert_eq!(pending, [request.repeat(6), request[..5].to_vec()].concat());
        assert!(!should_park_after_request_round(first.complete_remaining));

        let second = drain_protocol_requests_bounded(&mut pending, usize::MAX, |_| {
            seen += 1;
            true
        });
        assert_eq!(second.dispatched, 6);
        assert!(!second.complete_remaining);
        assert_eq!(pending, request[..5]);
        assert_eq!(seen, 10);
        assert!(should_park_after_request_round(second.complete_remaining));
    }

    #[test]
    fn valid_surface_commit_flood_is_limited_before_full_recomposition() {
        let mut server = display_server::Server::new();
        let client = server.accept();
        let registry = ObjectId::new(3);
        let compositor = ObjectId::new(5);
        let surface = ObjectId::new(7);
        server
            .dispatch_request(
                client,
                &protocol_request(ObjectId::DISPLAY, 2, &registry.raw().to_le_bytes()),
            )
            .unwrap();
        server
            .dispatch_request(
                client,
                &protocol_request(
                    registry,
                    1,
                    &registry_bind_payload(1, "pmd_compositor", compositor),
                ),
            )
            .unwrap();
        server
            .dispatch_request(
                client,
                &protocol_request(compositor, 1, &surface.raw().to_le_bytes()),
            )
            .unwrap();

        let commit = protocol_request(surface, 7, &[]);
        let mut pending = commit.repeat(REQUESTS_PER_CLIENT_PER_TURN + 2);
        let generation = server.scene_generation();
        server.begin_recomposition_batch();
        let drain = drain_protocol_requests_bounded(
            &mut pending,
            REQUESTS_PER_CLIENT_PER_TURN,
            |message| {
                let before = server.recomposition_serial();
                server.dispatch_request(client, message).unwrap();
                server.recomposition_serial() == before
            },
        );
        assert_eq!(drain.dispatched, 1);
        assert!(drain.stopped_early);
        assert_eq!(server.scene_generation(), generation);
        assert!(server.finish_recomposition_batch());
        assert_eq!(server.scene_generation().wrapping_sub(generation), 1);
        assert_eq!(pending, commit.repeat(REQUESTS_PER_CLIENT_PER_TURN + 1));
        assert!(drain.complete_remaining);
    }

    #[test]
    fn global_request_budget_rotates_service_across_sixty_four_clients() {
        let request = protocol_request(ObjectId::DISPLAY, 2, &3u32.to_le_bytes());
        let mut pending = vec![request.repeat(REQUESTS_PER_CLIENT_PER_TURN); 64];
        let mut counts = [0usize; 64];
        let mut cursor = 0usize;

        let clients_per_turn = REQUESTS_PER_TURN / REQUESTS_PER_CLIENT_PER_TURN;
        for (round, expected_start) in [0usize, clients_per_turn].into_iter().enumerate() {
            assert_eq!(cursor, expected_start);
            let mut remaining = REQUESTS_PER_TURN;
            let mut last = None;
            for index in rotating_indices(pending.len(), cursor) {
                if remaining == 0 {
                    break;
                }
                let drain = drain_protocol_requests_bounded(
                    &mut pending[index],
                    REQUESTS_PER_CLIENT_PER_TURN.min(remaining),
                    |_| {
                        counts[index] += 1;
                        true
                    },
                );
                remaining -= drain.dispatched;
                last = Some(index);
            }
            assert_eq!(remaining, 0);
            assert_eq!(
                counts.iter().sum::<usize>(),
                (round + 1) * REQUESTS_PER_TURN
            );
            cursor = (last.unwrap() + 1) % pending.len();
            assert_eq!(cursor, (round + 1) * clients_per_turn);
        }

        assert!(counts[..clients_per_turn * 2]
            .iter()
            .all(|count| *count == REQUESTS_PER_CLIENT_PER_TURN));
        assert!(counts[clients_per_turn * 2..]
            .iter()
            .all(|count| *count == 0));
    }

    fn frame(width: u32, height: u32) -> Vec<u8> {
        vec![0; width as usize * height as usize * 4]
    }

    #[test]
    fn ready_scene_presents_once_inside_the_read_pass_before_flush() {
        let mut schedule = PresentationSchedule::default();
        let mut phases = Vec::new();
        for client in 0..3 {
            phases.push(format!("read-{client}"));
            if client == 0 {
                schedule.mark_scene_if(true);
            }
            if schedule.take_scene_ready(false) {
                phases.push("present".to_string());
            }
        }
        phases.push("flush".to_string());

        assert_eq!(phases, ["read-0", "present", "read-1", "read-2", "flush"]);
        assert!(!schedule.take_ready(false));
    }

    #[test]
    fn continued_scan_prevents_starvation_across_repeated_dirty_turns() {
        let mut schedule = PresentationSchedule::default();
        let mut reads = [0u32; 6];
        let mut presents = 0u32;
        for _ in 0..32 {
            for (client, count) in reads.iter_mut().enumerate() {
                *count += 1;
                if client == 0 {
                    schedule.mark_scene_if(true);
                }
                if schedule.take_scene_ready(false) {
                    presents += 1;
                }
            }
        }

        assert_eq!(reads, [32; 6]);
        assert_eq!(presents, 32);
    }

    #[test]
    fn deferred_focus_stays_dirty_until_shell_commit() {
        let mut schedule = PresentationSchedule::default();
        schedule.mark_frame_dirty();
        schedule.mark_scene_if(true);

        assert!(!schedule.take_scene_ready(true));
        assert!(schedule.take_scene_ready(false));
        assert!(!schedule.take_ready(false));
    }

    #[test]
    fn pending_recomposition_cannot_consume_scene_or_damage_before_materialization() {
        let (mut server, client, callback) = server_with_awaiting_frame_callback();
        let mut schedule = PresentationSchedule::default();
        schedule.mark_scene_if(true);
        let generation = server.scene_generation();

        server.begin_recomposition_batch();
        server
            .dispatch_request(client, &protocol_request(ObjectId::new(5), 7, &[]))
            .unwrap();
        assert_eq!(server.scene_generation(), generation);
        assert!(server.presentation_deferred());
        assert!(!schedule.take_scene_ready(server.presentation_deferred()));
        assert_eq!(
            server.presentation_damage(),
            &display_server::PresentationDamage::Full,
        );
        assert_eq!(
            server
                .client(client)
                .unwrap()
                .awaiting_present_frame_callback_count(),
            1,
        );

        assert!(server.finish_recomposition_batch());
        assert!(!server.presentation_deferred());
        assert!(schedule.take_scene_ready(server.presentation_deferred()));
        let mut budget = display_server::MAX_FRAME_CALLBACK_COMPLETIONS_PER_TURN;
        assert_eq!(
            super::complete_successful_frame_presentation(
                &mut server,
                true,
                false,
                77,
                &mut budget,
            ),
            Ok(1),
        );
        assert_eq!(
            server.presentation_damage(),
            &display_server::PresentationDamage::Bounded(Vec::new()),
        );
        let events = server.drain_client_events(client).unwrap();
        let done = MessageHeader::decode(&events).unwrap();
        assert_eq!((done.object_id, done.opcode), (callback, 1));
    }

    #[test]
    fn frame_only_dirty_waits_for_end_of_turn_presentation() {
        let mut schedule = PresentationSchedule::default();
        schedule.mark_frame_dirty();

        assert!(!schedule.take_scene_ready(false));
        assert!(schedule.take_ready(false));
        assert!(!schedule.take_ready(false));
    }

    fn server_with_awaiting_frame_callback(
    ) -> (display_server::Server, display_server::ClientId, ObjectId) {
        let mut server = display_server::Server::new();
        let client = server.accept();
        let compositor = ObjectId::new(3);
        let surface = ObjectId::new(5);
        let callback = ObjectId::new(7);
        server
            .client_mut(client)
            .unwrap()
            .install_client_object(compositor, display_server::Interface::Compositor)
            .unwrap();
        server
            .dispatch_request(
                client,
                &protocol_request(compositor, 1, &surface.raw().to_le_bytes()),
            )
            .unwrap();
        server
            .dispatch_request(
                client,
                &protocol_request(surface, 4, &callback.raw().to_le_bytes()),
            )
            .unwrap();
        server
            .dispatch_request(client, &protocol_request(surface, 7, &[]))
            .unwrap();
        (server, client, callback)
    }

    #[test]
    fn frame_callback_crosses_only_a_successful_nondeferred_present_boundary() {
        let (mut server, client, callback) = server_with_awaiting_frame_callback();
        let mut budget = display_server::MAX_FRAME_CALLBACK_COMPLETIONS_PER_TURN;
        assert_eq!(
            server.presentation_damage(),
            &display_server::PresentationDamage::Full,
        );

        assert_eq!(
            super::complete_successful_frame_presentation(
                &mut server,
                false,
                false,
                10,
                &mut budget,
            ),
            Err(())
        );
        assert_eq!(
            server
                .client(client)
                .unwrap()
                .awaiting_present_frame_callback_count(),
            1
        );
        assert_eq!(
            server.presentation_damage(),
            &display_server::PresentationDamage::Full,
            "a failed write cannot retire the pending comparison bound",
        );
        assert!(server.drain_client_events(client).unwrap().is_empty());

        assert_eq!(
            super::complete_successful_frame_presentation(&mut server, true, true, 11, &mut budget,),
            Ok(0)
        );
        assert_eq!(
            server
                .client(client)
                .unwrap()
                .awaiting_present_frame_callback_count(),
            1
        );
        assert_eq!(
            server.presentation_damage(),
            &display_server::PresentationDamage::Full,
            "a deferred boundary cannot retire presentation damage",
        );
        assert!(server.drain_client_events(client).unwrap().is_empty());

        let completed = super::complete_successful_frame_presentation(
            &mut server,
            true,
            false,
            12,
            &mut budget,
        )
        .unwrap();
        assert_eq!(completed, 1);
        assert_eq!(
            server.presentation_damage(),
            &display_server::PresentationDamage::Bounded(Vec::new()),
        );
        let events = server.drain_client_events(client).unwrap();
        let done = MessageHeader::decode(&events).unwrap();
        assert_eq!((done.object_id, done.opcode), (callback, 1));
        assert_eq!(&events[HEADER_SIZE..HEADER_SIZE + 4], &12u32.to_le_bytes());
        let deleted = MessageHeader::decode(&events[usize::from(done.length)..]).unwrap();
        assert_eq!((deleted.object_id, deleted.opcode), (ObjectId::DISPLAY, 2));
    }

    #[test]
    fn end_turn_callback_events_force_one_transport_turn_without_capacity_spin() {
        let (mut server, client, _) = server_with_awaiting_frame_callback();
        let mut budget = display_server::MAX_FRAME_CALLBACK_COMPLETIONS_PER_TURN;
        let completed = super::complete_successful_frame_presentation(
            &mut server,
            true,
            false,
            20,
            &mut budget,
        )
        .unwrap();
        assert!(frame_callback_lifecycle_requires_local_turn(
            &server, completed
        ));
        let _ = server.drain_client_events(client).unwrap();

        let (mut blocked, blocked_client, _) = server_with_awaiting_frame_callback();
        blocked.mark_frame_callbacks_presented(21);
        for _ in 0..display_server::MAX_PENDING_EVENTS - 1 {
            blocked
                .client_mut(blocked_client)
                .unwrap()
                .emit_error(ObjectId::DISPLAY, 1, "")
                .unwrap();
        }
        assert!(!blocked.ready_frame_callback_lifecycle_can_progress());
        assert!(!frame_callback_lifecycle_requires_local_turn(&blocked, 0));
    }

    #[test]
    fn authenticated_fence_waits_for_dirty_and_deferred_presentation() {
        use abi::cap::{Cap, CapSet};

        let mut server = display_server::Server::new();
        let shell = server.accept_with_caps(CapSet::from_caps(&[Cap::Shell]));
        let registry = ObjectId::new(3);
        let shell_manager = ObjectId::new(5);
        server
            .dispatch_request(
                shell,
                &protocol_request(ObjectId::DISPLAY, 2, &registry.raw().to_le_bytes()),
            )
            .unwrap();
        server
            .dispatch_request(
                shell,
                &protocol_request(
                    registry,
                    1,
                    &registry_bind_payload(5, "pmd_shell_manager", shell_manager),
                ),
            )
            .unwrap();
        server
            .dispatch_request(shell, &protocol_request(shell_manager, 8, &[]))
            .unwrap();

        let mut schedule = PresentationSchedule::default();
        schedule.mark_frame_dirty();
        schedule.mark_scene_if(true);

        assert_eq!(
            take_ready_present_fence_with_deferred(&mut server, &schedule, true),
            None,
            "a deferred scene must hold the authenticated marker",
        );
        assert_eq!(server.pending_present_fence_count(), 1);
        assert!(!schedule.take_scene_ready(true));
        assert_eq!(take_ready_present_fence(&mut server, &schedule), None);
        assert_eq!(server.pending_present_fence_count(), 1);

        assert!(schedule.take_scene_ready(false));
        assert_eq!(take_ready_present_fence(&mut server, &schedule), Some(1));
        assert_eq!(server.pending_present_fence_count(), 0);
    }

    #[test]
    fn one_fence_write_per_turn_drains_two_shells_fairly_without_parking() {
        use abi::cap::{Cap, CapSet};

        let mut server = display_server::Server::new();
        for _ in 0..2 {
            let shell = server.accept_with_caps(CapSet::from_caps(&[Cap::Shell]));
            let registry = ObjectId::new(3);
            let shell_manager = ObjectId::new(5);
            server
                .dispatch_request(
                    shell,
                    &protocol_request(ObjectId::DISPLAY, 2, &registry.raw().to_le_bytes()),
                )
                .unwrap();
            server
                .dispatch_request(
                    shell,
                    &protocol_request(
                        registry,
                        1,
                        &registry_bind_payload(5, "pmd_shell_manager", shell_manager),
                    ),
                )
                .unwrap();
            server
                .dispatch_request(shell, &protocol_request(shell_manager, 8, &[]))
                .unwrap();
        }

        let presentation = PresentationSchedule::default();
        let mut emitted = Vec::new();

        let mut first_turn_budget = PRESENT_FENCE_WRITES_PER_TURN;
        assert!(drain_ready_present_fences_bounded(
            &mut server,
            &presentation,
            false,
            &mut first_turn_budget,
            |serial| {
                emitted.push(serial);
                true
            },
        ));
        assert_eq!(first_turn_budget, 0);
        assert_eq!(emitted, [1]);
        assert_eq!(server.pending_present_fence_count(), 1);
        let ready_fence_remains =
            server.pending_present_fence_count() > 0 && presentation.present_fence_ready(false);
        assert!(!should_park_after_request_round(ready_fence_remains));

        let mut second_turn_budget = PRESENT_FENCE_WRITES_PER_TURN;
        assert!(drain_ready_present_fences_bounded(
            &mut server,
            &presentation,
            false,
            &mut second_turn_budget,
            |serial| {
                emitted.push(serial);
                true
            },
        ));
        assert_eq!(second_turn_budget, 0);
        assert_eq!(emitted, [1, 2]);
        assert_eq!(server.pending_present_fence_count(), 0);
        assert!(should_park_after_request_round(false));
    }

    #[test]
    fn present_fence_command_is_opcode_plus_little_endian_serial() {
        assert_eq!(FB_OP_PRESENT_FENCE, 0x09);
        assert_eq!(
            encode_present_fence(0x0102_0304),
            [0x09, 0x04, 0x03, 0x02, 0x01],
        );
    }

    #[test]
    fn disconnect_swap_remove_does_not_invalidate_the_unchanged_scan() {
        let mut clients = vec![0u32, 1, 2, 3];
        let mut visited = Vec::new();
        let mut idx = 0usize;
        while idx < clients.len() {
            let client = clients[idx];
            visited.push(client);
            if client == 1 {
                clients.swap_remove(idx);
                continue;
            }
            idx += 1;
        }

        visited.sort_unstable();
        assert_eq!(visited, [0, 1, 2, 3]);
        clients.sort_unstable();
        assert_eq!(clients, [0, 2, 3]);
    }

    #[test]
    fn identical_frame_has_no_damage() {
        let pixels = frame(4, 3);
        assert_eq!(changed_bounds(&pixels, &pixels, 4, 3), None);
    }

    #[test]
    fn one_changed_pixel_produces_one_pixel_damage() {
        let previous = frame(4, 3);
        let mut current = previous.clone();
        let (x, y) = (2usize, 1usize);
        current[(y * 4 + x) * 4] = 0xff;
        assert_eq!(
            changed_bounds(&previous, &current, 4, 3),
            Some(DamageRect {
                x: 2,
                y: 1,
                width: 1,
                height: 1,
            })
        );
    }

    #[test]
    fn separated_changes_produce_minimal_bounding_rectangle() {
        let previous = frame(6, 5);
        let mut current = previous.clone();
        let (x1, y1) = (4usize, 1usize);
        let (x2, y2) = (1usize, 4usize);
        current[(y1 * 6 + x1) * 4 + 1] = 1;
        current[(y2 * 6 + x2) * 4 + 3] = 1;
        assert_eq!(
            changed_bounds(&previous, &current, 6, 5),
            Some(DamageRect {
                x: 1,
                y: 1,
                width: 4,
                height: 4,
            })
        );
    }

    #[test]
    fn missing_shadow_requires_a_full_frame() {
        assert_eq!(
            changed_bounds(&[], &frame(3, 2), 3, 2),
            Some(DamageRect {
                x: 0,
                y: 0,
                width: 3,
                height: 2,
            })
        );
    }

    #[test]
    fn disjoint_changed_rows_exclude_the_unchanged_gap() {
        let previous = frame(5, 6);
        let mut current = previous.clone();
        current[24] = 1;
        current[(4 * 5 + 3) * 4] = 2;
        assert_eq!(
            changed_bands(&previous, &current, 5, 6, 8),
            vec![
                DamageRect {
                    x: 1,
                    y: 1,
                    width: 1,
                    height: 1,
                },
                DamageRect {
                    x: 3,
                    y: 4,
                    width: 1,
                    height: 1,
                },
            ]
        );
    }

    #[test]
    fn fragmented_changed_rows_coalesce_at_the_band_cap() {
        let previous = frame(3, 5);
        let mut current = previous.clone();
        for y in [0usize, 2, 4] {
            current[(y * 3 + 1) * 4] = 1;
        }
        assert_eq!(
            changed_bands(&previous, &current, 3, 5, 2),
            vec![DamageRect {
                x: 1,
                y: 0,
                width: 1,
                height: 5,
            }]
        );
    }

    #[test]
    fn wordwise_row_equality_matches_slice_equality_for_words_and_tails() {
        for len in 0usize..40 {
            let previous = (0..len).map(|index| index as u8).collect::<Vec<_>>();
            let mut current = previous.clone();
            assert!(rows_equal_u64(&previous, &current));
            for changed in 0..len {
                current[changed] ^= 0xff;
                assert!(!rows_equal_u64(&previous, &current));
                current[changed] ^= 0xff;
            }
        }
        assert!(!rows_equal_u64(&[1, 2, 3], &[1, 2]));
    }

    #[test]
    fn bounded_scan_reports_only_changes_inside_validated_candidates() {
        let previous = frame(4, 4);
        let mut current = previous.clone();
        current[0] = 1;
        current[(2 * 4 + 2) * 4] = 2;
        let candidates = [display_server::OutputDamageRect {
            x: 2,
            y: 2,
            width: 1,
            height: 1,
        }];
        assert_eq!(
            changed_bands_bounded(&previous, &current, 4, 4, &candidates, 8),
            vec![DamageRect {
                x: 2,
                y: 2,
                width: 1,
                height: 1,
            }],
        );
    }

    #[test]
    fn bounded_scan_clips_candidates_and_missing_shadow_still_falls_back_full() {
        let previous = frame(4, 4);
        let mut current = previous.clone();
        current[(3 * 4 + 3) * 4] = 1;
        let candidates = [display_server::OutputDamageRect {
            x: 3,
            y: 3,
            width: 20,
            height: 20,
        }];
        assert_eq!(
            changed_bands_bounded(&previous, &current, 4, 4, &candidates, 8),
            vec![DamageRect {
                x: 3,
                y: 3,
                width: 1,
                height: 1,
            }],
        );
        assert_eq!(
            changed_bands_bounded(&[], &current, 4, 4, &candidates, 8),
            vec![DamageRect {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            }],
        );
    }

    #[test]
    fn bounded_shadow_advancement_never_claims_unwritten_pixels() {
        let mut presented = frame(4, 4);
        let mut current = presented.clone();
        current[0] = 1;
        current[(2 * 4 + 2) * 4] = 2;
        let candidates = [display_server::OutputDamageRect {
            x: 2,
            y: 2,
            width: 1,
            height: 1,
        }];
        let damages = changed_bands_bounded(&presented, &current, 4, 4, &candidates, 8);
        assert_eq!(damages.len(), 1);
        assert!(advance_presented_damage(
            &mut presented,
            &current,
            4,
            damages[0],
        ));
        assert_eq!(presented[(2 * 4 + 2) * 4], 2);
        assert_eq!(presented[0], 0, "outside-candidate shadow stays untouched");
        assert_eq!(
            changed_bands(&presented, &current, 4, 4, 8),
            vec![DamageRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }],
            "a later full fallback still discovers outside-candidate divergence",
        );
    }

    #[test]
    fn solid_damage_selects_one_bounded_rle_run() {
        let pixels = vec![0x2a; 4 * 3 * 4];
        let damage = DamageRect {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        };
        let command = encode_rle_patch(&pixels, 4, 3, damage, FB_MAX_COMMAND_BYTES)
            .expect("solid patch compresses");
        assert_eq!(command[0], FB_OP_PATCH_RLE);
        assert_eq!(command.len(), 1 + 16 + 8);
        assert_eq!(u32::from_le_bytes(command[17..21].try_into().unwrap()), 4);
        assert_eq!(&command[21..25], &[0x2a; 4]);
    }

    #[test]
    fn rle_falls_back_when_runs_are_not_smaller_or_do_not_fit() {
        let pixels = vec![1, 0, 0, 255, 2, 0, 0, 255, 3, 0, 0, 255, 4, 0, 0, 255];
        let damage = DamageRect {
            x: 0,
            y: 0,
            width: 4,
            height: 1,
        };
        assert!(encode_rle_patch(&pixels, 4, 1, damage, FB_MAX_COMMAND_BYTES).is_none());

        let solid = vec![0x11; 4 * 4];
        assert!(encode_rle_patch(&solid, 4, 1, damage, 24).is_none());
    }

    #[test]
    fn palette_batch_encodes_disjoint_rectangles_as_one_command() {
        let mut pixels = frame(5, 4);
        pixels[0..8].copy_from_slice(&[10, 20, 30, 255, 10, 20, 30, 255]);
        for y in 2usize..4 {
            let offset = (y * 5 + 3) * 4;
            pixels[offset..offset + 4].copy_from_slice(&[40, 50, 60, 255]);
        }
        let damages = [
            DamageRect {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            },
            DamageRect {
                x: 3,
                y: 2,
                width: 1,
                height: 2,
            },
        ];

        let command = encode_palette_rle_patch_batch(&pixels, 5, 4, &damages, FB_MAX_COMMAND_BYTES)
            .expect("two-color batch fits");
        assert_eq!(command[0], FB_OP_PATCH_PALETTE_RLE_BATCH);
        assert_eq!(command[1], 2);
        assert_eq!(command[2], 1);
        assert_eq!(&command[3..11], &[10, 20, 30, 255, 40, 50, 60, 255]);
        assert_eq!(command.len(), 1 + 2 + 2 * 4 + 2 * 16 + 2 * 3);
        assert_eq!(u32::from_le_bytes(command[11..15].try_into().unwrap()), 0);
        assert_eq!(u16::from_le_bytes(command[27..29].try_into().unwrap()), 2);
        assert_eq!(command[29], 0);
        assert_eq!(u32::from_le_bytes(command[30..34].try_into().unwrap()), 3);
        assert_eq!(u16::from_le_bytes(command[46..48].try_into().unwrap()), 2);
        assert_eq!(command[48], 1);
    }

    #[test]
    fn palette_batch_fits_when_independent_rle_commands_exceed_one_write() {
        let width = 6_600u32;
        let height = 3u32;
        let mut pixels = frame(width, height);
        for y in [0usize, 2] {
            for x in 0..width as usize {
                let offset = (y * width as usize + x) * 4;
                pixels[offset..offset + 4].copy_from_slice(if (x / 3) % 2 == 0 {
                    &[1, 2, 3, 255]
                } else {
                    &[4, 5, 6, 255]
                });
            }
        }
        let damages = [
            DamageRect {
                x: 0,
                y: 0,
                width,
                height: 1,
            },
            DamageRect {
                x: 0,
                y: 2,
                width,
                height: 1,
            },
        ];
        let independent_bytes: usize = damages
            .iter()
            .map(|damage| {
                encode_rle_patch(&pixels, width, height, *damage, FB_MAX_COMMAND_BYTES)
                    .expect("individual rectangle fits")
                    .len()
            })
            .sum();
        assert!(independent_bytes > FB_MAX_COMMAND_BYTES);

        let batch =
            encode_palette_rle_patch_batch(&pixels, width, height, &damages, FB_MAX_COMMAND_BYTES)
                .expect("shared palette keeps the atomic batch bounded");
        assert!(batch.len() < FB_MAX_COMMAND_BYTES);
    }

    #[test]
    fn palette_batch_falls_back_on_palette_or_command_budget_overflow() {
        let width = 257u32;
        let mut pixels = frame(width, 2);
        for x in 0..width as usize {
            let offset = x * 4;
            pixels[offset..offset + 4].copy_from_slice(&[(x & 0xff) as u8, (x >> 8) as u8, 0, 255]);
        }
        let damages = [
            DamageRect {
                x: 0,
                y: 0,
                width,
                height: 1,
            },
            DamageRect {
                x: 0,
                y: 1,
                width: 1,
                height: 1,
            },
        ];
        assert!(
            encode_palette_rle_patch_batch(&pixels, width, 2, &damages, FB_MAX_COMMAND_BYTES,)
                .is_none()
        );

        let solid = vec![7; 4 * 4 * 4];
        let small = [
            DamageRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            DamageRect {
                x: 3,
                y: 3,
                width: 1,
                height: 1,
            },
        ];
        assert!(encode_palette_rle_patch_batch(&solid, 4, 4, &small, 40).is_none());
    }

    #[test]
    fn presented_shadow_advances_only_after_the_selected_damage() {
        let mut presented = vec![0u8; 4 * 3 * 4];
        let mut pixels = presented.clone();
        for y in 1usize..3 {
            for x in 1usize..3 {
                let offset = (y * 4 + x) * 4;
                pixels[offset..offset + 4].copy_from_slice(&[9, 8, 7, 255]);
            }
        }
        let damage = DamageRect {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        };
        assert!(advance_presented_damage(&mut presented, &pixels, 4, damage));
        assert_eq!(presented, pixels);

        let before = presented.clone();
        let short_len = presented.len() - 1;
        assert!(!advance_presented_damage(
            &mut presented[..short_len],
            &pixels,
            4,
            damage,
        ));
        assert_eq!(presented, before);
    }

    #[test]
    fn shadow_advances_one_successful_band_at_a_time() {
        let mut presented = frame(4, 4);
        let mut pixels = presented.clone();
        pixels[(4 + 1) * 4] = 1;
        pixels[(12 + 2) * 4] = 2;
        let bands = changed_bands(&presented, &pixels, 4, 4, 8);
        assert_eq!(bands.len(), 2);

        assert!(advance_presented_damage(
            &mut presented,
            &pixels,
            4,
            bands[0],
        ));
        assert_eq!(changed_bands(&presented, &pixels, 4, 4, 8), vec![bands[1]]);
        assert!(advance_presented_damage(
            &mut presented,
            &pixels,
            4,
            bands[1],
        ));
        assert!(changed_bands(&presented, &pixels, 4, 4, 8).is_empty());
    }
}
