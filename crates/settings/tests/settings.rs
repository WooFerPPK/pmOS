//! T194 — settings roll-up isolation tests.
//!
//! Wraps the four set-* CLI subcommands in a single round-trip
//! check: write theme + wallpaper + keyboard + timezone via the
//! settings binary, then re-read /etc/preferences.toml and assert
//! every field landed. Detailed coverage lives in tests/cli.rs;
//! this file is the cross-cutting smoke test cited in T194.

use std::fs;
use std::process::Command;

fn settings_bin() -> std::path::PathBuf {
    // Find the settings binary built by `cargo test -p settings`.
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .unwrap()
                .join("target")
        });
    target_dir.join("debug").join("settings")
}

fn tempdir(prefix: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pmos-{}-{}-{}",
        prefix,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn settings_round_trip_via_set_subcommands() {
    let bin = settings_bin();
    if !bin.exists() {
        eprintln!("skipping: build settings binary first ({})", bin.display());
        return;
    }
    let tmp = tempdir("settings-rollup");
    let cfg = tmp.join("preferences.toml");

    let runs = [
        ("set-theme", "dark"),
        ("set-wallpaper", "mountains.png"),
        ("set-keyboard", "us-qwerty"),
        ("set-timezone", "America/New_York"),
    ];
    for (sub, value) in runs {
        let out = Command::new(&bin)
            .args([sub, value, "--config", cfg.to_str().unwrap()])
            .output()
            .expect("run settings");
        assert!(
            out.status.success(),
            "{} {}: stderr={}",
            sub,
            value,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let body = fs::read_to_string(&cfg).unwrap();
    assert!(body.contains(r#"name = "dark""#));
    assert!(body.contains(r#"name = "mountains.png""#));
    assert!(body.contains(r#"layout = "us-qwerty""#));
    assert!(body.contains(r#"iana = "America/New_York""#));

    fs::remove_dir_all(&tmp).unwrap();
}
