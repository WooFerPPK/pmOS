//! WASI transport for production display clients.
//!
//! The protocol state machine remains transport-agnostic; this module is the
//! one shared adapter used by shipped WASI applications. Backpressured writes
//! park on `FD_WRITE`; the registry handshake does the same until its requests
//! drain, then parks on `FD_READ` for callback.done. Ordinary event loops park
//! on current display readiness plus an optional real-duty clock deadline.

#[cfg(any(target_arch = "wasm32", test))]
use std::collections::VecDeque;
#[cfg(any(target_arch = "wasm32", test))]
use std::time::Duration;

#[cfg(target_arch = "wasm32")]
use crate::protocol::Connection;
#[cfg(any(target_arch = "wasm32", test))]
use crate::protocol::WaitFd;
#[cfg(any(target_arch = "wasm32", test))]
use crate::protocol::WaitInterest;

#[cfg(any(target_arch = "wasm32", test))]
const SUBSCRIPTION_SIZE: usize = abi::wasi::poll::SUBSCRIPTION_SIZE;
#[cfg(any(target_arch = "wasm32", test))]
const EVENT_SIZE: usize = abi::wasi::poll::EVENT_SIZE;
#[cfg(any(target_arch = "wasm32", test))]
const USERDATA_PRIMARY_FD: u64 = 1;
#[cfg(any(target_arch = "wasm32", test))]
const USERDATA_CLOCK: u64 = 2;
#[cfg(any(target_arch = "wasm32", test))]
const USERDATA_AUX_FD_BASE: u64 = 0x100;
#[cfg(any(target_arch = "wasm32", test))]
const DISPLAY_CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(any(target_arch = "wasm32", test))]
const DISPLAY_CONNECT_DEADLINE: Duration = Duration::from_secs(5);
#[cfg(any(target_arch = "wasm32", test))]
const FS_WATCH_READS_PER_TURN: usize = 16;
#[cfg(any(target_arch = "wasm32", test))]
const DISPLAY_OUTBOUND_BYTE_LIMIT: usize = 256 * 1024;

#[cfg(any(target_arch = "wasm32", test))]
fn display_wait_interest(outbound_pending: bool) -> WaitInterest {
    if outbound_pending {
        WaitInterest::Write
    } else {
        WaitInterest::Read
    }
}

#[cfg(any(target_arch = "wasm32", test))]
fn wait_after_explicit_flush<W>(
    fd: i32,
    outbound: &PendingWrite,
    additional: &[crate::protocol::WaitFd],
    timeout: Option<Duration>,
    wait: W,
) -> Result<(), i32>
where
    W: FnOnce(
        crate::protocol::WaitFd,
        &[crate::protocol::WaitFd],
        Option<Duration>,
    ) -> Result<(), i32>,
{
    wait(
        crate::protocol::WaitFd {
            fd,
            interest: display_wait_interest(!outbound.is_empty()),
        },
        additional,
        timeout,
    )
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Default)]
struct PendingWrite {
    bytes: VecDeque<u8>,
}

