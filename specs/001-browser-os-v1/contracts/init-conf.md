# Contract: init.conf

**Status**: canonical reference for v1.
**Audience**: init implementer, users who want to replace the
desktop shell with a different program, anyone writing integration
tests that need a different boot configuration.

The file lives at `/etc/init.conf`. Init (PID 1) reads it at boot
and uses it to decide what to spawn, in what order, and with what
capabilities. Changes take effect on the next boot. Because every process has
the v1 root preopen, this file is input to a security policy rather than the
policy itself: it can select only trusted bundled privileged services, and no
configured grant can exceed the compiled ceiling for that role.

---

## 1. File format

TOML, using the same TOML parser the package manifest uses (a
single crate is shared). All sections below are OPTIONAL unless
marked REQUIRED; a missing section is treated as "use the
default".

---

## 2. Schema

```toml
# /etc/init.conf

# REQUIRED. What init spawns first, after mounting filesystems.
[boot]
display_server = "/usr/bin/display-server"  # default
shell          = "/usr/bin/shell"           # default
autostart      = []                         # default

# OPTIONAL. Per-binary capability grants. Keys are binary paths or
# symbolic names matching `[boot]` entries.
[capabilities.display-server]
grant = ["DISPLAY_SERVER", "DEV_BLOCK"]

[capabilities.shell]
grant = ["DISPLAY_CLIENT", "SHELL", "PROC_ENUMERATE", "PROC_KILL_ANY", "KEYMAP_ADMIN", "HOST_TRANSFER"]

[capabilities.sysmon]
grant = ["DISPLAY_CLIENT", "PROC_ENUMERATE", "PROC_KILL_ANY"]

# OPTIONAL. Environment variables passed to every child of init.
[env]
PATH       = "/bin:/usr/bin"
HOME       = "/home/user"
USER       = "user"
XDG_RUNTIME_DIR = "/run"
PMOS_DISPLAY    = "/run/display"

# OPTIONAL. Boot-time debug knobs.
[debug]
kernel_log_level = "info"    # "trace" | "debug" | "info" | "warn" | "error"
serial_shell     = false     # if true, also spawn a headless shell on /dev/console
```

---

## 3. Semantics

### 3.1 Boot sequence

Before PID 1 starts, the kernel mounts a successfully opened OPFS image as
`/`, then overlays volatile `tmpfs` instances at `/tmp` and `/run`, `devfs` at
`/dev`, and `procfs` at `/proc`. The canonical system and user paths are
therefore `/etc`, `/usr`, and `/home/user`; there is no `/persist`
compatibility mount.

Only an image explicitly reported as newly created by the block driver may be
formatted. If persistent storage is unavailable, or an existing image cannot
be validated and mounted, the kernel leaves its bytes untouched, logs the
degraded boot, and may prepare a volatile `tmpfs` at `/`. In that fallback mode
`/proc/storage` reports `0 0 0`; init may follow the sequence below so recovery
is coherent, but the browser substrate blocks all ordinary interaction by
default. Main exposes only Retry and an explicitly labelled temporary-session
choice whose files will be lost on reload. Missing persistent configuration
and binaries take their documented fallback paths only after that explicit
choice; preparation of the recovery tree never authorises rewriting the
existing image.

1. Init waits for the kernel to have finished mounting `/`,
   `/tmp`, `/dev`, `/proc`, and `/run`.
2. Init reads `/etc/init.conf`. If missing or unparseable, init
   uses the built-in defaults (same as the example above) and
   logs a warning to `/dev/console`.
3. Init spawns `boot.display_server` with caps from
   `capabilities.display-server`, stdin/stdout/stderr piped to
   `/dev/console`.
4. Init waits (polling + brief `sched_yield`) until
   `/run/display` exists — this is the signal that the display
   server has bound its listening socket.
5. Init spawns `boot.shell` with caps from
   `capabilities.shell`.
6. Init spawns each entry in `boot.autostart` in order, with
   caps from a matching `capabilities.<name>` section, or an
   empty cap set if none matches.
7. Init enters its reap loop: it `proc_wait`s on any child,
   logs the exit, and — for the shell only — respawns it (see
   §3.2).

### 3.2 Shell respawn policy

If the shell exits for any reason, init logs the exit and
respawns the shell binary listed in `boot.shell` once per 1
second. This bounds "shell is crashing in a loop" to 1/sec of
spawn churn.

**This is how the layering test is observable from userland**:

1. sysmon calls `proc_kill(shell_pid, SIGKILL)`.
2. Kernel reaps the shell; its surfaces are released;
   `window_removed` events fire for every surface owned by the
   shell.
3. Running apps' surfaces remain (the display server never
   destroys surfaces on behalf of a dead client; that would be
   a privacy/protocol violation — see `display-protocol.md`).
4. Init's `proc_wait` returns the shell's exit status.
5. Init respawns `boot.shell`.
6. The new shell connects, subscribes to windows, receives
   replayed `window_added` events for each pre-existing
   top-level, and draws its own taskbar and chrome.
7. Test passes.

**Replacement with a different binary**: the layering test also
covers the case where the shell is replaced by the independently compiled
bundled `/usr/bin/alt-shell`, not just respawned. For that, sysmon (or a test)
edits `/etc/init.conf` to select `/usr/bin/alt-shell` and kills the shell. On
respawn, init reads the conf again and spawns the new binary. The rest of the
sequence is identical. V1 accepts only `/usr/bin/shell` and
`/usr/bin/alt-shell` for this privileged role; a future trusted package/signing
mechanism may extend that allow-list without changing lower layers.

