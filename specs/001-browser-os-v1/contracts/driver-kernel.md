# Contract: Driver ↔ Kernel Interface

**Status**: canonical reference for v1.
**Audience**: kernel implementers, driver implementers, authors of
the TypeScript bootstrap.

Drivers are TypeScript modules that run outside the kernel Worker.
They are the only layer allowed to touch browser APIs that the
kernel cannot reach (DOM, canvas, input events, `fetch`, OPFS).
They never hold privileges above what they expose to the kernel.
The kernel talks to drivers through a combination of
`postMessage` (for setup and cold paths) and shared-memory rings
(for hot paths).

This document covers the three contracts that matter:

1. **Syscall ring layout** — how user processes talk to the kernel.
2. **Kernel ↔ driver control channel** — how the kernel asks a
   driver to do something and receives the reply.
3. **Device-specific streams** — per-driver wire formats for the
   framebuffer, input, block, net, and console devices.

All byte offsets are in bytes. All integers are little-endian. All
atomics ops are as specified by the JS `Atomics` API.

---

## 1. Syscall ring layout (process ↔ kernel)

Each process Worker is given, at spawn time, a single
`SharedArrayBuffer` of size `0x10000` (64 KiB) with this layout:

```
offset  size  field
------  ----  -----------------------------------------------------
0x0000  4     req_head         (atomic u32) - producer index into the request ring
0x0004  4     req_tail         (atomic u32) - kernel's consumer index
0x0008  4     res_head         (atomic u32) - kernel's producer index into the response ring
0x000C  4     res_tail         (atomic u32) - user's consumer index
0x0010  4     user_wait_slot   (atomic u32) - user uses Atomics.wait here
0x0014  4     kernel_wait_slot (atomic u32) - kernel uses Atomics.wait here (shared across processes)
0x0018  4     user_block_count (atomic u32) - diagnostic counter
0x001C  4     kernel_block_count (atomic u32) - diagnostic counter
0x0020  32    flags + reserved header
0x0040  0x3FC0  request ring (16128 bytes, stores Request records)
0x4000  0x3FC0  response ring (16128 bytes, stores Response records)
0x8000  0x8000  heap scratch (32 KiB) — for request/response payloads that don't fit inline
```

`Request` and `Response` have the shapes defined in
`syscalls.md §1`. Each record is rounded to 32-byte alignment for
cache friendliness.

### 1.1 Request submission (user side)

```
1. write Request into request ring at offset (req_head % ring_size)
2. increment req_head atomically
3. Atomics.store(user_wait_slot, REQUESTED)    // magic u32 values
4. Atomics.notify(kernel_wait_slot, 1)
5. Atomics.wait(user_wait_slot, REQUESTED)     // blocks until kernel signals
6. read Response from response ring at offset (res_tail % ring_size)
7. increment res_tail atomically
```

Magic values (defined in `abi::ring`):

- `IDLE = 0`
- `REQUESTED = 1`
- `SERVICING = 2`
- `READY = 3`

### 1.2 Request dispatch (kernel side)

```
loop:
    Atomics.wait(kernel_wait_slot, 0)
    for each process with req_head > req_tail:
        read Request
        service it (may defer to a driver via the control channel)
        write Response
        Atomics.store(user_wait_slot, READY)
        Atomics.notify(user_wait_slot, 1)
        req_tail += 1
```

In practice the kernel holds a sorted queue of ready processes,
not a scan. The above is the semantic form.

### 1.3 Payload handling

A request whose argument payload is larger than the inline
`args[]` field uses the heap scratch region. The user writes the
payload into `heap_scratch` at some offset, sets `heap_ptr` and
`heap_len` in the Request record, and the kernel reads from that
offset. The kernel uses the same region for large responses.

### 1.4 Lifecycle

The SAB is created by the kernel Worker before spawning the user
Worker, and passed to the user Worker in the `postMessage` that
starts it. On process exit, the kernel reclaims the SAB by
dropping its JS reference; the user Worker is terminated by the
kernel so nothing can touch the SAB afterwards.

---

## 2. Kernel ↔ driver control channel

Drivers live on the main thread (or, for the block driver, in its
own Worker). The kernel Worker talks to them via `postMessage`
plus per-driver SAB rings for data-path operations.

### 2.1 Bootstrap

At boot, the JS bootstrap (`bootstrap.ts`) creates each driver,
creates the kernel Worker, and uses `postMessage` to send each
driver's `MessagePort` to the kernel. The kernel stores one port
per driver and uses it for the driver's control channel.

### 2.2 Control messages

All control messages are structured-cloneable objects with a
`kind` field. The message types are:

