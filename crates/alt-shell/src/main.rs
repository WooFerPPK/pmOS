//! `/usr/bin/alt-shell` — alternative desktop shell (T178).
//!
//! Layering-test fixture: visibly different chrome from
//! `/usr/bin/shell` so the Playwright layering test can tell
//! them apart in a screenshot. Setting `boot.shell` in
//! `/etc/init.conf` to `/usr/bin/alt-shell` produces a desktop
//! that is functionally identical (taskbar + launcher + window
//! manager) but uses the dark theme instead of light.
//!
//! This binary uses the standard shell's live launcher, preference,
//! taskbar, process-spawn, and reaping paths. Its only policy
//! difference is a dark taskbar palette override, which makes a live
//! shell replacement observable without sacrificing desktop features.

// (original shell main below)

//!
//! Production main: open `/run/display` via the kernel's
//! `pmos_ext.display_connect` extension syscall, wrap the
//! returned fd in an [`FdConnection`] adapter, and call
//! [`shell::run_shell_with_taskbar`] with a freshly-allocated
//! [`shell::Taskbar`]. The toolkit's protocol layer drives
//! the event loop; the FdConnection bridges its `send` /
//! `recv` calls to `fd_write` / `fd_read` syscalls.
//!
//! The native-host build (used by `cargo test -p shell`)
//! falls back to a no-op `MemoryConnection` so the binary
//! links in the workspace's host target. The fd-syscall path
//! is gated on `target_arch = "wasm32"` since the WASI ABI
//! shims aren't reachable from the host `cargo` target.

use shell::{run_shell, run_shell_with_taskbar, ShellExit, Taskbar};
use toolkit::{ClientError, MemoryConnection};

#[cfg(any(target_arch = "wasm32", test))]
fn force_dark_preferences(bytes: Option<&[u8]>) -> Result<Vec<u8>, preferences::PreferencesError> {
    let mut snapshot = bytes
        .and_then(|bytes| preferences::Preferences::parse(bytes).ok())
        .unwrap_or_default();
    snapshot.theme_name = Some("dark".to_string());
    snapshot.to_toml().map(String::into_bytes)
}

// Anchor references to the library's entry points so
// `just build` doesn't DCE the symbols out of the WASM
// binary even though `fn main` only invokes the production
// path on wasm32. Keeping the host-side function pointers
// alive catches any downstream link breakage early.
#[used]
static _KEEP_RUN_SHELL: fn(MemoryConnection, u32) -> Result<ShellExit, ClientError> =
    run_shell::<MemoryConnection>;
#[used]
static _KEEP_RUN_SHELL_WITH_TASKBAR: fn(
    MemoryConnection,
    u32,
    Taskbar,
) -> Result<ShellExit, ClientError> = run_shell_with_taskbar::<MemoryConnection>;

#[cfg(target_arch = "wasm32")]
mod wasm_main {
    use shell::{
        encode_with_spawn_timezone, run_desktop_shell_with_runtimes_and_events, DesktopEventSource,
        DesktopPreferenceRuntime, DesktopWake, FilesystemPreferenceSource, FilesystemStore,
        Launcher, LauncherRuntime, PreferenceSource, SystemLauncherClock, SystemPreferenceClock,
        Taskbar,
    };
    use std::io;
    use toolkit::{FdConnection, FsWatch, PathWatch, WaitFd};

    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    extern "C" {
        fn fd_read(fd: i32, iovs_ptr: *const Iovec, iovs_len: i32, nread_ptr: *mut u32) -> i32;
        fn proc_exit(rval: i32) -> !;
    }

    #[link(wasm_import_module = "pmos_ext")]
    extern "C" {
        /// Spawn a child process. Returns a positive child
        /// pid on success, negative errno on failure.
        fn proc_spawn_manifest(manifest_ptr: *const u8, manifest_len: u32) -> i32;
        /// Reap a zombie child. With WNOHANG (options=1) returns
        /// -EAGAIN if no zombie matches the target. With
        /// target=-1 (WAIT_ANY) reaps any child.
        fn proc_wait(target_pid: i32, options: i32, status_out_ptr: i32) -> i32;
    }

