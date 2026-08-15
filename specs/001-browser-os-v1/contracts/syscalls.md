# Contract: Syscall ABI

**Status**: canonical reference for the PMos v1 OS ABI.
**Audience**: kernel implementers, userland library authors,
third-party application developers.

This document is the single source of truth for PMos v1 syscall
numbers, request/response layouts, and extension-syscall
justifications. Anything else — the Rust bindings generated for
userland apps, the TypeScript stubs in the bootstrap — is derived
from this file.

**Versioning**: the `abi` crate in the Rust workspace holds a
constant `ABI_VERSION = (1, 4)` that kernel and userland check at
process spawn. Mismatches fail spawn with a distinct error code. A
breaking change to any entry below requires bumping the major
component. See §5 for the change log.

---

## 1. Transport

Every syscall travels over a `SharedArrayBuffer` ring buffer shared
between the calling process's Worker and the kernel Worker. The
buffer layout is specified in `driver-kernel.md` under "Syscall ring
layout"; this file treats the transport as opaque and specifies only
the _semantic_ contract (what request fields mean, what the kernel
returns).

Each syscall request has the shape:

```
struct Request {
    opcode:     u16,        // see tables below
    flags:      u16,        // reserved in v1, MUST be 0
    request_id: u32,        // monotonically increasing per-process; echoed in response
    args:       [u8; N],    // opcode-specific, fixed max 256 bytes; larger payloads go in heap pages
    heap_ptr:   u32,        // offset into the process's linear memory, for ops that take buffers
    heap_len:   u32,
}

struct Response {
    request_id: u32,        // echoes the request
    status:     i32,        // 0 on success, negative errno on failure
    value:      i64,        // return value (e.g., bytes read)
    extra_len:  u32,        // optional extra payload length in the ring's response slot
}
```

**Error numbering** follows WASI errno: `EACCES`, `EAGAIN`,
`EBADF`, `EBUSY`, `EEXIST`, `EINVAL`, `ENOENT`, `ENOMEM`, `ENOSPC`,
`ENOTDIR`, `EPIPE`, `EPERM`, … in the same numeric space the Rust
`wasi` crate uses.

**Blocking**: a syscall that returns `EAGAIN` with `O_NONBLOCK`
set otherwise blocks. Blocking is implemented by the user-side
Atomics.wait described in `research.md` / `driver-kernel.md`. The kernel
records the process block reason and retains the request metadata; the
event source queues one delayed response and transitions the process back
to Ready. The user Worker resumes only after that response reaches its SAB.

---

## 2. WASI preview 1 baseline

PMos implements **all** of WASI preview 1 as the baseline. The
opcodes, argument layouts, and errnos are those of the preview 1
snapshot (`snapshot_preview1` in Wasmtime's reference
implementation). They are reproduced here by category for
reference; the canonical normative source is the WASI preview 1
document
(`WebAssembly/WASI/legacy/preview1/docs.md`), which this file
defers to.

| Opcode symbol             | Category | Notes                                                                                |
| ------------------------- | -------- | ------------------------------------------------------------------------------------ |
| `args_get`                | args     |                                                                                      |
| `args_sizes_get`          | args     |                                                                                      |
| `environ_get`             | environ  |                                                                                      |
| `environ_sizes_get`       | environ  |                                                                                      |
| `clock_res_get`           | clocks   | monotonic, realtime, process_cputime_id supported; thread_cputime_id returns ENOTSUP |
| `clock_time_get`          | clocks   |                                                                                      |
| `fd_advise`               | file     | no-op in v1 (accepted, ignored)                                                      |
| `fd_allocate`             | file     | returns ENOTSUP if FS does not support it                                            |
| `fd_close`                | file     |                                                                                      |
| `fd_datasync`             | file     | forwarded to FS `sync`                                                               |
| `fd_fdstat_get`           | file     |                                                                                      |
| `fd_fdstat_set_flags`     | file     |                                                                                      |
| `fd_fdstat_set_rights`    | file     | rights enforced from `Process.caps`                                                  |
| `fd_filestat_get`         | file     |                                                                                      |
| `fd_filestat_set_size`    | file     |                                                                                      |
| `fd_filestat_set_times`   | file     |                                                                                      |
| `fd_pread`                | file     |                                                                                      |
| `fd_prestat_get`          | file     | directory preopen: `/` at fd 3                                                       |
| `fd_prestat_dir_name`     | file     | returns `/` for fd 3; `EBADF` otherwise                                              |
| `fd_pwrite`               | file     |                                                                                      |
| `fd_read`                 | file     | empty blocking pipes park until bytes or EOF; `O_NONBLOCK` returns `EAGAIN`          |
| `fd_readdir`              | file     |                                                                                      |
| `fd_renumber`             | file     | dup-like                                                                             |
| `fd_seek`                 | file     |                                                                                      |
| `fd_sync`                 | file     |                                                                                      |
| `fd_tell`                 | file     |                                                                                      |
| `fd_write`                | file     |                                                                                      |
| `path_create_directory`   | file     |                                                                                      |
| `path_filestat_get`       | file     |                                                                                      |
| `path_filestat_set_times` | file     |                                                                                      |
| `path_link`               | file     |                                                                                      |
| `path_open`               | file     |                                                                                      |
| `path_readlink`           | file     |                                                                                      |
| `path_remove_directory`   | file     |                                                                                      |
| `path_rename`             | file     |                                                                                      |
| `path_symlink`            | file     |                                                                                      |
| `path_unlink_file`        | file     |                                                                                      |
| `poll_oneoff`             | poll     | supports `fd_read`, `fd_write`, and clock subscriptions                              |
| `proc_exit`               | process  | exit status propagates to waitpid                                                    |
| `proc_raise`              | process  | delivers signal-equivalent to self via the signal channel                            |
| `sched_yield`             | sched    |                                                                                      |
| `random_get`              | random   | uses `crypto.getRandomValues` through the kernel                                     |
| `sock_accept`             | sock     | for AF_UNIX sockets in PMos, for TCP-over-WebSocket otherwise                        |
| `sock_recv`               | sock     |                                                                                      |
| `sock_send`               | sock     |                                                                                      |
| `sock_shutdown`           | sock     |                                                                                      |

