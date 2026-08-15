//! T097: Init unit tests — exercise the `/etc/init.conf` parser
//! and the shell-respawn rate limiter on the host target.
//!
//! Both tested surfaces live in `init`'s `lib.rs` siblings
//! (`conf.rs` + `respawn.rs`) so a `cargo test -p init` run
//! against the host triple compiles them straight into the
//! integration harness without dragging the `wasm32-wasip1`
//! `proc_spawn` extern in.

use init::conf::{InitConfig, ParseError};
use init::respawn::{RespawnDecision, RespawnLimiter, MIN_INTERVAL_NS};
use init::spawn::{encode_with_spawn_timezone, PreferenceSource};
use sh::SpawnWireManifest;
use std::collections::{BTreeMap, VecDeque};
use std::io;

#[test]
fn boot_child_grants_are_exact_and_never_all_capabilities() {
    use abi::cap::{initial, Cap};

    assert_eq!(init::grants::ORDINARY_APP, initial::ORDINARY_APP.0);
    assert_eq!(init::grants::DISPLAY_SERVER, initial::DISPLAY_SERVER.0);
    assert_eq!(init::grants::DESKTOP_SHELL, initial::DESKTOP_SHELL.0);
    assert_ne!(init::grants::ORDINARY_APP, u64::MAX);
    assert_ne!(init::grants::DISPLAY_SERVER, u64::MAX);
    assert_ne!(init::grants::DESKTOP_SHELL, u64::MAX);
    assert_eq!(
        init::grants::DISPLAY_SERVER & Cap::Shell.bit(),
        0,
        "display server must not inherit shell authority",
    );
    assert_eq!(
        init::grants::DESKTOP_SHELL & Cap::DisplayServer.bit(),
        0,
        "desktop shell must not open framebuffer/input devices",
    );
}

#[derive(Default)]
struct MemoryPreferenceSource {
    snapshots: VecDeque<io::Result<Option<Vec<u8>>>>,
    reads: usize,
}

impl PreferenceSource for MemoryPreferenceSource {
    fn read(&mut self) -> io::Result<Option<Vec<u8>>> {
        self.reads += 1;
        self.snapshots.pop_front().unwrap_or(Ok(None))
    }
}

#[derive(Debug, PartialEq, Eq)]
struct DecodedManifest {
    path: String,
    argv: Vec<String>,
    env: BTreeMap<String, String>,
    stdin_fd: i32,
    stdout_fd: i32,
    stderr_fd: i32,
    cwd: Option<String>,
    caps: u64,
    extra_fds: Vec<(u32, u32)>,
}

fn take_manifest_text(blob: &[u8], offset: &mut usize, length: usize) -> String {
    let text = String::from_utf8(blob[*offset..*offset + length].to_vec()).unwrap();
    *offset += length;
    text
}

