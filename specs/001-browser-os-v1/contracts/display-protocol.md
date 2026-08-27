# Contract: Display Server Protocol

**Status**: canonical reference for the PMos display server wire
protocol v1.
**Audience**: display server implementers, toolkit implementers,
third-party client authors writing apps that speak the protocol
directly.

This protocol is reached by calling `display_connect` (see
`syscalls.md`) which returns an IPC socket fd. Everything that
follows travels on that fd in both directions, as a stream of
framed messages.

The protocol is **inspired by Wayland** but is not wire-compatible
with it. Stealing the model is deliberate (Principle VII); stealing
the wire format would be harmful because the browser cannot pass
real file descriptors or DMA-BUFs.

---

## 1. Wire framing

Every message — both requests (client → server) and events
(server → client) — has this header followed by a payload:

```
struct MessageHeader {
    object_id:   u32,      // object this message is for; object 1 is pmd_display
    opcode:      u16,      // operation on that object
    length:      u16,      // total message length in bytes, including this header
    fd_passing:  u8,       // count of fds accompanying this message
    _reserved:   u8,       // MUST be 0
}
// followed by `length - 10` bytes of payload, layout determined by (object type, opcode)
// followed optionally by `fd_passing` entries from the ipc_recv fd queue
```

All integers are little-endian. Strings are length-prefixed
(`u32 byte_length` then UTF-8 bytes, padded to 4-byte boundary with
zeros). Objects are referenced by their 32-bit ID in the client's
local object space (each client has its own independent ID space —
object 1 always means "this client's display" on this connection).
These `ObjectId` values MUST NOT be exposed as cross-client window
identity; §15 defines a separate server-global `window_id` namespace.

**File-descriptor passing**: the header reserves a count for future messages
that attach kernel-mediated fds through the IPC side channel. Shipped v1 does
not define an fd-bearing display request. In particular, `pmd_shm.create_pool`
creates a server-owned byte store from a size and clients populate it with the
bounded inline `write` / `write_rows` requests in §7. A future shared-buffer
transport must add matching client, server, syscall, and integration coverage
before setting `fd_passing` to a non-zero value.

### 1.1 Initial stream classification and legacy compatibility

Transport reads do not preserve protocol message boundaries. The
server MUST accumulate the start of a new connection until it can
classify a complete header; a short first read MUST NOT select a
non-protocol path. Once a syntactically valid header is present, the
connection is permanently protocol-mode even when the declared payload
is still fragmented.

The sole legacy exception is the original `display-client-demo` fixture,
an exact 16-byte sequence encoding red, green, blue, and white RGBA
pixels (`ff0000ff 00ff00ff 0000ffff ffffffff`). The server MAY relay
that exact sequence through the compatibility raw-blit path after all 16
bytes have arrived. No other malformed or incomplete byte sequence may
be interpreted as pixels; it is rejected and the connection is closed.

---

## 2. Object types and IDs

Object IDs are allocated by the client for every shipped-v1 bound or
client-created object (registry, compositor, shm, pool, buffer, surface,
xdg role, seat, pointer, keyboard, callback). The display (object ID 1) is
implicit and always present. A client allocates IDs using odd/even
partitioning: odd IDs are allocated by the client and even IDs are reserved for
future server-created objects. Both sides MUST respect each other's partition;
registry-global numeric names are handles, not object IDs.

### 2.1 Per-connection resource ceilings

The display server applies the following hard ceilings independently to every
client connection. Admission checks happen before a new object or pool backing
store is installed. A request that would cross a ceiling is rejected with
`pmd_display.error(code = NO_MEMORY)` and leaves the relevant object ID and
resource map unchanged.

| Resource | Per-client ceiling |
|----------|--------------------|
| all live or lifecycle-deferred protocol object IDs, including implicit `pmd_display` | 512 |
| shm pools | 64 |
| aggregate shm-pool backing bytes | 128 MiB |
| buffers | 256 |
| surfaces | 64 |
| xdg toplevels | 64 |
| aggregate UTF-8 bytes retained in live toplevel titles and app IDs | 64 KiB |
| pending damage rectangles on one surface | 64 |
| queued protocol events | 1,024 events and 256 KiB |

The existing single-pool ceiling remains 64 MiB. A normal 1024x736 ARGB8888
double-buffered application pool is 6,029,312 bytes and therefore remains well
inside both pool limits.

Once a surface reaches the damage-rectangle ceiling, the next damage request
coalesces the retained rectangles and the new rectangle into one bounding
rectangle. Damage storage therefore remains bounded without losing the damaged
area. `commit` clears the pending damage collection as usual.

Protocol-event queue overflow is connection-fatal: the server queues no partial
event, marks the connection for teardown, and closes it after the current
dispatch pass. The production transport has a second 256 KiB per-connection
ceiling for bytes waiting on a kernel socket. Partial writes and `EAGAIN` retain
their unwritten suffix for a later turn; exceeding the ceiling, a zero-progress
write, or any non-`EAGAIN` write error disconnects the client. Disconnecting is
required because dropping an ordered event while keeping the connection alive
would desynchronise client and server object state.

The production client toolkit applies the same 256 KiB ceiling to its ordered
outbound byte queue. Request construction never waits inside `send`: one
bounded fd-write attempt is made per outer event-loop turn, and `EAGAIN` or a
partial write retains the exact suffix. While that suffix exists the display
socket is polled for `FD_WRITE` only (alongside any application-owned auxiliary
readiness); ordinary `FD_READ` interest resumes after it drains. Inline shm
uploads are staged in chunks of at most 24 KiB, with at most one upload request
created per turn. `surface.attach`, `surface.damage`, and `surface.commit` are
not queued until every upload chunk for that frame has been staged, so a
backpressured client cannot expose a partially uploaded buffer. A loop may park
only after display events and local paint/upload work are drained; a pending
local chunk with an empty transport queue forces another bounded turn, while a
non-empty transport queue parks on `FD_WRITE` rather than spinning.

The server additionally enforces two process-wide ceilings across all
connections:

| Resource | Server-wide ceiling |
|----------|---------------------|
| accepted client connections | 64 |
| authenticated `Cap::Shell` connections | 2 |
| aggregate shm-pool backing bytes | 128 MiB |
| live xdg toplevels | 512 |
| aggregate UTF-8 bytes retained in live toplevel titles and app IDs | 64 KiB |

