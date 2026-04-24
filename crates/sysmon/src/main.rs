//! `/usr/bin/sysmon` — CLI snapshot of live processes (T170 partial).
//!
//! The tabbed graphical sysmon (T170..) is blocked on the toolkit
//! `Container` widget + full frame-callback runtime, so this slice
//! ships a debugging CLI that walks `/proc`, reads each
//! `/proc/<pid>/status` file produced by the procfs source landed
//! in T168, and prints a fixed-width process table. Same
//! CLI-first / GUI-deferred pattern as the T184 settings slice.
//! `--proc-root <dir>` overrides the default `/proc` so tests can
//! drive the binary with a temp tree. Names longer than 16 chars
//! are truncated with a trailing `…`. Malformed status files are
//! skipped with a stderr warning so one broken pid cannot hide the
//! rest of the table.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const NAME_COL: usize = 16;

fn main() -> ExitCode {
    let proc_root = match parse_args() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("sysmon: {}", e);
            return ExitCode::from(1);
        }
    };

    let entries = match fs::read_dir(&proc_root) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("sysmon: failed to open {}: {}", proc_root.display(), e);
            return ExitCode::from(1);
        }
    };

    let mut pids: Vec<u32> = Vec::new();
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            if let Ok(pid) = name.parse::<u32>() {
                pids.push(pid);
            }
        }
    }
    pids.sort_unstable();

    println!("{:<7} {:<16}  {:<11} {}", "PID", "NAME", "STATE", "PPID");
    println!("{:<7} {:<16}  {:<11} {}", "-----", "----------------", "----------", "-----");

    for pid in pids {
        let status_path = proc_root.join(pid.to_string()).join("status");
        match read_status(&status_path) {
            Ok(snap) => {
                let name = truncate_name(&snap.name);
                println!("{:<7} {:<16}  {:<11} {}", pid, name, snap.state, snap.ppid);
            }
            Err(reason) => {
                eprintln!("sysmon: pid {}: failed to parse status: {}", pid, reason);
            }
        }
    }

    ExitCode::from(0)
}

fn parse_args() -> Result<PathBuf, String> {
    let mut args = std::env::args().skip(1);
    let mut proc_root: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--proc-root" => {
                let val = args
                    .next()
                    .ok_or_else(|| String::from("--proc-root requires a value"))?;
                proc_root = Some(PathBuf::from(val));
            }
            other => {
                return Err(format!("unrecognised argument: {}", other));
            }
        }
    }
    Ok(proc_root.unwrap_or_else(|| PathBuf::from("/proc")))
}

struct StatusSnapshot {
    name: String,
    state: String,
    ppid: u32,
}

fn read_status(path: &Path) -> Result<StatusSnapshot, String> {
    let bytes = fs::read(path).map_err(|e| format!("io: {}", e))?;
    let text = std::str::from_utf8(&bytes).map_err(|_| String::from("not utf-8"))?;

    let mut name: Option<String> = None;
    let mut state: Option<String> = None;
    let mut pid: Option<u32> = None;
    let mut ppid: Option<u32> = None;

    for line in text.lines() {
        if let Some(v) = line.strip_prefix("Name:\t") {
            name = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("State:\t") {
            state = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("Pid:\t") {
            pid = Some(v.parse::<u32>().map_err(|_| String::from("bad Pid"))?);
        } else if let Some(v) = line.strip_prefix("PPid:\t") {
            ppid = Some(v.parse::<u32>().map_err(|_| String::from("bad PPid"))?);
        }
    }

    let name = name.ok_or_else(|| String::from("missing Name"))?;
    let state = state.ok_or_else(|| String::from("missing State"))?;
    let _ = pid.ok_or_else(|| String::from("missing Pid"))?;
    let ppid = ppid.ok_or_else(|| String::from("missing PPid"))?;

    Ok(StatusSnapshot { name, state, ppid })
}

fn truncate_name(name: &str) -> String {
    if name.chars().count() <= NAME_COL {
        return name.to_string();
    }
    let mut out: String = name.chars().take(NAME_COL - 1).collect();
    out.push('…');
    out
}
