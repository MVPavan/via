//! VIA's managed private directories (runtime §6.1): every directory from
//! the daemon's `vendor/` down to the one a route keeps its state in must
//! be a directory, not a symlink, of the daemon's user, mode 0700. A
//! missing one is created 0700; an existing one is never chmod-ed or
//! followed, and any mismatch is a named refusal. Blocking: callers run it
//! off the async workers.

use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

/// Why a managed directory is not usable.
#[derive(Debug)]
pub(crate) enum Unsafe {
    /// A managed directory is not private: VIA's text naming it and the
    /// rule, never a value.
    Refused(String),
    /// A directory could not be read or created.
    Io,
}

impl From<std::io::Error> for Unsafe {
    fn from(_: std::io::Error) -> Self {
        Self::Io
    }
}

/// The daemon's user ID.
pub(crate) fn daemon_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// The managed directory `vendor_state_dir/<parts>`: each directory from
/// `vendor_state_dir` down, checked in order and created 0700 where
/// missing. `owner` names the state in a refusal, such as `OpenCode`.
pub(crate) fn managed(
    vendor_state_dir: &Path,
    parts: &[&str],
    owner: &str,
) -> Result<PathBuf, Unsafe> {
    let uid = daemon_uid();
    let mut path = vendor_state_dir.to_path_buf();
    let mut name = String::from("vendor");
    private_dir(&path, (owner, &name), uid)?;
    for part in parts {
        path.push(part);
        name.push('/');
        name.push_str(part);
        private_dir(&path, (owner, &name), uid)?;
    }
    Ok(path)
}

/// One managed directory `path`, named `name` in a refusal.
fn private_dir(path: &Path, (owner, name): (&str, &str), uid: u32) -> Result<(), Unsafe> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => {}
                // Created meanwhile: judged as found.
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
            std::fs::symlink_metadata(path)?
        }
        Err(error) => return Err(error.into()),
    };
    let why = if metadata.file_type().is_symlink() {
        "is a symlink".to_owned()
    } else if !metadata.is_dir() {
        "is not a directory".to_owned()
    } else if metadata.uid() != uid {
        "is not owned by the daemon's user".to_owned()
    } else if metadata.mode() & 0o777 != 0o700 {
        format!("has mode {:04o}, not 0700", metadata.mode() & 0o777)
    } else {
        return Ok(());
    };
    Err(Unsafe::Refused(format!(
        "VIA's {owner} state directory {name} {why}"
    )))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::{Unsafe, managed};

    fn private_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    fn refused(result: Result<std::path::PathBuf, Unsafe>) -> String {
        match result {
            Err(Unsafe::Refused(detail)) => detail,
            other => panic!("not refused: {other:?}"),
        }
    }

    #[test]
    fn missing_directories_are_created_private() {
        let root = private_root();
        let path = managed(root.path(), &["a", "b"], "Test").unwrap();
        assert_eq!(path, root.path().join("a").join("b"));
        for dir in [root.path().join("a"), path] {
            let mode = fs::symlink_metadata(dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }

    #[test]
    fn a_symlink_a_file_or_a_wider_mode_is_refused_and_left_alone() {
        let root = private_root();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("a")).unwrap();
        let detail = refused(managed(root.path(), &["a", "b"], "Test"));
        assert_eq!(detail, "VIA's Test state directory vendor/a is a symlink");
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0, "followed");
        fs::remove_file(root.path().join("a")).unwrap();

        fs::write(root.path().join("a"), b"").unwrap();
        let detail = refused(managed(root.path(), &["a"], "Test"));
        assert!(detail.ends_with("vendor/a is not a directory"), "{detail}");
        fs::remove_file(root.path().join("a")).unwrap();

        fs::create_dir(root.path().join("a")).unwrap();
        fs::set_permissions(root.path().join("a"), fs::Permissions::from_mode(0o755)).unwrap();
        let detail = refused(managed(root.path(), &["a"], "Test"));
        assert!(
            detail.ends_with("vendor/a has mode 0755, not 0700"),
            "{detail}"
        );
        let mode = fs::symlink_metadata(root.path().join("a"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "never chmod-ed");
    }

    #[test]
    fn the_vendor_directory_itself_is_checked() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o750)).unwrap();
        let detail = refused(managed(root.path(), &[], "Test"));
        assert!(
            detail.ends_with("vendor has mode 0750, not 0700"),
            "{detail}"
        );
    }
}