Connection admission occurs before allocating per-client protocol state; an
accepted kernel socket beyond the connection ceiling is closed immediately.
At most the active shell and one replacement shell may overlap. Two of the 64
connection slots are reserved for those authenticated shell connections, so
ordinary clients have a hard aggregate ceiling of 62 and cannot deny a shell
restart at maximum load. The transport services authenticated shells before
ordinary outbound connections each turn, reserving one of its four 32 KiB
write attempts for each while leaving at least two rotating app write attempts.
Closing a shell releases this admission slot.
Ordinary clients also cannot consume the resources required to make those two
shell slots usable: admission withholds one 1024×768 ARGB8888 double-buffer
pool (6 MiB), one toplevel, and 880 bytes of title/app-ID metadata per reserved
shell connection. Consequently ordinary requests are admitted against 116 MiB
of aggregate pool backing, 510 toplevels, and 63,776 metadata bytes; an
authenticated shell request is admitted against the full server ceilings.
Pool creation and positive resize deltas are admitted against both the
per-connection and server-wide byte ceilings before object-table mutation or
backing allocation. Backing allocation is fallible: allocation failure produces
`pmd_display.error(code = NO_MEMORY)` and leaves the old state intact.
Toplevel creation is likewise admitted before installing the role. The global
512-window and 64 KiB metadata ceilings keep a v2 replacement shell's ordered
catch-up snapshot at or below 106,524 encoded bytes and 514 events. A single
toplevel's combined title and app ID may retain at most 880 bytes. This keeps
its shell lifecycle events wire-representable and bounds the maximum event
production of one 32-request transport turn to at most 30,720 bytes, below one
32 KiB shell write.
Pending
`window_title_changed` events for the same window are state-coalesced to the
latest value for legacy subscribers. V2 `window_state_changed` events are
coalesced by `(snapshot_id, window_id)` and replace an unsent v2 creation event
for that same key. Socket-backpressured output is drained only up to its
remaining capacity. Together these rules leave queue headroom for
handshake/live events without disconnecting a responsive shell during state or
title churn. N is admitted;
N+1 receives `NO_MEMORY` without a partial role or global window ID, and
destroy/disconnect releases the charge.
An attempted title/app-ID replacement above the 880-byte per-window total also
receives `NO_MEMORY` atomically and preserves both prior fields.

For `pmd_xdg_toplevel.set_title` and `set_app_id`, the server examines the wire
string's declared content length before UTF-8 parsing or owned-string
allocation. Admission charges only retained UTF-8 content bytes, not the wire
length prefix or padding. Replacement atomically subtracts the old field and
adds the new field against both metadata ceilings. N is admitted and N+1 is
rejected with `pmd_display.error(code = NO_MEMORY)` while the old value and both
counters remain unchanged. Toplevel destruction releases its title and app-ID
bytes, and client disconnect releases that client's complete retained total.

Resource destruction is reference-safe. `pmd_buffer.destroy` and
`pmd_shm_pool.destroy` immediately make the object invalid for new requests,
new attachments, and new child buffers. If a committed or pending surface still
retains the buffer, its metadata and pool backing remain charged to both byte
budgets until that surface attaches another buffer, detaches, is destroyed, or
the connection closes. Retained pool and buffer entries also continue to count
against their corresponding object-type ceilings, including zero-byte pools.
The destroyed ObjectId is tombstoned during this period and MUST NOT be rebound.
The server emits `pmd_display.delete_id` only after the last retained reference
is gone and the backing has actually been reclaimed. This permits create-first
buffer-pool replacement without corrupting the frame currently being displayed
and prevents same-ID reuse or zero-size allocation from bypassing admission
control.

The client side has a matching two-phase lifecycle. Once it queues a destroy
request successfully, it MUST reject any further request targeting that ID but
MUST retain the object's interface as an inbound-only tombstone until
`pmd_display.delete_id` arrives. Each direction is FIFO, but queuing a client
request does not prove server receipt or drain events already queued in the
reverse direction. An event generated before the server processes destroy can
therefore reach client dispatch after the local destroy call. Ordinary client
dispatch MUST apply `delete_id` even when it also exposes that lifecycle event
to higher-level code; only then may it discard the tombstone.

Frame callbacks and destroyed surfaces remain object-cap charged while their
ordered lifecycle events wait for event-queue capacity. Destroying a roleless
surface makes it invalid for requests immediately, but its ID remains a
non-bindable tombstone until the server can queue that surface's trailing
`pmd_display.delete_id`. Deferred acknowledgements therefore transfer an
existing object charge rather than opening unbounded create/destroy churn.

### 2.2 Object type table

| Type                 | Created by | Notes                                                |
|----------------------|------------|------------------------------------------------------|
| `pmd_display`        | implicit   | object 1; root of all bindings                       |
| `pmd_registry`       | client     | `get_registry` on display                            |
| `pmd_compositor`     | client     | bound from registry                                  |
| `pmd_shm`            | client     | bound from registry; creates pools                   |
| `pmd_shm_pool`       | client     | server-owned byte pool populated by v1 write requests |
| `pmd_buffer`         | client     | sub-rectangle of a pool                              |
| `pmd_surface`        | client     | `compositor.create_surface`                          |
| `pmd_xdg_shell`      | client     | bound from registry; shipped-v1 collapsed role API  |
| `pmd_xdg_toplevel`   | client     | `xdg_shell.get_toplevel(surface)`                    |
| `pmd_xdg_popup`      | client     | reserved; not advertised or implemented in v1       |
| `pmd_output`         | server     | reserved; not advertised or implemented in v1       |
| `pmd_seat`           | client     | bound from registry; v1 has exactly one global       |
| `pmd_pointer`        | client     | `seat.get_pointer`                                   |
| `pmd_keyboard`       | client     | `seat.get_keyboard`                                  |
| `pmd_callback`       | client     | `display.sync(cb)` or `surface.frame(cb)`             |
| `pmd_keymap_manager` | client     | reserved; not advertised in v1 until §15a is implemented |

---

## 3. pmd_display (object 1)

### Requests

| opcode | name          | args                       | notes                           |
|--------|---------------|----------------------------|---------------------------------|
| 1      | `sync`        | `new_id callback`          | server replies with `done` then destroys |
| 2      | `get_registry`| `new_id registry`          |                                 |

### Events

| opcode | name       | args                         | notes                   |
|--------|------------|------------------------------|-------------------------|
| 1      | `error`    | `u32 object_id, u32 code, string message` | protocol error |
| 2      | `delete_id`| `u32 id`                     | server acknowledges release |

---

## 4. pmd_registry

### Events

| opcode | name      | args                                           | notes        |
|--------|-----------|------------------------------------------------|--------------|
| 1      | `global`  | `u32 name, string interface, u32 version`     | advertises a global singleton |
| 2      | `global_remove` | `u32 name`                                |              |

### Requests

| opcode | name   | args                                                   | notes   |
|--------|--------|--------------------------------------------------------|---------|
| 1      | `bind` | `u32 name, string interface, u32 version, new_id target` | |

Shipped v1 advertises `pmd_compositor`, `pmd_shm`, `pmd_xdg_shell`, and
`pmd_seat` to every display client, all at version 1. A client holding the
kernel-authenticated `SHELL` capability additionally sees
`pmd_shell_manager` at version 2. Its version-1 requests and events remain
byte-stable; the authoritative state and restore transaction are explicit
version-2 opcodes, so an existing shell that only calls `subscribe_windows`
continues to receive only the legacy stream.
Numeric global names are server choices and clients MUST discover them from
these events rather than assume the current catalogue ordering.

---

## 5. pmd_compositor

### Requests

| opcode | name             | args                 | notes                          |
|--------|------------------|----------------------|--------------------------------|
| 1      | `create_surface` | `new_id surface`     |                                |

---

## 6. pmd_shm

### Requests

