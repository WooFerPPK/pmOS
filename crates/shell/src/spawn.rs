//! Desktop-launcher spawn manifest construction.

use crate::desktop_preferences::PreferenceSource;
use sh::{encode_spawn_manifest_v1, SpawnEncodeError, SpawnWireManifest};

/// Read the latest preference snapshot, apply its validated timezone to the
/// child environment, and encode the canonical v1 spawn manifest.
///
/// Missing, malformed, unsupported, or temporarily unreadable preferences
/// select UTC. All non-environment manifest fields are copied exactly.
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
