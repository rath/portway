//! Authenticated HTTP/HTTPS reads. No redirects, ambient cookies or proxy state.
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use http::{Request, StatusCode, Uri};
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use ratatui::crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal,
};
use serde::de::DeserializeOwned;

use crate::{
    body::TimedBody,
    clock::PhaseClock,
    pool::{Lease, Upstream},
};

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_JSON: usize = 8 * 1024 * 1024;
const CACHE: &str = "remote-sessions.json";

pub enum Error {
    Expired,
    Other(String),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Expired => f.write_str("remote session is no longer valid; console access may have been reset; run --tui --attach again and enter the current console token"),
            Self::Other(message) => f.write_str(message),
        }
    }
}
impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
impl std::error::Error for Error {}
impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Other(error.to_string())
    }
}
impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self::Other("invalid remote console response".into())
    }
}

#[derive(Clone)]
pub struct Client {
    pub url: String,
    prefix: String,
    authority: String,
    upstream: Arc<Upstream>,
    cookie: Option<String>,
}
impl Client {
    pub fn new(raw: &str) -> Result<Self, Error> {
        let invalid = || {
            Error::Other("--attach requires an http:// or https:// console URL without credentials, query or fragment".into())
        };
        let uri: Uri = raw.parse().map_err(|_| invalid())?;
        let scheme = match uri.scheme_str() {
            Some("http") => "http",
            Some("https") => "https",
            _ => return Err(invalid()),
        };
        if raw.contains(['#', '@', '?']) {
            return Err(invalid());
        }
        let host = uri.host().ok_or_else(invalid)?.to_ascii_lowercase();
        let port = uri
            .port_u16()
            .unwrap_or(if scheme == "https" { 443 } else { 80 });
        // Match a browser's Host header, including omission of default ports:
        // reverse proxies commonly route on the bare public host name.
        let authority = if (scheme == "https" && port == 443) || (scheme == "http" && port == 80) {
            host
        } else {
            format!("{host}:{port}")
        };
        let prefix = uri.path().trim_end_matches('/').to_owned();
        let url = format!("{scheme}://{authority}{prefix}/");
        let upstream = Arc::new(Upstream::new(&url, None).map_err(|_| invalid())?);
        Ok(Self {
            url,
            prefix,
            authority,
            upstream,
            cookie: None,
        })
    }

    async fn request(
        &self,
        path: &str,
        body: Option<String>,
    ) -> Result<(http::Response<Incoming>, Lease), Error> {
        let clock = Arc::new(PhaseClock::new());
        let mut request = Request::builder()
            .method(if body.is_some() { "POST" } else { "GET" })
            .uri(format!("{}{path}", self.prefix))
            .header("host", &self.authority)
            .header("accept-encoding", "identity");
        if let Some(cookie) = &self.cookie {
            request = request.header("cookie", cookie);
        }
        let body = match body {
            Some(body) => {
                request = request
                    .header("content-type", "application/json")
                    .header("x-portway-console", "1");
                TimedBody::new(body.into(), Arc::clone(&clock))
            }
            None => TimedBody::empty(Arc::clone(&clock)),
        };
        let request = request
            .body(body)
            .map_err(|_| Error::Other("invalid console request".into()))?;
        let (response, lease) =
            tokio::time::timeout(REQUEST_TIMEOUT, self.upstream.send(request, clock))
                .await
                .map_err(|_| Error::Other("console request timed out".into()))?
                .map_err(|error| Error::Other(format!("console connection: {error}")))?;
        if response.status() == StatusCode::UNAUTHORIZED {
            return Err(Error::Expired);
        }
        if !response.status().is_success() {
            // Do not echo arbitrary proxy response bodies, URLs or headers.
            return Err(Error::Other(format!(
                "console returned HTTP {} (check the console URL, base path and proxy configuration)",
                response.status()
            )));
        }
        Ok((response, lease))
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        let (response, mut lease) = self.request(path, None).await?;
        let bytes = tokio::time::timeout(
            REQUEST_TIMEOUT,
            Limited::new(response.into_body(), MAX_JSON).collect(),
        )
        .await
        .map_err(|_| Error::Other("console response timed out".into()))?
        .map_err(|_| Error::Other("console response exceeds its limit or was interrupted".into()))?
        .to_bytes();
        lease.release();
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub async fn stream(&self, after: u64) -> Result<(Incoming, Lease), Error> {
        let (response, lease) = self
            .request(&format!("/api/stream?after={after}"), None)
            .await?;
        if !response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"))
        {
            return Err(Error::Other(
                "console did not return an event stream".into(),
            ));
        }
        Ok((response.into_body(), lease))
    }

    pub async fn login(&mut self, token: &str) -> Result<(), Error> {
        let (response, mut lease) = self
            .request(
                "/api/session",
                Some(serde_json::json!({"token": token}).to_string()),
            )
            .await?;
        let cookie = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .filter_map(|value| value.split(';').next())
            .find(|value| valid_cookie(value))
            .ok_or_else(|| Error::Other("console did not issue a session".into()))?
            .to_owned();
        tokio::time::timeout(
            REQUEST_TIMEOUT,
            Limited::new(response.into_body(), 1024).collect(),
        )
        .await
        .map_err(|_| Error::Other("console login timed out".into()))?
        .map_err(|_| Error::Other("console login interrupted".into()))?;
        lease.release();
        self.cookie = Some(cookie);
        Ok(())
    }

    pub async fn authenticate(&mut self, dir: &Path) -> Result<(), Error> {
        self.cookie = sessions(dir)?
            .remove(&self.url)
            .filter(|value| valid_cookie(value));
        let health: serde_json::Value = self.get("/api/health").await?;
        if health["console"] != "portway" {
            return Err(Error::Other("URL is not a Portway web console".into()));
        }
        if health["session"] == true {
            return Ok(());
        }
        self.cookie = None;
        save_session(dir, &self.url, None)?;
        loop {
            let token = prompt().await?;
            match self.login(&token).await {
                Ok(()) => {
                    return save_session(dir, &self.url, self.cookie.as_deref())
                        .map_err(Into::into);
                }
                Err(Error::Expired) => {
                    eprintln!("Invalid or expired token; enter the token from the running console.")
                }
                Err(error) => return Err(error),
            }
        }
    }
}

fn valid_cookie(value: &str) -> bool {
    value.split_once('=').is_some_and(|(name, secret)| {
        name.strip_prefix("portway_")
            .is_some_and(|port| port.parse::<u16>().is_ok())
            && secret.len() == 64
            && secret.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

fn owned(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::other(
            "remote session files must be owned by this user with mode 0600",
        ));
    }
    Ok(())
}
fn sessions(dir: &Path) -> io::Result<BTreeMap<String, String>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join(CACHE))
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error),
    };
    owned(&file)?;
    if file.metadata()?.len() > 1024 * 1024 {
        return Err(io::Error::other("remote session file too large"));
    }
    serde_json::from_reader(file).map_err(|_| io::Error::other("invalid remote session file"))
}