| opcode | name          | args                    | notes                                      |
|--------|---------------|-------------------------|--------------------------------------------|
| 1      | `create_pool` | `new_id pool, u32 size` | allocates zero-filled server-owned storage |

### Events

| opcode | name     | args          | notes |
|--------|----------|---------------|-------|
| 1      | `format` | `u32 format`  | advertises a supported pixel format |

V1 formats: `ARGB8888 = 0`, `XRGB8888 = 1`.

---

## 7. pmd_shm_pool

### Requests

| opcode | name           | args                                                                             | notes |
|--------|----------------|----------------------------------------------------------------------------------|-------|
| 1      | `create_buffer`| `new_id buffer, u32 offset, u32 width, u32 height, u32 stride, u32 format`       |       |
| 2      | `resize`       | `u32 new_size`                                                                   |       |
| 3      | `destroy`      | —                                                                                |       |
| 4      | `write`        | `u32 offset, byte[] data`                                                        | contiguous v1 pool upload |
| 5      | `write_rows`   | `u32 offset, u32 row_bytes, u32 rows, u32 stride, byte[] data`                  | packed-row v1 damage upload |

`resize` preserves the existing prefix and zero-fills growth. Shrink is allowed
only when every buffer retaining that pool has an exclusive byte end less than
or equal to `new_size`; a truncating resize is rejected atomically. Both growth
and shrink update per-connection and server-wide accounting immediately.

Because v1 pool creation does not yet transfer a shared-memory fd, `write`
copies inline bytes to the pool range beginning at `offset`. `write_rows` is
the damage-efficient form: `data` contains exactly `row_bytes * rows` densely
packed bytes, while consecutive destination rows begin `stride` bytes apart.
Both `row_bytes` and `rows` MUST be non-zero and `stride` MUST be at least
`row_bytes`. The server performs checked offset/stride/row arithmetic, verifies
the exact inline payload length and final exclusive pool extent, and rejects
the complete request before changing any pool byte if validation fails. A
single message remains bounded by the protocol's 16-bit message length and the
transport heap window; toolkit callers split larger rectangles into multiple
independently valid requests.

Pool writes MUST NOT mutate backing retained by a surface's current buffer.
After validating a `write` or every actual destination row of `write_rows`, the
server rejects the complete request atomically if any written byte intersects
any current attachment on that connection. Writes to an unattached alternate
buffer remain valid. Requests are evaluated in wire FIFO order, so an alternate
write queued after a commit may become valid when that earlier commit has
already promoted a different current buffer. This rule makes committed backing
immutable between an ordinary surface commit and either the next replacement
commit or the atomic `patch_current` transaction in §9.

---

## 8. pmd_buffer

### Requests

| opcode | name      | args | notes |
|--------|-----------|------|-------|
| 1      | `destroy` | —    |       |

### Events

| opcode | name       | args | notes                                  |
|--------|------------|------|----------------------------------------|
| 1      | `release`  | —    | server is done with this buffer; client may reuse it |

`release` applies to the buffer that has ceased to be current, never the buffer
newly promoted by a commit. The first attach, a damage-only commit, reattaching
the same buffer ID, and `patch_current` emit no release. Replacing or detaching
a current buffer, destroying its roleless surface, or removing its toplevel
role emits one release only after no surface on that connection retains that
buffer as current. A client-destroyed buffer ID is no longer an event target:
if its backing remains retained by a current surface, a later replacement or
detach emits no release and reclamation is acknowledged only by
`pmd_display.delete_id`. Disconnect likewise requires no release because there
is no surviving client to receive it. This rule prohibits generating a new
`release` after the server has processed destroy; it does not cancel an earlier
FIFO-queued `release`, which the client's inbound tombstone must still decode as
specified in §2.1.

---

## 9. pmd_surface

### Requests

| opcode | name          | args                                           | notes                                           |
|--------|---------------|------------------------------------------------|-------------------------------------------------|
| 1      | `destroy`     | —                                              |                                                 |
| 2      | `attach`      | `u32 buffer_id, i32 x, i32 y`                  | buffer_id = 0 detaches                          |
| 3      | `damage`      | `i32 x, i32 y, i32 w, i32 h`                   |                                                 |
| 4      | `frame`       | `new_id callback`                              | binds one typed callback for the next commit    |
| 5      | `set_opaque_region` | `region_data`                            |                                                 |
| 6      | `set_input_region`  | `region_data`                            |                                                 |
| 7      | `commit`      | —                                              | promotes pending → current                      |
| 8      | `patch_current` | `i32 x, i32 y, u32 width, u32 height, byte[] pixels` | atomic bounded current-buffer patch       |

`attach(buffer_id = 0)` followed by `commit` explicitly detaches the
current buffer. A commit with no intervening `attach` retains the current
buffer. After every commit that reaches compositor state, the server
clears the backbuffer to its configured scene background and recomposes
the complete live scene; it never relies on pixels left by a previous
frame.

For a commit whose old and newly-current attachments have identical validated
output geometry and visibility, a non-empty `damage` collection is the
client's complete declaration of every buffer-local pixel that may differ.
This remains true when the commit swaps between same-geometry buffers. The
server may use those rectangles only to bound comparison of the fully
recomposed scene with its last presented shadow. Omitting damage, supplying an
empty or invalid rectangle, changing attachment geometry or visibility, or
otherwise making the declaration unprovable requires a full-output comparison.
A client that changes pixels outside its declared damage violates the protocol
and may observe stale displayed pixels; the declaration is never trusted for
memory access, clipping, or resource validation.

`frame` has an exact four-byte payload containing one client-owned callback
ID; short or trailing payload bytes are malformed. A valid request binds a
real `pmd_callback` object and appends it to that surface's pending callback
FIFO. The request itself emits no event, changes no pixels or scene-dirty
state, and schedules no timer or wake. The surface's next successful `commit`
atomically moves all callbacks currently in that FIFO into the connection's
commit-ordered presentation wait FIFO. A callback requested after that commit
waits for a later commit. Multiple callbacks preserve request order, including
the order in which commits from different surfaces entered that connection's
FIFO.

A commit-associated callback becomes done-eligible only after production has
successfully completed the corresponding framebuffer presentation command
sequence while presentation is not deferred. Request dispatch, `commit`, and
CPU recomposition are not completion boundaries. A successful proof that the
new complete framebuffer is pixel-identical to the last presented framebuffer
is a logical presentation boundary and completes callbacks even though it
requires no framebuffer write. The event's `callback_data` is monotonic
presentation time in milliseconds modulo `u32`.

For each eligible callback the server atomically reserves capacity for both
ordered events, queues `pmd_callback.done`, removes the server callback object,
then queues `pmd_display.delete_id(callback_id)`. It never exposes `done`
without the following delete acknowledgement and never completes the same
callback twice. Earlier events caused by the commit, including any
`pmd_buffer.release`, retain their connection FIFO order before the callback
lifecycle.

Destroying the roleless surface before a callback crosses the presentation
boundary cancels that surface's pending and commit-associated callbacks:
each callback object is removed and receives `pmd_display.delete_id` without
`done`, followed by the surface's own `delete_id`. A callback that already
crossed a successful presentation boundary remains irrevocably done-eligible
even if the surface is destroyed before bounded event emission. Destroying
only the xdg-toplevel role leaves the surface and its callback lifecycle alive;
disconnect drops all remaining state silently because no peer remains.

