# Contract: Application Package Manifest

**Status**: canonical reference for the v1 app bundle format.
**Audience**: third-party app authors, the launcher implementer,
the `pkginstall` utility author.

---

## 1. Bundle format

A PMos application bundle is a single POSIX tar archive (no
compression for v1 — bundle sizes are small and transparency is
preferable to savings). The archive MUST have `manifest.toml` at
its root and at least one WASM binary referenced from that
manifest.

Canonical layout:

```
manifest.toml
bin/<name>.wasm
assets/...              (optional)
```

Paths inside the archive MUST be relative (no leading `/`), MUST
NOT contain `..` segments, and MUST NOT be symlinks. A bundle
that violates any of these rules is rejected at install time.
Regular-file paths MUST be unique. Link entries and archive types other than
regular files/directories are rejected. The installer verifies each tar header
checksum before trusting its path or size.

**File name of the archive**: `<name>-<version>.pmpkg.tar`. The
extension is informative; the launcher identifies bundles by
their content, not by name.

---

## 2. `manifest.toml` schema

```toml
# Required top section: identifies the package.
[package]
name         = "edit"              # REQUIRED; unique; [a-z0-9_-]+, 1..=40 chars
version      = "1.0.0"             # REQUIRED; semver
display_name = "Text Editor"       # REQUIRED; free-form, shown in launcher
author       = "PMos"              # REQUIRED; free-form
summary      = "Simple text editor for plain text and markdown."  # REQUIRED; one line

# Required execution section: how the launcher runs this app.
[exec]
binary = "bin/edit.wasm"           # REQUIRED; path inside the bundle
argv   = []                        # OPTIONAL; default []
envp   = {}                        # OPTIONAL; default {}

# Optional UI section: launcher presentation and MIME associations.
[ui]
icon       = "assets/icon.png"     # OPTIONAL; PNG, 32..256 px square
mime_types = ["text/plain"]        # OPTIONAL; informs file-manager "open with"
categories = ["Utility"]           # OPTIONAL; free-form tags, informational

# Required capabilities section: what this app needs from the kernel.
[capabilities]
required = ["DISPLAY_CLIENT"]      # REQUIRED array; may be empty
optional = []                      # OPTIONAL; install-time policy decides

# REQUIRED in the packaged manifest. `xtask package` generates this section
# from the final payload bytes; source `pkg.toml` files omit it.
[integrity]
sha256 = { "bin/edit.wasm" = "<64 lowercase hex characters>" }
```

### 2.1 Field details

| Field                    | Type          | Constraint                                     |
|--------------------------|---------------|------------------------------------------------|
| `package.name`           | string        | `[a-z0-9_-]+`, 1..=40 chars, unique per install |
| `package.version`        | string        | semver `MAJOR.MINOR.PATCH`                     |
| `package.display_name`   | string        | 1..=60 chars                                   |
| `package.author`         | string        | 1..=80 chars                                   |
| `package.summary`        | string        | 1..=160 chars                                  |
| `exec.binary`            | string (path) | relative path to a file in the bundle          |
| `exec.argv`              | [string]      | default `[]`                                   |
| `exec.envp`              | {string→string} | default `{}`                                 |
| `ui.icon`                | string (path) | relative path to a PNG file in the bundle      |
| `ui.mime_types`          | [string]      | MIME strings                                   |
| `ui.categories`          | [string]      | free-form                                      |
| `capabilities.required`  | [string]      | capability names (see `data-model.md §5`)      |
| `capabilities.optional`  | [string]      | capability names                               |
| `integrity.sha256`       | {path→string} | every non-manifest file mapped to lowercase SHA-256 |

### 2.2 Unknown keys

Unknown keys in any section MUST be ignored by the installer. This
is the forward compatibility story — future versions of PMos may
add new keys, and existing installers MUST not choke on them.

### 2.3 Validation

The installer performs these checks, in order. Failure at any step
aborts the install and reports a distinct error:

1. The archive is well-formed tar, every header checksum is valid,
   paths are unique, and it contains `manifest.toml` at the root.