**Deviation notes**:

- Every `proc_spawn` child starts with the following well-known fd layout:
  stdin/stdout/stderr at 0/1/2, the WASI `/` directory preopen at 3, and the
  PMos signal-inbox channel at 4. Rust and other preview-1 libcs therefore use
  ordinary `std::fs`/WASI path operations; applications must not bypass the
  WASI layer with a browser-specific filesystem adapter.
- `path_open`, `path_unlink_file`, and `path_remove_directory` resolve a
  relative path from the directory vnode named by their `dirfd`. An unopened
  fd returns `EBADF`; an open non-directory fd returns `ENOTDIR`. PMos also
  accepts absolute paths in these calls as a v1 convenience, in which case the
  path names the global VFS namespace and `dirfd` is ignored. Inode-relative
  paths containing `..` return `EINVAL` because v1 filesystems do not retain
  the parent links needed to resolve it without escaping the directory fd.
- `fd_advise` is a no-op; the FS back end does nothing with
  advice. This is permitted by WASI preview 1.
- `fd_fdstat_set_rights` is honoured: if the caller tries to
  widen a right, `ENOTCAPABLE` is returned.
- `poll_oneoff` copies and validates at most 32 subscriptions per ordinary or
  Shell process and at most 256 for a `DISPLAY_SERVER` process. At most 2,048
  parked subscriptions are admitted kernel-wide, partitioned into 1,760
  ordinary, 32 Shell, and 256 display-server slots so ordinary workloads
  cannot deny desktop-critical waiters. Limits are enforced before readiness
  scanning or registration; one process may own only one parked poll.
- `proc_raise` delivers to the caller's own signal channel; it
  never affects another process (use `proc_kill` for that, which
  is an extension syscall).
- `sock_send` accepts only the zero WASI send-flags value. `sock_recv`
  recognizes the preview-1 `RECV_PEEK` and `RECV_WAITALL` bits but returns
  `ENOTSUP` before consuming socket state because v1 does not implement those
  modes; unknown flag bits return `EINVAL`. `sock_accept` and `sock_shutdown`
  likewise reject unknown bits before accepting a connection or changing
  half-close state.
- Thread-local / WASI thread calls are absent because PMos has no
  threads in the WASI sense (each process is single-threaded
  WASM); any call in that category returns `ENOTSUP`.

---

## 3. Extension syscalls

Each extension syscall has an entry below with:

1. **Opcode** — a numeric constant allocated in the `abi` crate
   starting at `0x1000` (to avoid clashes with WASI's dense 0-based
   numbering).
2. **Signature** — request fields, return type.
3. **Semantics** — what the kernel does.
4. **Why WASI doesn't cover this** — the Principle VI
   justification.

### 3.1 IPC

#### `ipc_socket` (0x1000)

```
ipc_socket(ty: u8) -> fd
```

`ty`: `0 = STREAM`, `1 = DGRAM`. V1 implements only `STREAM`. `DGRAM` keeps its
wire discriminant reserved but returns `ENOTSUP` before an id, object, or fd
slot is consumed. Correct datagram support requires record boundaries and
source/destination addressing; it MUST NOT be simulated with stream
connect/listen/accept semantics. V1 has no creation-time flags; callers use
`fd_fdstat_set_flags` after creation when they need `NONBLOCK`.

Only a successful `STREAM` creation yields an fd in v1. Consequently,
`ipc_bind`, `ipc_listen`, `ipc_connect`, `ipc_accept`, `ipc_send`, `ipc_recv`,
`ipc_peer_caps`, and `ipc_peer_pid` operate only on STREAM sockets; no DGRAM fd
can reach those operations.

**Semantics**: allocates a `Socket` object in the kernel's IPC
table, returns a fresh fd referencing it. The socket starts in
the `Unbound` state. The kernel admits at most 2,048 live socket
objects; allocation beyond that limit returns `ENOSPC` before an id,
object, or fd-table slot is consumed.

**Why WASI doesn't cover this**: WASI's `sock_*` syscalls model
network sockets (AF_INET-style). AF_UNIX / AF_LOCAL and
file-descriptor passing are absent. File-descriptor passing in
particular is load-bearing for the display server connection —
there is no POSIX equivalent via WASI.

#### `ipc_bind` (0x1001)

```
ipc_bind(fd, path: &str) -> ()
```

**Semantics**: binds the socket to `path` in the kernel's abstract IPC
namespace. `path` is an opaque binding key: the call does not resolve it in
the VFS, create a vnode, or consult parent-directory permissions. Fails with
`EADDRINUSE` when another live socket owns the same key and `EINVAL` when the
socket is not in the unbound state. `/run/display` is reserved for the
capability-gated `display_bind` opcode and generic bind returns `ENOTCAPABLE`
without changing the socket or namespace. `/run/host-files` is kernel-owned
and generic bind returns `EADDRINUSE`. Closing a bound socket removes the
binding.

#### `ipc_listen` (0x1002)

```
ipc_listen(fd, backlog: u32) -> ()
```

**Semantics**: transitions the socket to `Listening`. The
backlog bounds the pending connect queue.

