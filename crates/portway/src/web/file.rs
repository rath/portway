//! `<data dir>/portway.web`: where a running console can be found again.
//!
//! It holds the console's pid and its full URLs, token included, one a line, so it is
//! created 0600 and never logged. Like the pid file it is held with an
//! exclusive `flock` for as long as the console runs: the lock is the
//! liveness check, and a file left by a killed process reads as nothing.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

pub const WEB_FILE: &str = "portway.web";

pub struct WebFile {
    /// Held for the lock.
    _file: File,
    path: PathBuf,
}

impl WebFile {
    /// Take `<dir>/portway.web` and write `pid` and `urls` into it. `None`
    /// when another live console already holds it: the first one stays the
    /// one `--status` names.
    pub fn claim(dir: &Path, urls: &[String]) -> Result<Option<WebFile>, String> {
        let path = dir.join(WEB_FILE);
        let error = |err: io::Error| format!("{}: {err}", path.display());
        let mut file = crate::private_file::open(
            &path,
            OpenOptions::new().read(true).write(true).create(true),
        )
        .map_err(error)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Ok(None);
            }
            return Err(error(err));
        }
        file.set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)).map(drop))
            .and_then(|()| writeln!(file, "{}\n{}", std::process::id(), urls.join("\n")))
            .and_then(|()| file.flush())
            .map_err(error)?;
        Ok(Some(WebFile { _file: file, path }))
    }
}

impl Drop for WebFile {
    fn drop(&mut self) {
        crate::private_file::remove_if_same(&self._file, &self.path);
    }
}

/// The URLs of the console running on `dir`; none when none is.
pub fn console_urls(dir: &Path) -> Vec<String> {
    let mut file =
        match crate::private_file::open(&dir.join(WEB_FILE), OpenOptions::new().read(true)) {
            Ok(file) => file,
            Err(_) => return Vec::new(),
        };
    // Taking the lock means nobody held it: what is in the file is stale.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0
        || io::Error::last_os_error().raw_os_error() != Some(libc::EWOULDBLOCK)
    {
        return Vec::new();
    }
    let mut text = String::new();
    if Read::by_ref(&mut file)
        .take(8192)
        .read_to_string(&mut text)
        .is_err()
    {
        return Vec::new();
    }
    text.lines()
        .skip(1)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn a_claimed_file_is_private_live_and_gone_on_drop() {
        let dir = std::env::temp_dir().join(format!("portway-web-file-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert!(console_urls(&dir).is_empty());

        let urls = vec![
            "http://127.0.0.1:8790/#token=abc".to_string(),
            "http://10.0.0.2:8790/#token=abc".to_string(),
        ];
        let file = WebFile::claim(&dir, &urls).unwrap().expect("first claim");
        let mode = fs::metadata(dir.join(WEB_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(console_urls(&dir), urls);
        // A second console on the same directory leaves the first one named.
        let other = ["http://other/".to_string()];
        assert!(WebFile::claim(&dir, &other).unwrap().is_none());
        assert_eq!(console_urls(&dir), urls);

        drop(file);
        assert!(!dir.join(WEB_FILE).exists());
        fs::write(dir.join(WEB_FILE), "1\nhttp://stale/\n").unwrap();
        assert!(console_urls(&dir).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn private_runtime_file_restricts_existing_permissions() {
        let dir = std::env::temp_dir().join(format!("portway-web-mode-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(WEB_FILE);
        fs::write(&path, "stale").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let file = WebFile::claim(&dir, &["http://localhost/#token=test".into()])
            .unwrap()
            .unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(file);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn private_runtime_file_refuses_links_without_touching_the_target() {
        let dir = std::env::temp_dir().join(format!("portway-web-links-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target");
        fs::write(&target, "keep me").unwrap();
        let path = dir.join(WEB_FILE);
        for hard_link in [false, true] {
            if hard_link {
                fs::hard_link(&target, &path).unwrap();
            } else {
                std::os::unix::fs::symlink(&target, &path).unwrap();
            }
            assert!(WebFile::claim(&dir, &["secret".into()]).is_err());
            assert_eq!(fs::read_to_string(&target).unwrap(), "keep me");
            fs::remove_file(&path).unwrap();
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