```ts
type ToDriver =
  | { kind: "device_open"; dev: DevId; flags: number; token: u32 }
  | { kind: "device_close"; dev: DevId; token: u32 }
  | { kind: "device_ioctl"; dev: DevId; op: u32; arg: ArrayBufferLike; token: u32 }
  | { kind: "attach_data_ring"; dev: DevId; sab: SharedArrayBuffer; layout: RingLayoutId; token: u32 };

type FromDriver =
  | { kind: "device_open_ok"; token: u32; handle: u32 }
  | { kind: "device_open_err"; token: u32; errno: number }
  | { kind: "device_close_ok"; token: u32 }
  | { kind: "device_ioctl_ok"; token: u32; result: u32; extra?: ArrayBufferLike }
  | { kind: "device_ioctl_err"; token: u32; errno: number }
  | { kind: "attach_data_ring_ok"; token: u32 }
  | { kind: "driver_event"; dev: DevId; event: DriverEvent };  // unsolicited
```

`token` is a kernel-assigned correlation id. Every request carries
one; every reply echoes it.

`driver_event` is an unsolicited upward message (driver → kernel)
for things like "input event available" or "buffer released by
framebuffer". The kernel translates these into its own internal
scheduling actions (waking processes, draining rings).

### 2.3 Fast-path rings per driver

For hot paths, the control channel is too slow. Each driver that
has hot data exports its own `SharedArrayBuffer` ring. These
rings are attached to the kernel at device-open time via
`attach_data_ring`. They have the same atomics-slot / producer /
consumer shape as the syscall ring, but their payload layouts
are per-driver.

Layouts are defined below.

---

## 3. Framebuffer driver (/dev/fb0)

### Ring layout

```
head:      atomic u32 (submitted frames)
tail:      atomic u32 (presented frames)
wait_slot: atomic u32
// slots of:
//   u32 buffer_offset   // into a SAB shared pool
//   u32 width
//   u32 height
//   u32 stride
//   u32 format
//   u32 damage_x, damage_y, damage_w, damage_h
```

Only one SAB pool exists per framebuffer connection. The display
server allocates the pool and passes it to the framebuffer driver
via `device_ioctl(SET_POOL, sab_handle)`.

### ioctls

| op              | arg            | returns           | notes                                     |
|-----------------|----------------|-------------------|-------------------------------------------|
| `SET_POOL`      | sab_handle, size | ok               | set the shared framebuffer pool           |
| `GET_MODE`      | —              | width, height, refresh_hz | the canvas current size/DPR        |
| `SET_CURSOR`    | cursor buffer  | ok                | upload a cursor bitmap                    |

### Events

- `present_complete(frame_id)`: fires when the driver has
  submitted a frame to the canvas. Triggers the display server's
  `frame` callback delivery.
- `mode_changed(width, height, refresh_hz)`: canvas size changed
  (e.g., window resize); display server recomposites.

**Main-thread behaviour**: the framebuffer driver holds a top-level
`<canvas>` in the DOM. Where `OffscreenCanvas` is available
(Chromium, Firefox), the driver transfers the OffscreenCanvas to
the display server Worker so the server can paint into it directly;
the main-thread driver's only job becomes `commit` + request
animation frames. Where OffscreenCanvas is not available, the
driver receives `ImageData` over a SAB copy and calls
`putImageData` itself.

---

## 4. Input driver (/dev/input/kbd, /dev/input/mouse)

### Ring layout

Two rings (one per device). Each slot is a 32-byte record:

```
ts:       u64     // ms since boot
kind:     u8      // KEY | BUTTON | MOTION | WHEEL | MODS
code:     u16     // key code or button code
value:    i32     // 0/1 for press/release, signed delta for motion/wheel
meta:     u32     // modifiers snapshot
reserved: u8[11]
```

The input driver listens on the canvas element for `keydown`,
`keyup`, `mousedown`, `mouseup`, `mousemove`, `wheel`, and
`contextmenu` (prevented). It normalises events (translating
browser key codes to a stable internal set) and pushes them into
the ring. It also raises a `driver_event: input_available` so the
kernel can wake any process blocked on reading the device.

### Key code space

The internal keycode space is a compact integer mapping, defined in
`abi::input`. It is NOT a DOM event code; it is a hand-written
table of ~130 physical keys with stable values. The keymap file
advertised by `pmd_keyboard.keymap` is a binary form of the same
table.

---

## 5. Block driver (/dev/sda — OPFS-backed)

The block driver is the only driver that runs in its own dedicated
Worker (not the main thread) because `FileSystemSyncAccessHandle`
is only available inside a Worker.

### Ring layout

