//! Canonical userland encoder for `abi::ext::spawn_v1`.

use std::collections::BTreeSet;

use abi::ext::spawn_v1 as wire;

/// Owned inputs for one packed spawn manifest.
pub struct SpawnWireManifest<'a> {
    pub path: &'a str,
    pub argv: &'a [String],
    pub env: &'a [(String, String)],
    pub stdin_fd: Option<u32>,
    pub stdout_fd: Option<u32>,
    pub stderr_fd: Option<u32>,
    pub extra_fds: &'a [(u32, u32)],
    pub cwd: Option<&'a str>,
    pub caps: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnEncodeError {
    InvalidPath,
    InvalidCwd,
    InvalidString,
    InvalidFd,
    DuplicateChildFd,
    TooManyEntries,
    TooLarge,
}

/// Encode one full manifest using the byte layout shared by the kernel and
/// TypeScript runtime.
pub fn encode_spawn_manifest_v1(
    manifest: &SpawnWireManifest<'_>,
) -> Result<Vec<u8>, SpawnEncodeError> {
    validate_text(manifest.path, false)?;
    if !manifest.path.starts_with('/') {
        return Err(SpawnEncodeError::InvalidPath);
    }
    if let Some(cwd) = manifest.cwd {
        validate_text(cwd, false)?;
        if !cwd.starts_with('/') {
            return Err(SpawnEncodeError::InvalidCwd);
        }
    }
    validate_fd(manifest.stdin_fd)?;
    validate_fd(manifest.stdout_fd)?;
    validate_fd(manifest.stderr_fd)?;
    if manifest.argv.len() > u16::MAX as usize
        || manifest.env.len() > u16::MAX as usize
        || manifest.extra_fds.len() > u16::MAX as usize
    {
        return Err(SpawnEncodeError::TooManyEntries);
    }
    for arg in manifest.argv {
        validate_text(arg, true)?;
    }
    let mut env_keys = BTreeSet::new();
    for (key, value) in manifest.env {
        validate_text(key, false)?;
        validate_text(value, true)?;
        if key.contains('=') || !env_keys.insert(key) {
            return Err(SpawnEncodeError::InvalidString);
        }
    }
    let mut child_fds = BTreeSet::new();
    for (parent_fd, child_fd) in manifest.extra_fds {
        validate_fd(Some(*parent_fd))?;
        validate_fd(Some(*child_fd))?;
        if *child_fd < abi::fd::FIRST_DYNAMIC || *child_fd >= 1024 {
            return Err(SpawnEncodeError::InvalidFd);
        }
        if !child_fds.insert(*child_fd) {
            return Err(SpawnEncodeError::DuplicateChildFd);
        }
    }

    let mut total_len = wire::HEADER_LEN;
    total_len = checked_add(total_len, manifest.path.len())?;
    total_len = checked_add(total_len, manifest.cwd.map(str::len).unwrap_or(0))?;
    for arg in manifest.argv {
        total_len = checked_add(total_len, 2)?;
        total_len = checked_add(total_len, arg.len())?;
    }
    for (key, value) in manifest.env {
        total_len = checked_add(total_len, 4)?;
        total_len = checked_add(total_len, key.len())?;
        total_len = checked_add(total_len, value.len())?;
    }
    total_len = checked_add(
        total_len,
        manifest
            .extra_fds
            .len()
            .checked_mul(8)
            .ok_or(SpawnEncodeError::TooLarge)?,
    )?;
    if total_len > abi::ring::HEAP_SCRATCH_BYTES || total_len > u32::MAX as usize {
        return Err(SpawnEncodeError::TooLarge);
    }

    let mut out = vec![0u8; total_len];
    let flags = if manifest.cwd.is_some() {
        wire::FLAG_CWD
    } else {
        0
    } | if manifest.caps.is_some() {
        wire::FLAG_CAPS
    } else {
        0
    };
    write_u32(&mut out, wire::OFF_MAGIC, wire::MAGIC);
    write_u16(&mut out, wire::OFF_VERSION, wire::VERSION);
    write_u16(&mut out, wire::OFF_FLAGS, flags);
    write_u32(&mut out, wire::OFF_TOTAL_LEN, total_len as u32);
    write_u16(&mut out, wire::OFF_PATH_LEN, manifest.path.len() as u16);
    write_u16(
        &mut out,
        wire::OFF_CWD_LEN,
        manifest.cwd.map(str::len).unwrap_or(0) as u16,
    );
    write_u16(&mut out, wire::OFF_ARGC, manifest.argv.len() as u16);
    write_u16(&mut out, wire::OFF_ENVC, manifest.env.len() as u16);
    write_u16(
        &mut out,
        wire::OFF_EXTRA_FD_COUNT,
        manifest.extra_fds.len() as u16,
    );
    write_i32(
        &mut out,
        wire::OFF_STDIN_FD,
        manifest
            .stdin_fd
            .map(|fd| fd as i32)
            .unwrap_or(wire::INHERIT_FD),
    );
    write_i32(
        &mut out,
        wire::OFF_STDOUT_FD,
        manifest
            .stdout_fd
            .map(|fd| fd as i32)
            .unwrap_or(wire::INHERIT_FD),
    );
    write_i32(
        &mut out,
        wire::OFF_STDERR_FD,
        manifest
            .stderr_fd
            .map(|fd| fd as i32)
            .unwrap_or(wire::INHERIT_FD),
    );
    write_u64(&mut out, wire::OFF_CAPS, manifest.caps.unwrap_or(0));

    let mut offset = wire::HEADER_LEN;
    put(&mut out, &mut offset, manifest.path.as_bytes());
    if let Some(cwd) = manifest.cwd {
        put(&mut out, &mut offset, cwd.as_bytes());
    }
    for arg in manifest.argv {
        put_u16(&mut out, &mut offset, arg.len() as u16);
        put(&mut out, &mut offset, arg.as_bytes());
    }
    for (key, value) in manifest.env {
        put_u16(&mut out, &mut offset, key.len() as u16);
        put_u16(&mut out, &mut offset, value.len() as u16);
        put(&mut out, &mut offset, key.as_bytes());
        put(&mut out, &mut offset, value.as_bytes());
    }
    for (parent_fd, child_fd) in manifest.extra_fds {
        put_u32(&mut out, &mut offset, *parent_fd);
        put_u32(&mut out, &mut offset, *child_fd);
    }
    debug_assert_eq!(offset, total_len);
    Ok(out)
}

