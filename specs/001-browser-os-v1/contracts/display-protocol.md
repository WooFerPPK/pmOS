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
// followed by `length - 8` bytes of payload, layout determined by (object type, opcode)
// followed optionally by `fd_passing` entries from the ipc_recv fd queue
```

All integers are little-endian. Strings are length-prefixed
(`u32 byte_length` then UTF-8 bytes, padded to 4-byte boundary with
zeros). Objects are referenced by their 32-bit ID in the client's
local object space (each client has its own independent ID space —
object 1 always means "this client's display" on this connection).

**File-descriptor passing**: messages that want to attach an fd
(e.g., a shared-memory pool handle) set `fd_passing` to N and pass
N fds through `ipc_send`'s side channel. The server receives them
in `ipc_recv` as ordinary fds in its own fd table.

---

## 2. Object types and IDs

Object IDs are allocated by the client for client-created objects
(registry, surface, buffer, pool, callback, xdg_*) and by the
server for server-created objects (output, seat, pointer,
keyboard). The display (object ID 1) is implicit and always
present. A client allocates IDs using odd/even partitioning: odd
IDs are allocated by the client; even IDs are allocated by the
server. Both sides MUST respect each other's partition.

### 2.1 Object type table

| Type                 | Created by | Notes                                                |
|----------------------|------------|------------------------------------------------------|
| `pmd_display`        | implicit   | object 1; root of all bindings                       |
| `pmd_registry`       | client     | `get_registry` on display                            |
| `pmd_compositor`     | client     | bound from registry                                  |
| `pmd_shm`            | client     | bound from registry; creates pools                   |
| `pmd_shm_pool`       | client     | holds a SAB-backed buffer pool                       |
| `pmd_buffer`         | client     | sub-rectangle of a pool                              |
| `pmd_surface`        | client     | `compositor.create_surface`                          |
| `pmd_xdg_wm_base`    | client     | bound from registry                                  |
| `pmd_xdg_surface`    | client     | `xdg_wm_base.get_xdg_surface(surface)`               |
| `pmd_xdg_toplevel`   | client     | `xdg_surface.get_toplevel`                           |
| `pmd_xdg_popup`      | client     | `xdg_surface.get_popup`                              |
| `pmd_output`         | server     | one per monitor; v1 has exactly one                  |
| `pmd_seat`           | server     | one per input bundle; v1 has exactly one             |
| `pmd_pointer`        | client     | `seat.get_pointer`                                   |
| `pmd_keyboard`       | client     | `seat.get_keyboard`                                  |
| `pmd_callback`       | client     | `surface.frame(cb)`                                  |
| `pmd_keymap_manager` | client     | bound from registry; requires `KEYMAP_ADMIN` capability |

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

---

## 5. pmd_compositor

### Requests

| opcode | name             | args                 | notes                          |
|--------|------------------|----------------------|--------------------------------|
| 1      | `create_surface` | `new_id surface`     |                                |

---

## 6. pmd_shm

### Requests

| opcode | name          | args                            | notes                                |
|--------|---------------|----------------------------------|--------------------------------------|
| 1      | `create_pool` | `new_id pool, fd sab_fd, u32 size` | passes a fd referencing a SAB region |

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

---

## 9. pmd_surface

### Requests

| opcode | name          | args                                           | notes                                           |
|--------|---------------|------------------------------------------------|-------------------------------------------------|
| 1      | `destroy`     | —                                              |                                                 |
| 2      | `attach`      | `u32 buffer_id, i32 x, i32 y`                  | buffer_id = 0 detaches                          |
| 3      | `damage`      | `i32 x, i32 y, i32 w, i32 h`                   |                                                 |
| 4      | `frame`       | `new_id callback`                              | one-shot; server emits `done` then destroys     |
| 5      | `set_opaque_region` | `region_data`                            |                                                 |
| 6      | `set_input_region`  | `region_data`                            |                                                 |
| 7      | `commit`      | —                                              | promotes pending → current                      |

---

## 10. pmd_xdg_wm_base

### Requests

| opcode | name              | args                                 | notes                              |
|--------|-------------------|--------------------------------------|------------------------------------|
| 1      | `destroy`         | —                                    |                                    |
| 2      | `get_xdg_surface` | `new_id xdg_surface, u32 surface_id` | adds the xdg role to a surface     |
| 3      | `pong`            | `u32 serial`                         | response to server `ping`          |

### Events

| opcode | name   | args           | notes             |
|--------|--------|----------------|-------------------|
| 1      | `ping` | `u32 serial`   | liveness check    |

---

## 11. pmd_xdg_surface

### Requests

| opcode | name            | args                                    | notes                                    |
|--------|-----------------|------------------------------------------|------------------------------------------|
| 1      | `destroy`       | —                                        |                                          |
| 2      | `get_toplevel`  | `new_id xdg_toplevel`                    |                                          |
| 3      | `get_popup`     | `new_id xdg_popup, u32 parent_xdg_surface, u32 positioner` |                       |
| 4      | `set_window_geometry` | `i32 x, i32 y, i32 w, i32 h`       |                                          |
| 5      | `ack_configure` | `u32 serial`                             |                                          |

### Events

| opcode | name        | args         | notes                               |
|--------|-------------|--------------|-------------------------------------|
| 1      | `configure` | `u32 serial` | client must ack with `ack_configure`|

---

## 12. pmd_xdg_toplevel

### Requests

| opcode | name               | args                                   | notes                                 |
|--------|--------------------|----------------------------------------|---------------------------------------|
| 1      | `destroy`          | —                                      |                                       |
| 2      | `set_parent`       | `u32 parent_toplevel_or_null`          |                                       |
| 3      | `set_title`        | `string title`                         |                                       |
| 4      | `set_app_id`       | `string app_id`                        |                                       |
| 5      | `set_min_size`     | `i32 w, i32 h`                         |                                       |
| 6      | `set_max_size`     | `i32 w, i32 h`                         |                                       |
| 7      | `set_maximized`    | —                                      |                                       |
| 8      | `unset_maximized`  | —                                      |                                       |
| 9      | `set_fullscreen`   | `u32 output_id_or_0`                   |                                       |
| 10     | `unset_fullscreen` | —                                      |                                       |
| 11     | `set_minimized`    | —                                      |                                       |
| 12     | `move`             | `u32 seat_id, u32 serial`              | start interactive move                |
| 13     | `resize`           | `u32 seat_id, u32 serial, u32 edges`   | start interactive resize              |
| 14     | `close`            | —                                      | programmatic close request            |

### Events

| opcode | name           | args                                   | notes                                    |
|--------|----------------|----------------------------------------|------------------------------------------|
| 1      | `configure`    | `i32 width, i32 height, array states` | suggested size + `maximized/fullscreen/...` |
| 2      | `close`        | —                                      | user clicked close; client should exit   |

---

## 13. pmd_xdg_popup

Minimal popup role: `destroy`, `grab`, `reposition`. Popups are
used by the shell's launcher.

---

## 14. pmd_seat / pmd_pointer / pmd_keyboard

Condensed for v1:

- `pmd_seat` events: `capabilities` (bitmask of pointer/keyboard
  present).
- `pmd_pointer` events: `enter(serial, surface, sx, sy)`,
  `leave(serial, surface)`, `motion(time, sx, sy)`,
  `button(serial, time, button, state)`, `axis(time, axis, value)`.
- `pmd_keyboard` events: `keymap(fd, size)` (format is
  "xkb_v1" with a simple built-in keymap in v1),
  `enter(serial, surface, keys)`, `leave(serial, surface)`,
  `key(serial, time, key, state)`, `modifiers(serial, depressed,
  latched, locked, group)`.

---

## 15. The shell extension (pmd_shell_manager)

**This global is only bound by clients holding the `SHELL`
capability.** Attempting to bind without the capability yields
`pmd_display.error(code = PERMISSION_DENIED)`. This is how the
desktop shell sees the window list; without it, a client only
knows about its own surfaces.

### Requests

| opcode | name                | args                         | notes                                |
|--------|---------------------|------------------------------|--------------------------------------|
| 1      | `subscribe_windows` | —                            | start receiving `window_*` events    |
| 2      | `focus_window`      | `u32 window_id`              | request to focus a top-level window  |
| 3      | `close_window`      | `u32 window_id`              | request the owning client to close   |
| 4      | `minimize_window`   | `u32 window_id`              |                                      |
| 5      | `unminimize_window` | `u32 window_id`              |                                      |

### Events

| opcode | name             | args                                                                 | notes                                                 |
|--------|------------------|----------------------------------------------------------------------|-------------------------------------------------------|
| 1      | `window_added`   | `u32 window_id, u32 pid, string app_id, string title, array states` | a new top-level came into being                       |
| 2      | `window_changed` | `u32 window_id, string title, array states`                          | title or state changed                                |
| 3      | `window_removed` | `u32 window_id`                                                      | top-level destroyed                                   |

**Replacement-shell reattach**: when a shell connects and calls
`subscribe_windows`, the server replays a `window_added` event for
every top-level currently alive, in current z-order, before sending
any future events. This is what makes the Principle II layering
test pass: a new shell connecting mid-session sees every existing
window exactly as if it had been present at launch.

---

## 15a. The keymap manager extension (pmd_keymap_manager)

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

`pmd_display` advertises a `version` integer when a client binds
the registry. V1 is `1`. Additions to the protocol (new requests,
new events, new objects) bump the minor component; existing clients
MUST continue to work without recompilation. Breaking changes bump
the major and MUST be considered protocol rewrites.

---

## 18. Toolkit-free conformance test

A small test program in the Rust workspace
(`crates/toolkit-free-client/`) implements this protocol by hand,
with no toolkit linked in. It:

1. calls `display_connect`
2. writes `get_registry` + binds `pmd_compositor`,
   `pmd_shm`, `pmd_xdg_wm_base`
3. allocates a 320×200 SAB region and passes it as a shm pool
4. creates a surface + xdg_surface + xdg_toplevel
5. attaches a buffer filled with a solid colour
6. commits
7. reads input events and closes cleanly on a close event

This program is compiled and run in both the display server's
mock-framebuffer tests and in the Playwright integration suite.
"It works" is the definitive check that the protocol is the
source of truth and the toolkit is a convenience.
