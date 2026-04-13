# Feature Specification: Browser OS v1 — Initial Release

**Feature Branch**: `001-browser-os-v1`
**Created**: 2026-04-13
**Status**: Draft
**Input**: User description: a real operating system running entirely inside a
browser tab — kernel, isolated userland processes, POSIX-flavored syscalls,
virtual filesystem, IPC, display server, window toolkit, desktop shell, a
starter set of bundled applications, a package format, and developer
documentation. No backend, no accounts, no telemetry. Persistent, offline,
private to each browser profile.

## Clarifications

### Session 2026-04-13

- Q: How do users move files between their host OS and PMos (import/export)? → A: Both drag-and-drop onto the file manager window AND explicit Import / Export menu items.
- Q: What accessibility baseline must v1 meet? → A: None — accessibility is an explicit v1 non-goal and a documented v2 amendment area.
- Q: What does `/home/user` contain on first boot? → A: A full starter kit: `README.md`, `Downloads/`, `Documents/` (with two sample text files), and `Pictures/`.
- Q: What happens when the kernel itself panics? → A: The bootstrap displays a distinct kernel-panic overlay with a short diagnostic, then automatically reloads the tab after a short delay; the filesystem recovers from the journal on the next boot.
- Q: What does the v1 Settings app expose beyond wallpaper/theme? → A: Full preference pane — wallpaper + wallpaper-fit mode, theme (light/dark), keyboard layout, timezone, default terminal font, and an about-this-system pane.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — First Boot to a Usable Desktop (Priority: P1)

A first-time visitor opens the URL on a fresh browser profile. Within
seconds they see a brief boot sequence followed by a desktop with a
wallpaper, a taskbar, and a launcher. They can open the terminal from the
launcher and type commands into it.

**Why this priority**: This is the minimum viable OS. Without "open URL,
see desktop, launch terminal," nothing else in the product is meaningful.
Every other story is a superset of this one.

**Independent Test**: Open the URL on a fresh browser profile. Within 10
seconds see a desktop. Open the terminal from the launcher. Type
`echo hello`. See `hello` in the terminal.

**Acceptance Scenarios**:

1. **Given** a fresh browser profile with no prior visit, **When** the
   user navigates to the URL, **Then** within 10 seconds the desktop is
   visible and interactive (responds to clicks and keystrokes).
2. **Given** the desktop is visible, **When** the user opens the launcher
   and selects Terminal, **Then** a terminal window appears and accepts
   keyboard input within one second.
3. **Given** the terminal is open and focused, **When** the user types a
   command and presses Enter, **Then** the command runs and its output
   appears in the terminal.

---

### User Story 2 — Windowed Multitasking (Priority: P1)

The user opens multiple applications at once, each in its own window.
They move windows by dragging titlebars, resize from edges, minimize to
the taskbar, maximize to fill the desktop, and close them. Clicking a
window brings it to the front and directs keystrokes to it.

**Why this priority**: A desktop OS without windowing is not a desktop.
Joint P1 with Story 1 — the two together are the minimum usable
experience.

**Independent Test**: Open two applications. Drag one on top of the
other. Click the underneath one; it raises to the front and takes
keyboard focus. Minimize it; it disappears from the desktop and is
reachable from the taskbar. Click it in the taskbar; it returns.

**Acceptance Scenarios**:

1. **Given** two apps are open in overlapping windows, **When** the user
   drags the background window by its titlebar, **Then** it moves under
   the cursor and other windows remain where they were.
2. **Given** two overlapping windows, **When** the user clicks the
   partially obscured one, **Then** it raises to the front, becomes the
   keyboard focus, and the previously-focused window loses focus.
3. **Given** a window, **When** the user clicks minimize, **Then** the
   window is hidden from the desktop but remains listed in the taskbar;
   clicking the taskbar entry restores it to its previous position.
4. **Given** a window, **When** the user clicks maximize, **Then** the
   window fills the available desktop area; clicking maximize again
   restores it to its previous size and position.
5. **Given** a window, **When** the user clicks close, **Then** the
   window disappears and the process that owned it is terminated (or is
   asked to save state first for applications that declare that need).

---

### User Story 3 — Persistent Filesystem (Priority: P1)

Files and directories the user creates persist across tab close, browser
restart, and machine reboot. On return, the filesystem is exactly as it
was left, stored in the user's own private browser storage. No other
visitor can see it, and the site operator cannot see it.

**Why this priority**: Without persistence, the product is a toy. Every
later story that writes to disk depends on this working. It is also the
core of the "private OS we cannot take down" product promise.

