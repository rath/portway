//! `<data dir>/portway.web`: where a running console can be found again.
//!
//! It holds the console's pid and its full URL, token included, so it is
//! created 0600 and never logged. Like the pid file it is held with an
//! exclusive `flock` for as long as the console runs: the lock is the
//! liveness check, and a file left by a killed process reads as nothing.

use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const WEB_FILE: &str = "portway.web";

pub struct WebFile {
    /// Held for the lock.
    _file: File,
    path: PathBuf,
}

impl WebFile {
    /// Take `<dir>/portway.web` and write `pid` and `url` into it. `None`
    /// when another live console already holds it: the first one stays the
    /// one `--status` names.
    pub fn claim(dir: &Path, url: &str) -> Result<Option<WebFile>, String> {
        let path = dir.join(WEB_FILE);
        let error = |err: io::Error| format!("{}: {err}", path.display());
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
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
            .and_then(|()| write!(file, "{}\n{url}\n", std::process::id()))
            .and_then(|()| file.flush())
            .map_err(error)?;
        Ok(Some(WebFile { _file: file, path }))
    }
}

impl Drop for WebFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// The URL of the console running on `dir`, when one is.
pub fn console_url(dir: &Path) -> Option<String> {
    let mut file = match File::open(dir.join(WEB_FILE)) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return None,
        Err(_) => return None,
    };
    // Taking the lock means nobody held it: what is in the file is stale.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
        return None;
    }
    let mut text = String::new();
    Read::by_ref(&mut file)
        .take(4096)
        .read_to_string(&mut text)
        .ok()?;
    text.lines().nth(1).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn a_claimed_file_is_private_live_and_gone_on_drop() {
        let dir = std::env::temp_dir().join(format!("portway-web-file-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(console_url(&dir), None);

        let url = "http://127.0.0.1:8790/#token=abc";
        let file = WebFile::claim(&dir, url).unwrap().expect("first claim");
        let mode = fs::metadata(dir.join(WEB_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(console_url(&dir).as_deref(), Some(url));
        // A second console on the same directory leaves the first one named.
        assert!(WebFile::claim(&dir, "http://other/").unwrap().is_none());
        assert_eq!(console_url(&dir).as_deref(), Some(url));

        drop(file);
        assert!(!dir.join(WEB_FILE).exists());
        fs::write(dir.join(WEB_FILE), "1\nhttp://stale/\n").unwrap();
        assert_eq!(console_url(&dir), None);
        fs::remove_dir_all(&dir).unwrap();
    }
}
