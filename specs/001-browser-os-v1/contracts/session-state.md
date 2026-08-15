# PMos v1 Session-State Contract

This contract defines the durable desktop-session state required by
Constitution Principle IV. The filesystem remains the source of truth for user
data and preferences; the session file records only enough bounded shell state
to reopen the same catalog-backed applications and restore their windows after
the browser tab is closed and reopened.

## 1. Ownership and layering

The desktop shell owns session policy and persistence. It reads and writes the
session through ordinary WASI filesystem calls. The display server remains the
authority for live window identity, geometry, mapping, stacking, and focus.
The kernel supplies only the generic authenticated peer PID already captured by
IPC socket credentials.

No browser-side code, driver, kernel component, display-server component, or
application may read or write the session file on the shell's behalf. The
display server MUST NOT spawn applications, and the shell MUST NOT infer a
process identity from client-controlled title or `app_id` strings.

## 2. Canonical file and bounds

The canonical file is `/home/user/.config/pmos/session-v1`. It is UTF-8 and is
capped at 64 KiB before parsing. A snapshot contains at most 64 application
instances and 64 top-level windows. Every identifier is capped at 64 UTF-8
bytes and every decimal field must fit its documented integer type. Duplicate
record IDs, dangling references, unknown required fields, trailing tokens,
invalid UTF-8, unknown versions, and values outside these bounds invalidate the
whole snapshot.

The 64-instance limit exceeds the v1 display server's 62 ordinary-client
capacity. V1 exposes one top-level per application instance; deterministic
multi-window restoration requires a future stable application-owned window
role and does not widen this file in v1.

The v1 line format is deliberately flat and deterministic:

```text
PMOS_SESSION_V1
output <width:u32> <height:u32>
focus <window-record-id:u32-or-0>
instance <instance-id:u32> <desktop-entry-id:string>
window <record-id:u32> <instance-id:u32> <ordinal:u32> <z-rank:u32> \
       <normal-x:i32> <normal-y:i32> <normal-width:u32> \
       <normal-height:u32> <flags:u32>
```

On disk, a desktop-entry ID is restricted to ASCII letters, digits, `.`, `_`,
and `-`, so whitespace and escaping are unnecessary.
`flags` admits only `MINIMIZED = 1 << 0` and `MAXIMIZED = 1 << 1`.

The snapshot MUST NOT contain PIDs, capabilities, environment variables,
arbitrary executable paths, command-line arguments, current working
directories, titles, document contents, preference values, or browser data.
Preferences remain canonical in `/etc/preferences.toml`; user documents remain
ordinary filesystem content.

## 3. Authenticated identity and capture

Every accepted display connection carries the PID from the connected IPC
socket's kernel-owned credentials. The display server includes that PID and a
monotonic per-PID top-level creation ordinal in privileged shell-manager
window-state events. The ordinal spans every display connection opened by the
same live process. Protocol bytes supplied by an application cannot replace
either value.

For each live PID represented by a window, the shell reads the bounded
`Name:` field from `/proc/<pid>/status` and requires an exact match with one
currently published launcher entry's `Exec=` path. Exactly one catalog entry
must satisfy that check. Only that launcher's desktop-entry ID is retained.
Missing, ambiguous, non-catalog, exited, or malformed identities are omitted
from the durable snapshot. This rule also covers an application spawned by
another application without trusting its client-controlled `app_id`.

Window position, normal size, minimized/maximized state, z rank, and focus come
only from the display server's authoritative settled-state stream. Pointer
motion and intermediate drag/resize geometry MUST NOT cause a durable write.
A create, first map, destroy, metadata settlement, drag/resize completion,
minimize, maximize, restore, stacking change, or focus change may schedule one
coalesced snapshot revision. Stable changes reset one bounded coalescing
deadline of at most 250 ms; there is no periodic session timer.

## 4. Atomic persistence

The shell serializes a complete canonical snapshot to a uniquely named sibling
file. It writes at most 16 KiB in one event-loop turn, calls `fd_sync` on the
complete temporary file, closes it, atomically renames it over the target,
reopens the target, and calls `fd_sync` again before publishing that revision as
durable. Readers therefore observe either the complete previous revision or the
complete new revision.

The post-sync, post-close diagnostic is additive and has this exact field
order:

```text
shell: session durable revision=<u64> apps=<usize> windows=<usize> bytes=<usize> digest=<16-lowercase-hex>
```

`bytes` is the length of the exact canonical UTF-8 byte sequence, including
its required final newline. `digest` is FNV-1a 64-bit over that same sequence,
using offset basis `0xcbf29ce484222325`, prime `0x100000001b3`, and wrapping
unsigned 64-bit multiplication after every byte. Revision, counts, byte length,
and digest are published as one writer-completion event only after the renamed
target has been reopened, synced, and closed; they MUST NOT be assembled from
separately sampled writer state.