**Independent Test**: Open the terminal. Create `/home/me/notes/hi.txt`
with content "hello". Close the tab. Reopen the URL. Read
`/home/me/notes/hi.txt`. See "hello".

**Acceptance Scenarios**:

1. **Given** the user has created files and directories during a
   session, **When** the user closes the tab and later reopens the URL,
   **Then** the same files and directories are present with the same
   contents.
2. **Given** the user has created files, **When** the user restarts the
   browser or the machine and reopens the URL, **Then** the files are
   still present.
3. **Given** two different browser profiles on the same machine, **When**
   profile A creates a file and profile B opens the URL, **Then** profile
   B does not see profile A's file.
4. **Given** an in-progress write, **When** the tab is closed abruptly,
   **Then** on next load the filesystem is consistent: fully-flushed
   writes are present and the filesystem is not corrupted.

---

### User Story 4 — Shell, Pipes, and Redirection (Priority: P2)

The user opens the terminal and runs real shell pipelines with multiple
processes: `ls | grep something > out.txt`. The shell creates separate
processes for each stage, connects them with pipes, redirects the last
process's stdout to a file, and waits for the pipeline to finish. After
the command completes, `out.txt` exists in the filesystem and is visible
from the file manager.

**Why this priority**: This is the user's stated acceptance test for
"real process model." It proves that the process model, IPC, and VFS
work together, and it is the most compelling demonstration that this is
a real OS and not a UI sharing one memory space.

**Independent Test**: In a directory containing files whose names match
and do not match `foo`, run `ls | grep foo > out.txt`. Confirm
`out.txt` contains only the matching names. Open the file manager and
see `out.txt` in the directory.

**Acceptance Scenarios**:

1. **Given** a directory with mixed filenames, **When** the user runs
   `ls | grep X > out.txt`, **Then** `out.txt` is created containing
   exactly the matching lines and nothing else.
2. **Given** any multi-stage pipeline, **When** the pipeline runs,
   **Then** each stage runs as a separate process and each stage
   terminates cleanly; no zombies are left behind.
3. **Given** a running foreground command, **When** the user sends
   Ctrl-C (or the job-control equivalent), **Then** the command
   terminates and the shell returns to its prompt.
4. **Given** a pipeline whose reader exits early, **When** a later
   stage tries to write to the closed pipe, **Then** it receives the
   standard end-of-pipe signal and terminates cleanly.
5. **Given** the shell, **When** the user sets and uses environment
   variables (`FOO=bar`, `echo $FOO`), **Then** variables behave as
   POSIX shells do, including inheritance into child processes.

---

### User Story 5 — File Manager + Text Editor (Priority: P2)

The user opens the file manager, browses home, creates a new folder,
opens the text editor, types content, saves as a new file in the new
folder, and closes the editor. On reopening the file from the file
manager, the same content is there.

**Why this priority**: This is the non-terminal path to file management,
and most visitors will try it before the terminal. It exercises the same
VFS calls the shell uses, from a different process lineage, so it is an
independent confidence signal.

**Independent Test**: In the file manager, create `/home/me/notes`. Open
the text editor, type "hello world," save as
`/home/me/notes/hi.txt`, close the editor. In the file manager see
`hi.txt` with size > 0. Double-click it; the text editor reopens with
"hello world" as its content.

**Acceptance Scenarios**:

1. **Given** the file manager is showing `/home/me`, **When** the user
   creates a new folder `notes`, **Then** `notes` appears immediately
   and is browsable.
2. **Given** the text editor is open on a new file, **When** the user
   types content and saves to a path, **Then** the file exists at that
   path with that content and is immediately visible in the file
   manager.
3. **Given** a file exists, **When** the user double-clicks it in the
   file manager, **Then** the text editor opens with the file's
   content.
4. **Given** a file, **When** the user renames it in the file manager,
   **Then** the file is renamed on disk; any text editor instance with
   that file open handles the rename without corrupting the file.
5. **Given** a file, **When** the user deletes it in the file manager,
   **Then** it is removed from disk and no longer listed.

---

### User Story 6 — Offline After First Load (Priority: P2)

After first load, the user can disable network connectivity entirely
and the system continues to boot and run. On reopening the URL with the
network off, the system still boots to a usable desktop with the
filesystem intact. Network traffic only happens when the user's own
programs initiate it.

**Why this priority**: Offline capability is both a product promise
("we cannot take it down") and a constitutional principle. It is
testable independently by a simple network-disable exercise.

**Independent Test**: Load the URL once with the network on. Disable
network connectivity. Close the tab. Reopen the URL. Confirm the
desktop boots and previously used apps still launch.

**Acceptance Scenarios**:

1. **Given** the user has loaded the URL once, **When** the user
   disables the network and reloads, **Then** the system boots to a
   usable desktop within the warm-load time budget.
2. **Given** the system is running offline, **When** the user opens
   the bundled apps (terminal, file manager, text editor, settings,
   system monitor), **Then** they all launch and function normally.
3. **Given** the system is running offline, **When** the user creates,
   edits, and deletes files, **Then** the changes persist locally and
   survive a subsequent reload.
4. **Given** the system is running, **When** the user inspects the
   network traffic, **Then** no traffic is initiated by the OS itself
   after first load; only user-program traffic is seen.

---

### User Story 7 — System Monitor & OS Introspection (Priority: P2)

The user opens the system monitor. It lists every running process with
its PID, name, memory use, and open files. Each app (terminal, file
manager, the system monitor itself, the desktop shell) appears as a
separate process with a separate memory region. The user can select a
process and terminate it; its windows disappear; other processes are
unaffected.

**Why this priority**: This is the most direct user-visible proof that
the kernel exposes real OS-level introspection, that the process table
is real, and that memory is separate per process. It is also the
debugging tool the rest of the system will use.

**Independent Test**: Open terminal, file manager, and system monitor.
In the system monitor, see at least those three plus the desktop shell,
each with unique PIDs and non-overlapping memory regions. Select the
file manager, click Terminate; its window disappears and the other
processes continue running.

**Acceptance Scenarios**:

1. **Given** several apps are running, **When** the user opens the
   system monitor, **Then** every running userland process is listed
   with a unique PID, a name, memory usage, and its list of open file
   descriptors.
2. **Given** the system monitor is showing a process list, **When**
   the user selects a process and clicks Terminate, **Then** that
   process exits within 1 second, its windows close, and the remaining
   processes continue running without disruption.
3. **Given** a deliberately malicious test program, **When** the
   program attempts to read or write another process's memory through
   any non-IPC channel, **Then** the attempt fails at the execution
   substrate level, not by convention or code review.
4. **Given** a process crashes unexpectedly, **When** the crash
   occurs, **Then** the system monitor reflects the exit, the
   process's resources are reclaimed, and no other process is
   affected.

---

### User Story 8 — Desktop Shell Replacement (Layering Test) (Priority: P2)

With multiple apps running, the user terminates the desktop shell from
the system monitor. The taskbar and launcher disappear, but the running
apps and their windows remain on screen and interactive. The user
launches a different desktop shell binary. The new shell starts, draws
its own taskbar and launcher, and enumerates the already-running apps
in its taskbar.

**Why this priority**: This is the single most important acceptance
test for whether the architecture is real. A product that passes every
other story but fails this one is a UI pretending to be an OS. Failing
this story is a build-breaking issue per the project constitution.

**Independent Test**: With terminal, file manager, and text editor
open, kill the desktop shell from the system monitor. Confirm the three
apps and their windows remain on screen and still accept input. Launch
an alternative shell binary. Confirm the new shell's taskbar lists the
three running apps; click a taskbar entry and confirm it focuses the
corresponding window.

**Acceptance Scenarios**:

1. **Given** the desktop shell is running with multiple apps open,
   **When** the user kills the desktop shell, **Then** the kernel, the
   display server, the toolkit, and the running apps all continue to
   run, and the apps' windows remain on screen and interactive.
2. **Given** the desktop shell has been killed, **When** the user
   launches a different program that holds the "shell" capability,
   **Then** the new shell starts, draws its own taskbar and launcher,
   and enumerates the already-running apps in its taskbar.
3. **Given** the replacement shell is active, **When** the user clicks
   a taskbar entry for an app that was launched before the original
   shell was killed, **Then** the corresponding window is raised and
   focused.
4. **Given** only the desktop shell binary has changed, **When** the
   replacement shell is in use, **Then** no other component (kernel,
   display server, toolkit, applications) required modification,
   recompilation, or reconfiguration for the replacement to work.

---

### User Story 9 — Settings: Wallpaper and Theme (Priority: P3)

The user opens the settings application, picks a different wallpaper
from several built-in options, and the desktop updates immediately.
The user changes the theme (e.g., light/dark); theme-aware applications
pick up the change without being restarted. Both changes persist
across reloads.

**Why this priority**: Table-stakes customization for a desktop. Lower
priority than the architectural stories because the OS is functional
without it. Also exercises the system's preferences story end-to-end.

**Independent Test**: Open settings. Change wallpaper from default to a
different built-in. Confirm the desktop wallpaper changes within one
frame. Close the tab. Reopen the URL. Confirm the new wallpaper is
active from the moment the desktop appears.

