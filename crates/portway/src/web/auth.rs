//! Who may talk to the console.
//!
//! A console keeps its random token and independent session secret in the
//! private `web-auth.json` file across restarts. The page trades the token for
//! a cookie whose value is the independent session secret, so the token never
//! rides a request after the first one and never lands in a browser store.
//!
//! The browser a run opens for itself gets neither: its command line is
//! readable by every local user, so it carries a launch code instead, good
//! for one session and only for two minutes.
//!
//! Three more checks close the ways a hostile page could reach a loopback
//! listener: the `Host` header must name this port on `localhost`, an IP
//! literal or a name the operator allowed (DNS rebinding), a present `Origin` must be this console's own, and
//! every POST must carry a custom header a cross-site form cannot set and a
//! cross-site script cannot send without a preflight nobody answers.

use std::io::{self, Read};
use std::net::IpAddr;
use std::path::Path;

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::credentials::{self, Credentials};

/// The header every state-changing request must carry.
pub const CSRF_HEADER: &str = "x-portway-console";

/// `bytes` of kernel entropy as lowercase hex.
pub fn random_hex(bytes: usize) -> io::Result<String> {
    let mut buffer = vec![0u8; bytes];
    fill_random(&mut buffer)?;
    Ok(buffer.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// `getentropy`, which takes at most 256 bytes a call, or `/dev/urandom`
/// where the call is refused.
fn fill_random(buffer: &mut [u8]) -> io::Result<()> {
    for chunk in buffer.chunks_mut(256) {
        if unsafe { libc::getentropy(chunk.as_mut_ptr().cast(), chunk.len()) } != 0 {
            return std::fs::File::open("/dev/urandom")?.read_exact(buffer);
        }
    }
    Ok(())
}

/// Equal without saying where the first difference is.
pub fn ct_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
        diff = std::hint::black_box(diff);
    }
    diff == 0
}

/// Which credential minted the session cookie.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Session {
    /// From the persistent token: survives restarts and renews its `Max-Age`.
    Persistent,
    /// From the argv-visible launch code: in-memory, browser-session only, and
    /// discarded when this console exits.
    Launch,
}

/// How long the launch code waits for the browser it was handed to.
pub const LAUNCH_TTL: Duration = Duration::from_secs(120);

pub struct Auth {
    token: String,
    session: String,
    /// Session secret minted only by the one-time launch code, never persisted.
    launch_session: String,
    /// The launch code and when it was made, until it is used or stale.
    launch: Mutex<Option<(String, Instant)>>,
    /// The code itself, for the address the browser is handed.
    launch_code: String,
    port: u16,
    /// Names allowed besides `localhost`, normalized (see `address`).
    names: Vec<String>,
    base: String,
}

impl Auth {
    pub fn load(dir: &Path, port: u16, names: Vec<String>, base: &str) -> io::Result<Self> {
        let base = super::address::base_path(base)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
        let credentials = credentials::load(dir, port, &base)?;
        Self::with_credentials(port, names, credentials, base)
    }

    /// Fresh in-memory credentials, for an ephemeral `--web-port 0` listener
    /// that must not pollute `web-auth.json` or inherit an old random port.
    pub fn ephemeral(port: u16, names: Vec<String>, base: &str) -> io::Result<Self> {
        let base = super::address::base_path(base)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
        Self::with_credentials(port, names, Credentials::generate()?, base)
    }

    #[cfg(test)]
    pub fn new(port: u16, names: Vec<String>) -> io::Result<Self> {
        Self::with_credentials(port, names, Credentials::generate()?, "/".into())
    }