fn validate_text(value: &str, allow_empty: bool) -> Result<(), SpawnEncodeError> {
    if (!allow_empty && value.is_empty()) || value.contains('\0') || value.len() > u16::MAX as usize
    {
        Err(SpawnEncodeError::InvalidString)
    } else {
        Ok(())
    }
}

fn validate_fd(fd: Option<u32>) -> Result<(), SpawnEncodeError> {
    if fd.is_some_and(|value| value > i32::MAX as u32) {
        Err(SpawnEncodeError::InvalidFd)
    } else {
        Ok(())
    }
}

fn checked_add(lhs: usize, rhs: usize) -> Result<usize, SpawnEncodeError> {
    lhs.checked_add(rhs).ok_or(SpawnEncodeError::TooLarge)
}

fn write_u16(out: &mut [u8], offset: usize, value: u16) {
    out[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(out: &mut [u8], offset: usize, value: u32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_i32(out: &mut [u8], offset: usize, value: i32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(out: &mut [u8], offset: usize, value: u64) {
    out[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn put(out: &mut [u8], offset: &mut usize, bytes: &[u8]) {
    out[*offset..*offset + bytes.len()].copy_from_slice(bytes);
    *offset += bytes.len();
}

fn put_u16(out: &mut [u8], offset: &mut usize, value: u16) {
    put(out, offset, &value.to_le_bytes());
}

fn put_u32(out: &mut [u8], offset: &mut usize, value: u32) {
    put(out, offset, &value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::{encode_spawn_manifest_v1, SpawnWireManifest};

    #[test]
    fn full_manifest_matches_canonical_offsets() {
        let argv = vec!["grep".to_string(), "two words".to_string()];
        let env = vec![("PATH".to_string(), "/bin".to_string())];
        let blob = encode_spawn_manifest_v1(&SpawnWireManifest {
            path: "/bin/grep",
            argv: &argv,
            env: &env,
            stdin_fd: Some(8),
            stdout_fd: Some(9),
            stderr_fd: None,
            extra_fds: &[(11, 7)],
            cwd: Some("/home/user"),
            caps: Some(0x1234),
        })
        .unwrap();
        assert_eq!(
            u32::from_le_bytes(blob[0..4].try_into().unwrap()),
            abi::ext::spawn_v1::MAGIC
        );
        assert_eq!(u16::from_le_bytes(blob[6..8].try_into().unwrap()), 3);
        assert_eq!(i32::from_le_bytes(blob[24..28].try_into().unwrap()), 8);
        assert_eq!(i32::from_le_bytes(blob[28..32].try_into().unwrap()), 9);
        assert_eq!(i32::from_le_bytes(blob[32..36].try_into().unwrap()), -1);
        assert_eq!(u64::from_le_bytes(blob[40..48].try_into().unwrap()), 0x1234);
    }
}
