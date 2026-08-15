# Preferences VFS Contract

This contract defines the VFS boundary shared by Settings, init, the desktop
shell, the display server's keymap reader, and future preference-aware
applications. Preferences never travel through a browser-side shortcut.
The shipped v1 path also does not serialize preference values into the display
protocol; the reserved keymap-manager extension is a future transport for
validated keymap objects, not the canonical preference store.

## 1. Canonical location and format

The canonical file is `/etc/preferences.toml`. It is UTF-8 and uses the
following deliberately narrow, string-only schema:

```toml
[theme]
name = "light"
fit = "stretch"

[wallpaper]
name = "blue.png"

[keyboard]
layout = "us-qwerty"

[timezone]
iana = "UTC"

[terminal]
font = "unifont-mono-14.pbm"
```

All sections and keys are optional. Unknown sections and unknown keys are
ignored for forward compatibility. Values are plain quoted strings; escapes,
embedded quotes, and line breaks are not part of the v1 format. The shared
`preferences` crate owns both parsing and canonical serialization so readers
and writers cannot drift.

## 2. Writer semantics

Settings performs read-modify-write so changing one control preserves every
other known field. It serializes the complete snapshot to a uniquely named
temporary file in the target directory and then renames that file over the
target. Readers therefore observe either the previous complete snapshot or the
new complete snapshot, never a partially written file. A serialization or
rename failure leaves the prior target intact and is reported in the Settings
UI/CLI. After the rename succeeds, Settings calls standard WASI `fd_sync` on
the renamed target before reporting success. An immediate tab reload therefore
does not depend on a periodic or page-hide flush racing browser teardown.

The v1 GUI offers `light`/`dark`, `stretch`/`tile`/`center`/`fill`, the bundled
`blue.png`/`green.png`/`dark.png` wallpaper names, and the supported keyboard,
timezone, and terminal-font choices. CLI setters remain forward-compatible and
may write a future value; consumers must normalize unsupported values safely.

## 3. Desktop-shell reader semantics

The shell reads the file at startup, then watches the stable `/etc` parent for
create/delete and the currently visible preference inode for modify. A parent
event drops the stale inode watch before registering the replacement. After a
bounded watch drain, the shell rereads the snapshot and marks its surface dirty
in the same event-loop turn; the ordinary double-buffered display commit path
performs the repaint. There is no periodic preference timer and no
display-server or browser notification path. The only idle shell deadline is
the real next-minute boundary for its taskbar clock.

The shell applies these fields live:

| Key | Supported values | Safe default | Visible effect |
|---|---|---|---|
| `theme.name` | `light`, `dark` | `light` | taskbar and launcher palette |
| `wallpaper.name` | `blue.png`, `green.png`, `dark.png` | `blue.png` | decoded VFS-backed desktop image |
| `theme.fit` | `stretch`, `tile`, `center`, `fill` | `stretch` | nearest-neighbour scale, repeat, unscaled center, or aspect-preserving center crop |
| `timezone.iana` | `UTC`, `America/New_York`, `Europe/London`, `Asia/Tokyo` | `UTC` | shell-owned taskbar clock, including the supported US/UK DST transitions |

Legacy `mountains.png`, `sunset.png`, and `abstract.png` names normalize to the
closest v1 palette so an older persisted file remains usable.

Missing or malformed content selects the safe defaults above. A transient VFS
read error retains the last usable snapshot; a later filesystem mutation
causes another read without introducing a retry timer.

The three wallpaper files live at `/usr/share/wallpapers/<name>`. The shell
constructs this path only from the normalized allow-list above, reads it through
its ordinary VFS namespace, decodes it inside the isolated shell process, and
paints through the toolkit canvas. No wallpaper bytes or filenames cross a
browser-side or display-server-specific API.

Wallpaper decoding accepts static, non-interlaced, 8-bit RGB or RGBA PNG only.
An encoded file is capped at 2 MiB; each dimension is capped at 2048 pixels;
the total is capped at 4,194,304 pixels and 16 MiB decoded. Text and ICC chunks
are ignored. A missing or transiently unreadable asset is retried no more than
once per second. A malformed or unsupported replacement is not repeatedly
decoded, and neither failure replaces the last successfully decoded image. If
there is no last-good image, the shell paints the selected filename's bounded
built-in fallback color, so filesystem damage cannot blank or crash the desktop.

Fresh OPFS images contain all three files byte-for-byte. When an older
persistent image is migrated, existing wallpaper paths are never overwritten.
Missing files are written to hidden staging names and atomically renamed only
after the complete payload is present. Migration preflights block-accounted
free space for the whole missing set; a near-full image skips these optional
presentation assets rather than failing boot or deleting user data. The shell's
fallback above keeps that degraded case usable. Volatile-root seeding follows
the same staged path but has no quota limit.

## 4. Display-server keyboard reader semantics

The display server reads `keyboard.layout` at startup and uses the same stable
parent/current-inode watch topology as the shell. Supported values are
`us-qwerty`, `uk-qwerty`, and `dvorak`; an absent file or absent key selects the
safe `us-qwerty` default. Its blocking wait includes both watch fds alongside
listener, signal, input, and client socket interests, with no keymap timer.

Settings' atomic rename means the reader normally sees only complete old or
new snapshots. A malformed snapshot, unsupported layout name, oversized file,
or transient VFS error retains the last successfully validated layout; a later
watch mutation causes another bounded read. All three bundled PMKM maps are
parsed and validated before selection; a bad embedded map likewise cannot
replace the last-good live map.

Once a changed snapshot is accepted, it applies before the next keyboard input
drain without restarting the display server or reconnecting clients. The
browser driver continues to supply physical HID-style scancodes. The display
server translates them through the active PMKM map into the v1 logical HID
namespace described by `display-protocol.md §14`, preserving modifier events
and unknown/non-printing keys. This lets already-running v1 applications use a
new layout without an application-layer or browser-layer shortcut.