Production drains at most 64 callback lifecycle items per outer turn with one
shared budget across every presentation and cancellation drain site. It makes
fair one-item-per-client passes with a rotating start. A completion preflights
two event slots and their exact bytes; a cancellation or trailing surface
acknowledgement preflights one. If capacity is unavailable, the complete item
and its object charge remain queued for retry after ordinary output drains;
other clients may still progress. Ready work causes another bounded local turn,
while capacity-blocked work relies on existing event/socket write readiness.
There is no idle timer, polling loop, or spin path (Principle IX).

Every newly current buffer must have an exclusive retained pool-byte extent.
Before promoting a pending non-null attachment, the server validates its full
buffer and pool backing and rejects the commit atomically if that extent
overlaps any other surface's current buffer. When replacing the same surface's
current buffer with a different buffer ID, the new extent must also be disjoint
from the old extent; an overlapping replacement requires an explicit
detach-and-commit transaction first. Reattaching the identical buffer ID on the
same surface remains valid. Rejection preserves the pending attachment,
pending damage, current attachment, pixels, and surface commit state.

`patch_current` is the bounded low-latency path for changing a small rectangle
of an already committed buffer without attaching and uploading an otherwise
identical full alternate buffer. `x` and `y` are non-negative buffer-local
coordinates. `width` and `height` MUST be non-zero, the rectangle MUST lie
wholly within the current buffer, and `pixels` MUST contain exactly
`width * height * 4` bytes as tightly packed rows in that buffer's advertised
`ARGB8888` or `XRGB8888` format. One request carries at most 24 KiB of pixel
bytes; 24 KiB is accepted and the next complete four-byte pixel is rejected.
The current buffer's stride and retained pool extent still govern each
destination row, and all length, format, geometry, arithmetic, and backing
checks complete before any pool byte changes.

The request is valid only when the surface has a current buffer and has no
attach or damage pending since its last commit. It never promotes, clears, or
otherwise reorders pending state. Because v1 permits buffer reuse and
overlapping buffers within one pool, the patched destination bytes also MUST
NOT overlap the retained byte extent of any other surface's current buffer on
the same connection. This prevents a patch from changing a second visible
surface without that surface's own commit. Any missing-current, pending-state,
alias, malformed, unsupported-format, out-of-bounds, over-cap, or invalid-
backing request is rejected atomically.

The current attachment resolves through retained buffer and pool metadata even
if the client has already destroyed either object's wire ID; §2.1 requires that
backing to remain alive while a surface still references it. Conversely, if
any other current attachment lacks complete retained metadata, the server MUST
reject the patch because it cannot prove that the destination is unaliased.

A successful request copies the complete rectangle, advances the surface's
content-commit state once, and requests exactly one scene recomposition. It
does not change the current attachment, enqueue `pmd_buffer.release`, synthesize
`attach`/`damage`/`commit` requests, or trigger focus/configure side effects.

### 9a. pmd_callback

`pmd_callback` is a server-completed, client-allocated one-shot ordering
object. It accepts no requests.

#### Events

| opcode | name   | args                | notes |
|--------|--------|---------------------|-------|
| 1      | `done` | `u32 callback_data` | one-shot; followed by `pmd_display.delete_id` |

For `pmd_display.sync`, `callback_data` is `0` and marks completion of earlier
requests/events. For `pmd_surface.frame`, it is the successful presentation
timestamp defined above. Client dispatch validates the exact four-byte payload,
retires the callback after exposing `done` to its caller, and fully drops it
only when the following `delete_id` arrives.

---

## 10. pmd_xdg_shell (shipped v1)

### Requests

| opcode | name           | args                                  | notes                            |
|--------|----------------|---------------------------------------|----------------------------------|
| 1      | `get_toplevel` | `new_id toplevel, u32 surface_id`     | adds the top-level role directly |

There are no `pmd_xdg_shell` events in shipped v1. In particular, the
canonical `pmd_xdg_wm_base.ping` / `pong` liveness exchange is a post-v1
extension and MUST NOT be advertised or claimed until both sides implement it.

This is a deliberate collapsed v1 shape: it keeps the role boundary on the
wire while avoiding an otherwise-empty `pmd_xdg_surface` object. It is not
wire-compatible with Wayland or with the aspirational split that appeared in
older revisions of this contract.

---

## 11. pmd_xdg_toplevel (shipped v1)

### Requests

| opcode | name              | args                    | notes                              |
|--------|-------------------|-------------------------|------------------------------------|
| 1      | `set_title`       | `string title`          |                                    |
| 2      | `set_app_id`      | `string app_id`         |                                    |
| 3      | `destroy`         | —                       | destroys the role                  |
| 4      | `ack_configure`   | `u32 serial`            | acknowledges the merged configure |
| 5      | `set_maximized`   | —                       |                                    |
| 6      | `unset_maximized` | —                       |                                    |
| 7      | `move`            | `u32 serial`            | echoes pointer-button serial       |
| 8      | `resize`          | `u32 serial, u32 edges` | serial plus resize-edge bits       |
| 9      | `set_minimized`   | —                       | minimizes only this owned toplevel |

### Events

| opcode | name        | args                                           | notes                                   |
|--------|-------------|------------------------------------------------|-----------------------------------------|
| 1      | `configure` | `u32 serial, i32 width, i32 height, u32 states` | client MUST ack the same serial        |
| 2      | `close`     | —                                              | client destroys its objects, then exits |

`configure` merges the canonical xdg-surface serial with the toplevel size and
state event. Width/height `0` means the client may use its preferred size. The
state bitfield uses `MAXIMIZED = 1<<0`, `FULLSCREEN = 1<<1`, `RESIZING = 1<<2`,
and `ACTIVATED = 1<<3`.

`ACTIVATED` mirrors the display server's authoritative keyboard-focus target.
When focus changes, the server emits a current-size `configure` to both the
previous and next live mapped toplevel (when present): the previous target has
`ACTIVATED` cleared and the next target has it set. Every configure composes
the toplevel's current persistent and transient state, so maximize and resize
configures preserve `ACTIVATED`, while focus configures preserve `MAXIMIZED`
and `RESIZING`. Minimizing, destroying, or disconnecting the focused window
deactivates it before removal when it is still live and activates the topmost
remaining focusable toplevel, if one exists.

---

## 12. Post-v1 xdg expansion

The more Wayland-like `pmd_xdg_wm_base` + `pmd_xdg_surface` split, wm-base
`ping`/`pong`, popups, parent/min/max-size setters, fullscreen setters, and a
client-issued close request are reserved post-v1 work. None is
advertised by the shipped server. Adding them requires matching
`display-proto`, server-dispatch, raw-client, toolkit-isolation, and browser
integration coverage; documentation alone does not make an opcode available.

---

## 13. pmd_xdg_popup (reserved)

The popup role is not advertised or implemented in shipped v1. The shell's
launcher is shell-owned UI and does not require an application popup object.

---