**Acceptance Scenarios**:

1. **Given** settings is open, **When** the user selects a different
   wallpaper, **Then** the desktop wallpaper updates immediately and
   the selection is reflected in the settings UI.
2. **Given** a wallpaper or theme has been changed, **When** the user
   closes the tab and reopens the URL, **Then** the new wallpaper or
   theme is in effect from the moment the desktop appears.
3. **Given** settings is open and a theme-aware app is running,
   **When** the user changes the theme, **Then** the running app
   receives the theme change and redraws accordingly without being
   restarted.

---

### User Story 10 — Third-Party App Installation (Priority: P3)

A third-party developer produces an application bundle following the
documented package format. An end user "installs" the app by placing
the bundle in a known filesystem location (e.g., `/apps`) through the
file manager or terminal. The launcher picks up the new app on its
next refresh and lets the user run it like any bundled app.

**Why this priority**: Extensibility is a product goal ("no central app
store") and the only path to a non-trivial third-party ecosystem.
Lower priority because no third-party apps exist on day one; a sample
bundle plus working documentation are sufficient for v1.

**Independent Test**: Using the file manager's `Import…` menu (or
drag-and-drop from the host OS), import a sample app bundle into
`/home/user/Downloads`. Move it into `/apps` via the file manager.
Open (or refresh) the launcher. See the new app listed by its
declared name and icon. Launch it. See it run in its own window.

**Acceptance Scenarios**:

1. **Given** a conformant app bundle, **When** the user places it
   under `/apps`, **Then** within 5 seconds the launcher lists the new
   app with its declared name and icon.
2. **Given** the new app is listed in the launcher, **When** the user
   clicks it, **Then** a new process is started from the bundle's
   entrypoint and its window appears.
3. **Given** the new app is running, **When** the user opens the
   system monitor, **Then** the new app appears as a separate process
   with its own PID and memory region.
4. **Given** the user removes the bundle from `/apps`, **When** the
   user refreshes the launcher, **Then** the app is no longer listed.

---

### Edge Cases

- **Storage denied or private-mode eviction**: when the browser
  refuses persistent local storage, the system shows a clear "cannot
  persist" warning and runs on an in-memory filesystem that will be
  lost on tab close. It does not pretend to have persisted data that
  it has not.
- **Process crash**: the kernel reaps the crashed process, closes its
  file descriptors and IPC endpoints, releases its display-server
  surfaces, and updates the system monitor. Other processes are
  unaffected.
- **Closed pipe with a surviving writer**: the writer receives the
  standard end-of-pipe signal and terminates cleanly; data already in
  flight to the reader is delivered.
- **Many concurrent windows under load**: the system stays
  responsive; input latency stays within budget until resource limits
  are reached, at which point new launches are refused cleanly rather
  than destabilizing the running system.
- **Storage quota exhausted**: writes fail with a clear "out of
  space" error; the filesystem remains consistent.
- **Process killed with open file handles**: the kernel closes the
  handles; files on disk are not corrupted.
- **Input before focus is routed**: keystrokes before any window
  holds focus are either buffered to the first window to receive
  focus or discarded — they never leak into an unintended window.
- **Window dragged off-screen**: a strip of the titlebar remains
  reachable, or a "gather windows" gesture pulls the window back onto
  the visible area.
- **Unsupported browser** (no service worker, no persistent local
  storage): the system shows a clear "unsupported browser" message
  instead of appearing to work and silently losing data.
- **Unauthorized memory access attempt**: fails at the substrate
  level; the attempting process cannot reach another process's memory
  through any non-IPC mechanism.
- **Corrupt app bundle in `/apps`**: the launcher refuses to list it
  and surfaces a diagnostic; other apps continue to work.

## Requirements *(mandatory)*

### Functional Requirements

**Boot, delivery, and product stance**

- **FR-001**: The system MUST, on first visit to the URL, present a
  boot sequence followed by an interactive desktop within 10 seconds
  on a fast connection and a mid-range laptop approximately 5 years
  old.
- **FR-002**: The system MUST, on subsequent visits, boot to an
  interactive desktop within 3 seconds (warm load, cached assets).
- **FR-003**: The system MUST be served from a static file host. The
  product MUST NOT depend on a backend service, database, account
  system, or telemetry endpoint to boot or run.
- **FR-004**: The product MUST NOT collect or transmit any data about
  the user to the site operator at any time. After first load, the
  only network traffic MUST be traffic initiated by the user's own
  programs through the OS's network facilities.

**Kernel, processes, IPC, syscalls**

