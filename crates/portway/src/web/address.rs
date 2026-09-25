//! Where the console can be reached, and by which names.
//!
//! The host check (see `auth`) answers to `localhost`, IP literals and the
//! names the operator vouched for with `--web-allow-host`, plus `--web-host`
//! itself when it is a name. A wildcard bind is reachable on every address
//! the machine has, so the links printed for it list them, loopback first.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::AsRawFd;

/// At most this many interface addresses are listed for a wildcard bind.
const LISTED: usize = 8;

/// A `--web-allow-host` value: a DNS name as a `Host` header carries it,
/// without a port. IP literals need no allowing; `localhost` is always in.
pub fn host_name(text: &str) -> Result<String, String> {
    let name = normalize(text);
    let label_ok = |label: &str| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    };
    if name.is_empty() || name.len() > 253 || !name.split('.').all(label_ok) {
        return Err(format!(
            "{text:?} is not a host name (letters, digits, '-' and '.', no port)"
        ));
    }
    if name.parse::<IpAddr>().is_ok() {
        return Err(format!(
            "{text:?} is an IP address, which needs no allowing"
        ));
    }
    Ok(name)
}

/// Lowercase, without the trailing dot of a fully qualified name.
pub fn normalize(name: &str) -> String {
    name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase()
}

