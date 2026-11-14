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
    hosts_for_identity(&discover(), port, scheme, domains)
}

/// LAN destination addresses only: shared CGNAT/public/loopback addresses are not RFC1918.
fn lan_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_private(),
        IpAddr::V6(address) => address.is_unique_local() || address.is_unicast_link_local(),
    }
}

#[cfg(any(unix, test))]
fn lan_interface(name: &str, point_to_point: bool, loopback: bool, tuntap: bool) -> bool {
    let name = name.to_ascii_lowercase();
    !point_to_point
        && !loopback
        && !tuntap
        && ![
            "tailscale",
            "tun",
            "utun",
            "tap",
            "wg",
            "ppp",
            "zt",
            "ipip",
            "sit",
            "gre",
            "erspan",
            "ip6tnl",
            "vti",
            "vxlan",
            "vpn",
        ]
        .iter()
        .any(|prefix| name.starts_with(*prefix))
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
        names.insert(short.clone());
        names.insert(format!("{short}.local"));
    }
    for address in &identity.addresses {
        if !lan_address(*address) {
            continue;
        }
        names.insert(match address {
            IpAddr::V4(_) => address.to_string(),
            IpAddr::V6(_) => format!("[{address}]"),
        });
    }
    // Preserve only DNS authorities explicitly trusted before this change, at this port.
    // Old IP entries cannot bypass current LAN interface/range checks; no resolver suffixes.
    let default_port = match scheme {
        RequestScheme::Http => 80,
        RequestScheme::Https => 443,
    };
    for authority in domains {
        if let Some((name, explicit_port)) = crate::terminal_stream::terminal_authority(authority) {
            if name.parse::<IpAddr>().is_err() && explicit_port.unwrap_or(default_port) == port {
                if let Some(name) = hostname(&name) {
                    names.insert(name);
                }
            }
        }
    }
    if names.is_empty() {
        return Err("local terminal authority discovery unavailable".into());
    }

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
    let mut identity = LocalIdentity {
        hostname: nix::unistd::gethostname()
            .ok()
            .and_then(|name| name.into_string().ok()),
        addresses: Vec::new(),
    };
    // nix owns/frees the interface snapshot and keeps all FFI outside this unsafe-free crate.
    let Ok(interfaces) = nix::ifaddrs::getifaddrs() else {
        return identity;
    };
    for interface in interfaces {
        use nix::net::if_::InterfaceFlags;
        let tuntap = std::path::Path::new("/sys/class/net")
            .join(&interface.interface_name)
            .join("tun_flags")
            .exists();
        if !lan_interface(
            &interface.interface_name,
            interface.flags.contains(InterfaceFlags::IFF_POINTOPOINT),
            interface.flags.contains(InterfaceFlags::IFF_LOOPBACK),
            tuntap,
        ) {
            continue;
        }
        let Some(address) = interface.address else {
            continue;
        };
        if let Some(address) = address.as_sockaddr_in() {
            identity.addresses.push(address.ip().into());
        } else if let Some(address) = address.as_sockaddr_in6() {
            identity.addresses.push(address.ip().into());
        }
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
    fn loopback_listener_is_not_automatically_a_lan_authority() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let hosts = local_hosts(address.port(), RequestScheme::Http, &[]).unwrap();
        assert!(!hosts.iter().any(|host| host == &address.to_string()));
    }

    #[test]
    fn local_aliases_interfaces_and_listener_port_define_authorities() {
        let identity = LocalIdentity {
            hostname: Some("Cell.lan".into()),
            addresses: vec!["192.168.0.209".parse().unwrap(), "fe80::1".parse().unwrap()],
        };
        let hosts = hosts_for_identity(
            &identity,
            8787,
            RequestScheme::Http,
            &[
                "cell.home.arpa:8787".into(),
                "cell.lan:8787".into(),
                "evil/path".into(),
            ],
        )
        .unwrap();
        for expected in [
            "cell:8787",
            "cell.local:8787",
            "cell.lan:8787",
            "cell.home.arpa:8787",
            "192.168.0.209:8787",
            "[fe80::1]:8787",
        ] {
            assert!(hosts.iter().any(|host| host == expected), "{expected}");
        }
        for rejected in [
            "cell",
            "192.168.0.209",
            "foreign:8787",
            "cell.evil/path:8787",
            "cell:80",
        ] {
            assert!(!hosts.iter().any(|host| host == rejected), "{rejected}");
        }
        for (scheme, port) in [(RequestScheme::Http, 80), (RequestScheme::Https, 443)] {
            let hosts = hosts_for_identity(&identity, port, scheme, &[]).unwrap();
            assert!(hosts.iter().any(|host| host == "cell"));
            assert!(hosts.iter().any(|host| host == "192.168.0.209"));
        }
        assert!(
            !hosts_for_identity(&identity, 443, RequestScheme::Http, &[])
                .unwrap()
                .iter()
                .any(|host| host == "cell")
        );
    }
    #[test]
    fn lan_scope_excludes_tailnet_search_names_and_tunnel_interfaces() {
        let identity = LocalIdentity {
            hostname: Some("cell".into()),
            addresses: vec![
                "192.168.0.209".parse().unwrap(),
                "100.91.92.49".parse().unwrap(),
                "203.0.113.9".parse().unwrap(),
                "127.0.0.1".parse().unwrap(),
            ],
        };
        let hosts = hosts_for_identity(&identity, 8787, RequestScheme::Http, &[]).unwrap();
        assert!(hosts.iter().any(|host| host == "192.168.0.209:8787"));
        assert!(hosts.iter().any(|host| host == "cell.local:8787"));
        for rejected in [
            "100.91.92.49:8787",
            "203.0.113.9:8787",
            "127.0.0.1:8787",
            "cell.tail68fe78.ts.net:8787",
        ] {
            assert!(!hosts.iter().any(|host| host == rejected), "{rejected}");
        }
        // The exception is an existing explicit DNS authorization, never a discovered suffix.
        assert!(hosts_for_identity(
            &identity,
            8787,
            RequestScheme::Http,
            &["cell.tail68fe78.ts.net:8787".into()]
        )
        .unwrap()
        .iter()
        .any(|host| host == "cell.tail68fe78.ts.net:8787"));
        assert!(lan_interface("enp0s31f6", false, false, false));
        for name in ["tailscale0", "tun0", "tap0", "wg0", "utun1", "vxlan10"] {
            assert!(!lan_interface(name, false, false, false), "{name}");
        }
        assert!(!lan_interface("renamed0", true, false, false));
        assert!(!lan_interface("renamed0", false, false, true));
        assert!(!lan_interface("lo", false, true, false));
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