#[cfg(any(target_arch = "wasm32", test))]
impl PendingWrite {
    fn enqueue(&mut self, bytes: &[u8]) -> Result<(), i32> {
        if self.bytes.len().saturating_add(bytes.len()) > DISPLAY_OUTBOUND_BYTE_LIMIT {
            return Err(abi::errno::ENOSPC);
        }
        self.bytes.extend(bytes.iter().copied());
        Ok(())
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Invoke `write` at most once. EAGAIN and zero progress leave the exact
    /// suffix queued for a later FD_WRITE wake.
    fn flush_once<W>(&mut self, mut write: W) -> Result<(), i32>
    where
        W: FnMut(&[u8]) -> Result<usize, i32>,
    {
        if self.bytes.is_empty() {
            return Ok(());
        }
        let contiguous = self.bytes.make_contiguous();
        match write(contiguous) {
            Ok(0) | Err(abi::errno::EAGAIN) => Ok(()),
            Ok(written) if written <= self.bytes.len() => {
                self.bytes.drain(..written);
                Ok(())
            }
            Ok(_) => Err(abi::errno::EIO),
            Err(errno) => Err(errno),
        }
    }
}

#[cfg(any(target_arch = "wasm32", test))]
fn drain_watch_records_bounded<R>(mut read: R) -> Result<bool, i32>
where
    R: FnMut(&mut [u8]) -> Result<usize, i32>,
{
    let mut changed = false;
    for _ in 0..FS_WATCH_READS_PER_TURN {
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

#[cfg(any(target_arch = "wasm32", test))]
fn encode_fd_subscription(out: &mut [u8], userdata: u64, fd: u32, interest: WaitInterest) {
    debug_assert!(out.len() >= SUBSCRIPTION_SIZE);
    out[..SUBSCRIPTION_SIZE].fill(0);
    out[abi::wasi::poll::SUB_OFF_USERDATA..abi::wasi::poll::SUB_OFF_USERDATA + 8]
        .copy_from_slice(&userdata.to_le_bytes());
    out[abi::wasi::poll::SUB_OFF_TAG] = match interest {
        WaitInterest::Read => abi::wasi::eventtype::FD_READ,
        WaitInterest::Write => abi::wasi::eventtype::FD_WRITE,
    };
    out[abi::wasi::poll::SUB_FDRW_OFF_FD..abi::wasi::poll::SUB_FDRW_OFF_FD + 4]
        .copy_from_slice(&fd.to_le_bytes());
}

#[cfg(any(target_arch = "wasm32", test))]
fn encode_clock_subscription(out: &mut [u8], timeout: Duration) {
    debug_assert!(out.len() >= SUBSCRIPTION_SIZE);
    out[..SUBSCRIPTION_SIZE].fill(0);
    out[abi::wasi::poll::SUB_OFF_USERDATA..abi::wasi::poll::SUB_OFF_USERDATA + 8]
        .copy_from_slice(&USERDATA_CLOCK.to_le_bytes());
    out[abi::wasi::poll::SUB_OFF_TAG] = abi::wasi::eventtype::CLOCK;
    out[abi::wasi::poll::SUB_CLOCK_OFF_ID..abi::wasi::poll::SUB_CLOCK_OFF_ID + 4]
        .copy_from_slice(&abi::wasi::CLOCKID_MONOTONIC.to_le_bytes());
    let timeout_ns = timeout.as_nanos().min(u128::from(u64::MAX)) as u64;
    out[abi::wasi::poll::SUB_CLOCK_OFF_TIMEOUT..abi::wasi::poll::SUB_CLOCK_OFF_TIMEOUT + 8]
        .copy_from_slice(&timeout_ns.to_le_bytes());
}

#[cfg(any(target_arch = "wasm32", test))]
fn connect_with_retry<C, W>(mut connect: C, mut wait: W) -> Result<i32, i32>
where
    C: FnMut() -> Result<i32, i32>,
    W: FnMut(Duration) -> Result<(), i32>,
{
    let mut waited = Duration::ZERO;
    loop {
        match connect() {
            Ok(fd) => return Ok(fd),
            Err(errno) if errno == abi::errno::ECONNREFUSED => {
                if waited >= DISPLAY_CONNECT_DEADLINE {
                    return Err(errno);
                }
                let delay = core::cmp::min(
                    DISPLAY_CONNECT_RETRY_INTERVAL,
                    DISPLAY_CONNECT_DEADLINE.saturating_sub(waited),
                );
                wait(delay)?;
                waited = waited.saturating_add(delay);
            }
            Err(errno) => return Err(errno),
        }
    }
}

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn poll_oneoff(
        subscriptions: *const u8,
        events: *mut u8,
        nsubscriptions: u32,
        nevents: *mut u32,
    ) -> i32;
    fn fd_read(fd: i32, iovs_ptr: *const Iovec, iovs_len: i32, nread_ptr: *mut u32) -> i32;
    fn fd_write(fd: i32, iovs_ptr: *const Ciovec, iovs_len: i32, nwritten_ptr: *mut u32) -> i32;
    fn fd_close(fd: i32) -> i32;
}

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "pmos_ext")]
extern "C" {
    fn display_connect() -> i32;
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
fn wait_fds(
    primary: WaitFd,
    additional: &[WaitFd],
    timeout: Option<Duration>,
    primary_hangup_is_error: bool,
) -> Result<(), i32> {
    let nsubscriptions = 1usize + additional.len() + usize::from(timeout.is_some());
    if nsubscriptions > abi::wasi::poll::MAX_SUBSCRIPTIONS {
        return Err(abi::errno::EINVAL);
    }
    let mut subscriptions = vec![0u8; SUBSCRIPTION_SIZE * nsubscriptions];
    encode_fd_subscription(
        &mut subscriptions[..SUBSCRIPTION_SIZE],
        USERDATA_PRIMARY_FD,
        primary.fd as u32,
        primary.interest,
    );
    for (index, fd) in additional.iter().enumerate() {
        let base = (index + 1) * SUBSCRIPTION_SIZE;
        encode_fd_subscription(
            &mut subscriptions[base..base + SUBSCRIPTION_SIZE],
            USERDATA_AUX_FD_BASE + index as u64,
            fd.fd as u32,
            fd.interest,
        );
    }
    if let Some(timeout) = timeout {
        let base = (nsubscriptions - 1) * SUBSCRIPTION_SIZE;
        encode_clock_subscription(&mut subscriptions[base..], timeout);
    }
    let mut events = vec![0u8; EVENT_SIZE * nsubscriptions];
    let mut nevents = 0u32;
    let errno = unsafe {
        poll_oneoff(
            subscriptions.as_ptr(),
            events.as_mut_ptr(),
            nsubscriptions as u32,
            &mut nevents,
        )
    };
    if errno != 0 {
        return Err(errno);
    }
    if nevents == 0 || nevents as usize > nsubscriptions {
        return Err(abi::errno::EIO);
    }
    validate_poll_events(
        &events,
        nevents as usize,
        nsubscriptions,
        primary_hangup_is_error,
    )
}

#[cfg(any(target_arch = "wasm32", test))]
fn validate_poll_events(
    events: &[u8],
    nevents: usize,
    nsubscriptions: usize,
    primary_hangup_is_error: bool,
) -> Result<(), i32> {
    if nevents == 0 || nevents > nsubscriptions || events.len() < nevents * EVENT_SIZE {
        return Err(abi::errno::EIO);
    }
    for index in 0..nevents {
        let base = index * EVENT_SIZE;
        let event_errno = u16::from_le_bytes([events[base + 8], events[base + 9]]) as i32;
        if event_errno != 0 {
            return Err(event_errno);
        }
        let userdata = u64::from_le_bytes(
            events[base..base + 8]
                .try_into()
                .expect("fixed event userdata"),
        );
        if primary_hangup_is_error && userdata == USERDATA_PRIMARY_FD {
            let flags = u16::from_le_bytes([events[base + 24], events[base + 25]]);
            if flags & abi::wasi::eventrwflags::FD_READWRITE_HANGUP != 0 {
                return Err(abi::errno::EPIPE);
            }
        }
    }
    Ok(())
}

/// Park until one non-display descriptor is ready. HANGUP is readiness, not
/// an error: callers must perform the read/write once more to observe EOF or
/// the descriptor-specific terminal status.
#[cfg(target_arch = "wasm32")]
pub fn wait_fd(fd: i32, interest: WaitInterest, timeout: Option<Duration>) -> Result<(), i32> {
    wait_fds(WaitFd { fd, interest }, &[], timeout, false)
}

/// Stable-inode filesystem watch used by event-driven applications. For an
/// atomically replaced file, watch its parent directory and re-open the file
/// after draining a CREATE/DELETE record.
#[cfg(target_arch = "wasm32")]
pub struct FsWatch {
    fd: i32,
}

#[cfg(target_arch = "wasm32")]
impl FsWatch {
    pub fn new(path: &str, mask: u32) -> Result<Self, i32> {
        let fd = unsafe { fs_watch(path.as_ptr(), path.len() as i32, mask, 0) };
        if fd < 0 {
            Err(-fd)
        } else {
            Ok(Self { fd })
        }
    }

    pub const fn fd(&self) -> i32 {
        self.fd
    }

    /// Drain every fixed 8-byte `(mask, inode)` record currently queued.
    pub fn drain(&mut self) -> Result<bool, i32> {
        drain_watch_records_bounded(|records| {
            let iov = Iovec {
                buf: records.as_mut_ptr(),
                buf_len: records.len() as u32,
            };
            let mut nread = 0u32;
            let errno = unsafe { fd_read(self.fd, &iov, 1, &mut nread) };
            if errno != 0 {
                Err(errno)
            } else {
                Ok(nread as usize)
            }
        })
    }
}

#[cfg(target_arch = "wasm32")]
impl Drop for FsWatch {
    fn drop(&mut self) {
        unsafe {
            let _ = fd_close(self.fd);
        }
    }
}

/// Watches an atomically replaceable file without pinning only its old inode:
/// the stable parent reports replacement CREATE/DELETE records, while the
/// current file inode reports direct in-place MODIFY writes.
#[cfg(target_arch = "wasm32")]
pub struct PathWatch {
    path: String,
    parent: FsWatch,
    file: Option<FsWatch>,
}

#[cfg(target_arch = "wasm32")]
impl PathWatch {
    pub fn new(path: &str) -> Result<Self, i32> {
        let parent_path = std::path::Path::new(path)
            .parent()
            .and_then(std::path::Path::to_str)
            .filter(|parent| !parent.is_empty())
            .unwrap_or("/");
        let parent = FsWatch::new(parent_path, abi::ext::WATCH_CREATE | abi::ext::WATCH_DELETE)?;
        let file = Self::open_file_watch(path)?;
        Ok(Self {
            path: path.to_string(),
            parent,
            file,
        })
    }

    fn open_file_watch(path: &str) -> Result<Option<FsWatch>, i32> {
        match FsWatch::new(path, abi::ext::WATCH_MODIFY) {
            Ok(watch) => Ok(Some(watch)),
            Err(errno) if errno == abi::errno::ENOENT => Ok(None),
            Err(errno) => Err(errno),
        }
    }

    pub fn wait_fds(&self) -> Vec<WaitFd> {
        let mut fds = Vec::with_capacity(2);
        fds.push(WaitFd::readable(self.parent.fd()));
        if let Some(file) = self.file.as_ref() {
            fds.push(WaitFd::readable(file.fd()));
        }
        fds
    }

    pub fn drain(&mut self) -> Result<bool, i32> {
        let parent_changed = self.parent.drain()?;
        let file_changed = match self.file.as_mut() {
            Some(file) => file.drain()?,
            None => false,
        };
        if parent_changed {
            // A rename may have replaced the inode. Close the old registration
            // and bind MODIFY readiness to the newly visible file, if present.
            drop(self.file.take());
            self.file = Self::open_file_watch(&self.path)?;
        }
        Ok(parent_changed || file_changed)
    }
}

#[cfg(target_arch = "wasm32")]
fn wait_clock(timeout: Duration) -> Result<(), i32> {
    let mut subscription = [0u8; SUBSCRIPTION_SIZE];
    encode_clock_subscription(&mut subscription, timeout);
    let mut event = [0u8; EVENT_SIZE];
    let mut nevents = 0u32;
    let errno = unsafe { poll_oneoff(subscription.as_ptr(), event.as_mut_ptr(), 1, &mut nevents) };
    if errno != 0 {
        return Err(errno);
    }
    if nevents != 1 {
        return Err(abi::errno::EIO);
    }
    let event_errno = u16::from_le_bytes([event[8], event[9]]) as i32;
    if event_errno != 0 {
        return Err(event_errno);
    }
    Ok(())
}

/// Production display-socket connection shared by every toolkit app.
#[cfg(target_arch = "wasm32")]
pub struct FdConnection {
    fd: i32,
    transport_error: Option<i32>,
    outbound: PendingWrite,
}

#[cfg(target_arch = "wasm32")]
impl FdConnection {
    pub fn connect() -> Result<Self, i32> {
        let fd = connect_with_retry(
            || {
                let result = unsafe { display_connect() };
                if result >= 0 {
                    Ok(result)
                } else {
                    Err(-result)
                }
            },
            wait_clock,
        )?;
        Ok(Self {
            fd,
            transport_error: None,
            outbound: PendingWrite::default(),
        })
    }

    pub fn from_fd(fd: i32) -> Self {
        Self {
            fd,
            transport_error: None,
            outbound: PendingWrite::default(),
        }
    }

    fn remember_error(&mut self, errno: i32) {
        self.transport_error.get_or_insert(errno);
    }

    fn flush_once(&mut self) -> Result<(), i32> {
        let fd = self.fd;
        self.outbound.flush_once(|remaining| {
            let iov = Ciovec {
                buf: remaining.as_ptr(),
                buf_len: remaining.len() as u32,
            };
            let mut nwritten = 0u32;
            let errno = unsafe { fd_write(fd, &iov, 1, &mut nwritten) };
            if errno == 0 {
                Ok(nwritten as usize)
            } else {
                Err(errno)
            }
        })
    }

    fn wait_for_activity(
        &mut self,
        additional: &[WaitFd],
        timeout: Option<Duration>,
    ) -> Result<(), i32> {
        if let Some(errno) = self.transport_error {
            return Err(errno);
        }
        // A readable display socket must not short-circuit a send-side park:
        // while the peer's receive buffer is full, already-buffered inbound
        // data can otherwise wake every poll without permitting one byte of
        // outbound progress. Waiting is observational: the event loop's one
        // explicit flush_outbound call is the sole write quantum for that
        // turn. Draining a suffix here could switch the interest to FD_READ
        // and strand locally-staged BufferPool work that needs the next turn.
        // Auxiliary application fds remain in the set so close/input/child
        // events stay responsive.
        wait_after_explicit_flush(
            self.fd,
            &self.outbound,
            additional,
            timeout,
            |primary, fds, deadline| wait_fds(primary, fds, deadline, true),
        )
    }
}

#[cfg(target_arch = "wasm32")]
impl Connection for FdConnection {
    fn send(&mut self, bytes: &[u8]) {
        if self.transport_error.is_some() {
            return;
        }
        if let Err(errno) = self.outbound.enqueue(bytes) {
            self.remember_error(errno);
        }
    }

    fn flush_outbound(&mut self) -> Result<(), i32> {
        if let Some(errno) = self.transport_error {
            return Err(errno);
        }
        if let Err(errno) = self.flush_once() {
            self.remember_error(errno);
            return Err(errno);
        }
        Ok(())
    }

    fn outbound_pending(&self) -> bool {
        !self.outbound.is_empty()
    }

    fn incremental_uploads(&self) -> bool {
        true
    }

    fn drain_outbound(&mut self) -> Vec<u8> {
        Vec::new()
    }

    fn recv(&mut self) -> Vec<u8> {
        if self.transport_error.is_some() {
            return Vec::new();
        }
        let mut buf = [0u8; 4096];
        let iov = Iovec {
            buf: buf.as_mut_ptr(),
            buf_len: buf.len() as u32,
        };
        let mut nread = 0u32;
        let errno = unsafe { fd_read(self.fd, &iov, 1, &mut nread) };
        if errno == 0 && nread > 0 {
            return buf[..nread as usize].to_vec();
        }
        if errno != 0 && errno != abi::errno::EAGAIN {
            self.remember_error(errno);
        }
        Vec::new()
    }

    fn wait(&mut self, timeout: Option<Duration>) -> Result<(), i32> {
        self.wait_for_activity(&[], timeout)
    }

    fn wait_with(&mut self, additional: &[WaitFd], timeout: Option<Duration>) -> Result<(), i32> {
        self.wait_for_activity(additional, timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fd_subscription_encodes_read_and_write_without_retaining_pointers() {
        let mut bytes = [0xAA; SUBSCRIPTION_SIZE];
        encode_fd_subscription(&mut bytes, USERDATA_PRIMARY_FD, 41, WaitInterest::Write);
        assert_eq!(
            u64::from_le_bytes(bytes[..8].try_into().unwrap()),
            USERDATA_PRIMARY_FD
        );
        assert_eq!(
            bytes[abi::wasi::poll::SUB_OFF_TAG],
            abi::wasi::eventtype::FD_WRITE
        );
        assert_eq!(
            u32::from_le_bytes(
                bytes[abi::wasi::poll::SUB_FDRW_OFF_FD..abi::wasi::poll::SUB_FDRW_OFF_FD + 4]
                    .try_into()
                    .unwrap()
            ),
            41
        );
        assert!(bytes[9..16].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn display_backpressure_waits_write_only_until_queue_drains() {
        assert_eq!(display_wait_interest(true), WaitInterest::Write);
        assert_eq!(display_wait_interest(false), WaitInterest::Read);
    }

    #[test]
    fn wait_after_explicit_partial_flush_preserves_suffix_and_write_interest() {
        let mut pending = PendingWrite::default();
        pending.enqueue(b"abc").unwrap();
        let mut write_attempts = 0;
        pending
            .flush_once(|bytes| {
                write_attempts += 1;
                assert_eq!(bytes, b"abc");
                Ok(2)
            })
            .unwrap();
        assert_eq!(pending.bytes.iter().copied().collect::<Vec<_>>(), b"c");

        let mut observed = None;
        wait_after_explicit_flush(41, &pending, &[], None, |primary, additional, timeout| {
            observed = Some((primary, additional.len(), timeout));
            Ok(())
        })
        .unwrap();

        assert_eq!(write_attempts, 1, "wait must not hide a second write");
        assert_eq!(pending.bytes.iter().copied().collect::<Vec<_>>(), b"c");
        assert_eq!(
            observed,
            Some((WaitFd::writable(41), 0, None)),
            "the retained suffix keeps the display fd on FD_WRITE",
        );
    }

    #[test]
    fn relative_clock_subscription_saturates_duration_to_u64_nanoseconds() {
        let mut bytes = [0u8; SUBSCRIPTION_SIZE];
        encode_clock_subscription(&mut bytes, Duration::MAX);
        assert_eq!(
            bytes[abi::wasi::poll::SUB_OFF_TAG],
            abi::wasi::eventtype::CLOCK
        );
        assert_eq!(
            u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            abi::wasi::CLOCKID_MONOTONIC
        );
        assert_eq!(
            u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
            u64::MAX
        );
        assert_eq!(u16::from_le_bytes(bytes[40..42].try_into().unwrap()), 0);
    }

    #[test]
    fn display_connect_waits_on_clock_until_listener_is_bound() {
        let mut attempts = 0;
        let mut waits = Vec::new();
        let fd = connect_with_retry(
            || {
                attempts += 1;
                if attempts < 4 {
                    Err(abi::errno::ECONNREFUSED)
                } else {
                    Ok(17)
                }
            },
            |delay| {
                waits.push(delay);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(fd, 17);
        assert_eq!(attempts, 4);
        assert_eq!(waits, vec![DISPLAY_CONNECT_RETRY_INTERVAL; 3]);
    }

    #[test]
    fn display_connect_retry_has_an_explicit_total_deadline() {
        let mut waited = Duration::ZERO;
        let result = connect_with_retry(
            || Err(abi::errno::ECONNREFUSED),
            |delay| {
                waited = waited.saturating_add(delay);
                Ok(())
            },
        );
        assert_eq!(result, Err(abi::errno::ECONNREFUSED));
        assert_eq!(waited, DISPLAY_CONNECT_DEADLINE);
    }

    #[test]
    fn display_connect_does_not_retry_other_errors() {
        let mut waits = 0;
        let result = connect_with_retry(
            || Err(abi::errno::ENOTCAPABLE),
            |_| {
                waits += 1;
                Ok(())
            },
        );
        assert_eq!(result, Err(abi::errno::ENOTCAPABLE));
        assert_eq!(waits, 0);
    }

    #[test]
    fn zero_global_ipc_bytes_keeps_exact_suffix_for_later_write_wake() {
        let mut pending = PendingWrite::default();
        pending.enqueue(b"ok").unwrap();
        let mut writes = 0;
        pending
            .flush_once(|_| {
                writes += 1;
                Err(abi::errno::EAGAIN)
            })
            .unwrap();
        assert_eq!(writes, 1, "one attempt, no send-side park loop");
        assert!(!pending.is_empty());

        let mut delivered = Vec::new();
        pending
            .flush_once(|remaining| {
                delivered.extend_from_slice(&remaining[..1]);
                Ok(1)
            })
            .unwrap();
        assert_eq!(delivered, b"o");
        assert!(!pending.is_empty());
        pending
            .flush_once(|remaining| {
                delivered.extend_from_slice(remaining);
                Ok(remaining.len())
            })
            .unwrap();
        assert_eq!(delivered, b"ok");
        assert!(pending.is_empty());
    }

    #[test]
    fn invalid_socket_state_is_fatal_not_a_backpressure_retry() {
        let mut pending = PendingWrite::default();
        pending.enqueue(b"x").unwrap();
        let result = pending.flush_once(|_| Err(abi::errno::EINVAL));
        assert_eq!(result, Err(abi::errno::EINVAL));
        assert!(!pending.is_empty());
    }

    #[test]
    fn outbound_queue_has_exact_n_n_plus_one_admission_boundary() {
        let mut pending = PendingWrite::default();
        pending
            .enqueue(&vec![0x5a; DISPLAY_OUTBOUND_BYTE_LIMIT])
            .unwrap();
        assert_eq!(pending.enqueue(&[0]), Err(abi::errno::ENOSPC));
    }

    #[test]
    fn auxiliary_hangup_wakes_without_poisoning_display_transport() {
        let mut event = [0u8; EVENT_SIZE];
        event[..8].copy_from_slice(&USERDATA_AUX_FD_BASE.to_le_bytes());
        event[abi::wasi::poll::EVENT_OFF_TYPE] = abi::wasi::eventtype::FD_READ;
        event[abi::wasi::poll::EVENT_OFF_RW_FLAGS..abi::wasi::poll::EVENT_OFF_RW_FLAGS + 2]
            .copy_from_slice(&abi::wasi::eventrwflags::FD_READWRITE_HANGUP.to_le_bytes());
        assert_eq!(validate_poll_events(&event, 1, 2, true), Ok(()));

        event[..8].copy_from_slice(&USERDATA_PRIMARY_FD.to_le_bytes());
        assert_eq!(
            validate_poll_events(&event, 1, 2, true),
            Err(abi::errno::EPIPE)
        );
        assert_eq!(validate_poll_events(&event, 1, 2, false), Ok(()));
    }

    #[test]
    fn watch_drain_budget_yields_under_refill_and_finite_backlog_drains() {
        let mut perpetual_reads = 0;
        assert_eq!(
            drain_watch_records_bounded(|_| {
                perpetual_reads += 1;
                Ok(abi::ext::FS_WATCH_EVENT_SIZE)
            }),
            Ok(true)
        );
        assert_eq!(perpetual_reads, FS_WATCH_READS_PER_TURN);

        let mut remaining = 5;
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
        assert_eq!(remaining, 0);
        assert_eq!(finite_reads, 6);
    }
}