## 14. pmd_seat / pmd_pointer / pmd_keyboard

Condensed for v1:

`pmd_seat` is a universal display-client global, not a shell-control
interface. Ordinary foreground apps may bind it and create pointer
and keyboard objects without `Cap::Shell`; only
`pmd_shell_manager` in §15 is capability-gated.

- `pmd_seat` events: `capabilities` (bitmask of pointer/keyboard
  present).
- `pmd_pointer` events: `enter(serial, surface, sx, sy)`,
  `leave(serial, surface)`, `motion(time, sx, sy)`,
  `button(serial, time, button, state)`, `axis(time, axis, value)`.
- `pmd_keyboard` events: `keymap(fd, size)` is reserved for the
  future negotiated client-side map path,
  `enter(serial, surface, keys)`, `leave(serial, surface)`,
  `key(serial, time, key, state)`, `modifiers(serial, depressed,
  latched, locked, group)`.

The condensed v1 implementation emits `key(surface_id, key, state)` where
`key` is a logical USB-HID-style scancode. The browser/input-driver boundary
provides a physical scancode; before routing it, the display server applies the
live `keyboard.layout` selected through `contracts/preferences.md §4` and maps
the resulting character back into the stable logical HID namespace consumed by
v1 clients. Modifier, non-printing, unknown, and not-representable keys retain
their physical value. This wire shape remains 12 bytes and existing clients do
not reconnect when the layout changes.

The browser boundary maps DOM `F4` to USB HID usage `0x3d` and maps left/right
Alt independently to `0xe2`/`0xe6`. The display server tracks those physical
modifier transitions before layout mapping so the replaceable shell can own
desktop shortcut policy through the capability-gated event below; the browser
substrate does not close guest windows directly.

The browser input bridge records only mapped key presses actually forwarded to
the guest. Because a host-window blur or transition of the document to hidden
may suppress later DOM `keyup` events, either transition synthesizes one guest
release for every such held key and clears the held set. Keys routed to browser
substrate controls and reserved host shortcuts are never included. A delayed
physical `keyup` for a synthetically released key is consumed rather than
forwarding a duplicate release.

The condensed v1 pointer-button shape is
`button(serial, surface_id, x, y, button, state)`. `serial` is allocated
monotonically by the display server and is echoed by an application when a
press in client-side decoration starts `pmd_xdg_toplevel.move` or `resize`.
Coordinates are surface-local. This shape is 24 bytes and keeps the input
grant on the display protocol rather than exposing compositor internals to
applications.

---

## 15. The shell extension (pmd_shell_manager)

**This global is only bound by clients holding the `SHELL`
capability.** Attempting to bind without the capability yields
`pmd_display.error(code = PERMISSION_DENIED)`. This is how the
desktop shell sees the window list; without it, a client only
knows about its own surfaces.

The display server obtains this capability from the
kernel-authenticated `ipc_peer_caps` snapshot on the accepted socket
(see `syscalls.md §3.1`). A capability claim carried in protocol
bytes has no authority and MUST be ignored.

### Requests

| opcode | name                | args                         | notes                                |
|--------|---------------------|------------------------------|--------------------------------------|
| 1      | `subscribe_windows` | —                            | start receiving `window_*` events    |
| 2      | `focus_window`      | `u32 window_id`              | request to focus a top-level window  |
| 3      | `close_window`      | `u32 window_id`              | request the owning client to close   |
| 4      | `minimize_window`   | `u32 window_id`              |                                      |
| 5      | `unminimize_window` | `u32 window_id`              | restore, raise, and activate target  |
| 6      | `set_work_area_bottom` | `u32 height_px`           | reserve bottom strip for shell chrome |
| 7      | `toggle_maximized_window` | `u32 window_id`         | activate and toggle maximize/restore  |
| 8      | `desktop_ready`       | —                            | one-shot authenticated presentation fence |
| 9      | `subscribe_window_state` | `u32 snapshot_id`        | v2 authoritative snapshot/live stream; ID MUST be non-zero |
| 10     | `begin_restore`       | `u32 restore_id, u32 timeout_ms` | v2; arms hidden admission before a `pmd_display.sync` barrier |
| 11     | `place_restored_window` | `u32 restore_id, u32 window_id, i32 normal_x, i32 normal_y, u32 normal_width, u32 normal_height, u32 z_rank, u32 flags` | v2; place one hidden restore candidate |
| 12     | `end_restore`         | `u32 restore_id, u32 focus_window_id` | v2 atomic reveal/order/focus; zero focus selects fallback |

### Events

| opcode | name                   | args                                      | notes                       |
|--------|------------------------|-------------------------------------------|-----------------------------|
| 1      | `window_created`       | `u32 window_id, string title, string app_id` | top-level came into being  |
| 2      | `window_destroyed`     | `u32 window_id`                           | top-level destroyed         |
| 3      | `window_focused`       | `u32 window_id`                           | keyboard focus changed      |
| 4      | `window_title_changed` | `u32 window_id, string title`             | visible title changed       |
| 5      | `window_created_v2`    | `window_state`                            | authoritative creation/catch-up state |
| 6      | `window_state_changed` | `window_state`                            | authoritative stable-boundary update |
| 7      | `window_snapshot_done` | `u32 snapshot_id`                         | catch-up terminator         |
| 8      | `restore_finished`     | `u32 restore_id, u32 status, u32 placed`  | completion or fail-open result |
| 9      | `close_shortcut`       | `u32 window_id`                           | v2 focused ordinary Alt+F4 target |

The v2 `window_state` payload is exactly:

```
u32 snapshot_id
u32 window_id
u32 owner_pid
u32 ordinal
i32 current_x
i32 current_y
u32 current_width
u32 current_height
i32 normal_x
i32 normal_y
u32 normal_width
u32 normal_height
u32 flags
u32 z_rank
string title
string app_id
```

`flags` uses `MAPPED = 1<<0`, `MINIMIZED = 1<<1`, `MAXIMIZED =
1<<2`, `FOCUSED = 1<<3`, `HIDDEN_FOR_RESTORE = 1<<4`,
`RESTORE_PLACEMENT_APPLIED = 1<<5`, and `SHELL_OWNED = 1<<6`; all other bits
are zero. `RESTORE_PLACEMENT_APPLIED` is set only while an active placement has
the causal buffer-size proof described in §15.1. `SHELL_OWNED` is set only when
the window's owning connection held the kernel-authenticated `SHELL`
capability when accepted. Title and app-ID strings cannot influence it, so an
ordinary application naming itself `pmos.shell` remains an ordinary task.
`z_rank` is bottom-to-top with zero at the bottom. The decoder MUST consume the
payload exactly, including both padded strings, and reject truncation or
trailing bytes.

`owner_pid` is the immutable kernel-authenticated `ipc_peer_pid` snapshot from
the accepted socket. `ordinal` is non-zero and allocated monotonically by the
display server per authenticated PID across all of that PID's display
connections. Neither identity field is accepted from application protocol
bytes. `(owner_pid, ordinal)` is therefore suitable for grouping arriving
windows during a bounded session restore without requiring toolkit
cooperation. PID values themselves MUST NOT be persisted as cross-boot
identity.