- **FR-005**: The system MUST include a kernel component that owns
  processes, scheduling, the filesystem, IPC, and the syscall surface.
  The kernel MUST NOT contain any concept of a window, button, or
  other user-interface element.
- **FR-006**: Every userland process MUST run in its own isolated
  execution context with its own memory. One process MUST NOT be able
  to read or write another process's memory directly. Isolation MUST
  be enforced by the execution substrate, not by convention.
- **FR-007**: Userland programs MUST be able to spawn child programs,
  terminate themselves or (subject to capability checks) their
  children, and communicate with other processes via kernel-mediated
  pipes and unix-socket-equivalent IPC.
- **FR-008**: The kernel MUST expose a POSIX-style syscall surface
  covering file open/read/write/close, directory traversal, process
  spawn and wait, pipe creation, IPC, time, and environment.
  Non-POSIX syscalls MUST be narrowly scoped to needs POSIX does not
  cover (display-server IPC, device nodes, capability management).
- **FR-009**: When a process crashes, exits, or is killed, the kernel
  MUST reclaim its resources (memory, file descriptors, IPC
  endpoints, display-server surfaces) and other processes MUST
  continue running unaffected.
- **FR-009a**: When the kernel itself panics or dies (an unhandled
  error in the kernel execution context, a fatal driver failure,
  or any other condition the kernel cannot recover from in
  place), the system MUST:
  (a) display a distinct full-screen "kernel panic" overlay
      containing a short human-readable diagnostic (panic
      message, timestamp, a hint to open the browser devtools
      console for the full trace);
  (b) automatically reload the browser tab after a bounded
      short delay (target: ~5 seconds, long enough to read the
      diagnostic, short enough to feel automatic);
  (c) on the subsequent boot, recover the filesystem from its
      journal (cf. FR-014) so no user-level data is lost for
      writes that had been flushed prior to the panic.
  In-place kernel restart (resuming existing user processes
  without reloading the tab) is an explicit v1 non-goal.
- **FR-010**: Privilege in the system MUST be expressed as
  kernel-granted capabilities. There MUST NOT be a distinction between
  "system programs" and "user programs" except in terms of which
  capabilities each has been granted.

**Virtual filesystem and persistence**

- **FR-011**: The system MUST provide a hierarchical virtual
  filesystem with operations: create, read, write, append, rename,
  move, delete, list directory, stat.
- **FR-012**: The filesystem MUST persist across tab close, browser
  restart, and machine reboot, stored entirely in the user's
  browser-local persistent storage.
- **FR-013**: The filesystem MUST include a home directory for the
  local user and a documented location from which the launcher
  enumerates installed application bundles.
- **FR-013a**: On first boot (i.e., when the root filesystem is
  being initialised from the initramfs because no prior PMos
  filesystem exists in OPFS), the system MUST populate
  `/home/user` with the following starter content:
  - `/home/user/README.md` — a short (10–30 line) plain-markdown
    document explaining what PMos is, how to launch apps from the
    launcher, and how to open a terminal. Readable in the bundled
    text editor.
  - `/home/user/Downloads/` — empty directory; the conventional
    landing spot for files imported from the host OS via the
    file manager (see FR-032a) and for third-party app bundles
    before installation.
  - `/home/user/Documents/` — containing two sample text files
    that demonstrate the text editor: one plain text
    (`welcome.txt`) and one markdown (`editing.md`).
  - `/home/user/Pictures/` — empty directory; reserved for
    user-imported images.
  After first boot, these files and directories are ordinary
  user content: the user may rename, move, modify, or delete any
  of them, and the system MUST NOT restore them on subsequent
  boots. The starter content is a first-run convenience, not a
  protected set.
- **FR-014**: The filesystem MUST remain consistent under abrupt tab
  close: on the next load, fully-flushed writes MUST be present, and
  in-flight writes MUST NOT corrupt unrelated data.
- **FR-015**: The filesystem MUST be private to the browser profile:
  a second profile or a second browser on the same machine MUST NOT
  see the first profile's files.

**Offline and asset caching**

- **FR-016**: After first load, the system MUST boot and function
  with no network connectivity, indefinitely. All OS assets required
  for boot and for running the bundled applications MUST be cached
  locally after the first load.
- **FR-017**: The system MUST NOT require any network request to
  reach a usable desktop on a subsequent load. User programs MAY
  require network for their own functionality, but the OS itself MUST
  NOT.

**Display server and protocol**

- **FR-018**: The system MUST include a display server process. The
  display server MUST be the only component in the system that
  touches the framebuffer-equivalent output device.
