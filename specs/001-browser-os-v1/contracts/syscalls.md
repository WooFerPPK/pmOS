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
constant `ABI_VERSION = (1, 1)` that kernel and userland check at
process spawn. Mismatches fail spawn with a distinct error code. A
breaking change to any entry below requires bumping the major
component. See §5 for the change log.

---

## 1. Transport

Every syscall travels over a `SharedArrayBuffer` ring buffer shared
between the calling process's Worker and the kernel Worker. The
buffer layout is specified in `driver-kernel.md` under "Syscall ring
layout"; this file treats the transport as opaque and specifies only
the *semantic* contract (what request fields mean, what the kernel
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
Atomics.wait described in `research.md` / `driver-kernel.md`. The
kernel never knows that the caller is blocked; it only knows that
the request is queued and will be serviced.

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

| Opcode symbol       | Category         | Notes |
|---------------------|------------------|-------|
| `args_get`          | args             |       |
| `args_sizes_get`    | args             |       |
| `environ_get`       | environ          |       |
| `environ_sizes_get` | environ          |       |
| `clock_res_get`     | clocks           | monotonic, realtime, process_cputime_id supported; thread_cputime_id returns ENOTSUP |
| `clock_time_get`    | clocks           |       |
| `fd_advise`         | file             | no-op in v1 (accepted, ignored) |
| `fd_allocate`       | file             | returns ENOTSUP if FS does not support it |
| `fd_close`          | file             |       |
| `fd_datasync`       | file             | forwarded to FS `sync` |
| `fd_fdstat_get`     | file             |       |
| `fd_fdstat_set_flags` | file           |       |
| `fd_fdstat_set_rights` | file          | rights enforced from `Process.caps` |
| `fd_filestat_get`   | file             |       |
| `fd_filestat_set_size` | file          |       |
| `fd_filestat_set_times` | file         |       |
| `fd_pread`          | file             |       |
| `fd_prestat_get`    | file             | preopens: `/` `stdin` `stdout` `stderr` |
| `fd_prestat_dir_name` | file           |       |
| `fd_pwrite`         | file             |       |
| `fd_read`           | file             |       |
| `fd_readdir`        | file             |       |
| `fd_renumber`       | file             | dup-like |
| `fd_seek`           | file             |       |
| `fd_sync`           | file             |       |
| `fd_tell`           | file             |       |
| `fd_write`          | file             |       |
| `path_create_directory` | file         |       |
| `path_filestat_get` | file             |       |
| `path_filestat_set_times` | file       |       |
| `path_link`         | file             |       |
| `path_open`         | file             |       |
| `path_readlink`     | file             |       |
| `path_remove_directory` | file         |       |
| `path_rename`       | file             |       |
| `path_symlink`      | file             |       |
| `path_unlink_file`  | file             |       |
| `poll_oneoff`       | poll             | supports `fd_read`, `fd_write`, and clock subscriptions |
| `proc_exit`         | process          | exit status propagates to waitpid |
| `proc_raise`        | process          | delivers signal-equivalent to self via the signal channel |
| `sched_yield`       | sched            |       |
| `random_get`        | random           | uses `crypto.getRandomValues` through the kernel |
| `sock_accept`       | sock             | for AF_UNIX sockets in PMos, for TCP-over-WebSocket otherwise |
| `sock_recv`         | sock             |       |
| `sock_send`         | sock             |       |
| `sock_shutdown`     | sock             |       |

**Deviation notes**:

- `fd_advise` is a no-op; the FS back end does nothing with
  advice. This is permitted by WASI preview 1.
- `fd_fdstat_set_rights` is honoured: if the caller tries to
  widen a right, `ENOTCAPABLE` is returned.
- `proc_raise` delivers to the caller's own signal channel; it
  never affects another process (use `proc_kill` for that, which
  is an extension syscall).
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
ipc_socket(ty: u8, flags: u16) -> fd
```

`ty`: `0 = STREAM`, `1 = DGRAM`. `flags`: `1 = O_NONBLOCK`,
`2 = O_CLOEXEC`.

**Semantics**: allocates a `Socket` object in the kernel's IPC
table, returns a fresh fd referencing it. The socket starts in
the `Unbound` state.

**Why WASI doesn't cover this**: WASI's `sock_*` syscalls model
network sockets (AF_INET-style). AF_UNIX / AF_LOCAL and
file-descriptor passing are absent. File-descriptor passing in
particular is load-bearing for the display server connection —
there is no POSIX equivalent via WASI.

#### `ipc_bind` (0x1001)

```
ipc_bind(fd, path: &str) -> ()
```

**Semantics**: binds the socket to `path` in the VFS. Creates a
`Socket` node at `path`. Fails with `EADDRINUSE` if already
bound, `EACCES` if the calling process may not write at the
parent directory, `EINVAL` if the socket is already bound.

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

**Semantics**: connects the socket to a listening server at
`path`. Blocks (or returns EAGAIN under NONBLOCK) until the
server `accept`s or refuses. Fails with `ECONNREFUSED` if no
listener, `ENOENT` if no node at `path`, `EACCES` on capability
denial.

#### `ipc_accept` (0x1004)

```
ipc_accept(fd, flags: u16) -> fd
```

**Semantics**: dequeues a pending connect and returns a new
connected socket fd on the server side.

#### `ipc_send` (0x1005)

```
ipc_send(fd, buf: &[u8], fds_to_pass: &[Fd]) -> n
```

**Semantics**: sends `buf` bytes on the socket and enqueues
`fds_to_pass` to be received on the peer's next
`ipc_recv` that asks for fds. Each passed fd is dup-equivalent
in the peer: the kernel installs a new `FdEntry` in the peer's
`FdTable` pointing at the same underlying object.

#### `ipc_recv` (0x1006)

```
ipc_recv(fd, buf: &mut [u8], max_fds: u32) -> (n, fds_out)
```

**Semantics**: reads bytes from the socket and returns up to
`max_fds` of any fds the peer has passed. The returned fds are
already installed in the caller's fd table.

### 3.2 Process management

#### `proc_spawn` (0x1100)

```
proc_spawn(manifest: &SpawnManifest) -> pid
```

`SpawnManifest` is serialised in the request body:

```
struct SpawnManifest {
    path:        PathBuf,            // absolute, to a regular file in the VFS
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

**Semantics**: the kernel validates the manifest, creates a new
Worker with a fresh WASM instance of the named binary, wires up
its fd table from the manifest's fd assignments (dupping each
parent-side fd into the child with its target number), applies
the caps (subset of caller's, enforced), sets cwd/envp, and
returns the new child's pid.

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

`pid`: `-1 = any child`, `0 = any in process group`,
`>0 = specific child`. `options` supports `WNOHANG`, `WUNTRACED`
(accepted but not meaningful in v1 since job control stops are
not implemented).

**Semantics**: blocks until a matching child transitions to
`Zombie`, reaps the zombie, returns its exit status.

#### `proc_kill` (0x1102)

```
proc_kill(pid: Pid, signum: u16) -> ()
```

**Semantics**: delivers a signal to `pid` via its signal channel.
Requires `PROC_KILL_ANY` capability to send to a process the
caller did not spawn. Signals are defined by number
(`SIGHUP=1, SIGINT=2, SIGQUIT=3, SIGKILL=9, SIGTERM=15,
SIGCHLD=17, SIGPIPE=13` — POSIX numbers).

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

**Semantics**: short-hand for `ipc_connect(fd,
"/run/display")`. The only semantic difference from an ordinary
`ipc_connect` is that the kernel short-circuits the capability
check for `DISPLAY_CLIENT` here: this is the one and only
entry point into the display server for ordinary clients. The
returned fd is an ordinary IPC socket fd and is used with
`ipc_send` / `ipc_recv` for the wire protocol documented in
`display-protocol.md`.

**Why a dedicated opcode**: the constitution (Principle II,
Principle VII) requires that connection to the display server
be a first-class, well-known entry — it is not just "open a
path." Having `display_connect` as an opcode makes it easy for
tools, tracers, and permission audits to find every attempt to
reach the display server in a single place.

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
mount(source: &str, target: &str, fstype: &str, flags: u32) -> ()
```

**Semantics**: mount a filesystem at `target`. Requires `MOUNT`
capability. V1 supports `tmpfs`, `devfs`, `procfs`. `opfs` can
only be mounted at boot by the kernel itself.

#### `umount` (0x1401)

```
umount(target: &str, flags: u32) -> ()
```

**Semantics**: unmount. Requires `MOUNT`.

### 3.6 Host import

#### `host_file_recv` (0x1500)

```
host_file_recv(token: u32) -> fd
```

**Semantics**: accepts a host-file `token` that the bootstrap
produces when the user drags a host-OS file onto a PMos window
or picks one via the file manager's `Import…` menu
(`<input type="file">`). Returns a read-only fd whose reads
stream the host file's bytes. Closing the fd releases the
browser-side `File` reference.

End-to-end flow:

1. A DOM `drop` or `<input type="file">` change event fires on
   the bootstrap.
2. The bootstrap stores the `File` object in a per-tab token
   table and sends a `host_file_dropped(token, name, size,
   mime)` message to the kernel via the driver control channel.
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

Access control: no capability is required beyond ordinary
IPC-endpoint subscription. The bootstrap is trusted to only
produce tokens for files the *user* chose via explicit DOM
events. Only processes that subscribe to `/run/host-files` and
receive an unsolicited `host_file_dropped` notification have a
meaningful reason to call this syscall; any other caller
receives `EBADF` for an unknown token.

**Why WASI doesn't cover this**: WASI has no notion of host
`File` objects or of a browser-side file picker / drag-drop
source. PMos is a guest OS inside the browser substrate; host
files reach the guest through DOM events that the kernel
cannot observe directly. Without this syscall there is no
mechanism for the user's own host files to enter PMos — FR-032a
(file import) would be impossible to satisfy without violating
layering.

### 3.7 Filesystem watch

#### `fs_watch` (0x1402)

```
fs_watch(path: &str, flags: u32) -> fd
```

**Semantics**: opens a watch on the VFS node at `path`. The
returned fd is readable; each read yields zero or more
fixed-size `FsWatchEvent` records describing subsequent
changes. The fd behaves like any other file descriptor for the
purposes of `fd_fdstat_set_flags` (`O_NONBLOCK`), `poll_oneoff`,
and `fd_close`; by default `fd_read` blocks until at least one
event is available.

```
struct FsWatchEvent {
    kind:     u8,       // 1=MODIFIED 2=CREATED 3=DELETED
                        // 4=RENAMED_FROM 5=RENAMED_TO
    _pad:     [u8; 3],
    ts_ns:    u64,      // monotonic timestamp of the event
    name_len: u16,      // length of the name payload that follows
    // followed by `name_len` UTF-8 bytes (relative name; empty
    // for events on the watched path itself), padded to 4-byte
    // boundary
}
```

`flags`:

| bit | name                       | meaning                                                              |
|-----|----------------------------|----------------------------------------------------------------------|
| 0   | `FS_WATCH_RECURSIVE`       | watch all descendants of a directory                                 |
| 1   | `FS_WATCH_COALESCE_MODIFY` | collapse consecutive `MODIFIED` events on the same path into one     |

Multiple watches on the same path are permitted; each gets its
own fd and its own event queue.

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

- **v1.1** (2026-04-13): added `fs_watch` (0x1402, §3.7) and
  `host_file_recv` (0x1500, §3.6). Both are additive; programs
  compiled against v1.0 continue to run unchanged. Bump is
  MINOR per the bump rules above.
- **v1.0**: initial release.
