//! PID 1 — desktop boot variant.
//!
//! T127 boot-to-desktop entry point. Where the regular
//! `init` binary spawns the demo flow (hello-std + display-
//! server + two demo clients, then SIGTERMs the server when
//! the demo clients close), this variant boots a real
//! desktop:
//!
//!   1. Parse `/etc/init.conf` (or use safe built-in defaults).
//!   2. Spawn the configured display server, shell, and autostart entries with
//!      the exact named capability grants.
//!   3. Reap every child. A shell exit triggers a rate-limited re-read and
//!      respawn, which is the live alternative-shell layering boundary.
//!
//! Selected via `#boot-to-desktop` URL hash (handled by
//! `web/src/bootstrap.ts`); the hash maps to `bootBinary =
//! "/bin/init-desktop"`. The Playwright spec in
//! `web/tests/integration/boot-to-desktop.spec.ts` asserts
//! the expected boot sequence (init-desktop spawns →
//! display-server starts → shell connects + draws wallpaper).

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "pmos_ext")]
extern "C" {
    fn proc_spawn_manifest(manifest_ptr: *const u8, manifest_len: u32) -> i32;
    fn proc_wait(target_pid: i32, options: i32, status_out_ptr: i32) -> i32;
}

#[cfg(not(target_arch = "wasm32"))]
unsafe fn proc_spawn_manifest(_manifest_ptr: *const u8, _manifest_len: u32) -> i32 {
    0
}
#[cfg(not(target_arch = "wasm32"))]
unsafe fn proc_wait(_target_pid: i32, _options: i32, _status_out_ptr: i32) -> i32 {
    0
}

const WAIT_ANY: i32 = -1;
const ECHILD: i32 = 9;
const EINVAL: i32 = 28;
const SHELL_RESPAWN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

fn spawn_child(
    preferences: &mut init::spawn::FilesystemPreferenceSource,
    static_env: &[(String, String)],
    path: &str,
    caps: u64,
) -> i32 {
    let argv = Vec::new();
    let manifest = sh::SpawnWireManifest {
        path,
        argv: &argv,
        env: static_env,
        stdin_fd: None,
        stdout_fd: None,
        stderr_fd: None,
        extra_fds: &[],
        cwd: None,
        caps: Some(caps),
    };
    match init::spawn::encode_with_spawn_timezone(preferences, &manifest) {
        Ok(blob) => unsafe { proc_spawn_manifest(blob.as_ptr(), blob.len() as u32) },
        Err(_) => -EINVAL,
    }
}

fn read_config() -> init::conf::InitConfig {
    match std::fs::read_to_string("/etc/init.conf") {
        Ok(text) => match init::conf::InitConfig::parse(&text) {
            Ok(config) => config,
            Err(error) => {
                println!("init-desktop: {error}; using built-in defaults");
                init::conf::InitConfig::builtin_defaults()
            }
        },
        Err(error) => {
            println!(
                "init-desktop: failed to read /etc/init.conf: {error}; using built-in defaults"
            );
            init::conf::InitConfig::builtin_defaults()
        }
    }
}

fn configured_caps(config: &init::conf::InitConfig, role: &str, ceiling: u64) -> u64 {
    let (bits, unknown, denied) = config.bounded_capability_bits(role, ceiling);
    for name in unknown {
        println!("init-desktop: ignoring unknown capability {name} for {role}");
    }
    for name in denied {
        println!("init-desktop: refusing capability {name} above the {role} role ceiling");
    }
    bits
}