2. `manifest.toml` is well-formed TOML.
3. All REQUIRED fields are present.
4. `package.name` matches `[a-z0-9_-]+` and length.
5. `package.version` parses as semver.
6. `package.display_name`, `package.author`, and `package.summary` are
   non-empty, within their character limits, and contain no control characters.
7. `exec.binary` and `ui.icon`, if present, resolve to regular files
   inside the bundle; the icon has a PNG signature/IHDR and square dimensions
   in the declared 32..256 px range.
8. `integrity.sha256` names every non-manifest regular file exactly
   once, names no missing file, and every digest matches the payload.
9. `exec.binary` begins with the WASM magic `\0asm`.
10. Every capability in `capabilities.required` and
   `capabilities.optional` is a known capability name.
11. For a v1 third-party install, every required capability is
    `DISPLAY_CLIENT`; privileged required capabilities are rejected before
    any filesystem mutation. Optional capabilities remain metadata only and
    are never copied into the v1 desktop entry.
12. A package with the same `package.name` is not already
   installed, OR the installer was invoked with `--upgrade`.

---

## 3. Installation

Installing means extracting the bundle under `/opt/<name>/` and
writing the desktop entry. It is scriptable from userland — the
launcher's "install" flow is just a wrapper around the same
operations a terminal user could do by hand.

`pkginstall` validates the complete archive before mutation, writes files to a
hidden sibling staging directory, syncs every file, sets the manifest executable
to mode `0755` (other payload files to `0644`), and publishes the application
directory before the desktop entry. Upgrades retain the previous
application and desktop entry as sibling backups until both new paths are
published. Any extraction, sync, rename, or desktop-entry failure rolls back
both paths; a failed install cannot remove or partially expose the live version.

### 3.1 Desktop entry file

After extraction, the installer writes a `.desktop`-style file at
`/usr/share/applications/<name>.desktop`:

```
[Desktop Entry]
Type=Application
Name=Text Editor
Exec=/opt/edit/bin/edit.wasm
Icon=/opt/edit/assets/icon.png
Summary=Simple text editor for plain text and markdown.
MimeType=text/plain;
Categories=Utility;
X-PMos-Caps=DISPLAY_CLIENT
```

The launcher reads `/usr/share/applications/*.desktop` to build
its app list. Each desktop entry maps 1:1 to a bundled app.

### 3.2 Manual install via shell

A user who opens the terminal can install an app like this:

```
$ pkginstall /home/user/Downloads/myapp-1.0.0.pmpkg.tar
```

This is the supported safe path because it performs integrity validation and
transactional publication. `pkginstall-desktop-entry` remains a low-level
recovery tool for an already-validated/extracted tree; it enforces the same v1
required-capability policy. The launcher's filesystem watch on
`/usr/share/applications/` wakes it after the published directory mutation; it
reloads the catalog without a periodic idle timer.

### 3.3 Install via the file manager

When the user drops a `.pmpkg.tar` file into a bundle-aware
location (e.g., right-click → Install), the file manager spawns
the same `pkginstall` tool as an ordinary userland process and
shows the result. The file manager has no special capability —
it runs as an ordinary app.

### 3.4 Uninstall

```
$ rm -rf /opt/myapp
$ rm /usr/share/applications/myapp.desktop
```

The launcher's next refresh removes the entry.

---

## 4. Launcher contract

The launcher is a userland program that:

1. At startup, reads `/usr/share/applications/*.desktop` and
   builds its app list.
2. Watches `/usr/share/applications/` for create/delete/modify readiness and
   reloads the catalog after each bounded watch drain. It does not periodically
   wake an otherwise idle desktop.
3. On user selection, spawns the app using `proc_spawn` with
   argv from the desktop entry's `Exec=`, env inherited from the
   launcher's own environment (plus `XDG_*` additions), and caps
   read from `X-PMos-Caps`.

The executable path remains the installed VFS path (for example,
`/opt/edit/bin/edit.wasm`). Loading it through a prebundled `/bin` alias is not
conformant: the Worker must execute the bytes that passed package validation.