`subscribe_window_state(snapshot_id)` replaces the calling shell manager's
prior v2 subscription generation and emits one opcode-5 event per live window
in current bottom-to-top order, at most one opcode-3 focused event, then exactly
one opcode-7 terminator carrying the same ID. Subsequent opcode-5/6 events keep
that `snapshot_id` until another subscription request. A shell discards events
from stale IDs. Legacy opcode 1 remains independent and continues to emit only
events 1-4.

A v2 shell uses `SHELL_OWNED`, not application metadata, to classify desktop
infrastructure. It records its private desktop surface only when both the flag
is set and `owner_pid` equals its own kernel-authenticated PID. Flagged surfaces
owned by an overlapping old or replacement shell are removed from the visible
task list. Because the independent v1 stream may provisionally announce such
a surface first, the v2 flagged event actively removes any transient entry.
All `SHELL_OWNED` states are excluded from session-runtime live state, identity
resolution, replacement-app detection, and persisted capture; desktop shells
are infrastructure rather than restorable application instances.

`window_state_changed` is emitted only at stable state boundaries: accepted
title/app-ID replacement, first map or mapped/unmapped transition, a
normal-size-changing commit, interactive drag release, minimize/restore,
maximize/unmaximize, restore placement or settlement, and atomic restore
completion. Pointer motion during a drag and damage-only commits do not emit
state events. Pending updates coalesce as specified in §2.1; app-ID changes are
consequently visible without masquerading as title-only updates. Opcode 3 with
`window_id = 0` means keyboard focus was cleared.

`window_id` is an opaque, monotonically allocated identifier in a
server-global namespace. It is not any client's `ObjectId`. The
server records a bidirectional mapping between every live
`window_id` and the exact `(client connection, client-local
xdg_toplevel ObjectId)` owner. Consequently, two clients may both
use `ObjectId(3)` without colliding in shell events or control
requests.

The ID remains stable for the lifetime of that toplevel and is not
reused during the display-server lifetime. Toplevel destruction or
client disconnect atomically retires the mapping before
`window_destroyed` is emitted. `focus_window`, `close_window`,
`minimize_window`, `unminimize_window`, and `toggle_maximized_window` resolve
only through this mapping; an unknown or retired ID is a no-op and MUST never fall back to scanning
client-local object tables.

`close_shortcut(window_id)` preserves the boundary between display input
routing and replaceable shell policy. The server recognizes only the first F4
press of a physical hold while either independently tracked Alt key is down.
It emits exactly one event to the authenticated shell-manager client that owns
the current bottom work-area reservation, and only when that object was bound
at negotiated version 2 or newer and keyboard focus resolves to a mapped,
non-minimized, non-restore-hidden ordinary toplevel. The server retains the
version from `registry.bind`; an explicit v1 binding never receives opcode 9.
A shell-owned toplevel, absent focus, absent compatible active shell recipient,
or an event-queue admission failure produces no privileged event; in those
cases F4 keeps its ordinary focused-client routing.

After successfully queuing `close_shortcut`, the server consumes the initial
F4 press, browser repeat presses, and its matching release. Alt press/release
events continue to route normally, and releasing one Alt key does not clear the
other. The event carries the exact authoritative global ID captured at the
gesture; it is never broadcast or exposed on an ordinary-client object. The
shell verifies that the ID still names its focused visible ordinary task and
then sends the existing authenticated `close_window(window_id)` request.
Thus the owning application still receives the advisory
`pmd_xdg_toplevel.close` lifecycle and decides when to destroy and exit.

### 15.1 Atomic session-restore transaction (v2)

`begin_restore` is accepted only on the authenticated shell-manager object,
requires a non-zero `restore_id`, and admits at most one owner at a time. A
second begin receives `restore_finished(status = BUSY, placed = 0)` for its own
ID and does not disturb the active transaction. The server clamps the requested
timeout to `1..=2500 ms`; this hard deadline is not extendable by later
requests. The production event loop places that one deadline in its existing
`poll_oneoff` wait set. It uses no periodic timer, busy loop, or unnecessary
wakeup.

The initiating shell sends `begin_restore`, then `pmd_display.sync`, flushes,
and waits for that callback before spawning restored applications. FIFO
dispatch plus the sync completion proves the transaction was armed before a
child can connect. Every ordinary toplevel created after the begin is assigned
its normal global ID and can configure, commit buffers, and exchange protocol
events, but it is marked `HIDDEN_FOR_RESTORE`: composition and hit testing skip
it, and its first map cannot take focus. Authenticated shell toplevels and
windows that existed before the begin are not hidden.

While a toplevel is `HIDDEN_FOR_RESTORE`, the restore transaction owns its
placement state. Owner `set_maximized`, `unset_maximized`, `move`, and `resize`
and `set_minimized` requests are accepted as bounded no-ops, as are
shell-manager focus, minimize/unminimize, and maximize toggles targeting that
hidden window. This prevents an application or concurrent shell command from
invalidating a saved normal rectangle or min/max state after placement has
been proven. Ordinary title, app-ID, buffer, and lifecycle requests remain
live.

`place_restored_window` must repeat the active non-zero `restore_id` and target
a window in that transaction's hidden set. Width/height must be non-zero and
the only accepted flags are `MINIMIZED = 1<<0` and `MAXIMIZED = 1<<1`.
Validation completes before mutation. Normal size is clamped to the work area
and origin is clamped so the complete normal rectangle remains visible. The
server emits an exact normal-size configure; for a maximized target it then
emits a work-area maximize configure, preserving the normal rectangle for
later restore. The placement records the surface commit count and its effective
expected buffer size (the clamped normal size, or the work-area size when
maximized). `RESTORE_PLACEMENT_APPLIED` is set immediately only when the
already-current buffer has that exact size. Otherwise it remains clear until a
later surface commit strictly advances past the recorded count and makes an
exactly-sized buffer current. A wrong-size or detach commit does not settle the
placement, and clears a previously-set bit before an end can reveal stale or
detached content. Replacing a placement records a new causal boundary and
clears the bit unless the new expected size already matches.

Before changing stacking, visibility, or focus, `end_restore` requires every
placed window to have `RESTORE_PLACEMENT_APPLIED`. An early end is a complete
no-op: it emits no acknowledgement, preserves the transaction and deadline,
and may be retried after later settlement events. Once settled, end validates
that the accepted placement ranks are unique and exactly contiguous
`0..placed-1`. A duplicate or hole aborts fail-open with `status = ABORTED`; it
never applies a partial reorder. A valid end sorts the placed windows by that
normalized rank, reveals every hidden candidate (including unplaced windows,
which retain their safe cascade defaults), applies minimized/maximized states,
and selects focus. A non-zero focus ID is honored only when it names a placed,
mapped, non-minimized candidate. Zero, an unknown ID, an unplaced ID, or a
minimized/unmapped target instead selects the restoring shell's own mapped
focusable toplevel, preferring its existing focused surface. This uses the
established shell focus-and-raise/show-desktop semantics. Only when that shell
has no mapped focusable toplevel may the server choose the highest visible
restored or global fallback. The server then rebuilds the scene once, emits the
resulting state/focus events, and finally emits
`restore_finished(status = COMPLETED, placed = N)`. Completion statuses are
`COMPLETED = 0`, `ABORTED = 1`, `TIMED_OUT = 2`, and `BUSY = 3`.

