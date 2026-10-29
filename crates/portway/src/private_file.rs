//! Open runtime files without following links or trusting their initial mode.
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Validate the opened inode before changing its permissions or contents.
/// O_NONBLOCK lets us reject a FIFO without waiting for its other endpoint.
pub(crate) fn open(path: &Path, options: &mut OpenOptions) -> io::Result<File> {
    let file = options
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } || meta.nlink() != 1 {
        return Err(io::Error::other(
            "runtime files must be regular files owned by this user with no other links",
        ));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// A newer process may have replaced the path while we held the old inode.
pub(crate) fn remove_if_same(file: &File, path: &Path) {
    if let (Ok(held), Ok(now)) = (file.metadata(), fs::symlink_metadata(path))
        && (held.dev(), held.ino()) == (now.dev(), now.ino())
    {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_runtime_fifo_is_rejected_without_blocking() {
        let path = std::env::temp_dir().join(format!("portway-fifo-{}", std::process::id()));
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(open(&path, OpenOptions::new().read(true).write(true)).is_err());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn cleanup_keeps_a_replacement_file() {
        let path = std::env::temp_dir().join(format!("portway-replaced-{}", std::process::id()));
        let file = open(&path, OpenOptions::new().write(true).create_new(true)).unwrap();
        fs::remove_file(&path).unwrap();
        fs::write(&path, "replacement").unwrap();
        remove_if_same(&file, &path);
        assert_eq!(fs::read_to_string(&path).unwrap(), "replacement");
        fs::remove_file(path).unwrap();
    }
}