- **FR-019**: Applications MUST interact with the display server
  exclusively through a wire protocol over IPC. The protocol MUST
  support, at minimum: surface creation, buffer attachment, surface
  commit, frame callbacks, input events (pointer and keyboard), input
  focus changes, and window roles.
- **FR-020**: A hand-written application that speaks the display
  server protocol directly, with no toolkit linked, MUST be able to
  create a window, draw into it, and receive input. The protocol MUST
  be the source of truth; any toolkit MUST be a convenience wrapper
  over it.

**Window toolkit**

- **FR-021**: The system MUST provide a window-toolkit library that
  wraps the display server protocol and offers primitives for
  windows, buttons, text inputs, menus, layout, and event handling.

**Desktop shell**

- **FR-022**: The desktop shell MUST be an ordinary userland process.
  Its only elevated privilege MUST be a single, documented "shell"
  capability granted by the kernel at startup. The kernel MUST be
  capable of granting this capability to any conformant userland
  program.
- **FR-023**: The desktop shell MUST provide, at minimum: a
  wallpaper, a taskbar listing currently running graphical
  applications, a launcher that enumerates installed applications,
  and window management chrome (titlebars with close and
  minimize/maximize affordances, resize handles).
- **FR-024**: The desktop shell MUST be replaceable at runtime: when
  the running shell is terminated and a different shell binary that
  holds the "shell" capability is launched, the kernel, display
  server, toolkit, and every running application MUST continue to run
  unchanged, and the new shell MUST be able to enumerate the
  already-running applications.

**Window management (user-visible)**

- **FR-025**: The user MUST be able to open multiple applications
  simultaneously, each in its own window.
- **FR-026**: The user MUST be able to move a window by dragging its
  titlebar, resize it from its edges, minimize it to the taskbar,
  maximize it to the desktop area, and close it.
- **FR-027**: Clicking a window MUST raise it to the front and route
  keyboard input to it. At any moment, at most one window MUST hold
  keyboard focus.
- **FR-028**: Closing a window MUST terminate the owning process
  unless the application declares otherwise and is given a chance to
  save state first. Terminating a process MUST close its windows.

**Bundled starter applications**

- **FR-029**: The initial release MUST ship with the following
  applications, each running as an ordinary userland process:
  terminal emulator, shell, file manager, text editor, settings, and
  system monitor.
- **FR-030**: The shell MUST support command execution, pipes between
  commands, input/output redirection to files, environment variables,
  the builtins `cd`, `pwd`, `echo`, `exit`, `export`, `env`, and
  basic job control (foreground, background, interrupt, optional
  suspend/resume).
- **FR-031**: The terminal emulator MUST host a shell, display its
  output, pass user input to it, and handle a minimum subset of
  ANSI/VT escape sequences sufficient for the bundled shell and
  utilities.
- **FR-032**: The file manager MUST let the user browse directories,
  create files, create folders, rename, move, copy, and delete, and
  open files in the appropriate default application.
- **FR-032a**: The file manager MUST support importing files from
  the user's host operating system into the current directory via
  BOTH (a) drag-and-drop of one or more files onto the file
  manager window, AND (b) an explicit `Import…` menu item that
  opens the browser's native file picker. Both paths MUST copy
  each chosen host file into the PMos filesystem as an ordinary
  file at a non-colliding name in the current directory.
- **FR-032b**: The file manager MUST support exporting a selected
  PMos file to the user's host operating system via an explicit
  `Export…` / `Download…` menu item that triggers a standard
  browser save of the file's content under the file's PMos name.
- **FR-033**: The text editor MUST let the user open a file, edit
  its content, save, save-as, and show an unsaved-changes prompt on
  close.
- **FR-034**: The settings application MUST provide a preference
  pane covering the following controls. Every change MUST be
  persisted to a file under `/etc/` and MUST survive across
  sessions:
  1. **Wallpaper** — chosen from several built-in options; the
     desktop shell updates immediately (within one frame) when
     the selection changes.
  2. **Wallpaper fit mode** — one of `stretch`, `tile`, `center`,
     `fill`; the desktop shell re-renders the wallpaper surface
     when the selection changes.
  3. **Theme** — one of `light` or `dark`. Theme-aware
     applications receive the change and redraw without being
     restarted.
  4. **Keyboard layout** — chosen from a small set of bundled
     layouts (e.g., US QWERTY, UK QWERTY, Dvorak). The display
     server loads the new keymap and subsequent key events route
     through it; applications need not be restarted.
  5. **Timezone** — chosen from a list of IANA zone names (or
     "UTC"). Settings writes the selection to
     `/etc/preferences.toml`. Init reads the preferences file at
     every `proc_spawn` and sets `TZ` in the new child's
     environment. **Already running processes keep their
     spawn-time `TZ` until they exit**; there is no live
     re-delivery of timezone changes to running processes and no
     synthetic `/etc/localtime` file.
  6. **Default terminal font** — chosen from a small set of
     bundled bitmap fonts. The terminal emulator reads the
     preference on launch; already-running terminals continue
     with their previous font.
  7. **About this system** — a read-only pane showing the PMos
     version, the kernel ABI version, the compositor version, a
     storage-usage gauge (OPFS quota and used/free, read from
     `/proc/storage`), and a short license/credits blurb.
