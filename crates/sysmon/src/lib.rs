//! `sysmon` library — `/proc` reader shared between the GUI
//! binary, the CLI binary, and the isolation tests (T173).

use std::fs;
use std::path::Path;

const NAME_COL: usize = 16;

pub struct StatusSnapshot {
    pub name: String,
    pub state: String,
    pub ppid: u32,
}

/// Collect a list of formatted `PID NAME STATE PPID` rows from
/// the given proc-root.
pub fn collect_snapshot(proc_root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(proc_root) {
        Ok(e) => e,
        Err(_) => return out,
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
    for pid in pids {
        let status_path = proc_root.join(pid.to_string()).join("status");
        if let Ok(snap) = read_status(&status_path) {
            let name = truncate_name(&snap.name);
            out.push(format!(
                "{:<7}{:<16}  {:<11} {}",
                pid, name, snap.state, snap.ppid
            ));
        }
    }
    out
}

pub fn read_status(path: &Path) -> Result<StatusSnapshot, String> {
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

pub fn truncate_name(name: &str) -> String {
    if name.chars().count() <= NAME_COL {
        return name.to_string();
    }
    let mut out: String = name.chars().take(NAME_COL - 1).collect();
    out.push('…');
    out
}