### 3.3 Autostart failures

If an autostart app fails to spawn or exits non-zero, init logs
the failure but does NOT retry and does NOT hold up the boot.
Autostart is best-effort.

### 3.4 Capability inheritance

Init holds every capability in the system — it is PID 1 and the root of all
grants. When init spawns a child, it intersects the caps listed in the child's
`capabilities.<name>` section with a compiled role ceiling. The display server
ceiling is `DISPLAY_SERVER|DEV_BLOCK`; the shell ceiling is the documented
desktop-shell set; bundled Files, Settings, and Sysmon autostarts use their
documented initial sets; every other autostart is capped at `DISPLAY_CLIENT`.
A missing section yields an empty cap set. Unknown names and known names above
the ceiling produce boot-time warnings and are ignored. Mutable configuration
therefore cannot turn init's `CAP_GRANT` authority into privilege escalation.

The privileged boot paths are also constrained: `boot.display_server` must be
`/usr/bin/display-server`, and `boot.shell` must be `/usr/bin/shell` or
`/usr/bin/alt-shell`. The kernel resolves `/bin/*` and `/usr/bin/*` only through
the immutable bundled registry, so a writable VFS shadow cannot replace those
bytes. Dynamically installed executable content belongs under `/opt` and is
never eligible for a privileged init role in v1.

### 3.5 Missing binaries

If `boot.display_server`, `boot.shell`, or any autostart binary
is missing from the filesystem, init logs the error to
`/dev/console` and, for the display server only, drops into
"serial shell mode": it spawns `/bin/sh` wired to
`/dev/console` so a developer can fix the problem. This is the
same fallback Principle VIII requires for headless testing.

### 3.6 Dynamic env vars from preferences

At every `proc_spawn`, init reads a small whitelist of dynamic
environment variables from `/etc/preferences.toml` (if that
file exists) and sets them in the child process's `envp`, **in
addition to** whatever is in the static `[env]` table of this
file. The whitelist for v1 is:

| env var | preferences key     | default if absent |
|---------|---------------------|-------------------|
| `TZ`    | `timezone.iana`     | `UTC`             |

The v1 timezone allow-list is `UTC`, `America/New_York`, `Europe/London`, and
`Asia/Tokyo`. Init must normalize through the shared `preferences` crate;
missing files, malformed snapshots, missing keys, and unsupported names all
produce `TZ=UTC`. Preference text is never copied directly into an environment
entry or filesystem path.

Semantics:

1. The static `[env]` table in `/etc/init.conf` provides the
   baseline environment; values from the preferences whitelist
   are applied **after** the static table and MAY override
   baseline values.
2. Init parses `/etc/preferences.toml` lazily — either once per
   spawn (simple, correct) or with a cache invalidated by a
   future `fs_watch` on the file (optimised). Implementations
   MUST NOT cache across a preferences-file edit in a way that
   leaves stale values visible to newly spawned children.
3. **Already running processes are not affected** by a
   preferences change. They retain their spawn-time environment
   until they exit. This is the deliberate v1 decision (see
   spec FR-034 and the 2026-04-13 clarifications): there is no
   live re-delivery of timezone or other dynamic env changes.
4. Expansion of the whitelist in a future version is a
   backward-compatible amendment (new keys added; existing
   keys unchanged).

No signal-broadcast infrastructure (SIGHUP or equivalent) is
needed or used for this mechanism.

Init launches these children through the documented
`pmos_ext.proc_spawn_manifest` import and the canonical `spawn_v1` encoder.
Adding `TZ` must not change the launch's path, argv, fd mappings, inherited
stdio/cwd markers, or least-privilege capability grant. The preference file is
read separately for every direct spawn and every future respawn attempt.

---

## 4. Default file shipped in the initramfs

```toml
[boot]
display_server = "/usr/bin/display-server"
shell          = "/usr/bin/shell"
autostart      = []

[capabilities.display-server]
grant = ["DISPLAY_SERVER", "DEV_BLOCK"]

[capabilities.shell]
grant = ["DISPLAY_CLIENT", "SHELL", "PROC_ENUMERATE", "PROC_KILL_ANY", "KEYMAP_ADMIN", "HOST_TRANSFER"]

[capabilities.sysmon]
grant = ["DISPLAY_CLIENT", "PROC_ENUMERATE", "PROC_KILL_ANY"]

[env]
PATH            = "/bin:/usr/bin"
HOME            = "/home/user"
USER            = "user"
XDG_RUNTIME_DIR = "/run"
PMOS_DISPLAY    = "/run/display"

[debug]
kernel_log_level = "info"
serial_shell     = false
```

This is what new users start with. It can be edited from a text editor; a
reboot (tab reload) applies valid changes. A path or capability grant outside
the security policy is rejected and the complete built-in safe configuration
is used.

---

## 5. Validation

Init validates the file at boot with the same TOML schema-check the package
manifest uses, including the trusted privileged-service path allow-list.
Validation errors produce a single warning line to `/dev/console` and defaults
are used. Init never panics on a bad `/etc/init.conf` — PID 1 crashing is a
product failure, so invalid conf always falls back to built-in defaults.

---

## 6. Forward compatibility

New keys added in later versions MUST be ignored by older init
binaries. New sections MUST NOT change the semantics of existing
sections. A future "services" section is reserved (currently
absent) and MUST NOT be added in a way that breaks v1's boot
semantics.