- **FR-034a**: The initial release MUST bundle assets sufficient
  to satisfy FR-034: at least three wallpapers, two themes
  (light and dark), at least three keyboard layouts, a timezone
  database covering the common IANA zones, and at least two
  bitmap terminal fonts. All of these MUST be part of the
  initramfs so they are available offline from first boot.
- **FR-035**: The system monitor MUST list every running userland
  process with PID, name, memory use, and open file descriptors,
  MUST let the user terminate a selected process, and MUST reflect
  process lifecycle events (spawn, exit, terminate) within 1 second.

**Package format and third-party installation**

- **FR-036**: The system MUST define an application package format
  that bundles the program's executable code, metadata (name, icon,
  entrypoint), and declared capability requirements.
- **FR-037**: Installing an application MUST be possible by placing a
  package bundle in a known filesystem location. The launcher MUST
  pick up new bundles without any central registration, app store,
  or system restart.
- **FR-038**: The system MUST validate bundles before exposing them
  in the launcher; malformed bundles MUST be refused with a
  diagnostic and MUST NOT affect other installed apps.

**Developer documentation**

- **FR-039**: The system MUST ship with developer documentation that
  describes the syscall surface, the display server protocol, the
  toolkit API, the package format, and a worked end-to-end example
  of writing, packaging, and installing an application.

**Non-goals (explicit exclusions for v1)**

- **FR-040**: The initial release MUST NOT support multiple users per
  browser profile.
- **FR-041**: The initial release MUST NOT include any account system,
  remote sync, cloud storage, or cross-device state.
- **FR-042**: The initial release MUST NOT emulate x86 or any other
  CPU architecture and MUST NOT attempt to run unmodified native
  binaries from other operating systems.
- **FR-043**: The initial release MUST NOT include GPU-accelerated 3D
  rendering or an audio mixing server.
- **FR-044**: The initial release MUST NOT include a raw TCP/IP stack.
  Network access by user programs is limited to the high-level
  facilities the browser substrate naturally exposes.
- **FR-045**: The initial release MUST NOT claim any accessibility
  conformance. Keyboard-only operability, screen-reader semantics,
  and contrast/theming requirements are explicit v1 non-goals and
  are reserved for a v2 amendment. The toolkit and desktop shell
  MAY ship with partial keyboard navigation where it falls out
  naturally, but MUST NOT be held to a completeness bar in v1.

### Key Entities

- **Process**: a running program with an isolated execution context,
  its own memory, a PID, parent/child relationships, open file
  descriptors, IPC endpoints, and a set of capabilities. Created by
  `spawn`, destroyed by exit or termination.
- **File / Directory**: named entries in the virtual filesystem, with
  content, size, timestamps, and a parent directory. Files persist in
  browser-local storage.
- **Filesystem**: the full hierarchical tree of files and
  directories, private to the browser profile, backed by
  browser-local persistent storage.