#### `ipc_connect` (0x1003)

```
ipc_connect(fd, path: &str) -> ()
```

**Semantics**: queues the socket on the listener and returns success
immediately with the fd in `Connecting`. Until the server accepts it,
`fd_read` and `fd_write` return `EAGAIN` without side effects and both
`poll_oneoff(FD_READ)` and `poll_oneoff(FD_WRITE)` remain unready. Accept
transitions the endpoint to `Connected`: an empty stream is immediately
write-ready, while read readiness still requires bytes, ancillary data, or
EOF. Closing the listener before accept makes both poll directions ready with
`FD_READWRITE_HANGUP`; later I/O returns `ECONNREFUSED`. A missing listener or
full backlog rejects the connect synchronously with `ECONNREFUSED`; capability
denial returns `EACCES`/`ENOTCAPABLE` at a protected endpoint.
`/run/display` is not reachable through this generic opcode: it returns
`ENOTCAPABLE` without changing client or listener state, and callers must use
`display_connect` so the display capability boundary remains a single
auditable entry point.

#### `ipc_accept` (0x1004)

```
ipc_accept(fd, flags: u16) -> fd
```

**Semantics**: dequeues a pending connect and returns a new
connected socket fd on the server side.

- `flags = 0` (default): blocks the caller until a client
  `ipc_connect`s to the listener or the process is signalled.
  Corresponds to POSIX `accept(2)`. The kernel parks the caller
  on the listener via `BlockReason::Ipc { endpoint_id }`; the
  peer's `ipc_connect` wakes the parker with the new fd via the
  kernel's pending-wakes queue.
- `flags & accept_flags::NONBLOCK` (bit 0x0001): returns
  `-EAGAIN` immediately when the backlog is empty. Corresponds
  to POSIX `accept4(SOCK_NONBLOCK)`. Preserves v1's historical
  non-blocking-by-default behaviour for callers that want it.

Other flag bits are reserved and MUST be zero. A second blocking
`ipc_accept` on a listener that already has a parked acceptor
returns `-EAGAIN` regardless of flags (one-parker-per-listener
invariant; v1 display-server needs only one acceptor at a time).
Accept returns `ENOSPC` when minting the server endpoint would cross the hard
live-socket limit; the pending client remains on the backlog.

#### `ipc_send` (0x1005)

```
ipc_send(fd, buf: &[u8], fd_to_pass: Option<Fd>) -> n
```

The wire request carries `fd:u32`, `len:u32`, `fd_to_pass:i32`, and
`flags:u32` in the 16-byte argument window. A negative `fd_to_pass` means no
descriptor; a non-negative value names exactly one descriptor. `flags` MUST
be zero, and the heap input is exactly `len` payload bytes. Malformed lengths,
nonzero flags, and ancillary data on a pipe return `EINVAL` before any write.

**Semantics**: sends `buf` bytes on the socket and optionally enqueues that one
descriptor to be received on the peer's next `ipc_recv` that asks for it. A
passed fd is dup-equivalent
in the peer: the kernel installs a new `FdEntry` in the peer's
`FdTable` pointing at the same underlying object.

The kernel snapshots and retains that underlying object during
`ipc_send`; it never queues the sender-local fd number. Closing or reusing
the sender's numeric fd after a successful send cannot change the object the
receiver obtains. v1 supports vnode, character-device, pipe-read, and
pipe-write objects. Socket, display-connection, signal-channel, filesystem-
watch, host-file, and host-download objects have single-owner or process-local
semantics and are rejected with `ENOTSUP` before payload or ancillary state is
enqueued.

Each socket receive buffer is capped at 64 KiB. Stream sends may return a
positive short count bounded by both the peer's local free space and the
kernel-wide 32 MiB aggregate pipe/socket byte budget. If neither budget admits
one byte, a non-empty send returns `EAGAIN`; draining any IPC buffer can make a
parked `FD_WRITE` ready again. This byte pressure is transient.

The per-socket ancillary queue is capped at 64 entries and the aggregate
queued ancillary-reference count at 1,024. Crossing either descriptor bound
is a hard admission failure: `ENOSPC`, with neither payload nor the optional
descriptor enqueued. On a successful positive short send, that descriptor is
atomically queued with the first accepted prefix. The caller MUST omit it when
retrying the unsent byte suffix. A zero-byte send may carry one descriptor and
is governed by the same hard ancillary limits.

#### `ipc_recv` (0x1006)

```
ipc_recv(fd, buf: &mut [u8], recv_fd: bool, flags: u32) -> (n, fd_out?)
```

The wire request carries `fd:u32`, `len:u32`, `recv_fd_slot:i32`, and
`flags:u32`. A negative `recv_fd_slot` requests bytes only and leaves any
queued descriptor untouched. Any non-negative value opts into receiving
exactly one descriptor; v1 ignores the numeric slot value and installs the
descriptor in the lowest free fd-table slot. `flags = 0` is blocking.
`flags = 0x0001` (`NONBLOCK`) returns `EAGAIN` when neither bytes nor an
ancillary descriptor are available. Every other flag bit returns `EINVAL`
without consuming receive state.

**Semantics**: reads at most `len` bytes from the socket and optionally returns
one passed fd, already installed in the caller's fd table. With blocking flags,
an empty connected socket parks until bytes, a descriptor, EOF, a signal, or
close makes progress possible.

`Response.value` is the payload-byte count. With no installed descriptor,
payload begins at heap offset 0 and `extra_len = value`. When a descriptor is
installed, its new fd number is a 4-byte little-endian prefix, payload begins
at offset 4, and `extra_len = 4 + value`. The caller's output region must
therefore provide `len + 4` bytes when descriptor receipt is requested.

