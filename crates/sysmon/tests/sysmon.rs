//! T173 — sysmon isolation tests against a synthetic /proc.

use std::fs;
use std::path::PathBuf;

#[test]
fn collect_snapshot_lists_each_pid_with_status() {
    let tmp = tempdir("sysmon-collect");
    write_pid(&tmp, 1, "init", "R", 0);
    write_pid(&tmp, 5, "shell", "S", 1);
    write_pid(&tmp, 9, "edit", "S", 1);

    let rows = sysmon::collect_snapshot(&tmp);
    assert_eq!(rows.len(), 3);
    // Sorted ascending by pid.
    assert!(rows[0].starts_with("1      init"));
    assert!(rows[1].starts_with("5      shell"));
    assert!(rows[2].starts_with("9      edit"));

    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn collect_snapshot_skips_malformed_pid_dirs() {
    let tmp = tempdir("sysmon-malformed");
    fs::create_dir(tmp.join("not-a-pid")).unwrap();
    write_pid(&tmp, 7, "good", "R", 1);
    let rows = sysmon::collect_snapshot(&tmp);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].starts_with("7      good"));
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn collect_snapshot_truncates_long_names() {
    let tmp = tempdir("sysmon-trunc");
    write_pid(&tmp, 3, "a-name-longer-than-sixteen-chars", "R", 1);
    let rows = sysmon::collect_snapshot(&tmp);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].contains("…"));
    fs::remove_dir_all(&tmp).unwrap();
}

fn tempdir(prefix: &str) -> PathBuf {
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

fn write_pid(root: &std::path::Path, pid: u32, name: &str, state: &str, ppid: u32) {
    let dir = root.join(pid.to_string());
    fs::create_dir(&dir).unwrap();
    let body = format!(
        "Name:\t{}\nState:\t{}\nPid:\t{}\nPPid:\t{}\n",
        name, state, pid, ppid
    );
    fs::write(dir.join("status"), body).unwrap();
}