### 4a. Toolkit theme reader semantics

Theme-aware applications opt in through `toolkit::watch_theme()`. Construction
reads `/etc/preferences.toml` synchronously before the application's first
frame. `theme.name = "dark"` selects the dark toolkit palette; `light`, an
absent file/key, malformed content, and unsupported names select the safe light
palette. Each reader caps the snapshot at 64 KiB.

The application pairs the returned snapshot reader with toolkit `PathWatch`,
which watches the stable parent and current inode exactly as above. The
ordinary display loop includes those fds in `wait_with`, drains at most sixteen
watch reads per turn, and calls `ThemeWatcher::refresh` only after a mutation.
A transient VFS error retains the last usable palette. An accepted normalized
change is returned exactly once so the application can repaint its existing
buffers without being restarted; unchanged snapshots produce no framebuffer
work. Applications that do not opt in retain their startup palette. Sysmon is
the v1 reference consumer.

No theme value or notification crosses the display protocol, and there is no
browser-side preference channel. Rename-aware kernel notification and the
user-runtime `fs_watch` import are part of this boundary, so atomic replacement
and direct in-place writes both wake shipped consumers.

## 5. Terminal-font reader semantics

A terminal process reads `terminal.font` once at startup. Supported values are
`unifont-mono-14.pbm` and `pc-vga-16.pbm`; an absent, malformed, oversized, or
unsupported preference selects `unifont-mono-14.pbm`. The allow-list is applied
before constructing `/usr/share/fonts/<name>`, so preference data cannot escape
the font directory.

Both assets are ASCII PBM P1 atlases with a 16×16 codepoint grid. The reader
accepts only the shipped 128×224 (8×14 cells) and 128×256 (8×16 cells)
dimensions and caps both the preference file and encoded atlas at 64 KiB. A
missing, malformed, truncated, or oversized selected asset falls back to the
embedded validated 8×14 atlas; font damage therefore cannot prevent a terminal
from opening. Font selection and parsing occur once, not in the paint loop.

The selected font remains fixed for that terminal process. Settings changes
affect terminals launched afterward; already-running terminals are neither
repainted nor restarted.

Fresh and volatile roots contain both files. Migration adds a missing font
without overwriting an existing path, but insufficient persistent free space
must not fail boot: the embedded fallback makes these presentation assets
optional for an older near-full image. Existing-root migration preflights
block-accounted space for the complete missing set, writes each payload to a
hidden sibling, and atomically renames only after the full file is present. A
font-only write failure is cleaned up and cannot make the persistent root
unbootable; volatile-root seeding remains strict.

## 6. Timezone boundary

The live taskbar clock is shell-owned presentation state. Reloading its zone
does not mutate the shell process environment and does not deliver a timezone
change to any other running process. Init continues to apply
`timezone.iana` as `TZ` only when spawning a new child, as required by
`contracts/init-conf.md §3.6`; existing processes retain their spawn-time
environment.

The shared preferences library owns the canonical allow-list (`UTC`,
`America/New_York`, `Europe/London`, `Asia/Tokyo`) and normalization helper,
with UTC first and therefore the safe default. Init and the desktop shell read
the latest complete `/etc/preferences.toml` independently at every spawn,
overlay exactly one validated `TZ` entry on the child's baseline environment,
and encode the launch through `proc_spawn_manifest`. Missing, malformed,
unsupported, or temporarily unreadable preference content selects `UTC` for
that new child. Neither process caches a spawn-time value across edits.

A graphical Terminal forwards its own validated spawn-time `TZ` into the
persistent `/bin/sh` manifest it creates. It does not reread preferences, so a
Terminal already running at the moment of a Settings change keeps its original
zone while a newly launched Terminal and its child shell receive the new one.

The four checked-in TZif payloads are installed byte-for-byte at
`/etc/zoneinfo/UTC`, `/etc/zoneinfo/America_New_York`,
`/etc/zoneinfo/Europe_London`, and `/etc/zoneinfo/Asia_Tokyo`. Fresh OPFS and
volatile roots require the complete bundle. An older persistent root is
migrated only after a whole-set free-space preflight; each missing payload is
written to a hidden sibling and atomically renamed, existing targets are never
overwritten, and a zoneinfo-only failure or insufficient space does not block
boot.

## 7. Performance and notification path

An unchanged preference file causes no periodic VFS read and no idle wake.
Filesystem readiness triggers bounded watch drains and at most one snapshot
refresh per consumer turn. A framebuffer repaint occurs only when normalized
preferences or the displayed minute change.

Only one decoded wallpaper is resident at a time. PNG decode occurs at shell
startup, on a wallpaper selection change, or on the bounded transient retry;
ordinary taskbar/launcher repaints reuse the decoded pixels. Fit sampling is
bounded by the framebuffer pixel count and performs no I/O or allocation.

The display server caps each preference read at 64 KiB and performs no
framebuffer repaint for a layout change. Key dispatch adds two bounded map
lookups only when a non-default layout is active; the default US path remains a
direct pass-through.

Each opted-in toolkit application performs a repaint only when the normalized
palette changes. The preference snapshot is capped at 64 KiB and no per-frame
allocation, periodic clock read, or VFS I/O is added.
Settings issues one standard WASI `fd_sync` after each explicit Apply; this is
user-action durability work and never runs in an idle or paint loop.

Each terminal performs at most two capped startup reads and stores the selected
atlas bit-packed (3.5–4 KiB). Rasterization uses the selected 8-pixel cell width
and 14- or 16-pixel height without filesystem I/O, polling, or per-frame font
allocation.