    pub fn shell_reap_zombies() {
        let mut status_out: i64 = 0;
        let status_ptr = &mut status_out as *mut i64 as i32;
        loop {
            let rc = unsafe { proc_wait(-1, 1, status_ptr) };
            if rc <= 0 {
                return;
            }
        }
    }

    /// Closure-friendly wrapper the shell library calls when the launcher
    /// dispatches a row. This mirrors the standard shell's role-specific
    /// capability delegation and timezone-aware spawn manifest.
    pub fn shell_spawn(path: &str) -> i32 {
        let caps = if path == "/bin/settings" {
            abi::cap::initial::SETTINGS.0
        } else if path == "/bin/sysmon" {
            abi::cap::initial::SYSMON.0
        } else if path == "/bin/files" {
            abi::cap::initial::FILES.0
        } else {
            abi::cap::initial::ORDINARY_APP.0
        };
        let argv = Vec::new();
        let environment = std::env::vars().collect::<Vec<_>>();
        let manifest = sh::SpawnWireManifest {
            path,
            argv: &argv,
            env: &environment,
            stdin_fd: None,
            stdout_fd: None,
            stderr_fd: None,
            extra_fds: &[],
            cwd: None,
            caps: Some(caps),
        };
        let mut preferences = FilesystemPreferenceSource::new(preferences::DEFAULT_PATH);
        match encode_with_spawn_timezone(&mut preferences, &manifest) {
            Ok(blob) => unsafe { proc_spawn_manifest(blob.as_ptr(), blob.len() as u32) },
            Err(_) => -EINVAL,
        }
    }

    /// Preference source that keeps wallpaper, clock, keymap, and font choices
    /// live while deliberately pinning only the alternate shell's taskbar
    /// palette to dark chrome.
    struct AlternatePreferenceSource {
        inner: FilesystemPreferenceSource,
    }

    impl AlternatePreferenceSource {
        fn new(path: &str) -> Self {
            Self {
                inner: FilesystemPreferenceSource::new(path),
            }
        }
    }

    impl PreferenceSource for AlternatePreferenceSource {
        fn read(&mut self) -> io::Result<Option<Vec<u8>>> {
            let bytes = self.inner.read()?;
            super::force_dark_preferences(bytes.as_deref())
                .map(Some)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid preferences"))
        }
    }

    const EINVAL: i32 = 28;

    #[repr(C)]
    struct Iovec {
        buf: *mut u8,
        buf_len: u32,
    }

    struct ShellEvents {
        preferences: PathWatch,
        launcher: FsWatch,
    }

    impl ShellEvents {
        fn new() -> Result<Self, i32> {
            Ok(Self {
                preferences: PathWatch::new(preferences::DEFAULT_PATH)?,
                launcher: FsWatch::new(
                    "/usr/share/applications",
                    abi::ext::WATCH_CREATE | abi::ext::WATCH_DELETE | abi::ext::WATCH_MODIFY,
                )?,
            })
        }

        fn drain_sigchld() -> bool {
            let mut saw_sigchld = false;
            for _ in 0..16 {
                let mut signals = [0u8; 32];
                let iov = Iovec {
                    buf: signals.as_mut_ptr(),
                    buf_len: signals.len() as u32,
                };
                let mut nread = 0u32;
                let errno = unsafe { fd_read(abi::fd::SIGNAL as i32, &iov, 1, &mut nread) };
                if errno == abi::errno::EAGAIN || (errno == 0 && nread == 0) {
                    break;
                }
                if errno != 0 {
                    break;
                }
                saw_sigchld |= signals[..nread as usize].chunks_exact(2).any(|record| {
                    u16::from_le_bytes([record[0], record[1]]) == abi::ext::sig::SIGCHLD
                });
            }
            saw_sigchld
        }
    }

