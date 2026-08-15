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
1. Atomics.store(user_wait_slot, REQUESTED)    // stage before publication
2. write Request into request ring at offset (req_head % ring_size)
3. increment req_head atomically
4. increment kernel_wait_slot and Atomics.notify(kernel_wait_slot, 1)
5. while res_head == res_tail:
   a. Atomics.store(user_wait_slot, REQUESTED)
   b. re-check res_head != res_tail and continue if a response landed
   c. Atomics.wait(user_wait_slot, REQUESTED)
6. read Response from response ring at offset (res_tail % ring_size)
7. increment res_tail atomically
```

The response-ring predicate, not one `Atomics.wait` return, is authoritative.
`user_wait_slot` is reused for consecutive syscalls: a process may observe
`READY`, consume response A, and begin waiting for B before the kernel executes
A's following `Atomics.notify`. That late notification may legally wake B. The
condition loop treats it as stale and parks again; the store-plus-re-check closes
the corresponding response-publication versus park race.

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

The main thread creates the per-pid SAB and user Worker after the
kernel Worker publishes `proc:spawn`. It sends the SAB in the user
Worker's one `boot` message, but MUST NOT publish `proc:sab` to the
kernel Worker until the user Worker replies `booted`. If Worker
construction or boot delivery fails first, main sends
`proc:exited(pid, -1, trap)` instead; the kernel reconciles and
removes the tentative process so no unreachable live pid remains.

The kernel Worker resolves program identity before publishing that message.
Normalised `/bin/*` and `/usr/bin/*` paths bypass writable VFS shadows and use
only the immutable registry. Other executable paths (canonically dynamic
packages under `/opt`) may come from the VFS when regular, executable, and at
most 16 MiB. The resulting
`proc:spawn` message always carries owned `wasmBytes`; main never reads OPFS or
performs a runtime network fetch to resolve a program.

The kernel admits at most 256 non-reaped processes globally and 64 non-reaped
children per parent, returning `EAGAIN` before consuming a pid or allocating
child state. Main independently limits the spawn router to 256 live user
Workers and checks the bound before SAB or Worker construction.

Every host-observed Worker return, trap, or script error produces one
`proc:exited` notification. The kernel treats that notification as an
idempotent acknowledgement when the process already called
`proc_exit`, and otherwise performs the authoritative exit transition,
resource release, `SIGCHLD`, and parked-parent wake/reap.

For SIGKILL, the kernel first makes the process terminal, then emits
`proc:terminate(pid, signal=9)` to main. Main terminates the backing
Worker, drops its pid/SAB routing entry, and replies `proc:exited` as
an idempotent teardown acknowledgement. Once all references are
dropped, the SAB is reclaimed by the host GC.

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
  | {
      kind: "device_ioctl";
      dev: DevId;
      op: u32;
      arg: ArrayBufferLike;
      token: u32;
    }
  | {
      kind: "attach_data_ring";
      dev: DevId;
      sab: SharedArrayBuffer;
      layout: RingLayoutId;
      token: u32;
    };

type FromDriver =
  | { kind: "device_open_ok"; token: u32; handle: u32 }
  | { kind: "device_open_err"; token: u32; errno: number }
  | { kind: "device_close_ok"; token: u32 }
  | {
      kind: "device_ioctl_ok";
      token: u32;
      result: u32;
      extra?: ArrayBufferLike;
    }
  | { kind: "device_ioctl_err"; token: u32; errno: number }
  | { kind: "attach_data_ring_ok"; token: u32 }
  | { kind: "driver_event"; dev: DevId; event: DriverEvent }; // unsolicited
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

| op           | arg              | returns                   | notes                           |
| ------------ | ---------------- | ------------------------- | ------------------------------- |
| `SET_POOL`   | sab_handle, size | ok                        | set the shared framebuffer pool |
| `GET_MODE`   | —                | width, height, refresh_hz | the canvas current size/DPR     |
| `SET_CURSOR` | cursor buffer    | ok                        | upload a cursor bitmap          |

### Framed write opcodes

The current WASI path writes framebuffer commands to `/dev/fb0`. Byte 0 is
the opcode and the remaining bytes are its payload:

| opcode | name                      | payload                                                                                                                                           |
| ------ | ------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| `0x01` | `SET_MODE`                | `width:u32, height:u32`                                                                                                                           |
| `0x02` | `BLIT`                    | `width:u32, height:u32, rgba:RGBA8[]`                                                                                                             |
| `0x03` | `BLIT_BEGIN`              | `width:u32, height:u32`                                                                                                                           |
| `0x04` | `BLIT_CHUNK`              | `offset:u32, rgba_chunk:u8[]`                                                                                                                     |
| `0x05` | `BLIT_END`                | empty                                                                                                                                             |
| `0x06` | `PATCH`                   | `x:u32, y:u32, width:u32, height:u32, rgba:RGBA8[]`                                                                                               |
| `0x07` | `PATCH_RLE`               | `x:u32, y:u32, width:u32, height:u32, runs:(count:u32, rgba:[u8;4])[]`                                                                            |
| `0x08` | `PATCH_PALETTE_RLE_BATCH` | `rect_count:u8, palette_count_minus_one:u8, palette:RGBA8[], rects:{x:u32, y:u32, width:u32, height:u32, runs:(count:u16, palette_index:u8)[]}[]` |
| `0x09` | `PRESENT_FENCE`           | `serial:u32`                                                                                                                                      |

All integers are little-endian. `PATCH` pixels are tightly packed,
row-major RGBA8 with exactly `width * height * 4` bytes; width and height
MUST be non-zero. The rectangle MUST fit the active framebuffer mode and
its coordinates/extents MUST NOT overflow their u32 representation.

One user-process `fd_write` is limited by the 32 KiB SAB heap-scratch
window. Therefore a complete patch, including its one-byte opcode and
16-byte rectangle header, MUST be at most `0x8000` bytes. The maximum RGBA
body is 32,751 bytes (8,187 pixels). Larger damaged areas are split into
independently paintable horizontal strips; no patch assembly state exists
in the browser driver.

For a compressible rectangle, `PATCH_RLE` may replace those raw strips. Runs
are row-major and may cross scanline boundaries. Each count is non-zero and
the checked sum of all counts MUST equal `width * height`; the run stream has
no padding or trailing bytes. Before allocating decoded pixels or posting any
message, the driver validates the complete stream, verifies the rectangle is
inside the current `SET_MODE` geometry, and bounds decoded bytes by that
framebuffer size. Malformed input produces no partial presentation. The
display server selects RLE only when the complete command fits the same 32 KiB
write limit and is strictly smaller than the equivalent raw patch; otherwise
it falls back to `PATCH` strips.

When one scene changes two through eight vertically disjoint scanline bands,
`PATCH_PALETTE_RLE_BATCH` may carry them in one atomic write. The palette count
is `palette_count_minus_one + 1`, giving a range of one through 256 shared RGBA8
colors. Each rectangle has positive in-mode geometry and is followed by
row-major runs until exactly `width * height` pixels have been produced; run
counts are non-zero and palette indexes MUST be in range. The complete command
MUST fit the same 32 KiB write ceiling. The driver validates the entire stream,
requires no trailing bytes, and bounds the sum of all decoded rectangle areas
to the active framebuffer pixel count before allocating or posting anything.
If the palette, run stream, geometry, or command budget cannot satisfy these
rules, the display server uses the existing per-band `PATCH_RLE`/`PATCH` path
without changing damage. Band detection remains capped at eight; more
fragmented damage coalesces to one conservative bounding rectangle rather than
creating an attacker-controlled number of rectangles or writes.

After validation, the TypeScript driver copies ownership of the RGBA bytes
and emits the typed main-thread message
`fb:patch { x, y, width, height, rgba }`. `PATCH_RLE` expands to that same
single typed message, so it produces one presentation sequence event rather
than one event per run. A valid `PATCH_PALETTE_RLE_BATCH` expands to one
`fb:patch-batch { patches }` message. Main validates every decoded rectangle
before changing the canvas, paints the batch in one main-thread task, and emits
exactly one presentation-complete event after all rectangles land. Full
`fb:blit` messages remain supported for boot splashes, compatibility fixtures,
and complete redraws.

`PRESENT_FENCE` is a generic ordering marker, not a drawing command and not a
desktop-ready policy primitive. Its payload is exactly one little-endian `u32`
serial; the production display server allocates non-zero serials that advance
monotonically, wrapping from `u32::MAX` to 1. The driver rejects any other
payload length without emitting a main-thread message. A valid command changes
neither framebuffer mode nor pixels and emits the typed message
`fb:present-fence { serial }`; it does not advance the ordinary
presentation-complete sequence by itself.

The display server writes a fence only after every causally preceding scene
update has either been submitted successfully to `/dev/fb0` or compared equal
to the last successfully submitted pixel shadow. Framebuffer messages and the
fence leave the kernel worker through the same FIFO channel. Main-thread frame,
patch, and patch-batch handlers paint synchronously, so handling
`fb:present-fence` proves that all preceding visible updates have completed.
This ordering also covers the no-damage case without manufacturing a redundant
pixel upload. The framebuffer driver remains unaware of which higher-level
protocol or userland policy requested the fence.

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
`putImageData` itself. A rectangular patch is converted directly into a
rect-sized `ImageData` and painted at `(x, y)`; it MUST NOT allocate or copy
a full-frame pixel buffer. An atomic patch batch uses one rect-sized `ImageData`
per member and one completion after the complete batch. Full blits, patches,
and patch batches produce the same presentation-complete notification after
painting.

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

Every normalised mouse button event carries the pointer's current framebuffer
coordinates. Those coordinates are authoritative for that press or release:
the display input boundary updates its pointer position from the button record
before hit-testing. Correct click routing therefore does not depend on a
separate browser motion event surviving coalescing or scheduling delay.

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
- `IMAGE_STATE` reports whether the image was newly created by this open or
  already existed. Filesystem contents are not used to infer this state.

`WRITE` is successful only if `SyncAccessHandle.write()` returns the full
requested byte count. A short write is reported as `EIO` and must not be
reported to the kernel as a completed block write.

All operations are synchronous inside the block driver Worker —
OPFS gives us `read`/`write`/`flush` as blocking calls when
called on a `FileSystemSyncAccessHandle`. The driver processes
one request at a time; the kernel serialises requests.

### OPFS layout (driver side)

The driver opens one `SyncAccessHandle` for `pmos.img`. LBA `n` maps to byte
offset `n * 4096`. The superblock at LBA 0 records the contiguous journal,
inode-table, and data ranges used by the kernel filesystem implementation.

### First-boot bootstrap

On first boot, the block driver finds no `pmos.img` image. It creates the
image, pre-sizes it, and reports `NewlyCreated` to the kernel. Only a
`NotFoundError` while opening the image authorises this state. The kernel then
runs a `mkfs` routine that writes the v1 superblock, allocates root inode
1, creates `/bin`, `/etc`, `/usr`, `/usr/bin`, `/usr/share`,
`/usr/share/applications`, `/opt`, `/home`, `/home/user`, `/dev`,
`/proc`, `/run`, `/tmp`, and copies the bundled binaries (from
the read-only initramfs embedded in the kernel WASM) into `/bin`
and `/usr/bin`. On subsequent boots the driver reports the image as existing,
including when it is zero-length, all-zero, incompatible, or corrupt. The
kernel attempts a mount and skips `mkfs` even when that mount fails, preserving
the existing bytes for recovery.

A successfully formatted or mounted OPFS filesystem is installed as `/`, not
as a secondary mount. The kernel then overlays volatile filesystems at `/tmp`
and `/run`, devfs at `/dev`, and procfs at `/proc`. Userland always uses the
canonical `/home/user`, `/etc`, and `/usr` paths; `/persist` is not provided as
an alias. If the block device is unavailable or an existing image cannot be
mounted, the kernel may prepare a volatile tmpfs recovery root and
`/proc/storage` reports `0 0 0`, but main MUST NOT expose that root as an
ordinary interactive desktop by default. An invalid existing image is never
rewritten as part of this fallback.

### Degraded-storage boot protocol

Both failure boundaries publish the same typed kernel-Worker-to-main event:

```ts
type StorageDegraded = {
  kind: "storage:degraded";
  reason:
    | "opfs-open-failed"
    | "persistent-root-unavailable"
    | "persistent-root-invalid";
  detail: string;
  existingImagePreserved: true;
};
```

The Worker emits `opfs-open-failed` when `BlockDriver.openInOpfs()` rejects.
If the driver opened but Rust rejects or cannot install the root image, the
Worker translates the kernel's preserved-image fallback diagnostic into
`persistent-root-invalid` or `persistent-root-unavailable`. Only one event is
emitted per boot even when both boundaries observe the same failure.

Main installs a full interaction gate before acknowledging an ordinary
desktop. The gate has no automatic dismissal. `Retry persistent storage`
reloads the boot path while remaining blocked; the only way to use the
prepared tmpfs root is the explicit choice `Continue temporary session — files
will be lost on reload`. Keyboard, pointer, and host-file interaction remain
blocked until the kernel is ready and either persistence succeeded or that
choice was made. The temporary choice never formats, truncates, deletes, or
replaces an existing `pmos.img`.

---

## 6. Net driver (/dev/net)

### ioctls

| op            | arg              | returns      | notes              |
| ------------- | ---------------- | ------------ | ------------------ |
| `FETCH_BEGIN` | request metadata | handle       | POST, GET, headers |
| `FETCH_POLL`  | handle           | state, bytes | non-blocking read  |
| `WS_OPEN`     | url              | handle       | WebSocket open     |
| `WS_SEND`     | handle, bytes    | ok           |                    |
| `WS_RECV`     | handle           | bytes        |                    |
| `WS_CLOSE`    | handle           | ok           |                    |

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
- Complete lines are forwarded immediately. The kernel retains at most 16 KiB
  of an unterminated line; excess leading bytes are discarded and the newest
  diagnostic tail is preserved.
- Main mirrors console output to `console.log` unchanged. Its visible DOM
  transcript is a rolling tail capped independently at 512 lines and 256 KiB
  of UTF-8 text. Each render assigns that bounded private tail; it MUST NOT use
  an unbounded `textContent += chunk` read/copy/write loop.
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

## 8. Host file bridge

Host file transfer is a cold-path browser-substrate service, not a userland DOM
shortcut. Main owns picker and download browser APIs; the kernel Worker owns
capability checks, token publication, byte limits, and fd lifecycle.

Main → kernel uses the structured-clone message
`{ kind: "host:dropped", token, name, mime, bytes }`. One message carries at
most 16 MiB. The Worker reserves the declared size, copies metadata once, then
copies bytes through repeated kernel heap-scratch chunks. Concurrent incomplete
imports reserve at most 32 MiB; failure calls the abort export and publishes no
token.

Before calling `File.arrayBuffer()`, main reads the browser-owned immutable
`File.size` and rejects an individual file above 16 MiB. One picker/drop batch
is admitted only when it contains at most 64 files and at most 32 MiB in
aggregate, matching the kernel's exact live-import ceilings. Picker and drop
batches share one bounded FIFO and file bodies are read sequentially, so
overlapping gestures cannot materialize multiple bodies concurrently or retain
an unbounded queue of `File` objects. Main verifies the returned byte length
still equals `File.size` and remains within 16 MiB before posting it; the Worker
and kernel repeat their authoritative checks.

The storage-recovery interaction predicate gates both entry points. An
authorized `host:pick` mounts at most one browser-substrate confirmation;
the native picker opens synchronously only from that control's trusted click,
because activation is not assumed to survive the Worker round trip. Browser
keyboard routing leaves the focused confirmation's keydown and keyup events to
native button semantics rather than forwarding them into the guest. The
confirmation MUST NOT open the browser picker while normal interaction is
blocked, and a global drop MUST prevent browser navigation but MUST NOT read or
deliver files. The predicate is checked again at confirmation and when a
delayed picker selection or queued batch is about to read, preventing a
selection made before a degraded transition from becoming a hidden import
afterward.

Kernel → main uses:

```ts
type HostControl =
  | { kind: "host:pick" }
  | { kind: "host:download"; name: string; mime: string; bytes: Uint8Array };
```

`host:pick` is emitted only after the calling process passes the
`HOST_TRANSFER` check. `host:download` is emitted only when a write-only
download fd is explicitly closed successfully. Exit, signal teardown, fd
replacement, a failed write, or browser transport failure cancels staging.
Main copies the download bytes before asynchronous DOM work so kernel linear
memory is never retained by a browser callback.

## 9. Driver lifecycle and error recovery

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

## 10. Testability contract

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
