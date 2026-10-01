//! Console credentials outlive the process; the launch/discovery file does not.
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::auth::random_hex;

const FILE: &str = "web-auth.json";
const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Credentials {
    pub token: String,
    pub session: String,
}

impl Credentials {
    pub fn generate() -> io::Result<Self> {
        Ok(Self {
            token: random_hex(32)?,
            session: random_hex(32)?,
        })
    }

    fn valid(&self) -> bool {
        [&self.token, &self.session].into_iter().all(|secret| {
            secret.len() == 64
                && secret
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) && self.token != self.session
    }
}

#[derive(Deserialize, Serialize)]
struct Saved {
    version: u8,
    consoles: BTreeMap<String, Credentials>,
}

fn private(file: &File) -> io::Result<()> {
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.permissions().mode() & 0o777 != 0o600
    {
        return Err(io::Error::other(
            "console auth files must be regular files owned by this user with mode 0600",
        ));
    }
    Ok(())
}

fn invalid() -> io::Error {
    io::Error::other("invalid web-auth.json; restore the file or explicitly reset console access")
}

/// Serialize creation across processes, including consoles with different ports.
/// Only absence creates credentials. A broken or unsafe file never rotates them.
pub(super) fn load(dir: &Path, port: u16, base: &str) -> io::Result<Credentials> {
    crate::store::ensure_dir(dir).map_err(io::Error::other)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join("web-auth.lock"))?;
    private(&lock)?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let path = dir.join(FILE);
    let mut saved = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(file) => {
            private(&file)?;
            if file.metadata()?.len() > MAX_BYTES {
                return Err(invalid());
            }
            let saved: Saved = serde_json::from_reader(file).map_err(|_| invalid())?;
            if saved.version != 1 || saved.consoles.values().any(|entry| !entry.valid()) {
                return Err(invalid());
            }
            saved
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Saved {
            version: 1,
            consoles: BTreeMap::new(),
        },
        Err(error) => return Err(error),
    };
    let key = format!("{port}:{base}");
    if let Some(credentials) = saved.consoles.get(&key) {
        return Ok(credentials.clone());
    }
    let credentials = Credentials::generate()?;
    saved.consoles.insert(key, credentials.clone());
    let mut bytes = serde_json::to_vec(&saved).map_err(|_| invalid())?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_BYTES {
        return Err(io::Error::other("console auth file is full"));
    }
    let temp = dir.join(format!(".web-auth-{}", random_hex(16)?));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temp, &path)?;
        File::open(dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result?;
    Ok(credentials)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::auth::Auth;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("portway-auth-{}", random_hex(16).unwrap()));
            crate::store::ensure_dir(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn credentials_survive_restart_but_launch_codes_do_not() {
        let dir = Temp::new();
        let first = Auth::load(&dir.0, 8790, vec![], "/console").unwrap();
        let next = Auth::load(&dir.0, 8790, vec![], "/console").unwrap();
        assert_eq!(first.token(), next.token());
        assert_eq!(first.set_cookie(), next.set_cookie());
        assert_ne!(first.launch_code(), next.launch_code());
        assert!(!next.launch_ok(first.launch_code()));
        assert!(next.has_session([first.set_cookie().as_str()]));
        for path in [FILE, "web-auth.lock"] {
            assert_eq!(
                fs::metadata(dir.0.join(path)).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        for (port, base) in [(8791, "/console"), (8790, "/other")] {
            let other = Auth::load(&dir.0, port, vec![], base).unwrap();
            assert!(!other.token_ok(first.token()));
            assert!(!other.has_session([first.set_cookie().as_str()]));
        }
        let other_dir = Temp::new();
        let other = Auth::load(&other_dir.0, 8790, vec![], "/console").unwrap();
        assert!(!other.token_ok(first.token()));
        drop(next);
        let token = first.token().to_owned();
        let cookie = first.set_cookie();
        drop(first);
        fs::remove_file(dir.0.join(FILE)).unwrap();
        let reset = Auth::load(&dir.0, 8790, vec![], "/console").unwrap();
        assert!(!reset.token_ok(&token));
        assert!(!reset.has_session([cookie.as_str()]));
    }

    #[test]
    fn concurrent_creation_preserves_every_console() {
        let dir = Temp::new();
        let barrier = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let path = dir.0.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    let port = 8790 + i % 4;
                    (port, load(&path, port, "/").unwrap())
                })
            })
            .collect();
        for thread in threads {
            let (port, credentials) = thread.join().unwrap();
            let saved = load(&dir.0, port, "/").unwrap();
            assert_eq!(credentials.token, saved.token);
            assert_eq!(credentials.session, saved.session);
        }
        let saved: Saved = serde_json::from_reader(File::open(dir.0.join(FILE)).unwrap()).unwrap();
        assert_eq!(saved.consoles.len(), 4);
    }

    #[test]
    fn unsafe_or_corrupt_files_are_not_replaced() {
        let dir = Temp::new();
        load(&dir.0, 8790, "/").unwrap();
        let path = dir.0.join(FILE);
        let original = fs::read(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load(&dir.0, 8790, "/").is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        for bad in [
            "{",
            r#"{"version":2,"consoles":{}}"#,
            r#"{"version":1,"consoles":{"8790:/":{"token":"bad","session":"bad"}}}"#,
        ] {
            fs::write(&path, bad).unwrap();
            assert!(load(&dir.0, 8790, "/").is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), bad);
        }
        fs::write(&path, vec![b' '; MAX_BYTES as usize + 1]).unwrap();
        assert!(load(&dir.0, 8790, "/").is_err());
        fs::remove_file(&path).unwrap();
        let target = dir.0.join("missing");
        symlink(&target, &path).unwrap();
        assert!(load(&dir.0, 8790, "/").is_err());
        assert!(!target.exists());
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(load(&dir.0, 8790, "/").is_err());
        fs::remove_dir(&path).unwrap();
        fs::remove_file(dir.0.join("web-auth.lock")).unwrap();
        symlink(&target, dir.0.join("web-auth.lock")).unwrap();
        assert!(load(&dir.0, 8790, "/").is_err());
        assert!(!path.exists());
    }
}