    impl DesktopEventSource for ShellEvents {
        fn wait_fds(&self) -> Vec<WaitFd> {
            let mut fds = vec![WaitFd::readable(abi::fd::SIGNAL as i32)];
            fds.extend(self.preferences.wait_fds());
            fds.push(WaitFd::readable(self.launcher.fd()));
            fds
        }

        fn drain(&mut self) -> DesktopWake {
            let preferences = match self.preferences.drain() {
                Ok(changed) => changed,
                Err(errno) => {
                    return DesktopWake {
                        fatal_errno: Some(errno),
                        ..DesktopWake::default()
                    }
                }
            };
            let launcher = match self.launcher.drain() {
                Ok(changed) => changed,
                Err(errno) => {
                    return DesktopWake {
                        fatal_errno: Some(errno),
                        ..DesktopWake::default()
                    }
                }
            };
            DesktopWake {
                preferences,
                launcher,
                sigchld: Self::drain_sigchld(),
                fatal_errno: None,
            }
        }

        fn event_driven(&self) -> bool {
            true
        }

        fn catalog_published(&mut self, entry_count: usize) {
            println!("alt-shell: loaded {entry_count} applications from /usr/share/applications");
        }

        fn desktop_ready(&mut self) {
            println!("alt-shell: desktop ready");
        }
    }

    pub fn run() {
        println!("alt-shell: starting");
        let conn = match FdConnection::connect() {
            Ok(connection) => connection,
            Err(errno) => unsafe { proc_exit(errno) },
        };
        let events = match ShellEvents::new() {
            Ok(events) => events,
            Err(errno) => unsafe { proc_exit(errno) },
        };
        let taskbar = Taskbar::new(0, 0);
        let catalog =
            Launcher::new_stepwise(Box::new(FilesystemStore::new("/usr/share/applications")));
        println!("alt-shell: application catalog loading incrementally");
        println!("alt-shell: connected to /run/display");
        let launcher = LauncherRuntime::new(catalog, SystemLauncherClock::new());
        let preferences = DesktopPreferenceRuntime::new(
            AlternatePreferenceSource::new(preferences::DEFAULT_PATH),
            SystemPreferenceClock::new(),
        );
        match run_desktop_shell_with_runtimes_and_events(
            conn,
            u32::MAX,
            taskbar,
            launcher,
            shell_spawn,
            shell_reap_zombies,
            preferences,
            events,
        ) {
            Ok(_) => unsafe { proc_exit(0) },
            Err(_) => unsafe { proc_exit(1) },
        }
    }
}

#[cfg(target_arch = "wasm32")]
extern crate alloc;

fn main() {
    #[cfg(target_arch = "wasm32")]
    wasm_main::run();
    #[cfg(not(target_arch = "wasm32"))]
    println!("alt-shell (host build): use `cargo build --target wasm32-wasip1 -p alt-shell` for the production binary");
}

#[cfg(test)]
mod tests {
    use super::force_dark_preferences;

    #[test]
    fn dark_override_preserves_other_preferences() {
        let bytes = force_dark_preferences(Some(
            b"[theme]\nname = \"light\"\nfit = \"cover\"\n\n[wallpaper]\nname = \"green.png\"\n\n[timezone]\niana = \"Europe/London\"\n",
        ))
        .unwrap();
        let parsed = preferences::Preferences::parse(&bytes).unwrap();
        assert_eq!(parsed.theme_name.as_deref(), Some("dark"));
        assert_eq!(parsed.theme_fit.as_deref(), Some("cover"));
        assert_eq!(parsed.wallpaper_name.as_deref(), Some("green.png"));
        assert_eq!(parsed.timezone_iana.as_deref(), Some("Europe/London"));
    }

    #[test]
    fn dark_override_uses_safe_defaults_for_malformed_preferences() {
        let bytes = force_dark_preferences(Some(b"not preferences")).unwrap();
        let parsed = preferences::Preferences::parse(&bytes).unwrap();
        assert_eq!(parsed.theme_name.as_deref(), Some("dark"));
        assert_eq!(parsed.wallpaper_name, None);
        assert_eq!(parsed.timezone_iana, None);
    }
}