- **Capability**: a kernel-granted token that authorizes a process to
  perform a specific privileged operation (e.g., "act as the desktop
  shell," "enumerate other processes," "open network sockets").
  Capabilities are the system's only privilege model.
- **Syscall**: a kernel-mediated operation a process may request —
  filesystem, process, IPC, time, and so on. POSIX-style surface.
- **IPC Endpoint**: a kernel-owned channel between processes (pipe,
  unix-socket-equivalent, or display-server protocol socket). All
  inter-process communication goes through an IPC endpoint.
- **Surface**: a display server object representing a region of
  pixels a client application wants to display. Clients attach
  buffers to surfaces and commit them.
- **Buffer**: a region of pixel data a client produces and submits to
  the display server for display on a surface.
- **Event**: a message from the display server to a client over the
  protocol — pointer input, keyboard input, frame callback, focus
  change, window-role change.
- **Window Role**: a display-server concept classifying what a
  surface is (ordinary app window, panel/taskbar, popup menu, desktop
  background). Roles determine compositor stacking and input routing.
- **Application Package**: a file bundle containing an application's
  program code, metadata (name, icon, entrypoint), and declared
  capability requirements. The unit of installation.
- **Desktop Shell**: a userland program holding the "shell"
  capability that provides wallpaper, launcher, taskbar, and window
  chrome. Replaceable without modifying any other component.
- **Session**: the per-visit state of the running system — which
  apps are open, UI preferences, focus, window positions. Distinct
  from the filesystem, which always persists; session state MAY
  optionally be restored on next visit.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A first-time visitor reaches a fully interactive desktop
  within 10 seconds of navigating to the URL on a fast connection and
  a mid-range laptop approximately 5 years old.
- **SC-002**: A returning visitor reaches a fully interactive desktop
  within 3 seconds of navigating to the URL (warm load).
- **SC-003**: Perceived latency from a user input (keystroke, click,
  window drag) to the corresponding visual response is under 100 ms
  in at least 95 % of interactions under typical desktop use (up to
  6 apps open).
- **SC-004**: 100 % of user-created files survive tab close, browser
  restart, and machine reboot, verified by a repeatable test that
  creates, closes, reopens, and reads a diverse set of files.
- **SC-005**: With the network disabled after first load, 100 % of
  the advertised capabilities (boot, launch apps, create/edit/save
  files, run shell pipelines, inspect processes) are available.
- **SC-006**: The pipeline `ls | grep <pattern> > out.txt` runs
  correctly: `out.txt` contains exactly the filtered lines and is
  visible in both the file manager and through the VFS within 1
  second of pipeline completion.
- **SC-007**: The system monitor shows every running userland
  process with a unique PID and a distinct memory region. Terminating
  a process causes it to exit and its windows to close within 1
  second, with no impact on other processes.
- **SC-008**: The layering test succeeds: with three apps running,
  killing the desktop shell leaves all three apps on screen and
  interactive, and launching a different desktop shell binary
  restores taskbar and window management without restarting,
  recompiling, or reconfiguring the kernel, display server, toolkit,
  or any running application. The replacement shell enumerates the
  already-running apps in its taskbar.
- **SC-009**: Process isolation is verifiable: a deliberately
  adversarial test program that attempts to read another process's
  memory through any non-IPC channel fails at the execution substrate
  level in 100 % of attempts.
- **SC-010**: A third-party application packaged to the documented
  format and dropped into the installation directory appears in the
  launcher within 5 seconds and runs as a separate process on first
  launch, without any manual system action.
- **SC-011**: The entire deployed product is a set of static files. A
  test deployment to a plain static host (no backend, no dynamic
  origin) boots and runs the whole product. Taking the origin offline
  after first load does not impair a running system.
- **SC-012**: A developer who has not worked on the project before
  can, using only the public documentation, write a new application,
  speak the toolkit or the display server protocol directly, package
  it, drop it into the install directory, and successfully run it.
- **SC-013**: A hand-written application that speaks the display
  server protocol with no toolkit linked can create a window, draw
  into it, and receive input events — proving the toolkit is not
  privileged.

## Assumptions

- **One anonymous user per browser profile**: the "user" is whoever
  has this browser profile; there is no login, account, or concept of
  "other users." The home directory belongs to that anonymous user.
- **Session restore scope for v1**: the filesystem always persists;
  restoring open windows to their previous positions on next visit is
  a stretch goal, not an acceptance requirement.
- **Launcher UI style**: "a launcher for installed applications"
  permits any discoverable list of installed apps (start-menu, dock,
  spotlight-style). The exact UI form is a plan-level decision.
- **Target device**: "a mid-range laptop from five years ago" is
  interpreted as roughly 4 cores, 8 GB RAM, and integrated graphics.
- **Storage budget**: the system is constrained by the browser-local
  storage quota the browser grants. Quota-exhaustion errors are
  surfaced clearly and do not corrupt the filesystem.
- **Browsers without persistent storage or service workers are not
  supported**: a clear "unsupported browser" message is shown rather
  than silently losing user data.
- **"Disable the network" refers to machine connectivity**, not to
  the in-OS network facilities. The in-OS network stack is whatever
  userland programs get via the browser substrate.
- **Day-one app ecosystem**: only the six bundled apps exist at v1
  launch. The package format is considered accepted when a sample
  third-party bundle plus the documentation demonstrate end-to-end
  install and run.
- **Boot time is measured end-to-end**: "10 seconds" is measured from
  URL navigation to a desktop that accepts and responds to a click on
  the launcher, not to the first visual frame.
- **Constitutional alignment**: this spec is written against the
  project constitution at `.specify/memory/constitution.md`. Where
  user wording and constitutional principles could be read in
  conflict, the constitution wins; this spec contains no cross-layer
  shortcuts and no violations of Principles I–X.