/// Lock across the read/modify/rename so two viewers cannot lose each other's sessions.
pub fn save_session(dir: &Path, url: &str, cookie: Option<&str>) -> io::Result<()> {
    crate::store::ensure_dir(dir).map_err(io::Error::other)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join("remote-sessions.lock"))?;
    owned(&lock)?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut saved = sessions(dir)?;
    match cookie {
        Some(cookie) => {
            saved.insert(url.to_owned(), cookie.to_owned());
        }
        None => {
            saved.remove(url);
        }
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let temp: PathBuf = dir.join(format!(".remote-sessions-{}-{nonce}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let result = (|| {
        serde_json::to_writer(&mut file, &saved).map_err(io::Error::other)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temp, dir.join(CACHE))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

async fn prompt() -> io::Result<String> {
    if !io::stdin().is_terminal() {
        return Err(io::Error::other(
            "a terminal is required to enter the remote console token",
        ));
    }
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = terminal::disable_raw_mode();
        }
    }
    terminal::enable_raw_mode()?;
    let guard = Restore;
    eprint!("Console token (hidden; Esc cancels): ");
    io::stderr().flush()?;
    let mut token = String::new();
    let interrupted = super::interrupted();
    tokio::pin!(interrupted);
    let result = loop {
        tokio::select! {
            _ = &mut interrupted => break Err(io::Error::new(io::ErrorKind::Interrupted, "token entry cancelled")),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {},
        }
        if !event::poll(Duration::ZERO)? {
            continue;
        }
        let key = match event::read()? {
            Event::Key(key) => key,
            Event::Paste(text) => {
                let text = text.trim();
                if token.len() + text.len() <= 128 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
                    token.push_str(text);
                }
                continue;
            }
            _ => continue,
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Enter if !token.is_empty() => break Ok(token),
            KeyCode::Esc => {
                break Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "token entry cancelled",
                ));
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                break Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "token entry cancelled",
                ));
            }
            KeyCode::Backspace => {
                token.pop();
            }
            KeyCode::Char(c) if c.is_ascii_hexdigit() && token.len() < 128 => token.push(c),
            _ => {}
        }
    };
    drop(guard);
    eprintln!();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_keep_prefixes_and_isolate_credentials() {
        assert_eq!(
            Client::new("https://EXAMPLE.com/portway").unwrap().url,
            "https://example.com/portway/"
        );
        assert_eq!(
            Client::new("https://example.com:443/portway/").unwrap().url,
            "https://example.com/portway/"
        );
        assert_eq!(
            Client::new("http://[::1]:8790/").unwrap().url,
            "http://[::1]:8790/"
        );
        for bad in [
            "server",
            "ftp://server/",
            "https://user:secret@server/",
            "https://server/?token=secret",
            "https://server/#token=secret",
        ] {
            let error = Client::new(bad).err().expect("bad URL").to_string();
            assert!(!error.contains("secret"));
        }
    }

    #[test]
    fn sessions_are_private_scoped_and_atomically_replaced() {
        let dir = std::env::temp_dir().join(format!("pw-remote-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let one = Client::new("https://example.com/a").unwrap();
        let two = Client::new("https://example.com/b").unwrap();
        let cookie = format!("portway_8790={}", "a".repeat(64));
        save_session(&dir, &one.url, Some(&cookie)).unwrap();
        save_session(&dir, &two.url, Some(&cookie)).unwrap();
        let file = dir.join(CACHE);
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(sessions(&dir).unwrap().len(), 2);
        save_session(&dir, &one.url, None).unwrap();
        assert!(!sessions(&dir).unwrap().contains_key(&one.url));
        assert!(sessions(&dir).unwrap().contains_key(&two.url));
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(sessions(&dir).is_err());
        fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere"), &file).unwrap();
        assert!(sessions(&dir).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