The launcher **MUST NOT** grant caps the desktop entry does not
declare. It **MUST NOT** grant caps the launcher itself does not
possess (`cap_grant` enforces this). This means an ordinary
launcher (without `CAP_GRANT`) cannot start a `SHELL`-capability
app — only init can. In v1 this is fine because the only
`SHELL`-capable program (the desktop shell itself) is launched by
init, not by the user-facing launcher.

### 4.1 File-manager default application dispatch

For v1 text MIME associations, Files resolves the selected extension to
`text/plain` or `text/markdown`, then reads the installed
`/usr/share/applications/edit.desktop` entry through its ordinary WASI root
preopen. Files validates a bounded UTF-8 `[Desktop Entry]` section, requires
`Type=Application` and a matching `MimeType=`, parses `Exec=` into direct argv
without invoking a command shell, and passes the selected absolute path as
`%f` or as the final argument when `%f` is absent. Unsupported field codes,
ambiguous duplicate security-sensitive keys, relative executable paths,
unknown capabilities, and malformed quoting are rejected before spawn.

Files calls the documented `proc_spawn_manifest` extension itself. The child
is therefore an ordinary isolated child of Files, not a shell-owned UI
shortcut. `X-PMos-Caps` must be a subset of both the documented Files role
caps and the live set returned by `cap_list`; the kernel repeats the mandatory
subset check. Files inherits argv-independent environment and stdio through
the spawn manifest and reaps exited children. Keyboard Open/Enter and bounded
pointer double-activation use this path. Preview remains a separate explicit,
read-only Files action.

Once Edit opens an existing document it retains the read/write WASI fd for
the lifetime of that binding. Normal Save seeks, writes, truncates, and syncs
through that fd, so a concurrent Files rename changes only the directory entry
and Save continues to update the same inode without recreating the stale name.
New documents and Save As have no pre-existing inode identity to preserve;
they continue to use a synced temporary sibling plus atomic pathname rename,
then bind a newly opened fd for subsequent normal Saves.

---

## 5. Capability declaration policy

An app declares the caps it needs in `capabilities.required`.

- **At install time**: v1 has no trusted/elevated third-party package
  class and no capability-consent GUI. `required` is therefore confined to
  `DISPLAY_CLIENT`; any other required capability is a distinct, atomic
  install failure. `optional` may name known capabilities for forward
  compatibility, but v1 never writes optional caps into `X-PMos-Caps`.
- **At spawn time**: the kernel verifies that the launcher
  (the parent) holds every cap it tries to pass into the
  child. A cap the parent does not hold yields
  `ENOTCAPABLE`.
- **Runtime cap grant** (`cap_grant`): v1 uses this only
  through init; no user-facing workflow for runtime elevation
  exists. The forward-compatible hook is there.

---

## 6. Sample bundle

A sample "hello" app bundle ships in the repo under
`crates/sample-app/`. Running `just package sample-app` produces
`dist/pkgs/hello-0.1.0.pmpkg.tar`. This is the fixture used by the
Playwright integration test that covers User Story 10 (third-party
app installation).

---

## 7. Non-goals for v1 (explicitly)

- **No package signing/authenticity**: v1 verifies declared SHA-256 payload
  integrity to detect corruption and incomplete/tampered transfers, but the
  digest is stored in the same unsigned archive. It does not establish who
  authored the package. Users still install only sources they trust; signing
  is a v2 amendment.
- **No version conflict resolution**: installing a package
  `name=X version=2` over an existing `name=X version=1`
  requires `--upgrade` and blindly replaces; no migration
  hooks in v1.
- **No central registry**: Principle III forbids a backend. The
  `.pmpkg.tar` distribution channel is "however the user got
  the file" — email, USB stick, direct download from a
  developer's static site.
- **No dependencies**: a bundle may not declare a dependency on
  another package. Everything it needs ships in the tar. v2 may
  add a dependency format and a resolver.

---

## 8. Forward-compatibility promise

A v1 installer, given a bundle whose `manifest.toml` contains a
new key introduced in v1.1, MUST successfully install the bundle
and ignore the unknown key. A v1.1 installer installing a v1
bundle SHOULD fill defaults for any v1.1-introduced keys. Neither
direction introduces a hard break.