    fn with_credentials(
        port: u16,
        names: Vec<String>,
        credentials: Credentials,
        base: String,
    ) -> io::Result<Self> {
        let launch_code = random_hex(32)?;
        let launch_session = random_hex(32)?;
        Ok(Auth {
            token: credentials.token,
            session: credentials.session,
            launch_session,
            launch: Mutex::new(Some((launch_code.clone(), Instant::now()))),
            launch_code,
            port,
            names,
            base,
        })
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn token_ok(&self, candidate: &str) -> bool {
        ct_eq(candidate.as_bytes(), self.token.as_bytes())
    }

    pub fn launch_code(&self) -> &str {
        &self.launch_code
    }

    /// Whether `candidate` is the launch code, unused and fresh. A match uses
    /// it up, so a code read off a command line is worth nothing once the
    /// browser it was meant for has come.
    pub fn launch_ok(&self, candidate: &str) -> bool {
        self.launch_ok_at(candidate, Instant::now())
    }

    fn launch_ok_at(&self, candidate: &str, now: Instant) -> bool {
        let mut launch = self.launch.lock().unwrap_or_else(PoisonError::into_inner);
        let Some((code, made)) = launch.as_ref() else {
            return false;
        };
        if now.saturating_duration_since(*made) > LAUNCH_TTL {
            *launch = None;
            return false;
        }
        if !ct_eq(candidate.as_bytes(), code.as_bytes()) {
            return false;
        }
        *launch = None;
        true
    }

    /// Named per port, so two consoles on one host do not overwrite each
    /// other's session: cookies are scoped by host, not by port.
    pub fn cookie_name(&self) -> String {
        format!("portway_{}", self.port)
    }

    pub fn set_cookie(&self) -> String {
        format!(
            "{}={}; HttpOnly; SameSite=Strict; Path={}; Max-Age=34560000",
            self.cookie_name(),
            self.session,
            self.base
        )
    }

    /// Browser-session cookie for a launch-code sign-in: never carries the
    /// persistent `web-auth.json` secret and has no `Max-Age`.
    pub fn set_launch_cookie(&self) -> String {
        format!(
            "{}={}; HttpOnly; SameSite=Strict; Path={}",
            self.cookie_name(),
            self.launch_session,
            self.base
        )
    }

    /// Which session any `Cookie` header carries, if any.
    pub fn session<'a>(&self, headers: impl IntoIterator<Item = &'a str>) -> Option<Session> {
        let name = self.cookie_name();
        let mut matched = None;
        for (key, value) in headers
            .into_iter()
            .flat_map(|header| header.split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
        {
            if key != name {
                continue;
            }
            if ct_eq(value.as_bytes(), self.session.as_bytes()) {
                return Some(Session::Persistent);
            }
            if ct_eq(value.as_bytes(), self.launch_session.as_bytes()) {
                matched = Some(Session::Launch);
            }
        }
        matched
    }

    /// `localhost`, an IP literal or an allowed name, on exactly this port. A
    /// name that merely resolves here is refused: that is what a rebinding
    /// attack looks like.
    pub fn host_allowed(&self, host: Option<&str>) -> bool {
        let default_port = (self.port == 80).then_some(80);
        let Some((name, port)) = host.and_then(|host| split_host(host, default_port)) else {
            return false;
        };
        if port != self.port {
            return false;
        }
        let name = super::address::normalize(name);
        name == "localhost" || name.parse::<IpAddr>().is_ok() || self.names.contains(&name)
    }

    /// No `Origin` is a client that is not a browser page; one that is there
    /// must be this console, as the `Host` header names it.
    pub fn origin_allowed(&self, origin: Option<&str>, host: &str) -> bool {
        let Some(origin) = origin else {
            return true;
        };
        let Some(origin_host) = origin.strip_prefix("http://") else {
            return false;
        };
        let default_port = (self.port == 80).then_some(80);
        let (Some((orig_name, orig_port)), Some((req_name, req_port))) = (
            split_host(origin_host, default_port),
            split_host(host, default_port),
        ) else {
            return false;
        };
        orig_port == req_port
            && super::address::normalize(orig_name) == super::address::normalize(req_name)
    }
}

/// `name:port`, with the brackets of an IPv6 literal taken off the name.
/// When `default_port` is `Some(p)`, an omitted port defaults to `p`.
fn split_host(host: &str, default_port: Option<u16>) -> Option<(&str, u16)> {
    if let Some(rest) = host.strip_prefix('[') {
        let (addr, after) = rest.split_once(']')?;
        addr.parse::<std::net::Ipv6Addr>().ok()?;
        let port = match after {
            "" => default_port?,
            _ => parse_port(after.strip_prefix(':')?)?,
        };
        return Some((addr, port));
    }
    if host.contains('[') || host.contains(']') {
        return None;
    }
    match host.rsplit_once(':') {
        Some((name, port)) => {
            if name.is_empty() || name.contains(':') {
                return None;
            }
            Some((name, parse_port(port)?))
        }
        None => {
            if host.is_empty() {
                return None;
            }
            Some((host, default_port?))
        }
    }
}

fn parse_port(port: &str) -> Option<u16> {
    if port.is_empty() || port.starts_with('0') || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    port.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_fresh_hex() {
        let one = random_hex(32).unwrap();
        let two = random_hex(32).unwrap();
        assert_eq!(one.len(), 64);
        assert!(one.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(one, two);
        assert_eq!(random_hex(300).unwrap().len(), 600);
    }

    #[test]
    fn only_this_port_on_a_literal_or_localhost_is_served() {
        let auth = Auth::new(8790, Vec::new()).unwrap();
        for good in [
            "127.0.0.1:8790",
            "localhost:8790",
            "[::1]:8790",
            "10.0.0.2:8790",
        ] {
            assert!(auth.host_allowed(Some(good)), "{good}");
        }
        for bad in [
            "evil.example:8790",
            "127.0.0.1:8791",
            "127.0.0.1",
            "::1:8790",
            "[::1:8790",
            "[localhost]:8790",
            "[127.0.0.1]:8790",
            "127.0.0.1:+8790",
            "127.0.0.1:08790",
        ] {
            assert!(!auth.host_allowed(Some(bad)), "{bad}");
        }
        assert!(!auth.host_allowed(None));

        let port80 = Auth::new(80, vec!["sender-host".to_string()]).unwrap();
        for good in [
            "127.0.0.1",
            "127.0.0.1:80",
            "localhost",
            "[::1]",
            "[::1]:80",
            "sender-host",
            "SENDER-HOST:80",
        ] {
            assert!(port80.host_allowed(Some(good)), "{good}");
        }
        assert!(!port80.host_allowed(Some("sender-host:8080")));
        assert!(!port80.host_allowed(Some("evil.example")));
    }

    #[test]
    fn an_allowed_name_is_served_on_this_port_only() {
        let auth = Auth::new(8790, vec!["sender-host".to_string()]).unwrap();
        for good in ["sender-host:8790", "SENDER-HOST:8790", "sender-host.:8790"] {
            assert!(auth.host_allowed(Some(good)), "{good}");
        }
        for bad in [
            "sender-host:8791",
            "sender-host.lan:8790",
            "evil.example:8790",
        ] {
            assert!(!auth.host_allowed(Some(bad)), "{bad}");
        }
    }

    #[test]
    fn a_session_is_the_cookie_value_not_the_token() {
        let auth = Auth::new(8790, Vec::new()).unwrap();
        let cookie = auth.set_cookie();
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Max-Age=34560000"));
        let pair = cookie.split(';').next().unwrap();
        assert_eq!(
            auth.session([format!("a=b; {pair}").as_str()]),
            Some(Session::Persistent)
        );
        let launch_cookie = auth.set_launch_cookie();
        assert!(launch_cookie.contains("HttpOnly") && launch_cookie.contains("SameSite=Strict"));
        assert!(!launch_cookie.contains("Max-Age="));
        let launch_pair = launch_cookie.split(';').next().unwrap();
        assert_ne!(launch_pair, pair);
        assert_eq!(auth.session([launch_pair]), Some(Session::Launch));
        let forged = format!("{}={}", auth.cookie_name(), auth.token());
        assert_eq!(auth.session([forged.as_str()]), None);
        assert!(auth.token_ok(auth.token()));
        assert!(!auth.token_ok(&auth.token()[1..]));
    }

    #[test]
    fn the_launch_code_opens_one_session_while_fresh() {
        let auth = Auth::new(8790, Vec::new()).unwrap();
        let code = auth.launch_code().to_owned();
        assert_ne!(code, auth.token());
        assert!(!auth.token_ok(&code), "the code is not the token");
        assert!(!auth.launch_ok(auth.token()), "nor the token the code");
        assert!(!auth.launch_ok(&code[1..]));
        assert!(auth.launch_ok(&code));
        assert!(!auth.launch_ok(&code), "used once, gone");
        assert!(auth.token_ok(auth.token()), "the token still works");

        let stale = Auth::new(8790, Vec::new()).unwrap();
        let code = stale.launch_code().to_owned();
        let later = Instant::now() + LAUNCH_TTL + Duration::from_secs(1);
        assert!(!stale.launch_ok_at(&code, later));
        assert!(!stale.launch_ok(&code), "a stale code is dropped");
    }

    #[test]
    fn a_foreign_origin_is_refused() {
        let auth = Auth::new(8790, vec!["sender-host".to_string()]).unwrap();
        assert!(auth.origin_allowed(None, "127.0.0.1:8790"));
        assert!(auth.origin_allowed(Some("http://127.0.0.1:8790"), "127.0.0.1:8790"));
        assert!(auth.origin_allowed(Some("http://sender-host:8790"), "SENDER-HOST:8790"));
        assert!(auth.origin_allowed(Some("http://sender-host:8790"), "sender-host.:8790"));
        assert!(!auth.origin_allowed(Some("http://evil.example"), "127.0.0.1:8790"));
        assert!(!auth.origin_allowed(Some("https://127.0.0.1:8790"), "127.0.0.1:8790"));
        assert!(!auth.origin_allowed(Some("http://127.0.0.1:8790/path"), "127.0.0.1:8790"));
        assert!(!auth.origin_allowed(Some("null"), "127.0.0.1:8790"));

        let port80 = Auth::new(80, vec!["sender-host".to_string()]).unwrap();
        assert!(port80.origin_allowed(Some("http://sender-host"), "sender-host:80"));
        assert!(port80.origin_allowed(Some("http://sender-host:80"), "sender-host"));
    }
}
