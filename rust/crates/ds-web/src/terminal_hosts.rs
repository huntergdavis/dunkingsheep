//! Terminal authorities belong to this machine and the listener's actual port.
//! Discover interfaces on each request so DHCP changes cannot leave stale IPs trusted.
use crate::routes::auth::RequestScheme;
use std::{collections::BTreeSet, net::IpAddr};

#[derive(Default)]
pub(crate) struct LocalIdentity {
    pub hostname: Option<String>,
    pub addresses: Vec<IpAddr>,
}

pub(crate) fn local_hosts(
    port: u16,
    scheme: RequestScheme,
    domains: &[String],
) -> Result<Vec<String>, String> {
    let identity = discover();
    let mut domains = domains.to_vec();
    // These are locally configured search suffixes, not DNS answers for an untrusted Host.
    if let Ok(resolver) = std::fs::read_to_string("/etc/resolv.conf") {
        for line in resolver.lines() {
            let mut words = line
                .split(['#', ';'])
                .next()
                .unwrap_or_default()
                .split_whitespace();
            if matches!(words.next(), Some("search" | "domain")) {
                domains.extend(words.map(str::to_owned));
            }
        }
    }
    hosts_for_identity(&identity, port, scheme, &domains)
}

fn hostname(value: &str) -> Option<String> {
    let value = value.trim().trim_end_matches('.').to_ascii_lowercase();
    if value.parse::<IpAddr>().is_ok()
        || value.is_empty()
        || value.len() > 253
        || value.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        })
    {
        return None;
    }
    Some(value)
}

pub(crate) fn hosts_for_identity(
    identity: &LocalIdentity,
    port: u16,
    scheme: RequestScheme,
    domains: &[String],
) -> Result<Vec<String>, String> {
    if port == 0 {
        return Err("terminal listener port is unavailable".into());
    }
    let mut names = BTreeSet::new();
    if let Some(full) = identity.hostname.as_deref().and_then(hostname) {
        let short = full.split('.').next().unwrap().to_owned();
        names.insert(full);
        names.insert(short.clone());
        names.insert(format!("{short}.local"));
        for domain in domains.iter().filter_map(|domain| hostname(domain)) {
            if let Some(alias) = hostname(&format!("{short}.{domain}")) {
                names.insert(alias);
            }
        }
    }
    for address in &identity.addresses {
        if address.is_unspecified() || address.is_multicast() {
            continue;
        }
        names.insert(match address {
            IpAddr::V4(_) => address.to_string(),
            IpAddr::V6(_) => format!("[{address}]"),
        });
    }
    if names.is_empty() {
        return Err("local terminal authority discovery unavailable".into());
    }
    let default_port = match scheme {
        RequestScheme::Http => 80,
        RequestScheme::Https => 443,
    };
    let mut hosts = Vec::new();
    for name in names {
        hosts.push(format!("{name}:{port}"));
        if port == default_port {
            hosts.push(name);
        }
    }
    Ok(hosts)
}

#[cfg(unix)]
fn discover() -> LocalIdentity {
    let mut identity = LocalIdentity::default();
    // Buffers and interface lists are owned here; no request value reaches a syscall.
    let mut name = [0_u8; 256];
    if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } == 0 {
        let length = name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(name.len());
        identity.hostname = std::str::from_utf8(&name[..length]).ok().map(str::to_owned);
    }
    struct Interfaces(*mut libc::ifaddrs);
    impl Drop for Interfaces {
        fn drop(&mut self) {
            unsafe { libc::freeifaddrs(self.0) };
        }
    }
    let mut first = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut first) } != 0 {
        return identity;
    }
    let interfaces = Interfaces(first);
    let mut next = interfaces.0;
    while let Some(interface) = unsafe { next.as_ref() } {
        if let Some(address) = unsafe { interface.ifa_addr.as_ref() } {
            match i32::from(address.sa_family) {
                libc::AF_INET => {
                    let address = unsafe { &*interface.ifa_addr.cast::<libc::sockaddr_in>() };
                    identity.addresses.push(
                        std::net::Ipv4Addr::from(address.sin_addr.s_addr.to_ne_bytes()).into(),
                    );
                }
                libc::AF_INET6 => {
                    let address = unsafe { &*interface.ifa_addr.cast::<libc::sockaddr_in6>() };
                    identity
                        .addresses
                        .push(std::net::Ipv6Addr::from(address.sin6_addr.s6_addr).into());
                }
                _ => {}
            }
        }
        next = interface.ifa_next;
    }
    identity
}

#[cfg(not(unix))]
fn discover() -> LocalIdentity {
    LocalIdentity::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn interface_discovery_recognizes_a_real_private_loopback_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let hosts = local_hosts(address.port(), RequestScheme::Http, &[]).unwrap();
        assert!(hosts.iter().any(|host| host == &address.to_string()));
    }

    #[test]
    fn local_aliases_interfaces_and_listener_port_define_authorities() {
        let identity = LocalIdentity {
            hostname: Some("Cell.lan".into()),
            addresses: vec!["192.0.2.8".parse().unwrap(), "::1".parse().unwrap()],
        };
        let hosts = hosts_for_identity(
            &identity,
            8787,
            RequestScheme::Http,
            &["home.arpa".into(), "evil/path".into()],
        )
        .unwrap();
        for expected in [
            "cell:8787",
            "cell.local:8787",
            "cell.lan:8787",
            "cell.home.arpa:8787",
            "192.0.2.8:8787",
            "[::1]:8787",
        ] {
            assert!(hosts.iter().any(|host| host == expected), "{expected}");
        }
        for rejected in [
            "cell",
            "192.0.2.8",
            "foreign:8787",
            "cell.evil/path:8787",
            "cell:80",
        ] {
            assert!(!hosts.iter().any(|host| host == rejected), "{rejected}");
        }
        for (scheme, port) in [(RequestScheme::Http, 80), (RequestScheme::Https, 443)] {
            let hosts = hosts_for_identity(&identity, port, scheme, &[]).unwrap();
            assert!(hosts.iter().any(|host| host == "cell"));
            assert!(hosts.iter().any(|host| host == "192.0.2.8"));
        }
        assert!(
            !hosts_for_identity(&identity, 443, RequestScheme::Http, &[])
                .unwrap()
                .iter()
                .any(|host| host == "cell")
        );
    }
    #[test]
    fn stale_or_unavailable_interface_data_does_not_create_trust() {
        assert!(
            hosts_for_identity(&LocalIdentity::default(), 8787, RequestScheme::Http, &[]).is_err()
        );
        let identity = LocalIdentity {
            hostname: None,
            addresses: vec!["0.0.0.0".parse().unwrap(), "ff02::1".parse().unwrap()],
        };
        assert!(hosts_for_identity(&identity, 8787, RequestScheme::Http, &[]).is_err());
    }
}
