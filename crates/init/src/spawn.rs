//! Spawn-manifest construction for PID 1.
//!
//! Init reads the preference file for every child launch, overlays the finite
//! dynamic environment whitelist on the static `[env]` table, then delegates
//! the actual v1 wire encoding to the command shell's canonical encoder.

use std::io;
use std::path::{Path, PathBuf};

use sh::{encode_spawn_manifest_v1, SpawnEncodeError, SpawnWireManifest};

/// Read seam used by PID 1 and host-target isolation tests.
pub trait PreferenceSource {
    /// `Ok(None)` means the canonical preference file does not exist.
    fn read(&mut self) -> io::Result<Option<Vec<u8>>>;
}

/// VFS-backed source used by the production init binaries.
pub struct FilesystemPreferenceSource {
    path: PathBuf,
}

impl FilesystemPreferenceSource {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    pub fn canonical() -> Self {
        Self::new(preferences::DEFAULT_PATH)
    }
}

impl PreferenceSource for FilesystemPreferenceSource {
    fn read(&mut self) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

/// Encode one child launch after reading the latest preference snapshot.
///
/// Read failures, a missing file, malformed syntax, and unsupported timezone
/// names all select UTC. Every non-environment manifest field is copied
/// exactly, including explicit capabilities and inherited stdio/cwd markers.
pub fn encode_with_spawn_timezone<S: PreferenceSource>(
    source: &mut S,
    manifest: &SpawnWireManifest<'_>,
) -> Result<Vec<u8>, SpawnEncodeError> {
    let preference_bytes = source.read().ok().flatten();
    let environment =
        preferences::spawn_environment_with_timezone(manifest.env, preference_bytes.as_deref());
    encode_spawn_manifest_v1(&SpawnWireManifest {
        path: manifest.path,
        argv: manifest.argv,
        env: &environment,
        stdin_fd: manifest.stdin_fd,
        stdout_fd: manifest.stdout_fd,
        stderr_fd: manifest.stderr_fd,
        extra_fds: manifest.extra_fds,
        cwd: manifest.cwd,
        caps: manifest.caps,
    })
}
