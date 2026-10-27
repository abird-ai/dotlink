use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

/// Trusted network requester boundary used only for anonymous-work admission.
/// It is never persisted, logged, or treated as an OAuth principal.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(crate) enum RequesterId {
    Local,
    Ipv4(Ipv4Addr),
    /// One identity per /48 covers common /56 and /64 delegations, preventing
    /// cheap privacy-address and delegated-subnet rotation.
    Ipv6Prefix([u8; 6]),
}

impl fmt::Debug for RequesterId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RequesterId(<redacted>)")
    }
}

impl RequesterId {
    pub(crate) fn from_socket(address: SocketAddr) -> Option<Self> {
        if address.port() == 0 {
            return None;
        }
        Self::from_ip(address.ip())
    }

    fn from_ip(ip: IpAddr) -> Option<Self> {
        let ip = match ip {
            IpAddr::V6(ip) => ip.to_ipv4_mapped().map_or(IpAddr::V6(ip), IpAddr::V4),
            ip => ip,
        };
        if ip.is_unspecified() {
            return None;
        }
        if ip.is_loopback() {
            return Some(Self::Local);
        }
        match ip {
            IpAddr::V4(ip) => Some(Self::Ipv4(ip)),
            IpAddr::V6(ip) => {
                let mut prefix = [0; 6];
                prefix.copy_from_slice(&ip.octets()[..6]);
                Some(Self::Ipv6Prefix(prefix))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requester_identity_ignores_ports_and_normalizes_network_forms() {
        assert_eq!(
            RequesterId::from_socket("192.0.2.4:1000".parse().unwrap()),
            RequesterId::from_socket("192.0.2.4:2000".parse().unwrap())
        );
        assert_eq!(
            RequesterId::from_socket("[::ffff:192.0.2.4]:1000".parse().unwrap()),
            RequesterId::from_socket("192.0.2.4:2000".parse().unwrap())
        );
        assert_eq!(
            RequesterId::from_socket("[2001:db8:1:2::1]:1000".parse().unwrap()),
            RequesterId::from_socket("[2001:db8:1:ffff::1]:2000".parse().unwrap())
        );
        assert_eq!(
            RequesterId::from_socket("127.0.0.1:1000".parse().unwrap()),
            RequesterId::from_socket("[::1]:2000".parse().unwrap())
        );
    }

    #[test]
    fn requester_debug_never_exposes_network_identity() {
        for requester in [
            RequesterId::Local,
            RequesterId::from_socket("192.0.2.44:1234".parse().unwrap()).unwrap(),
            RequesterId::from_socket("[2001:db8:abcd::1]:1234".parse().unwrap()).unwrap(),
        ] {
            let debug = format!("{requester:?}");
            assert_eq!(debug, "RequesterId(<redacted>)");
            assert!(!debug.contains("192.0.2.44"));
            assert!(!debug.contains("2001:db8"));
        }
    }

    #[test]
    fn requester_identity_rejects_invalid_transport_fallbacks() {
        assert!(RequesterId::from_socket("0.0.0.0:1234".parse().unwrap()).is_none());
        assert!(RequesterId::from_socket("192.0.2.4:0".parse().unwrap()).is_none());
    }
}