If installing a waiting descriptor would cross the receiver's fd-table limit,
`ipc_recv` returns `EMFILE` before consuming payload bytes or the ancillary
snapshot. The complete receive state remains queued for a later retry.

Closing a socket immediately drops that endpoint's unread byte/fd queues and
removes its IPC-table record. A connected peer may drain bytes already queued
on its own endpoint, then observes EOF; subsequent sends return `EPIPE`. Pipes
use the same ownership rule: once both endpoint refcounts reach zero the pipe
and unread bytes are reclaimed without requiring a final read.
Queued descriptor references are owned by the socket until receive transfers
them into an fd table. Close or read-side shutdown releases every unreceived
reference.

#### `ipc_pipe` (0x1007)

```
ipc_pipe() -> (read_fd, write_fd)
```

**Semantics**: allocates one unidirectional pipe and atomically installs its
read and write endpoints in the caller's fd table. The kernel admits at most
1,024 live pipe objects; allocation beyond that bound returns `ENOSPC` before
an id, object, or fd is consumed. The aggregate bytes queued across all pipes
and socket receive buffers share the 32 MiB limit documented above. Pipe
writes use the same stream rule: return a positive short count when some local
and aggregate capacity exists, and `EAGAIN` only when zero bytes can progress.

#### `ipc_peer_caps` (0x1008)

```
ipc_peer_caps(fd) -> CapSet
```

**Semantics**: returns the capability set the kernel captured for
the process on the other end of connected IPC socket `fd`. The
snapshot is taken when that endpoint connects or is accepted and
does not widen if the peer later receives additional capabilities.

This is PMos's `SO_PEERCRED` equivalent. Possession of the connected
fd is the authority to query its peer, so no `PROC_INSPECT`
capability is required. The caller cannot supply a pid, and no bytes
sent by the peer participate in the result. `EBADF` means `fd` is
not open, `ENOTSOCK` means it is not an IPC socket, and `ENOTCONN`
means the socket has no authenticated connected peer.

The display server MUST call this on every fd returned by
`ipc_accept` and use the returned set as that protocol client's
immutable capability snapshot. A failed credential query closes the
connection; the server MUST NOT accept capability claims in display
protocol messages.

**Why WASI doesn't cover this**: WASI preview 1 has no Unix-domain
peer-credential query. Kernel-authenticated IPC service authorization
cannot be built safely from client-provided protocol data.

#### `ipc_peer_pid` (0x1009)

```
ipc_peer_pid(fd) -> Pid
```

**Wire layout**: `args[0..4]` is the connected socket fd as little-endian
`u32`; there is no heap input or output. On success, `Response.value` is the
kernel-captured peer pid widened from signed `Pid` to `i64`.

**Semantics**: returns the pid the kernel captured for the process on the
other end of connected IPC socket `fd`. It reads the same immutable
connection-time credential record as `ipc_peer_caps`. Possession of `fd` is
the authority to query it: the caller cannot name an arbitrary process, no
`PROC_INSPECT` capability is required, and no bytes supplied by either peer
participate in the result. `EBADF` means `fd` is not open, `ENOTSOCK` means it
is not an IPC socket, and `ENOTCONN` means the socket has no authenticated
connected peer.

IPC services that associate protocol objects with processes MUST use this
query instead of accepting a pid in protocol payloads. In particular, the
display server MUST query every fd returned by `ipc_accept`, retain that pid
as immutable client metadata, and close the connection if the query fails.

**Why WASI doesn't cover this**: WASI preview 1 has no Unix-domain
peer-credential query. A pid carried in application protocol bytes would be a
claim, not an authenticated process identity, so it cannot safely replace
this fd-scoped kernel primitive.

### 3.2 Process management

#### `proc_spawn` (0x1100)

```
proc_spawn(manifest: &SpawnManifest) -> pid
```

`SpawnManifest` is serialised in the request body:

```
struct SpawnManifest {
    path:        PathBuf,            // absolute guest VFS or bundled-binary path
    argv:        Vec<String>,
    envp:        Vec<(String, String)>,
    stdin_fd:    Option<Fd>,         // dup from caller's fd table
    stdout_fd:   Option<Fd>,
    stderr_fd:   Option<Fd>,
    extra_fds:   Vec<(Fd, Fd)>,      // (from_caller, target_number_in_child)
    cwd:         Option<PathBuf>,    // inherits caller's cwd if None
    caps:        Option<CapSet>,     // inherit caller's caps if None; subset only
}
```

The canonical `SPAWN_V1` transport is one packed little-endian blob of at
most 32 KiB. Its fixed 48-byte header is:

|     Offset |  Type | Field                                             |
| ---------: | ----: | ------------------------------------------------- |
|          0 | `u32` | magic `SPN1` (`0x314e5053`)                       |
|          4 | `u16` | version `1`                                       |
|          6 | `u16` | flags: bit 0 cwd present, bit 1 caps present      |
|          8 | `u32` | total blob length                                 |
|     12, 14 | `u16` | path length, cwd length                           |
| 16, 18, 20 | `u16` | argv count, env count, extra-fd count             |
|         22 | `u16` | reserved, zero                                    |
| 24, 28, 32 | `i32` | stdin/stdout/stderr caller fd; `-1` means inherit |
|         36 | `u32` | reserved, zero                                    |
|         40 | `u64` | caps (zero when omitted)                          |

