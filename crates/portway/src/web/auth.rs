//! Who may talk to the console.
//!
//! A run starts with a random token that only the launching terminal (and the
//! 0600 `portway.web` file) ever sees. The page trades it once for a session
//! cookie whose value is a second, independent secret, so the token never
//! rides a request after the first one and never lands in a browser store.
//!
//! Three more checks close the ways a hostile page could reach a loopback
//! listener: the `Host` header must name this port on `localhost` or an IP
//! literal (DNS rebinding), a present `Origin` must be this console's own, and
//! every POST must carry a custom header a cross-site form cannot set and a
//! cross-site script cannot send without a preflight nobody answers.

use std::io::{self, Read};
use std::net::IpAddr;

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
    left.iter()
        .zip(right)
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

pub struct Auth {
    token: String,
    session: String,
    port: u16,
}

impl Auth {
    pub fn new(port: u16) -> io::Result<Self> {
        Ok(Auth {
            token: random_hex(32)?,
            session: random_hex(32)?,
            port,
        })
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn token_ok(&self, candidate: &str) -> bool {
        ct_eq(candidate.as_bytes(), self.token.as_bytes())
    }

    /// Named per port, so two consoles on one host do not overwrite each
    /// other's session: cookies are scoped by host, not by port.
    pub fn cookie_name(&self) -> String {
        format!("portway_{}", self.port)
    }

    pub fn set_cookie(&self) -> String {
        format!(
            "{}={}; HttpOnly; SameSite=Strict; Path=/",
            self.cookie_name(),
            self.session
        )
    }

    /// Whether any `Cookie` header carries this console's session.
    pub fn has_session<'a>(&self, headers: impl IntoIterator<Item = &'a str>) -> bool {
        let name = self.cookie_name();
        headers
            .into_iter()
            .flat_map(|header| header.split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
            .any(|(key, value)| key == name && ct_eq(value.as_bytes(), self.session.as_bytes()))
    }

    /// `localhost` or an IP literal, on exactly this port. A name that merely
    /// resolves here is refused: that is what a rebinding attack looks like.
    pub fn host_allowed(&self, host: Option<&str>) -> bool {
        let Some((name, port)) = host.and_then(split_host) else {
            return false;
        };
        port == self.port
            && (name.eq_ignore_ascii_case("localhost") || name.parse::<IpAddr>().is_ok())
    }

    /// No `Origin` is a client that is not a browser page; one that is there
    /// must be this console, as the `Host` header names it.
    pub fn origin_allowed(&self, origin: Option<&str>, host: &str) -> bool {
        match origin {
            None => true,
            Some(origin) => origin == format!("http://{host}"),
        }
    }
}

/// `name:port`, with the brackets of an IPv6 literal taken off the name.
fn split_host(host: &str) -> Option<(&str, u16)> {
    let (name, port) = host.rsplit_once(':')?;
    let port = port.parse().ok()?;
    let name = match name.strip_prefix('[') {
        Some(inner) => inner.strip_suffix(']')?,
        None if name.contains(':') => return None,
        None => name,
    };
    Some((name, port))
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
        let auth = Auth::new(8790).unwrap();
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
        ] {
            assert!(!auth.host_allowed(Some(bad)), "{bad}");
        }
        assert!(!auth.host_allowed(None));
    }

    #[test]
    fn a_session_is_the_cookie_value_not_the_token() {
        let auth = Auth::new(8790).unwrap();
        let cookie = auth.set_cookie();
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
        let pair = cookie.split(';').next().unwrap();
        assert!(auth.has_session([format!("a=b; {pair}").as_str()]));
        let forged = format!("{}={}", auth.cookie_name(), auth.token());
        assert!(!auth.has_session([forged.as_str()]));
        assert!(auth.token_ok(auth.token()));
        assert!(!auth.token_ok(&auth.token()[1..]));
    }

    #[test]
    fn a_foreign_origin_is_refused() {
        let auth = Auth::new(8790).unwrap();
        assert!(auth.origin_allowed(None, "127.0.0.1:8790"));
        assert!(auth.origin_allowed(Some("http://127.0.0.1:8790"), "127.0.0.1:8790"));
        assert!(!auth.origin_allowed(Some("http://evil.example"), "127.0.0.1:8790"));
        assert!(!auth.origin_allowed(Some("null"), "127.0.0.1:8790"));
    }
}