```
head, tail, wait_slot
// slots of:
//   u16 op         // READ=1 | WRITE=2 | FLUSH=3 | TRIM=4
//   u16 _pad
//   u32 request_id
//   u64 lba        // logical block address
//   u32 count      // number of blocks
//   u32 buf_offset // into the block driver's dedicated SAB payload pool
//   u32 buf_len
```

The kernel allocates a persistent SAB payload pool at boot and
passes it to the block driver. Block numbers are 4 KiB.

### Semantics

- `READ` reads `count * 4096` bytes from LBA into `buf`.
- `WRITE` writes `count * 4096` bytes from `buf` to LBA.
- `FLUSH` calls `SyncAccessHandle.flush()` on all open handles.
- `TRIM` is a no-op in v1.

All operations are synchronous inside the block driver Worker —
OPFS gives us `read`/`write`/`flush` as blocking calls when
called on a `FileSystemSyncAccessHandle`. The driver processes
one request at a time; the kernel serialises requests.

### OPFS layout (driver side)

The driver opens four `SyncAccessHandle`s at boot, one per OPFS
file listed in `data-model.md §3.3`:

- `pmos.img/superblock`
- `pmos.img/inodes.segment`
- `pmos.img/data.segment.0`
- `pmos.img/journal`

LBAs are translated to (file, offset) by a simple partition table
stored in the superblock: block 0..N₀ is the superblock, blocks
N₀..N₁ are inode segment, etc. When the data segment grows past
its current file, the driver lazily creates
`pmos.img/data.segment.1` and updates the partition table.

### First-boot bootstrap

On first boot, the block driver finds no `pmos.img/superblock`
file. It creates the four files, writes a zeroed superblock, and
reports "uninitialised" to the kernel. The kernel then runs a
`mkfs` routine that writes the v1 superblock, allocates root inode
1, creates `/bin`, `/etc`, `/usr`, `/usr/bin`, `/usr/share`,
`/usr/share/applications`, `/opt`, `/home`, `/home/user`, `/dev`,
`/proc`, `/run`, `/tmp`, and copies the bundled binaries (from
the read-only initramfs embedded in the kernel WASM) into `/bin`
and `/usr/bin`. On subsequent boots the driver reads an existing
superblock and the kernel skips `mkfs`.

---

## 6. Net driver (/dev/net)

### ioctls

| op              | arg                 | returns        | notes                  |
|-----------------|---------------------|----------------|------------------------|
| `FETCH_BEGIN`   | request metadata    | handle         | POST, GET, headers     |
| `FETCH_POLL`    | handle              | state, bytes   | non-blocking read      |
| `WS_OPEN`       | url                 | handle         | WebSocket open         |
| `WS_SEND`       | handle, bytes       | ok             |                        |
| `WS_RECV`       | handle              | bytes          |                        |
| `WS_CLOSE`      | handle              | ok             |                        |

No shared-memory ring for v1; the net driver uses plain
postMessage because network syscall throughput is not a hot path.

### Ownership

Net syscalls are gated on the `NET` capability. Processes without
`NET` cannot open `/dev/net` and the corresponding `sock_*`
WASI calls fail with `EPERM`.

---

## 7. Console driver (/dev/console)

A very simple serial-style character device:

- Writes go to `console.log` (and optionally a ring buffer the
  test harness reads via Playwright).
- Reads block until a hidden `<textarea>` element receives input
  (or the harness injects input for tests).

Used for:

1. Early kernel debug output when no display server is up yet.
2. The "headless shell over a serial-style device" that
   Principle VIII requires: even without a display server, a
   shell can be spawned, wired to `/dev/console` for stdin/stdout,
   and the whole kernel can be exercised from JS.
3. The Playwright integration test harness's communication
   channel.

---

## 8. Driver lifecycle and error recovery

- A driver that fails to initialise at boot reports an error via
  `postMessage` and the kernel marks the device unavailable.
  Apps that try to open the device get `ENODEV`.
- A driver that crashes mid-run (uncaught exception) notifies the
  kernel via a global error handler in `bootstrap.ts` and the
  kernel marks the device in error. In v1 the kernel does not
  automatically restart drivers; doing so is a v2 amendment.
- A driver MUST NOT throw synchronously during a control-channel
  handler. Errors MUST come back as `*_err` replies.

---

## 9. Testability contract

Every driver module exports a `createDriver(options?)` factory so
that:

- Test code can pass a `mockKernelPort` instead of the real
  kernel port, and the driver's control channel is directed at
  the mock.
- Test code can pass a stub `document` / `canvas` object for
  headless unit tests.

This is how the Vitest unit tests for drivers run without a
browser at all. Integration tests (Playwright) use the real
drivers against the real kernel.
