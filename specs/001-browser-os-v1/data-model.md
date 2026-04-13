# Data Model: Browser OS v1

**Branch**: `001-browser-os-v1` | **Date**: 2026-04-13
**Feature**: [spec.md](./spec.md) | **Plan**: [plan.md](./plan.md) | **Research**: [research.md](./research.md)

This document is the single source of truth for the kernel's
in-memory data structures, the VFS/on-disk layout, the display
server's object model, and the package manifest. Every field, every
invariant, and every state transition that multiple crates depend on
lives here. Field names use `snake_case`; types are Rust-flavoured
but intentionally abstract so the contracts/*.md files can translate
them into wire layouts without contradiction.

---

## 1. Process

A **process** is the unit of isolated execution. One WASM Instance,
one Worker, one linear memory.

```rust
struct Process {
    pid: Pid,                      // NonZero u32; 1 is init
    ppid: Pid,                     // 0 for init (no parent)
    pgid: Pid,                     // process group (for job control)
    state: ProcState,
    exit_status: Option<ExitStatus>,
    name: String,                  // argv[0] stem, for /proc and ps
    argv: Vec<String>,
    envp: BTreeMap<String, String>,
    cwd: PathBuf,                  // absolute, canonicalised
    umask: Mode,
    fd_table: FdTable,
    caps: CapSet,
    worker_handle: WorkerHandle,   // opaque; the host's reference to the Worker
    sab_region: SabRegion,         // the shared SAB holding the syscall ring
    spawn_time: NanosSinceBoot,
    cpu_time_ns: u64,              // accumulated; updated on preemption
    mem_limit: Option<usize>,      // linear memory cap
    signal_queue: VecDeque<Signal>,  // pending signals
    signal_handlers: [SigDisp; NSIG],
}

enum ProcState {
    Starting,           // Worker created, WASM instantiating
    Ready,              // eligible to run
    Running,            // currently executing on its Worker
    BlockedOnSyscall,   // parked on Atomics.wait in its own SAB
    BlockedOnIpc,       // waiting on an IPC endpoint (pipe read, socket accept, etc.)
    BlockedOnWait,      // waitpid() on a child
    Zombie,             // exited but not yet reaped
    Dead,               // reaped; slot reclaimable
}
```

**Invariants**:

- `pid` is unique over the lifetime of the system. PIDs are not
  reused within a boot (64k wraparound is not a real concern in a
  single tab; a free list keeps reuse cheap if needed).
- `ppid == 0` if and only if `pid == 1` (init).
- `fd_table`, `caps`, and `cwd` are owned by the process, never
  shared. `envp` is owned.
- `worker_handle` is valid exactly from `Starting` through
  `Zombie`. On reaching `Dead` the host Worker is terminated and
  the handle is dropped.
- A process in `Zombie` state has all of its other resources (fds,
  IPC endpoints, display-server surfaces) already reclaimed. Only
  its `pid` and `exit_status` are retained, for `waitpid` to read.
- `BlockedOnSyscall` and `BlockedOnIpc` are distinct so the
  scheduler can decide what event unblocks the process.

**State transitions**:

```
Starting --(wasm instantiated)--> Ready
Ready    --(scheduler picks)----> Running
Running  --(syscall entry)------> BlockedOnSyscall
Running  --(exit / crash)-------> Zombie
BlockedOnSyscall --(result ready)--> Ready
BlockedOnIpc     --(data arrived)--> Ready
BlockedOnWait    --(child exited)--> Ready
Zombie   --(waitpid reaps it)----> Dead
```

---

## 2. Per-process file descriptor table

```rust
struct FdTable {
    entries: Vec<Option<FdEntry>>,   // indexed by fd number; lowest-free-on-alloc
    soft_limit: usize,               // configurable cap
}

struct FdEntry {
    object: FdObject,                // what this fd refers to
    flags: FdFlags,                  // O_CLOEXEC-ish, O_NONBLOCK, ...
    offset: FileOffset,              // only for seekable file objects
}

enum FdObject {
    VnodeHandle(Arc<Vnode>),         // file, directory, device
    PipeRead(PipeId),
    PipeWrite(PipeId),
    SocketEndpoint(SocketId),
    DisplayConn(ConnId),             // connection to /run/display (special-cased for cap check)
    SignalChannel,                   // the per-process signal inbox
}
```

**Invariants**:

- `fd 0, 1, 2` are reserved for `stdin`, `stdout`, `stderr`. They
  may refer to any `FdObject`; `proc_spawn` sets them up from the
  parent's fd table per the spawn manifest.
- `fd` numbers are not stable across processes; dup/spawn inherit by
  contents, not by index.
- Closing the last reference to a `VnodeHandle` drops any in-memory
  cache associated with that inode.

---

## 3. Virtual filesystem

### 3.1 Inode / Vnode

```rust
struct Vnode {
    ino: Ino,                        // unique within a mount
    mount: MountId,
    ty: NodeType,
    mode: Mode,                      // POSIX permission bits
    nlink: u32,
    size: u64,
    mtime: NanosSinceEpoch,
    atime: NanosSinceEpoch,
    ctime: NanosSinceEpoch,
    fs_specific: FsSpecific,         // per-filesystem data (extent list, devnum, etc.)
}

enum NodeType {
    RegularFile,
    Directory,
    CharDevice(DevNum),
    Socket,
    SymLink,
    Fifo,
}
```

**Invariants**:

- `(mount, ino)` is a globally unique vnode identity.
- `nlink == 0 && no open fd references` → vnode is reclaimed by the
  underlying filesystem.
- Directory vnodes own a name → child-ino map; children do not carry
  their own name (POSIX semantics).

### 3.2 Mount table

```rust
struct Mount {
    id: MountId,
    mountpoint: PathBuf,             // absolute path where this fs is mounted
    fs: Box<dyn Filesystem>,         // dyn trait object
    mount_flags: MountFlags,
}

trait Filesystem {
    fn root(&self) -> Vnode;
    fn lookup(&self, dir: &Vnode, name: &str) -> Result<Vnode>;
    fn read(&self, node: &Vnode, offset: u64, buf: &mut [u8]) -> Result<usize>;
    fn write(&self, node: &Vnode, offset: u64, buf: &[u8]) -> Result<usize>;
    // ... create, unlink, rename, readdir, stat, truncate, sync, ...
}
```

**Default mount set at boot** (created by the kernel before
spawning init):

| Mountpoint | Filesystem  | Backing                                 |
|------------|-------------|------------------------------------------|
| `/`        | `opfs`      | OPFS-backed block device via block driver |
| `/tmp`     | `tmpfs`     | kernel linear memory                     |
| `/dev`     | `devfs`     | kernel-synthesised device nodes          |
| `/proc`    | `procfs`    | kernel-synthesised process introspection |
| `/run`     | `tmpfs`     | kernel linear memory (sockets, pidfiles) |

### 3.3 On-disk layout (OPFS-backed root filesystem)

The OPFS block device is **not** one browser file per PMos file.
It is a small number of large OPFS files addressed as a single
block device:

```
opfs://pmos.img/superblock           (4 KiB)
opfs://pmos.img/inodes.segment       (grows in 1 MiB steps)
opfs://pmos.img/data.segment.<n>     (grows in 4 MiB steps, 0..N)
opfs://pmos.img/journal              (ring, 1 MiB)
```

**Superblock** (fixed 4 KiB, first 256 B used in v1):

```
magic          [u8; 8]     = b"PMOSFS\x01\x00"
version_major  u16         = 1
version_minor  u16         = 0
block_size     u32         = 4096
inode_count    u64
inode_free     u64
data_block_count u64
data_block_free u64
root_ino       u64         = 1
journal_head   u64
journal_tail   u64
checksum       u32         // of the remainder
```

**Inode on disk** (256 B fixed):

```
ino              u64
mode             u32
nlink            u32
uid              u32        // reserved; single-user -> 1000
gid              u32        // reserved -> 1000
size             u64
atime_ns         u64
mtime_ns         u64
ctime_ns         u64
extent_count     u16
flags            u16
direct_blocks    [u64; 12]  // first 12 * 4 KiB inline
indirect_block   u64        // pointer to an indirect block of more pointers
double_indirect  u64
```

Enough inline capacity for 48 KiB per file before any indirection;
good enough for the bundled apps' typical outputs.

**Journaling**: every write that mutates metadata goes through the
ring journal. On mount, the kernel replays unfinished transactions.
This gives the consistency guarantee in FR-014 (no corruption on
abrupt tab close). The journal's `head`/`tail` pointers in the
superblock are updated with an atomic pair of writes (write journal
entry → fsync segment → write superblock → fsync superblock).

---

## 4. Pipes and IPC sockets

### 4.1 Pipe

```rust
struct Pipe {
    id: PipeId,
    buffer: RingBuffer,              // kernel-owned; default 64 KiB
    reader_closed: bool,
    writer_closed: bool,
    readers: Vec<Pid>,               // processes blocked reading
    writers: Vec<Pid>,               // processes blocked writing
}
```

**Invariants**:

- A pipe lives as long as at least one of its ends has an open fd.
  When the last writer closes, readers see EOF; when the last reader
  closes, writers receive a pipe-closed signal-equivalent.

### 4.2 Unix-domain-socket-equivalent

```rust
struct Socket {
    id: SocketId,
    ty: SocketType,                  // Stream, Dgram
    state: SocketState,              // Unbound, Listening, Connected, Closed
    bound_path: Option<PathBuf>,     // present after ipc_bind
    backlog: VecDeque<PendingConnect>,
    rx: RingBuffer,                  // inbox
    tx: RingBuffer,                  // outbox (points into the peer's rx)
    passed_fds_rx: VecDeque<FdObject>, // fd-passing queue
    peer: Option<SocketId>,
}
```

The `/run/display` socket is an ordinary `Socket` with a
well-known bound path, plus a kernel check that only processes
holding the `DISPLAY_CLIENT` capability may `ipc_connect` to it.
(`display_connect` is a thin wrapper around that connect that
encodes the capability check in a dedicated opcode.)

---

## 5. Capabilities

```rust
#[repr(u32)]
enum Cap {
    DISPLAY_CLIENT   = 1,  // may open a connection to /run/display
    DISPLAY_SERVER   = 2,  // may open /dev/fb0 and /dev/input/*
    SHELL            = 3,  // may subscribe to the display server's window-list events (pmd_shell_manager)
    PROC_ENUMERATE   = 4,  // may enumerate /proc entries for processes other than itself
    PROC_KILL_ANY    = 5,  // may send signals to processes it does not own
    NET              = 6,  // may use the net syscalls
    MOUNT            = 7,  // may call mount/unmount
    CAP_GRANT        = 8,  // may call cap_grant
    DEV_BLOCK        = 9,  // may open block devices directly
    KEYMAP_ADMIN     = 10, // may bind pmd_keymap_manager and switch the system keyboard layout
}

struct CapSet(u64);    // bitset — v1 has < 64 caps
```

**Initial grants**:

- **init (pid 1)**: every capability. It is the master from which
  all capabilities derive.
- **display server**: `DISPLAY_SERVER`, `DEV_BLOCK` (for reading
  its own binary/config).
- **desktop shell**: `DISPLAY_CLIENT`, `SHELL`, `PROC_ENUMERATE`,
  `KEYMAP_ADMIN` (so the taskbar can see all windows and
  processes, and so the launcher can hand `KEYMAP_ADMIN` to the
  settings app at spawn time).
- **bundled sysmon**: `PROC_ENUMERATE`, `PROC_KILL_ANY` (so it can
  list processes and kill them).
- **bundled settings** (when launched via the desktop shell's
  launcher): `DISPLAY_CLIENT`, `KEYMAP_ADMIN`. The launcher
  passes `KEYMAP_ADMIN` through from its own cap set on spawn;
  the settings `.desktop` entry declares it via
  `X-PMos-Caps=DISPLAY_CLIENT;KEYMAP_ADMIN`.
- **everything else** (terminal, files, edit, third-party apps):
  `DISPLAY_CLIENT` and nothing else by default.

`cap_grant` is the only way to widen a process's capability set
after startup, and it requires the caller to hold `CAP_GRANT`. In
v1 only init has `CAP_GRANT`, which means capability grants
effectively happen at spawn time and not at runtime. Runtime
grant is the forward-compatible hook.

---

## 6. Display server object model

The display server's state is structured around clients, globals,
and per-surface objects. Wire-layer details (opcodes, event
numbers) live in `contracts/display-protocol.md`; this section is
the in-memory shape.

```rust
struct Client {
    id: ClientId,
    pid: Pid,                    // for /proc lookups and window-list → pid mapping
    socket: SocketId,
    objects: BTreeMap<ObjectId, Object>,  // client-local object ID space
    pending_frame_callbacks: Vec<CallbackId>,
    caps_snapshot: CapSet,       // checked at connect time
}

enum Object {
    Display,                     // object 1, always present
    Registry,
    Compositor,
    ShmPool(ShmPool),
    Buffer(Buffer),
    Surface(Surface),
    XdgSurface(XdgSurface),
    XdgToplevel(XdgToplevel),
    XdgPopup(XdgPopup),
    Output(Output),
    Seat(Seat),
    Pointer(Pointer),
    Keyboard(Keyboard),
    Callback(Callback),
}

struct Surface {
    id: ObjectId,
    pending: SurfaceState,       // double-buffered state, applied on commit
    current: SurfaceState,
    role: Option<SurfaceRole>,
    frame_callbacks: Vec<CallbackId>,
    input_region: Option<Region>,
    opaque_region: Option<Region>,
}

struct SurfaceState {
    attached_buffer: Option<BufferHandle>,
    position: (i32, i32),        // only meaningful for child surfaces
    damage: Vec<Rect>,
}

enum SurfaceRole {
    XdgToplevel { title: String, app_id: String, size: (u32, u32), state: ToplevelState },
    XdgPopup    { parent: ObjectId, anchor_rect: Rect, constraint: Anchor },
    Cursor,
    DesktopBackground,           // reserved for a future lower-layer role; in v1 the shell uses XdgToplevel with a special role hint
}

struct Buffer {
    id: ObjectId,
    shm_pool: ObjectId,
    offset: u32,
    width: u32,
    height: u32,
    stride: u32,
    format: PixelFormat,
}

struct ShmPool {
    id: ObjectId,
    sab_handle: SabHandle,       // shared with the client; the server has read-only access
    size: u32,
}
```

**Invariants**:

- `SurfaceState` is double-buffered: clients set `pending` via
  requests and promote it to `current` via `commit`. The
  compositor only reads `current`.
- A `Buffer`'s underlying pixel data lives in the client's SAB
  shm pool, visible to the server as a borrowed byte slice. No
  copy is needed on commit.
- A frame callback fires exactly once: the compositor inserts a
  "ready to draw next frame" event onto the client's event
  queue after presenting this frame, then removes the callback.

---

## 7. Compositor state

```rust
struct Compositor {
    outputs: Vec<Output>,        // v1: exactly one
    stack: Vec<LayerEntry>,      // z-ordered visible surfaces, bottom first
    keyboard_focus: Option<(ClientId, ObjectId)>,  // the surface receiving keys
    pointer_focus: Option<(ClientId, ObjectId)>,
    pointer_pos: (i32, i32),
    keyboard_state: KeyboardState,
    damage_accum: Region,        // since last present
    frame_budget_ns: u64,        // derived from refresh rate
}

struct LayerEntry {
    client: ClientId,
    surface: ObjectId,
    layer: Layer,                // Background, Bottom, Normal, Top, Overlay
}
```

**Focus policy (v1)**: click-to-focus. Moving the pointer does not
change keyboard focus. A click on a surface re-issues
`keyboard_focus` to that surface and emits `enter`/`leave`
keyboard-protocol events to the previous and new focused clients.

---

## 8. Package manifest (`manifest.toml`)

```toml
# manifest.toml — v1 schema
[package]
name          = "edit"               # globally unique, [a-z0-9_-], 1..=40 chars
version       = "1.0.0"              # semver
display_name  = "Text Editor"
author        = "PMos"               # free text
summary       = "Simple text editor" # short description

[exec]
binary        = "bin/edit.wasm"      # path inside the bundle
argv          = []                   # extra args prepended to user invocations
envp          = {}                   # initial environment overrides (string -> string)

[ui]
icon          = "assets/icon.png"    # optional; 32x32 min, 256x256 max
mime_types    = ["text/plain", "text/markdown"]  # files that open with this app
categories    = ["Utility", "TextEditor"]        # freedesktop-style, informational

[capabilities]
required      = ["DISPLAY_CLIENT"]
# optional caps the app requests; install-time policy decides whether to grant
optional      = []
```

**Bundle layout inside the tar**:

```
manifest.toml
bin/<binary>.wasm
assets/...              (optional; icon, localisations eventually)
```

**Install path** after extraction: `/opt/<name>/`. Desktop entry
written to `/usr/share/applications/<name>.desktop`:

```
[Desktop Entry]
Name=Text Editor
Exec=/opt/edit/bin/edit.wasm
Icon=/opt/edit/assets/icon.png
MimeType=text/plain;text/markdown;
Categories=Utility;TextEditor;
X-PMos-Caps=DISPLAY_CLIENT
```

**Uninstall** = `rm -rf /opt/<name>` + `rm
/usr/share/applications/<name>.desktop`. Launcher's next refresh
picks up the removal.

---

## 9. Init configuration (`/etc/init.conf`)

```toml
# /etc/init.conf — read by init at boot

[boot]
display_server = "/usr/bin/display-server"
shell          = "/usr/bin/shell"
autostart      = []                    # v1: empty; optional apps to launch

[capabilities.display-server]
grant = ["DISPLAY_SERVER", "DEV_BLOCK"]

[capabilities.shell]
grant = ["DISPLAY_CLIENT", "SHELL", "PROC_ENUMERATE"]

[capabilities.sysmon]
grant = ["DISPLAY_CLIENT", "PROC_ENUMERATE", "PROC_KILL_ANY"]
```

Changes to `/etc/init.conf` take effect on the next boot. Init
reads this file, spawns the display server, waits for its
`/run/display` socket to exist, spawns the shell, then spawns
anything under `autostart`.

---

## 10. /proc schema

`/proc` is a read-only synthetic filesystem. Its entries:

```
/proc/                       (directory)
/proc/self -> /proc/<pid>    (symlink to the caller's own pid dir)
/proc/version                (kernel version string)
/proc/uptime                 ("<secs> <idle_secs>")
/proc/meminfo                (total/free/used, in bytes)
/proc/loadavg                ("<1m> <5m> <15m> <running>/<total> <lastpid>")
/proc/storage                ("<quota_total> <quota_used> <quota_free>" in bytes, read from the block driver; a single line, space-separated, terminated by a newline. All three values are decimal u64.)
/proc/<pid>/                 (per-process directory; exists iff pid is alive)
/proc/<pid>/status           (name, state, pid, ppid, uid, vmsize, ...)
/proc/<pid>/cmdline          (argv joined by '\0')
/proc/<pid>/environ          (envp joined by '\0')
/proc/<pid>/cwd -> ...       (symlink to cwd)
/proc/<pid>/fd/              (directory; one symlink per open fd)
/proc/<pid>/comm             (short name)
/proc/<pid>/stat              (terse one-line status; ps-style)
```

**Access control**: listing `/proc/<pid>` for a pid the caller
does not own requires `PROC_ENUMERATE`. Reading
`/proc/<pid>/environ` is gated on the same capability (environments
may contain secrets). This is how sysmon gets its view.

---

## 11. State transitions at a glance

- **Boot**: kernel starts → VFS mounts → init spawns →
  display server spawns → shell spawns → autostart apps spawn.
- **App launch**: shell calls `proc_spawn` with the app's
  manifest; kernel creates process; kernel wakes the launcher's
  waitlist on `exec_ok`.
- **App exit**: app calls `proc_exit` → kernel moves process
  to `Zombie`, closes fds, releases caps, releases display-server
  surfaces (by posting a synthetic `destroy` through the display
  socket), notifies parent's waitpid.
- **Shell kill + replace** (layering test): sysmon calls
  `proc_kill(shell_pid, SIGKILL)` → kernel reaps shell → display
  server remains; apps' surfaces remain. init's child-exit handler
  is notified. The user (or sysmon, or any authorised process)
  calls `proc_spawn` for the replacement shell binary with the
  `SHELL` capability. New shell connects to the display server,
  subscribes to the window-list, re-enumerates open windows, and
  draws its own chrome.

Every transition described here maps to functional requirements in
the spec and to test cases in the contracts and in the Playwright
integration suite.
