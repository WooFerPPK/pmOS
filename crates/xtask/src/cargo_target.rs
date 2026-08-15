use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) fn resolve(workspace_root: &Path) -> io::Result<PathBuf> {
    let current_dir = std::env::current_dir()?;
    resolve_from(
        workspace_root,
        &current_dir,
        std::env::var_os("CARGO_TARGET_DIR").as_deref(),
    )
}

fn resolve_from(
    workspace_root: &Path,
    current_dir: &Path,
    configured: Option<&OsStr>,
) -> io::Result<PathBuf> {
    let Some(configured) = configured else {
        return Ok(workspace_root.join("target"));
    };
    if configured.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "CARGO_TARGET_DIR is set to an empty string",
        ));
    }

    let configured = PathBuf::from(configured);
    if configured.is_absolute() {
        Ok(configured)
    } else {
        Ok(current_dir.join(configured))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_target_directory_uses_workspace_default() {
        assert_eq!(
            resolve_from(Path::new("/workspace"), Path::new("/work/cwd"), None)
                .expect("resolve default target directory"),
            Path::new("/workspace/target")
        );
    }

    #[test]
    fn relative_target_directory_is_relative_to_process_cwd() {
        assert_eq!(
            resolve_from(
                Path::new("/workspace"),
                Path::new("/work/cwd"),
                Some(OsStr::new("artifacts/cargo")),
            )
            .expect("resolve relative target directory"),
            Path::new("/work/cwd/artifacts/cargo")
        );
    }

    #[test]
    fn absolute_target_directory_is_unchanged() {
        assert_eq!(
            resolve_from(
                Path::new("/workspace"),
                Path::new("/work/cwd"),
                Some(OsStr::new("/var/tmp/pmos-target")),
            )
            .expect("resolve absolute target directory"),
            Path::new("/var/tmp/pmos-target")
        );
    }

    #[test]
    fn empty_target_directory_is_rejected() {
        let error = resolve_from(
            Path::new("/workspace"),
            Path::new("/work/cwd"),
            Some(OsStr::new("")),
        )
        .expect_err("empty target directory must fail closed");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("empty string"));
    }
}