If the owner disconnects or the hard deadline expires, the display server
fails open: every hidden window is revealed using its actual current buffer
geometry at its last validated or safe default origin, minimized windows remain
minimized, and the scene is rebuilt once. If focus is empty, the surviving
restoring shell's mapped focusable toplevel is preferred before a safe
highest-visible fallback; a disconnected owner necessarily cannot qualify. All
placement-applied flags are cleared with the transaction. Timeout notifies the
surviving owner with `TIMED_OUT`; owner disconnect cannot receive an
acknowledgement. Destroying an application or toplevel during restore
atomically retires it from both the hidden and placed sets. Stale/mismatched
restore IDs and unknown/non-hidden window IDs mutate nothing.

`desktop_ready` is the trusted initial-desktop publication boundary. Its
payload MUST be exactly empty. The request is accepted only on a
`pmd_shell_manager` object belonging to a connection whose `SHELL` capability
was authenticated by the kernel; protocol bytes cannot forge it. At most one
request is effective for each live shell connection. Repeats are idempotent,
and disconnecting the requesting client before completion cancels its pending
fence.

After this request reaches the head of that client's FIFO stream, the display
server retains the requesting client identity and allocates a non-zero,
monotonically advancing `u32` fence serial (`u32::MAX` wraps to 1). It emits
the generic framebuffer `PRESENT_FENCE(serial)` command only after all earlier requests and their
resulting composition have reached a successful framebuffer presentation, or
the compositor has proved the pixels equal to its last successfully presented
shadow. A fence MUST remain pending while focus presentation is deferred or
while frame/scene work is dirty. The transport writes at most one fence per
outer turn; another ready fence counts as local work for the next bounded turn,
while a deferred fence parks for the causal display event instead of spinning.

The framebuffer command carries no shell or desktop policy and changes no
pixels. Its sole meaning is that earlier framebuffer presentation commands are
ordered before the serial marker. Browser policy may treat a received marker
as interactive desktop readiness because only this authenticated request can
schedule it; lower driver layers continue to expose a generic presentation
fence as specified by `driver-kernel.md §3`.

`unminimize_window` is the taskbar restore operation: it clears the
minimized flag, raises the window, assigns keyboard focus, recomposes the
scene, and emits `window_focused`. This makes restore a single atomic
server operation rather than a client-observable unminimize/focus race.

`pmd_xdg_toplevel.set_minimized` is the owner-scoped titlebar minimize
operation. The request carries no global ID and can affect only the toplevel
object named in the requesting client's object namespace. The server resolves
that exact `(connection, toplevel ObjectId)` through its global registry, then
uses the same authoritative minimize transition as `minimize_window`: it clears
focus and any active drag for the target, recomposes the scene, and emits the
v2 `window_state_changed` broadcast. Applications do not gain shell-manager
access, and restoration remains the shell's atomic taskbar operation above.

`set_work_area_bottom` is capability-gated with the shell-manager object. The
height is clamped to the output height. Subsequent non-shell initial configures
are constrained by their server-assigned origin. Maximizing snapshots that
origin, moves the toplevel to the work-area origin, and configures the complete
remaining work area; unmaximizing restores the snapshot. Application buffers
therefore cannot cover or receive input through a visible taskbar. The
reservation is retired when its owning shell client disconnects.

Repeated `set_maximized` requests while a toplevel is already maximized are
idempotent with respect to its saved normal rectangle: they may re-emit the
current work-area configure, but MUST NOT replace the pre-maximize origin or
dimensions with the maximized geometry. Repeated `unset_maximized` requests
likewise leave the already-restored normal rectangle unchanged.

`toggle_maximized_window` reads the exact owner's current maximized state and
performs the inverse transition. Maximizing snapshots the normal origin, moves
the toplevel to `(0, 0)`, and configures the complete work area, including its
bottom reservation. Restoring returns to the snapshot and emits a `0 × 0`
configure so the client resumes its preferred size. Either transition also
unminimizes, raises, and activates the target. The server is authoritative for
the toggle state; the taskbar does not infer it from client-local IDs.

`close_window` is an advisory graceful-close operation. Before emitting
`xdg_toplevel.close`, the server restores (if necessary), raises, and assigns
keyboard focus to the target. A client may keep the toplevel alive while it
shows a save/discard/cancel prompt; that prompt therefore remains visible and
receives keyboard input. The window is retired only when the client actually
destroys the toplevel or disconnects.

The server maintains an explicit global z-order of live `window_id`
values, from bottom to top. A new ordinary toplevel enters at the top. A
toplevel owned by an authenticated `SHELL` client enters at the global bottom,
including when a replacement shell registers after applications already
survive; its v2 creation event carries rank zero and every existing v2 rank
shifted by that insertion is republished authoritatively. Successful
`focus_window` and ordinary pointer-button focus can still raise any target,
including a shell toplevel used for show-desktop or overlaid shell UI, to the
top. Hit-testing walks the same stack from top to bottom, and composition walks
it from bottom to top. Registry map order, client IDs, and client-local object
IDs carry no stacking meaning. Every base insertion, raise, removal, or
restore-transaction reorder publishes fresh v2 authoritative state for each
surviving window whose rank changed; a multi-window client disconnect and
atomic restore end publish only their final ranks. The separate
`window_focused` or `window_destroyed` event is not a substitute for z-order
state. A press delivered to the shell client that owns the active bottom
work-area reservation, at or below
the work-area boundary, is a shell-command press: the server delivers the
pointer event without automatically changing keyboard focus, z-order, or
broadcasting `window_focused`. The shell command then uses `focus_window` when
its action needs to activate either itself (for overlaid shell UI) or a taskbar
target. This avoids an unrelated shell raise before every taskbar action while
keeping the policy in replaceable userland. When that reserved-strip command
uses `focus_window`, keyboard focus and logical z-order change immediately, but
the compositor withholds the target-only intermediate presentation until the
initiating shell commits its matching taskbar or menu feedback. That commit
composes and publishes both changes as one visible transaction. Once the
matching commit releases deferral, the server presents that ready scene before
polling later unrelated client sockets or flushing the resulting outbound event
queues. The socket scan still continues in the same turn and queued protocol
bytes retain their original order; only the already-ready framebuffer
presentation moves ahead of unrelated synchronous round-trips. A later
independent pointer press or shell disconnect cancels an incomplete transaction
so a failed replacement shell cannot indefinitely suppress presentation.

When a toplevel receives its first committed non-null buffer, it becomes
mapped, is raised, and receives keyboard focus; subscribed shell managers
receive `window_focused(window_id)`. This first-map focus happens once per
toplevel. The one exception is an authenticated shell whose first non-null
buffer arrives while any ordinary mapped, non-minimized, non-restore-hidden
toplevel survives: its late wallpaper/chrome map is recorded without changing
focus or stack order, even when keyboard focus is temporarily empty. This
preserves the surviving application and its pixels across shell replacement
and while the shell's bounded initial wallpaper load finishes. A cold or
shell-only first map with no such ordinary focusable toplevel still takes
initial focus normally. A later damage-only commit or detach/reattach does not
steal focus again. Merely creating a role with no current buffer does not dirty
or present the scene.