The body contains path bytes, optional cwd bytes, argv records
`[u16 length][UTF-8]`, env records
`[u16 key length][u16 value length][key][value]`, then extra-fd records
`[u32 caller fd][u32 child fd]`. Strings are length-delimited UTF-8 without
NUL. Paths are absolute; env keys are unique and do not contain `=`; explicit
child fds start at 5 because fd 3 is the WASI root preopen and fd 4 is the
signal channel. Lengths, integer conversions, reserved fields, duplicate fd
targets, and trailing bytes are rejected before a pid is published.

WASM callers may pass this blob through the additive
`pmos_ext.proc_spawn_manifest(ptr, len) -> i32` import. The legacy
`pmos_ext.proc_spawn(path_ptr, path_len, caps) -> i32` import and its older
path/caps request encoding remain accepted; `SPN1` is larger than the ring
heap and therefore deterministically distinguishes the versioned form from a
legacy path length.

**Semantics**: the kernel validates the manifest, allocates the
process identity and requests a new Worker with a fresh WASM instance
of the named binary, wires up
its fd table from the manifest's fd assignments (dupping each
parent-side fd into the child with its target number), applies
the caps (subset of caller's, enforced), sets cwd/envp, and
returns the new child's pid.

Socket fd mappings, whether selected for stdin/stdout/stderr or listed in
`extra_fds`, return `ENOTSUP`. V1 socket endpoints are single-owner objects
without a dup refcount; copying their `SocketId` would let either process close
the other process's live endpoint. The kernel resolves and validates every
`extra_fds` source before allocating a pid, so rejection leaves the parent's
socket and signal inbox intact and publishes no child or Worker.

Program resolution is kernel-authoritative. Normalised paths below `/bin/` or
`/usr/bin/` always resolve through the immutable build-time browser registry;
writable VFS shadows at those names are ignored. This prevents a root-preopen
application from replacing a binary that init later launches with privileged
capabilities. Dynamic guest-VFS executable bytes may resolve only from a
normalised path strictly below `/opt/`; symlink expansion is confined beneath
that root. An escape or any other namespace fails with `EACCES`. The target
must be regular, executable, readable, and no larger than 16 MiB; the kernel
copies those exact bytes synchronously into the host spawn request. A missing
`/opt/` target returns `ENOENT` without consulting the registry. Only the two
immutable bundled namespaces may fall back to that registry, and no runtime
fetch is permitted.

The kernel retains at most 256 non-reaped processes globally and 64 non-reaped
children per parent. Zombies count against both bounds until reaped. A spawn
that would exceed either limit returns `EAGAIN` before consuming a pid or
allocating child fd, capability, signal, scheduler, SAB, or Worker state. The
main-thread spawn router independently caps live user Workers at 256 and
rejects before allocating a SAB or constructing a Worker as defense in depth.

The host must synchronously reject a spawn request it cannot publish;
the kernel then rolls back the tentative pid and returns `EIO`.
Worker construction/boot happens asynchronously after publication.
Failure before the Worker's `booted` acknowledgement is reported as
an abnormal child exit, reconciled by the kernel, and observable by
the parent through `proc_wait`; it never leaves a runnable pid without
a Worker/SAB route.

**No `fork`**: intentionally absent. See `research.md` for the
rationale; briefly, there is no way to duplicate a WASM linear
memory and all of its host resources atomically.

**Why WASI doesn't cover this**: WASI preview 1 has `proc_exit`
but no process creation. A real OS must have it. This syscall is
the minimum process-creation interface that does not replicate
WASI and does not attempt `fork`.

#### `proc_wait` (0x1101)

```
proc_wait(pid: Pid, options: u32) -> (status, signum)
```

`pid`: `-1` or `0` = any child, `>0 = specific child`; values below `-1`
return `EINVAL` because process-group waits are not implemented. V1 supports
only the `WNOHANG` option bit. `WUNTRACED` and every unknown bit return
`EINVAL`.

**Semantics**: with `options = 0`, parks until a matching child transitions to
`Zombie`, then reaps it and returns its exit status. With `WNOHANG`, a live
matching child that is not yet a zombie returns `EAGAIN` immediately. A
matching zombie is always reaped synchronously.

#### `proc_kill` (0x1102)

```
proc_kill(pid: Pid, signum: u16) -> ()
```

**Semantics**: delivers a signal to `pid` via its signal channel. A process may
signal itself or one of its direct children; every other target requires
`PROC_KILL_ANY`. Signals are defined by number
(`SIGINT=2`, `SIGKILL=9`, `SIGUSR1=10`, `SIGUSR2=12`, `SIGPIPE=13`,
`SIGTERM=15`, and `SIGCHLD=17` — POSIX numbers). `SIGHUP`, `SIGQUIT`, and
every other unsupported nonzero number return `EINVAL` without delivery.

`signum = 0` is an existence-and-permission probe. It applies the same target
and capability checks as real delivery but queues no signal and does not
interrupt a parked syscall. It returns `ESRCH` for an absent or reaped target
and `ENOTCAPABLE` when the sender is neither the target, its parent, nor a
holder of `PROC_KILL_ANY`.

`SIGKILL` is non-catchable: the kernel synchronously transitions the
target to `Zombie`, releases its kernel resources, wakes/reaps a
matching parked parent, and requests forced termination of the
target's host Worker. Repeated SIGKILL against that terminal pid
returns `ESRCH` and does not repeat teardown side effects.

#### `proc_self` (0x1103) / `proc_parent` (0x1104)

Return the caller's pid / ppid. Trivial.

#### `proc_caps_get` (0x1105)

```
proc_caps_get(pid: Pid) -> CapSet
```

Returns the caps of `pid`. Self is always allowed; other
processes require `PROC_ENUMERATE`.

### 3.3 Display server access

#### `display_connect` (0x1200)

```
display_connect() -> fd
```

**Semantics**: atomically creates a stream socket and performs the asynchronous
`ipc_connect(fd, "/run/display")` queueing step. The caller must hold
`DISPLAY_CLIENT`; denial returns `ENOTCAPABLE`. This is the one and only entry
point into the display server for ordinary clients. The returned fd is an
ordinary IPC socket fd, follows the `Connecting` readiness semantics above,
and is used with
`ipc_send` / `ipc_recv` for the wire protocol documented in
`display-protocol.md`.

**Why a dedicated opcode**: the constitution (Principle II,
Principle VII) requires that connection to the display server
be a first-class, well-known entry — it is not just "open a
path." Having `display_connect` as an opcode makes it easy for
tools, tracers, and permission audits to find every attempt to
reach the display server in a single place.

#### `display_bind` (0x1201)

```
display_bind() -> fd
```

**Semantics**: atomically creates a stream listener, binds it to the reserved
`/run/display` abstract endpoint, and listens with the kernel-defined display
backlog. The caller must hold `DISPLAY_SERVER`; denial returns `ENOTCAPABLE`.
Only this opcode may claim the reserved binding, so an ordinary IPC listener
cannot race or impersonate the compositor. A second live display listener
returns `EADDRINUSE` without leaking a socket or fd.

### 3.4 Capability management

#### `cap_check` (0x1300)

```
cap_check(cap: Cap) -> bool
```

**Semantics**: returns whether the calling process holds `cap`.

#### `cap_list` (0x1301)

```
cap_list() -> CapSet
```

**Semantics**: returns the calling process's cap set.

#### `cap_grant` (0x1302)

```
cap_grant(pid: Pid, caps: CapSet) -> ()
```

**Semantics**: grants `caps` to `pid`. The calling process must
hold `CAP_GRANT`. `caps` must be a subset of the caller's own
caps (no privilege escalation).

**Why WASI doesn't cover this**: WASI has no capability model at
all beyond "what you were handed at instantiation." PMos's
privilege model (Principle II — desktop shell is just a program
with a capability) needs first-class kernel support for
capability query and grant.

### 3.5 Mount management (privileged)

#### `mount` (0x1400)

```
mount(target: &str, fstype: &str, flags: u16) -> ()
```

The wire argument window holds target pointer/length and fstype
pointer/length; the request `flags` field carries the mount flags. There is no
user-supplied source parameter in v1.

**Semantics**: without `MOUNT_REMOUNT`, mount a fresh `tmpfs` at an existing
empty directory `target`. Requires `MOUNT`; every other fstype returns
`EINVAL`. `devfs` and `procfs` are kernel-created singleton mounts and cannot
be instantiated through this syscall. `opfs` is likewise boot-only. With
`MOUNT_REMOUNT`, the syscall changes flags on an existing mount in place and
does not instantiate a filesystem; the supplied fstype is ignored.

The normal boot layout mounts
the validated OPFS image at `/`, with `tmpfs` overlays at `/tmp` and `/run`,
`devfs` at `/dev`, and `procfs` at `/proc`. There is no `/persist` alias.
When OPFS is unavailable or an existing image is invalid, the kernel preserves
the image, logs the degraded state, and prepares a volatile `tmpfs` recovery
root. The browser substrate blocks ordinary interaction unless the user
explicitly accepts the temporary, reload-loss session defined by the block
driver contract; recovery preparation does not make persistence optional.

#### `umount` (0x1401)

```
umount(target: &str, flags: u32) -> ()
```

**Semantics**: unmount. Requires `MOUNT`. Returns `EBUSY` while any open vnode
fd or live filesystem watch pins the target mount. Closing the last such fd
releases the pin; watches on unrelated mounts do not block the operation.

#### `fs_chmod` (0x1403)

```text
fs_chmod(path: &str, mode: u32) -> ()
```

WASI preview 1 has no chmod operation, but package installation must preserve
archive executable modes. Wire request: `args[0..4] = path_len:u32`,
`args[4..8] = mode:u32`, `args[8..16] = 0`, and the heap payload is exactly
the absolute UTF-8 path. Only the low nine POSIX permission bits (`0..0777`)
are accepted. The call follows the final symlink, updates `ctime`, marks the
mount dirty, and persists through the normal VFS sync policy.

No additional capability is required in v1. PMos v1 is single-user and every
process receives the same root preopen; this extension changes metadata on a
path the process could already replace through WASI. A future multi-user
ownership model must add owner/capability enforcement here and to the ordinary
VFS mutation calls together.

### 3.6 Host file transfer

#### `host_file_recv` (0x1500)

```
host_file_recv(token: u32) -> fd
```

Wire request: `args[0..4] = token:u32`, `args[4..16] = 0`, with no heap
payload. The response value is the read-only fd.

**Semantics**: accepts a host-file `token` that the bootstrap
produces when the user drags a host-OS file onto a PMos window
or picks one via the file manager's `Import…` menu
(`<input type="file">`). Returns a read-only fd whose reads
stream the host file's bytes. Closing the fd releases the
browser-side `File` reference.

End-to-end flow:

1. A DOM `drop` or `<input type="file">` change event fires on
   the bootstrap.
2. The bootstrap reads the chosen file, assigns a monotonically
   increasing token, and sends `host_file_dropped(token, name,
mime, bytes)` to the kernel Worker. The browser message owns one
   byte array; the Worker copies it into the kernel through bounded
   heap-scratch chunks after reserving its declared size.
3. The kernel forwards the message as a readable notification
   on a well-known IPC endpoint (`/run/host-files`) that the
   file manager subscribes to.
4. The file manager calls `host_file_recv(token)` to obtain an
   fd, reads to completion, and writes the bytes to the target
   path in the VFS.

The returned fd is compatible with `fd_read`, `fd_pread`,
`fd_close`. `fd_seek` MAY return `ENOTSUP` (sequential
implementations are conformant); if supported, seek MUST be
relative to the original host-file contents. `fd_filestat_get`
returns the host file's size in `st_size` and zero for all
timestamps.

Exactly one `host_file_recv` call is permitted per token; a
second call with the same token returns `EBADF`. Tokens expire
when the tab is closed; a token that references an expired
`File` returns `ENOENT`.

Access control: `HOST_TRANSFER` is required both to connect to
`/run/host-files` and to call `host_file_recv`. The kernel checks
the capability independently at both boundaries. Unknown or
already-consumed tokens return `EBADF`.

The v1 import limit is 16 MiB per file, 32 MiB across all live import bytes,
and 64 live import tokens. "Live" includes in-progress reservations, completed
pending tokens, and bytes held behind an open `host_file_recv` fd. A declaration
or completed drop beyond a bound is rejected with `EFBIG`; closing the imported
fd releases its bytes and token slot. An incomplete or aborted transfer is
never published as a token and releases its reservation.

**Why WASI doesn't cover this**: WASI has no notion of host
`File` objects or of a browser-side file picker / drag-drop
source. PMos is a guest OS inside the browser substrate; host
files reach the guest through DOM events that the kernel
cannot observe directly. Without this syscall there is no
mechanism for the user's own host files to enter PMos — FR-032a
(file import) would be impossible to satisfy without violating
layering.

#### `/run/host-files` notification stream

`/run/host-files` is a kernel-owned stream endpoint. A subscriber creates a
STREAM socket and connects normally with `ipc_connect`; userland cannot bind or
impersonate the endpoint. Connecting requires `HOST_TRANSFER`. The kernel
accepts subscribers internally and writes
one framed metadata record for every pending or newly selected host file. A
subscriber that connects after a selection receives records for every token
that is still pending, so opening Files after a drag does not lose the user's
choice. Publishing a record follows the normal stream-send wake semantics: if
the subscriber is blocked in a receive on its endpoint, the kernel makes that
process runnable and completes the pending receive with the record bytes.

Each record is little-endian and self-delimiting:

```text
u32 frame_len       # complete record length, including this word
u32 magic           # ASCII "PMHF" (0x46484d50 little-endian)
u16 version         # 1
u16 flags           # 0 in v1
u32 token
u64 size
u16 name_len
u16 mime_len
u8  name[name_len]  # UTF-8
u8  mime[mime_len]  # UTF-8
```

`frame_len` is at least 28. Names are limited to 255 UTF-8 bytes and MIME
labels to 127 bytes. A subscriber must buffer partial reads and may receive
multiple records in one read. Unknown versions or malformed lengths are
discarded without calling `host_file_recv`.

#### `host_file_pick` (0x1501)

```text
host_file_pick() -> ()
```

Requests the browser's native multi-file picker. Selection is asynchronous:
success means the browser accepted the request, not that the user chose a
file. Chosen files return through `/run/host-files` and `host_file_recv`; cancel
produces no notification. Requires `HOST_TRANSFER`. This kernel-mediated call
is the only userland route to the DOM picker. After this authorization reaches
the browser main thread, the substrate MUST expose a one-shot confirmation and
invoke the native picker synchronously from that trusted confirmation click;
it MUST NOT assume transient user activation survives the kernel and Worker
message round trip. The confirmation is never exposed before authorization.

Wire request: no heap payload and every inline argument byte is zero. The
success response has `value = 0`.

#### `host_file_send` (0x1502)

```text
host_file_send(name: &str, mime: &str) -> fd
```

Creates a write-only host-download stream. Userland writes the selected PMos
file through ordinary `fd_write`; an explicit `fd_close` finalises the stream
and asks the browser to save a `Blob` under `name`. `fd_read` and seeking are
unsupported. Process exit, signal termination, fd replacement, or transport
failure before explicit close cancels the partial download rather than
emitting a truncated file. Empty files are valid. V1 limits a single download
to 16 MiB and all simultaneously open download streams to 32 MiB; a write that
would cross either bound returns `EFBIG` without appending bytes. Requires
`HOST_TRANSFER`.

Wire request: `args[0..4] = name_len:u32`, `args[4..8] = mime_len:u32`, and
`args[8..16] = 0`; the heap payload is exactly `name || mime` in UTF-8. Names
are limited to 255 bytes and MIME labels to 127 bytes, with control characters
rejected. The response value is the new fd. An `EFBIG` write marks that stream
failed, so a later close cancels it instead of downloading the valid prefix.

`HOST_TRANSFER` is granted to the bundled Files app by the desktop shell and
is not part of the ordinary application capability set. This prevents an
untrusted app from repeatedly opening host dialogs or forcing browser
downloads or consuming explicit-user-choice import tokens.

**Why WASI doesn't cover this**: preview 1 can read and write guest file
descriptors but cannot invoke a browser picker or create a host download. The
write-only fd keeps bulk bytes on the ordinary bounded syscall path and keeps
DOM access below the kernel/driver boundary.

### 3.7 Filesystem watch

#### `fs_watch` (0x1402)

```
fs_watch(path: &str, mask: u32, flags: u32) -> fd
```

**Semantics**: opens a watch on the VFS node at `path`. The
returned fd is read-only; each successful read yields one or more fixed-size,
8-byte `FsWatchEvent` records describing subsequent changes. Watch reads are
always nonblocking: an empty queue returns `EAGAIN`, and `poll_oneoff(FD_READ)`
is the blocking primitive. Callers MUST defensively retry `fd_read` on
`EAGAIN` after a readiness wake. A read buffer shorter than 8 bytes returns
`EINVAL` without consuming the queued record. `fd_close` unregisters the
watch.

```
struct FsWatchEvent {
    mask:  u32, // little-endian WATCH_CREATE, WATCH_DELETE, or WATCH_MODIFY
    inode: u32, // little-endian affected inode
}
```

`mask` is a non-empty bitwise OR of:

| bit | name           | meaning                                           |
| --- | -------------- | ------------------------------------------------- |
| 0   | `WATCH_CREATE` | a direct child of a watched directory was created |
| 1   | `WATCH_DELETE` | a direct child of a watched directory was removed |
| 2   | `WATCH_MODIFY` | the watched file's contents were written          |

`flags` MUST be zero in v1; non-zero flags return `EINVAL`. Recursive watches,
rename records, timestamps, and inline names are not part of this ABI.

Multiple watches on the same path are permitted; each gets its
own fd and its own event queue. Each queue retains at most 256 unread records;
on overflow the oldest record is discarded so a slow consumer sees the most
recent state without allowing unbounded kernel memory growth.

Admission is bounded to 4 live watches per process, 1,024 watches on one
`(mount,inode)` target, and 2,048 watches kernel-wide. The per-process limit,
together with the 256-process ceiling, keeps a later Shell or display-server
restart admissible even after ordinary-client saturation. The first call past
any applicable bound returns `ENOSPC` atomically; closing a watch fd or exiting
its owner releases every charge and permits re-admission.

Not every filesystem is required to support watches. In v1,
`tmpfs` and the OPFS root filesystem MUST support `fs_watch`;
`devfs` and `procfs` MAY return `ENOTSUP` for any `fs_watch`
call. A filesystem that does not support watches MUST return
`ENOTSUP` at `fs_watch` time rather than silently accepting the
watch and never firing events.

**Why WASI doesn't cover this**: WASI preview 1 has no
filesystem-notification API (no `inotify`, no
`FileSystemWatcher`, no equivalent). Any PMos feature that
needs to react to a filesystem change — most notably the
toolkit's theme live-reload path (FR-034) and future preference
reloaders — would have to poll, burning the idle performance
budget (Principle IX). `fs_watch` is the minimum kernel
primitive that lets userland listen for preference-file,
configuration-file, and installed-package changes efficiently,
without introducing a new protocol-layer event on top of the
display server (which would violate Principle II layering by
binding userland preferences to the compositor).

The fixed-size record keeps queue accounting bounded and lets callers drain
multiple events without parsing variable-length payloads. Userland resolves
the inode or refreshes its watched state after a notification when it needs a
name-level view.

---

## 4. Error codes

All syscalls return a non-negative value on success and a negative
errno on failure. The full errno list follows WASI preview 1
(`ESUCCESS = 0`, `EACCES = 2`, `EADDRINUSE = 3`, … through
`EXDEV = 75`), reproduced in the `abi` crate's generated
`errno.rs`. Extension-specific additions:

- `ENOTCAPABLE (76)`: the caller's capability set does not permit
  this operation.
- `ENOABIVER (77)`: the spawned process uses an incompatible ABI
  version.

---

## 5. Compatibility and growth policy

- Adding a new extension syscall is a MINOR bump of `ABI_VERSION`.
- Changing the meaning of an existing argument or its layout is a
  MAJOR bump.
- Removing an existing syscall is a MAJOR bump.
- Widening a return type (adding output bits in reserved flags) is
  a MINOR bump, provided existing consumers see `0` in the new
  bits.

The kernel's syscall dispatcher and the `abi` crate both encode
`ABI_VERSION`; mismatched processes fail at spawn with `ENOABIVER`
rather than silently misbehaving. A test in the kernel verifies
that the WASI preview 1 table is complete and that the extension
table is dense (no gaps between 0x1000 and the highest extension
opcode, with the exception of the deliberate gap between the
`0x14xx` mount/watch range and the `0x15xx` host-import range).

### Change log

- **v1.4** (2026-08-11): added `ipc_peer_pid` (0x1009, §3.1), an
  additive fd-scoped query for the immutable pid in a connected socket's
  kernel-authenticated credential snapshot. Programs compiled against v1.3
  continue to run unchanged.
- **v1.3** (2026-08-07): added `host_file_pick` (0x1501), the
  write-only `host_file_send` stream (0x1502), `fs_chmod` (0x1403), and the
  `HOST_TRANSFER` capability. These additions do not affect conforming v1.2
  programs. `proc_spawn` also gained kernel-owned VFS executable loading
  without changing its user-visible request layout. See the pre-release alpha
  erratum below for the reserved socket-type value `1`.
- **v1.2** (2026-08-07): added `ipc_peer_caps` (0x1008, §3.1),
  an additive fd-scoped peer-credential query used by the display
  server and other privileged IPC services. Programs compiled
  against v1.1 continue to run unchanged.
- **v1.1** (2026-04-13): added `fs_watch` (0x1402, §3.7) and
  `host_file_recv` (0x1500, §3.6). Both are additive; programs
  compiled against v1.0 continue to run unchanged. Bump is
  MINOR per the bump rules above.
- **v1.0**: initial release.

**Pre-release alpha erratum (2026-08-09)**: earlier alpha code accepted
`ipc_socket(1)` while constructing the same byte-stream object as type `0`.
That behavior did not implement DGRAM record or address semantics and is not
retained. Before the first stable release, v1.3 reserves type `1` and returns
`ENOTSUP` atomically without changing the wire layout or consuming a socket
id, object, fd slot, quota, or namespace state.