/// The names the host check accepts beyond `localhost` and IP literals:
/// `--web-host` when it is a name, then each `--web-allow-host`.
pub fn allowed_names(host: &str, allow: &[String]) -> Vec<String> {
    let mut names = Vec::new();
    let host = normalize(host);
    if host != "localhost" && host.parse::<IpAddr>().is_err() {
        names.push(host);
    }
    for name in allow {
        let name = normalize(name);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// Every `host:port` the console answers on, the one to open locally first:
/// the bound address (loopback for a wildcard, the name `--web-host` gave
/// when it gave one), then, for a wildcard, the machine's own addresses, then
/// the allowed names. Never empty.
pub fn authorities(
    host: &str,
    listener: &tokio::net::TcpListener,
    local: SocketAddr,
    names: &[String],
) -> Vec<String> {
    let interfaces = if local.ip().is_unspecified() {
        let dual = local.is_ipv6() && !v6_only(listener);
        reachable(local.ip(), dual, &interfaces())
    } else {
        Vec::new()
    };
    list(host, local, &interfaces, names)
}

fn list(host: &str, local: SocketAddr, interfaces: &[IpAddr], names: &[String]) -> Vec<String> {
    let port = local.port();
    let primary = match local.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    let host = normalize(host);
    let mut all = vec![if host == "localhost" || names.contains(&host) {
        format!("{host}:{port}")
    } else {
        authority(primary, port)
    }];
    for entry in interfaces
        .iter()
        .map(|ip| authority(*ip, port))
        .chain(names.iter().map(|name| format!("{name}:{port}")))
    {
        if !all.contains(&entry) {
            all.push(entry);
        }
    }
    all
}

fn authority(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(ip) => format!("{ip}:{port}"),
        IpAddr::V6(ip) => format!("[{ip}]:{port}"),
    }
}

/// Of the machine's addresses, the ones a wildcard bound on `bound` answers
/// on and another machine could use: no loopback, no link-local (a browser
/// cannot put the zone in a URL), IPv4 on an IPv6 wildcard only when the
/// socket is dual-stack.
fn reachable(bound: IpAddr, dual: bool, interfaces: &[IpAddr]) -> Vec<IpAddr> {
    let usable = |ip: &IpAddr| match ip {
        IpAddr::V4(ip) => {
            (bound.is_ipv4() || dual)
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_unspecified()
        }
        IpAddr::V6(ip) => {
            bound.is_ipv6()
                && !ip.is_loopback()
                && !ip.is_unspecified()
                && !ip.is_unicast_link_local()
                && ip.to_ipv4_mapped().is_none()
        }
    };
    let mut listed: Vec<IpAddr> = Vec::new();
    // IPv4 first: the shorter link, and the one people type.
    for ip in interfaces
        .iter()
        .filter(|ip| ip.is_ipv4())
        .chain(interfaces.iter().filter(|ip| ip.is_ipv6()))
        .filter(|ip| usable(ip))
    {
        if !listed.contains(ip) && listed.len() < LISTED {
            listed.push(*ip);
        }
    }
    listed
}

/// Whether an IPv6 listener refuses IPv4: the system default decides when
/// the socket does not say.
fn v6_only(listener: &tokio::net::TcpListener) -> bool {
    let mut value: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    let status = unsafe {
        libc::getsockopt(
            listener.as_raw_fd(),
            libc::IPPROTO_IPV6,
            libc::IPV6_V6ONLY,
            (&raw mut value).cast(),
            &mut size,
        )
    };
    status != 0 || value != 0
}

/// The addresses of the interfaces that are up and running.
fn interfaces() -> Vec<IpAddr> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut entry = head;
    while let Some(ifa) = unsafe { entry.as_ref() } {
        entry = ifa.ifa_next;
        let flags = ifa.ifa_flags as libc::c_int;
        if flags & libc::IFF_UP == 0 || flags & libc::IFF_RUNNING == 0 || ifa.ifa_addr.is_null() {
            continue;
        }
        let family = libc::c_int::from(unsafe { (*ifa.ifa_addr).sa_family });
        if family == libc::AF_INET {
            let addr = unsafe { &*ifa.ifa_addr.cast::<libc::sockaddr_in>() };
            found.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                addr.sin_addr.s_addr,
            ))));
        } else if family == libc::AF_INET6 {
            let addr = unsafe { &*ifa.ifa_addr.cast::<libc::sockaddr_in6>() };
            found.push(IpAddr::V6(Ipv6Addr::from(addr.sin6_addr.s6_addr)));
        }
    }
    unsafe { libc::freeifaddrs(head) };
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    fn at(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    #[test]
    fn a_host_name_is_a_bare_dns_name() {
        assert_eq!(host_name("Sender-Host").unwrap(), "sender-host");
        assert_eq!(
            host_name("sender-host.tail1234.ts.net.").unwrap(),
            "sender-host.tail1234.ts.net"
        );
        for bad in [
            "",
            "sender-host:8790",
            "*.lan",
            "-sender-host",
            "sp ark",
            "10.0.0.2",
            "a..b",
            "http://sender-host",
        ] {
            assert!(host_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn web_host_counts_as_allowed_when_it_is_a_name() {
        let allow = vec![
            "sender-host".to_string(),
            "SENDER-HOST".to_string(),
            "box.lan".to_string(),
        ];
        assert_eq!(allowed_names("0.0.0.0", &allow), ["sender-host", "box.lan"]);
        assert_eq!(allowed_names("Box.Lan.", &[]), ["box.lan"]);
        assert!(allowed_names("localhost", &[]).is_empty());
        assert!(allowed_names("::", &[]).is_empty());
    }

    #[test]
    fn a_wildcard_lists_loopback_then_what_others_can_reach() {
        let interfaces = [
            ip("127.0.0.1"),
            ip("192.168.0.10"),
            ip("169.254.1.1"),
            ip("::1"),
            ip("fe80::1"),
            ip("fd7a:115c::5"),
            ip("100.64.0.7"),
            ip("192.168.0.10"),
        ];
        assert_eq!(
            reachable(ip("0.0.0.0"), false, &interfaces),
            [ip("192.168.0.10"), ip("100.64.0.7")]
        );
        assert_eq!(
            reachable(ip("::"), false, &interfaces),
            [ip("fd7a:115c::5")]
        );
        assert_eq!(
            reachable(ip("::"), true, &interfaces),
            [ip("192.168.0.10"), ip("100.64.0.7"), ip("fd7a:115c::5")]
        );
        let many: Vec<IpAddr> = (1..=20)
            .map(|n| IpAddr::V4(Ipv4Addr::new(10, 0, 0, n)))
            .collect();
        assert_eq!(reachable(ip("0.0.0.0"), false, &many).len(), LISTED);

        let names = vec!["sender-host".to_string()];
        assert_eq!(
            list(
                "0.0.0.0",
                at("0.0.0.0:8790"),
                &[ip("192.168.0.10"), ip("fd7a::5")],
                &names
            ),
            [
                "127.0.0.1:8790",
                "192.168.0.10:8790",
                "[fd7a::5]:8790",
                "sender-host:8790"
            ]
        );
        assert_eq!(list("::", at("[::]:1"), &[], &[]), ["[::1]:1"]);
    }

    #[test]
    fn a_named_host_is_its_own_first_link() {
        let names = allowed_names("box.lan", &[]);
        assert_eq!(
            list("box.lan", at("10.0.0.2:1"), &[], &names),
            ["box.lan:1"]
        );
        assert_eq!(
            list("localhost", at("127.0.0.1:1"), &[], &[]),
            ["localhost:1"]
        );
        assert_eq!(list("10.0.0.2", at("10.0.0.2:1"), &[], &[]), ["10.0.0.2:1"]);
    }
}
