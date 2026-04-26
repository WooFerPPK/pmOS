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
grant = ["DISPLAY_CLIENT", "SHELL", "PROC_ENUMERATE", "KEYMAP_ADMIN"]

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
            "KEYMAP_ADMIN".to_string(),
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
    assert!(matches!(
        r.should_respawn(0),
        RespawnDecision::Wait { .. }
    ));
}