Every scene mutation that can expose old pixels triggers an immediate
full-scene clear and recomposition: surface commit or detach, interactive
move, minimize or restore, toplevel destruction, and client disconnect.
Minimized windows are absent from both composition and hit-testing.
Sending `xdg_toplevel.close` alone does not destroy the window; the
recomposition occurs when the client destroys its toplevel or disconnects.

**Replacement-shell reattach**: when a shell connects and calls
`subscribe_windows`, the server replays a `window_created` event for
every top-level currently alive, in current z-order, before sending
any future events. This is what makes the Principle II layering
test pass: a new shell connecting mid-session sees every existing
window exactly as if it had been present at launch.
Version-2 shells use `subscribe_window_state` instead and receive the bounded
authoritative replay plus `window_snapshot_done`; they need not infer geometry,
owner identity, mapping, minimization, maximization, focus, or stack rank from
legacy lifecycle history.

---

## 15a. The keymap manager extension (pmd_keymap_manager)

**Reserved, not advertised by the v1 server.** The request/event table below
defines the future negotiated client-side keymap transport. The shipped
Settings workflow instead writes the canonical VFS preference atomically and
the display server reloads it according to `contracts/preferences.md §4`.
Implementations MUST NOT advertise this global until its fd-carrying keymap
events and capability checks are implemented end to end.

**This global is only bound by clients holding the
`KEYMAP_ADMIN` capability** (see `data-model.md §5`). Attempting
to bind without the capability yields
`pmd_display.error(code = PERMISSION_DENIED)`. This is how the
settings application changes the system-wide keyboard layout;
an ordinary application cannot alter the keymap, and the shell
does not need to be involved.

### Requests

| opcode | name            | args                  | notes                                                       |
|--------|-----------------|-----------------------|-------------------------------------------------------------|
| 1      | `list_keymaps`  | —                     | request `keymap_available` events for every bundled keymap  |
| 2      | `set_keymap`    | `string keymap_name`  | ask the server to load the named bundled keymap             |

### Events

| opcode | name               | args                             | notes                                                         |
|--------|--------------------|----------------------------------|---------------------------------------------------------------|
| 1      | `keymap_available` | `string keymap_name`             | emitted once per bundled keymap in response to `list_keymaps` |
| 2      | `keymap_changed`   | `string keymap_name, u32 serial` | the server has loaded a new keymap                            |

### Semantics

- Bundled keymaps live under `/usr/share/keymaps/` in the
  display server's asset tree; the set is fixed for the
  duration of a boot. `keymap_name` is the filename without
  extension (e.g. `us`, `uk`, `dvorak`).
- `set_keymap(name)` with a valid `name` causes the server to
  load the new keymap and emit a `pmd_keyboard.keymap(fd,
  size)` event to **every** `pmd_keyboard` on **every** client
  connected to the server. Clients that cache a keymap MUST
  invalidate that cache on receiving the new event.
- After the broadcast, the server emits a `keymap_changed`
  event on this manager to the requesting client with a
  monotonic `serial` so the requester can confirm the change
  took effect.
- `set_keymap(name)` with an unknown name yields
  `pmd_display.error(code = INVALID_ARGS)` and no broadcast
  occurs.
- `list_keymaps` triggers a burst of `keymap_available` events,
  one per bundled keymap, in deterministic alphabetical order.
  The server does not emit a "done" sentinel; the caller knows
  the list is exhausted by reaching a subsequent reply to a
  different request.

The keymap bitstream format delivered via
`pmd_keyboard.keymap(fd, size)` is the xkb_v1 binary format as
already specified in §14. This extension does not change the
keymap format — only the mechanism by which the active keymap
is selected.

---

## 16. Error codes

| code | name                | meaning                                     |
|------|---------------------|---------------------------------------------|
| 0    | `OK`                | no error                                    |
| 1    | `INVALID_OBJECT`    | object_id in a request does not exist       |
| 2    | `INVALID_METHOD`    | opcode does not exist on this object type   |
| 3    | `INVALID_ARGS`      | argument validation failed                  |
| 4    | `NO_MEMORY`         | server out of memory                        |
| 5    | `PERMISSION_DENIED` | capability check failed                     |
| 6    | `INVALID_BUFFER`    | buffer refers to a destroyed pool or out of range |
| 7    | `PROTOCOL_MISMATCH` | client version incompatible with server     |

---

## 17. Versioning

Registry globals advertise a `version` integer. Universal v1 globals remain
version `1`; `pmd_shell_manager` advertises version `2` because its
authoritative state and atomic restore messages are additive. Its version-1
messages remain byte-stable, and the server emits v2-only events solely after
the explicit v2 subscription/transaction requests. Additions to the protocol
(new requests, new events, new objects) bump the affected interface version;
existing clients MUST continue to work without recompilation. Breaking changes
bump the major and MUST be considered protocol rewrites.

The version requested by `registry.bind` MUST be non-zero and no greater than
the advertised interface version; an unsupported bind is rejected without
installing its target object. A client connection may bind the singleton
`pmd_shell_manager` at most once. The server MUST reject a request introduced
after that object's negotiated version before mutating state or emitting any
event from that request.

---

## 18. Toolkit-free conformance test

A small test program in the Rust workspace
(`crates/toolkit-free-client/`) implements this protocol by hand,
with no toolkit linked in. It:

1. calls `display_connect`
2. writes `get_registry`, discovers the real numeric globals from
   `registry.global`, and binds `pmd_compositor`, `pmd_shm`,
   `pmd_xdg_shell`, and `pmd_seat`
3. creates a `pmd_keyboard`, surface, and collapsed-v1 xdg toplevel role
4. handles the initial configure and replies with `ack_configure`
5. allocates a double-buffered 320×200 pool, uploads a distinctive bounded
   RGBA frame through `pmd_shm_pool.write`, attaches, damages, and commits it
6. receives a real keyboard event and proves receipt by presenting the second
   buffer with a different distinctive colour
7. handles `xdg_toplevel.close`, explicitly destroys its role, surface,
   buffers, and pool, then exits cleanly

The library state machine is paired with the real `display_server::Server` and
software framebuffer in `crates/toolkit-free-client/tests/conformance.rs`,
including byte-at-a-time stream fragmentation. The standalone WASI binary is
then launched as its own Worker by
`web/tests/integration/toolkit-free-client.spec.ts` in Chromium and Firefox.
That browser gate requires exact framebuffer colours before and after input,
the extra live Worker while the client runs, and its disappearance after the
production close event. "It works" at both layers is the definitive check that
the protocol is the source of truth and the toolkit is a convenience.

The conformance program allocates one bounded 512,000-byte double-buffered pool
and uploads a 256,000-byte frame only on initial configure and on the first key
press. It adds no steady-state work to the desktop or display server after the
test client exits, so it does not relax Principle IX's boot or sub-100 ms input
budgets.