fn decode_manifest(blob: &[u8]) -> DecodedManifest {
    let read_u16 = |offset| u16::from_le_bytes(blob[offset..offset + 2].try_into().unwrap());
    let read_i32 = |offset| i32::from_le_bytes(blob[offset..offset + 4].try_into().unwrap());
    let path_len = read_u16(12) as usize;
    let cwd_len = read_u16(14) as usize;
    let argc = read_u16(16) as usize;
    let envc = read_u16(18) as usize;
    let extra_count = read_u16(20) as usize;
    let mut offset = abi::ext::spawn_v1::HEADER_LEN;
    let path = take_manifest_text(blob, &mut offset, path_len);
    let cwd = (cwd_len != 0).then(|| take_manifest_text(blob, &mut offset, cwd_len));
    let mut argv = Vec::with_capacity(argc);
    for _ in 0..argc {
        let length = u16::from_le_bytes(blob[offset..offset + 2].try_into().unwrap()) as usize;
        offset += 2;
        argv.push(take_manifest_text(blob, &mut offset, length));
    }
    let mut env = BTreeMap::new();
    for _ in 0..envc {
        let key_len = u16::from_le_bytes(blob[offset..offset + 2].try_into().unwrap()) as usize;
        let value_len =
            u16::from_le_bytes(blob[offset + 2..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        let key = take_manifest_text(blob, &mut offset, key_len);
        let value = take_manifest_text(blob, &mut offset, value_len);
        env.insert(key, value);
    }
    let mut extra_fds = Vec::with_capacity(extra_count);
    for _ in 0..extra_count {
        let parent = u32::from_le_bytes(blob[offset..offset + 4].try_into().unwrap());
        let child = u32::from_le_bytes(blob[offset + 4..offset + 8].try_into().unwrap());
        offset += 8;
        extra_fds.push((parent, child));
    }
    assert_eq!(offset, blob.len());
    DecodedManifest {
        path,
        argv,
        env,
        stdin_fd: read_i32(24),
        stdout_fd: read_i32(28),
        stderr_fd: read_i32(32),
        cwd,
        caps: u64::from_le_bytes(blob[40..48].try_into().unwrap()),
        extra_fds,
    }
}

fn manifest<'a>(argv: &'a [String], env: &'a [(String, String)]) -> SpawnWireManifest<'a> {
    SpawnWireManifest {
        path: "/bin/example",
        argv,
        env,
        stdin_fd: Some(7),
        stdout_fd: Some(8),
        stderr_fd: Some(9),
        extra_fds: &[(11, 5)],
        cwd: Some("/home/user"),
        caps: Some(0x1234),
    }
}

#[test]
fn init_spawn_applies_valid_timezone_and_preserves_the_manifest() {
    let mut source = MemoryPreferenceSource {
        snapshots: [Ok(Some(
            b"[timezone]\niana = \"America/New_York\"\n".to_vec(),
        ))]
        .into(),
        reads: 0,
    };
    let argv = vec!["example".to_string(), "--flag".to_string()];
    let env = vec![
        ("PATH".to_string(), "/bin:/usr/bin".to_string()),
        ("TZ".to_string(), "Europe/London".to_string()),
    ];
    let decoded =
        decode_manifest(&encode_with_spawn_timezone(&mut source, &manifest(&argv, &env)).unwrap());
    assert_eq!(source.reads, 1);
    assert_eq!(decoded.path, "/bin/example");
    assert_eq!(decoded.argv, argv);
    assert_eq!(
        decoded.env.get("PATH").map(String::as_str),
        Some("/bin:/usr/bin")
    );
    assert_eq!(
        decoded.env.get("TZ").map(String::as_str),
        Some("America/New_York")
    );
    assert_eq!(decoded.stdin_fd, 7);
    assert_eq!(decoded.stdout_fd, 8);
    assert_eq!(decoded.stderr_fd, 9);
    assert_eq!(decoded.cwd.as_deref(), Some("/home/user"));
    assert_eq!(decoded.caps, 0x1234);
    assert_eq!(decoded.extra_fds, vec![(11, 5)]);
}

#[test]
fn init_spawn_defaults_missing_malformed_unsupported_and_read_errors_to_utc() {
    let mut source = MemoryPreferenceSource {
        snapshots: [
            Ok(None),
            Ok(Some(b"malformed".to_vec())),
            Ok(Some(b"[timezone]\niana = \"Australia/Sydney\"\n".to_vec())),
            Err(io::Error::other("transient read failure")),
        ]
        .into(),
        reads: 0,
    };
    let argv = Vec::new();
    let env = Vec::new();
    for _ in 0..4 {
        let decoded = decode_manifest(
            &encode_with_spawn_timezone(&mut source, &manifest(&argv, &env)).unwrap(),
        );
        assert_eq!(decoded.env.get("TZ").map(String::as_str), Some("UTC"));
    }
    assert_eq!(source.reads, 4);
}

#[test]
fn successive_init_spawns_read_changed_timezone_without_mutating_prior_manifest() {
    let mut source = MemoryPreferenceSource {
        snapshots: [
            Ok(Some(b"[timezone]\niana = \"Europe/London\"\n".to_vec())),
            Ok(Some(b"[timezone]\niana = \"Asia/Tokyo\"\n".to_vec())),
        ]
        .into(),
        reads: 0,
    };
    let argv = Vec::new();
    let env = Vec::new();
    let first =
        decode_manifest(&encode_with_spawn_timezone(&mut source, &manifest(&argv, &env)).unwrap());
    let second =
        decode_manifest(&encode_with_spawn_timezone(&mut source, &manifest(&argv, &env)).unwrap());
    assert_eq!(
        first.env.get("TZ").map(String::as_str),
        Some("Europe/London")
    );
    assert_eq!(second.env.get("TZ").map(String::as_str), Some("Asia/Tokyo"));
    assert_eq!(
        first.env.get("TZ").map(String::as_str),
        Some("Europe/London")
    );
    assert_eq!(source.reads, 2);
}

// ---- parser default-fallback path -----------------------------------

#[test]
fn parser_falls_back_to_defaults_on_missing_file() {
    // Caller-side "missing file" simulation: the contract says
    // "if missing or unparseable, init uses the built-in
    // defaults". Init's actual flow is `read_to_string` + `parse`
    // wrapped in a fall-back match; this test pins the
    // built-in-defaults shape so the fall-back outcome is
    // deterministic.
    let cfg = InitConfig::builtin_defaults();
    assert_eq!(cfg.boot.shell, "/usr/bin/shell");
    assert_eq!(cfg.boot.display_server, "/usr/bin/display-server");
    assert!(cfg.boot.autostart.is_empty());
}

#[test]
fn parser_falls_back_to_defaults_on_unparseable_content() {
    let result = InitConfig::parse("this is not valid TOML\n[ broken");
    assert!(result.is_err());
    let err: ParseError = result.unwrap_err();
    assert_eq!(err.line, 1);
}

#[test]
fn parser_handles_documented_default_example_round_trip() {
    let example = r#"[boot]
display_server = "/usr/bin/display-server"
shell          = "/usr/bin/shell"
autostart      = []

[capabilities.shell]
grant = ["DISPLAY_CLIENT", "SHELL", "PROC_ENUMERATE", "PROC_KILL_ANY", "KEYMAP_ADMIN", "HOST_TRANSFER"]

[env]
PATH       = "/bin:/usr/bin"
HOME       = "/home/user"
USER       = "user"
"#;
    let cfg = InitConfig::parse(example).unwrap();
    assert_eq!(cfg.boot.display_server, "/usr/bin/display-server");
    assert_eq!(cfg.boot.shell, "/usr/bin/shell");
    assert_eq!(
        cfg.capabilities.get("shell").unwrap(),
        &vec![
            "DISPLAY_CLIENT".to_string(),
            "SHELL".to_string(),
            "PROC_ENUMERATE".to_string(),
            "PROC_KILL_ANY".to_string(),
            "KEYMAP_ADMIN".to_string(),
            "HOST_TRANSFER".to_string(),
        ]
    );
    assert_eq!(cfg.env.get("HOME").unwrap(), "/home/user");
    assert_eq!(cfg.env.get("PATH").unwrap(), "/bin:/usr/bin");
}

// ---- shell respawn cap ---------------------------------------------

#[test]
fn respawn_cap_grants_first_spawn_immediately() {
    let r = RespawnLimiter::new();
    assert_eq!(r.should_respawn(0), RespawnDecision::Spawn);
}

#[test]
fn respawn_cap_blocks_within_one_second_window() {
    let mut r = RespawnLimiter::new();
    r.record_spawn(0);
    // Halfway through the window — should be blocked, with half
    // the interval remaining as the wait_ns.
    assert_eq!(
        r.should_respawn(500_000_000),
        RespawnDecision::Wait {
            wait_ns: 500_000_000
        }
    );
}

#[test]
fn respawn_cap_grants_after_full_interval() {
    let mut r = RespawnLimiter::new();
    r.record_spawn(0);
    assert_eq!(r.should_respawn(MIN_INTERVAL_NS), RespawnDecision::Spawn);
}

#[test]
fn respawn_cap_simulated_crash_loop_caps_at_one_per_second() {
    // The contract's "1/sec spawn churn" cap. Simulate a shell
    // that crashes the moment it starts — init keeps trying to
    // respawn, and the limiter must let exactly one spawn through
    // per second of wall clock.
    let mut r = RespawnLimiter::new();
    let mut spawns = 0;
    // 5 seconds of attempts, every microsecond.
    for tick in 0..(5 * 1_000_000u64) {
        let now = tick * 1_000;
        if r.should_respawn(now) == RespawnDecision::Spawn {
            spawns += 1;
            r.record_spawn(now);
        }
    }
    // 5 seconds → 5 spawns (first at t=0, then one each second).
    assert_eq!(spawns, 5);
}

#[test]
fn respawn_cap_handles_clock_going_backwards_safely() {
    let mut r = RespawnLimiter::new();
    r.record_spawn(2_000_000_000);
    // A non-monotonic clock that moves backward should not
    // surprise the limiter into thinking a full interval has
    // passed.
    assert!(matches!(r.should_respawn(0), RespawnDecision::Wait { .. }));
}