fn environment(config: &init::conf::InitConfig) -> Vec<(String, String)> {
    config
        .env
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn autostart_path(entry: &str) -> String {
    if entry.starts_with('/') {
        entry.to_string()
    } else {
        format!("/bin/{entry}")
    }
}

fn capability_role(path: &str) -> &str {
    path.rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
}

fn autostart_capability_ceiling(path: &str) -> u64 {
    match path {
        "/bin/files" | "/usr/bin/files" => abi::cap::initial::FILES.0,
        "/bin/settings" | "/usr/bin/settings" => abi::cap::initial::SETTINGS.0,
        "/bin/sysmon" | "/usr/bin/sysmon" => abi::cap::initial::SYSMON.0,
        _ => abi::cap::initial::ORDINARY_APP.0,
    }
}

fn spawn_shell(
    preferences: &mut init::spawn::FilesystemPreferenceSource,
    config: &init::conf::InitConfig,
) -> i32 {
    let environment = environment(config);
    spawn_child(
        preferences,
        &environment,
        &config.boot.shell,
        configured_caps(config, "shell", abi::cap::initial::DESKTOP_SHELL.0),
    )
}

fn main() {
    println!("init-desktop starting");

    let config = read_config();
    let static_env = environment(&config);
    let mut preferences = init::spawn::FilesystemPreferenceSource::canonical();

    let ds_rc = spawn_child(
        &mut preferences,
        &static_env,
        &config.boot.display_server,
        configured_caps(
            &config,
            "display-server",
            abi::cap::initial::DISPLAY_SERVER.0,
        ),
    );
    if ds_rc < 0 {
        println!(
            "init-desktop: proc_spawn {} failed errno={}",
            config.boot.display_server, -ds_rc
        );
        std::process::exit(1);
    }
    println!("init-desktop spawned display-server pid={}", ds_rc);
    println!(
        "init-desktop display-server path={} pid={ds_rc}",
        config.boot.display_server
    );

    let mut sh_rc = spawn_shell(&mut preferences, &config);
    if sh_rc < 0 {
        println!(
            "init-desktop: proc_spawn {} failed errno={}",
            config.boot.shell, -sh_rc
        );
        std::process::exit(2);
    }
    println!("init-desktop spawned shell pid={}", sh_rc);
    println!("init-desktop shell path={} pid={sh_rc}", config.boot.shell);

    for entry in &config.boot.autostart {
        let path = autostart_path(entry);
        let role = capability_role(&path);
        let rc = spawn_child(
            &mut preferences,
            &static_env,
            &path,
            configured_caps(&config, role, autostart_capability_ceiling(&path)),
        );
        if rc < 0 {
            println!("init-desktop: autostart {path} failed errno={}", -rc);
        } else {
            println!("init-desktop autostart path={path} pid={rc}");
        }
    }

    println!("init-desktop entering supervision loop");

    let mut status_out: i64 = 0;
    let mut last_shell_spawn = std::time::Instant::now();
    loop {
        let status_ptr = &mut status_out as *mut i64 as i32;
        let rc = unsafe { proc_wait(WAIT_ANY, 0, status_ptr) };
        if rc < 0 {
            if rc == -ECHILD {
                // No more children to wait on — every spawned
                // process has been reaped. Under v1 desktop
                // boot this means both display-server and shell
                // have exited; print a final marker and fall
                // through.
                println!("init-desktop: no more children, exiting");
                break;
            }
            println!("init-desktop: proc_wait errno={}", -rc);
            break;
        }
        println!("init-desktop reaped child pid={} status={status_out}", rc);
        if rc != sh_rc {
            continue;
        }

        println!("init-desktop shell exited pid={sh_rc}; scheduling respawn");
        loop {
            let elapsed = last_shell_spawn.elapsed();
            if elapsed < SHELL_RESPAWN_INTERVAL {
                std::thread::sleep(SHELL_RESPAWN_INTERVAL - elapsed);
            }
            let next_config = read_config();
            let next_path = next_config.boot.shell.clone();
            let next_pid = spawn_shell(&mut preferences, &next_config);
            last_shell_spawn = std::time::Instant::now();
            if next_pid < 0 {
                println!(
                    "init-desktop: respawn {next_path} failed errno={}; retrying",
                    -next_pid
                );
                continue;
            }
            sh_rc = next_pid;
            println!("init-desktop respawned shell path={next_path} pid={sh_rc}");
            break;
        }
    }

    println!("init-desktop exiting");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_autostart_is_capped_as_an_ordinary_app() {
        let config = init::conf::InitConfig::parse(
            "[capabilities.evil]\ngrant = [\"DISPLAY_CLIENT\", \"CAP_GRANT\", \"DISPLAY_SERVER\"]\n",
        )
        .unwrap();
        let path = "/opt/evil/bin/evil";
        assert_eq!(
            configured_caps(
                &config,
                capability_role(path),
                autostart_capability_ceiling(path),
            ),
            abi::cap::initial::ORDINARY_APP.0
        );
    }

    #[test]
    fn only_exact_bundled_apps_receive_privileged_autostart_ceilings() {
        assert_eq!(
            autostart_capability_ceiling("/bin/sysmon"),
            abi::cap::initial::SYSMON.0
        );
        assert_eq!(
            autostart_capability_ceiling("/opt/sysmon"),
            abi::cap::initial::ORDINARY_APP.0
        );
        assert_eq!(
            autostart_capability_ceiling("/bin/settings"),
            abi::cap::initial::SETTINGS.0
        );
        assert_eq!(
            autostart_capability_ceiling("/bin/files"),
            abi::cap::initial::FILES.0
        );
    }
}