Display input is dispatched before every persistence step. The session state
machine performs at most one filesystem operation and one bounded transport
write per turn. If an operation would block, the shell waits on the exact
descriptor readiness; it MUST NOT retry on a timer or spin. New state arriving
during a write is retained as a newer revision and cannot be cleared by
completion of the older write.

A failure before rename preserves the last good target. After rename, the
target is already a complete new snapshot even if the final durability check
cannot be acknowledged; no reader can observe a partial file. Every failure is
reported without making the desktop unusable. A missing, malformed, oversized,
or unknown-version target starts a normal empty session. It is not partially
applied; the next valid settled state may replace it atomically.

## 5. Restore transaction

After the launcher catalog and the shell-manager catch-up snapshot are
complete, but before spawning restored applications, the shell starts one
privileged display restore transaction and places an ordered display `sync`
request behind it. It MUST observe the matching callback before spawning, so a
child cannot connect or map before suppression is active.

During the transaction, newly created ordinary top-levels remain live and may
process protocol and submit buffers, but the display server excludes them from
composition, hit-testing, and automatic focus. The shell:

1. resolves every saved desktop-entry ID against the current launcher catalog;
2. spawns each valid saved instance once through the current launcher policy;
3. associates the returned PID with that instance, without replaying stored
   authority;
4. matches arriving windows by authenticated PID and creation ordinal;
5. submits the saved normal geometry, flags, and stack rank;
6. waits for the server-authored placement-applied state proving either that
   the current buffer already had the exact effective size or that an exact-
   size buffer was committed after the placement configure; and
7. ends the transaction after every available placement is applied, every
   failed child is known to be unavailable, or the bounded deadline expires.

The display server validates and clamps every normal rectangle to the current
work area with a reachable titlebar. A maximized window fills the current work
area while retaining the clamped normal rectangle for later unmaximize. A
minimized window is never the restored focus target. An early end request while
an accepted placement is not applied is a no-op and may be retried; the hard
deadline still fails open. At transaction end the server atomically orders and
reveals placed windows, applies minimized and maximized state, chooses the
saved visible focus target or the restoring shell when that target is absent,
minimized, or zero, recomposes once, and emits one ordered completion event.

An unmatched, uninstalled, ambiguous, failed, or timed-out instance is skipped
without blocking the remaining session. A shell-manager disconnect or server
deadline aborts fail open: hidden windows receive their last validated
placement or a safe cascade/default origin, become visible, and normal focus
resumes. Only one bounded restore transaction may exist. The shell uses a 2250
ms soft deadline and the display server enforces a 2500 ms hard deadline without
introducing a polling wake. The 250 ms separation reserves a bounded tail for
the final configure, exact-size commit, placement-applied proof, and end request.

The shell suppresses durable capture while restoration is in progress and
schedules one complete settled snapshot afterward. That bounded background
write does not delay the interactive readiness fence. No PID or capability
from the old visit is replayed.

## 6. Readiness and acceptance

On a restored visit, `desktop_ready` means the restored scene—not an empty
intermediate desktop—is interactive. The shell MUST wait for restore
completion, the final coalesced taskbar/wallpaper frame, and drained display
outbound bytes before queuing the existing authenticated desktop-ready
presentation fence. Identity resolution, coalescing, and an atomic capture of
the resulting settled session continue as bounded background work; they MUST
NOT add their 250 ms durability tail to the warm interactive-ready budget. A
normal empty or invalid session follows the same boundary without spawning
restored apps.

The required Chromium and Firefox acceptance test creates a persistent file,
opens at least Files and Terminal, changes window geometry/state/stacking/focus,
waits for the session revision to become durable, closes the tab, and reopens
the same browser profile. Before accepting input it proves that the file,
application instances, window geometry, minimized/maximized state, z order,
and keyboard focus were restored through causal framebuffer/protocol fences.
The pre-close durability diagnostic, not an arbitrary sleep or page-shutdown
hook, is the causal proof that the state being tested reached the canonical
file. The test hashes the exact canonical file text, including its final
newline, and accepts a snapshot only when byte length and digest match the
newest diagnostic revision it observed. After restore it always waits for the
six-application/six-window background rewrite, rereads the canonical file, and
checks the resulting semantic state outside the warm-readiness timing budget,
even when its bytes equal the pre-close revision.
The representative restored warm boot must remain under three seconds, input
p95 under 100 ms, and idle CPU under the existing release limit.
