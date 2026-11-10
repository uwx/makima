//! How a session is reached over the LAN: the link another machine sets as its
//! agent address, the token it presents, and the mDNS service that lets it find
//! the session without being told an address.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::Path;

pub mod discovery;

/// The token a client presents: the one the environment overrides, then the
/// one stored for this machine.
pub const TOKEN_ENV: &str = "MAKIMA_LAN_TOKEN";
/// What a client puts in front of `{host}:{port}`.
pub const LINK_SCHEME: &str = "lan://";

/// Reads or generates the token this machine shares, so a listener answers the
/// same token as the next one.
pub fn token(state: &Path) -> io::Result<String> {
    if let Ok(token) = std::env::var(TOKEN_ENV) {
        let token = token.trim();
        if !token.is_empty() {
            return Ok(token.to_owned());
        }
    }
    maki_storage::lan::token(state)
}

/// Where another machine reaches this listener: a bound wildcard address
/// becomes the one the default route leaves by, which is the one peers see.
pub fn reachable(bound: SocketAddr) -> SocketAddr {
    let ip = if bound.ip().is_unspecified() {
        lan_ip().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
    } else {
        bound.ip()
    };
    SocketAddr::new(ip, bound.port())
}

/// The agent address a remote window sets.
pub fn link(bound: SocketAddr) -> String {
    format!("{LINK_SCHEME}{}", reachable(bound))
}

/// No packet is sent: connecting a UDP socket only asks the system for the
/// route to the outside, which is the address other machines can reach.
fn lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    Some(socket.local_addr().ok()?.ip())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("127.0.0.1:4000" => "lan://127.0.0.1:4000")]
    #[test_case("[::1]:4000" => "lan://[::1]:4000")]
    fn link_keeps_the_address_it_was_given(spec: &str) -> String {
        link(spec.parse().unwrap())
    }

    #[test]
    fn a_wildcard_listener_is_reached_at_a_route_address() {
        let reachable = reachable((Ipv4Addr::UNSPECIFIED, 4000).into());
        assert_eq!(reachable.port(), 4000);
        assert!(!reachable.ip().is_unspecified());
    }

    #[test]
    fn a_token_is_stored_once_per_state_directory() {
        let state = tempfile::tempdir().unwrap();
        let stored = token(state.path()).unwrap();
        assert_eq!(stored.len(), 32);
        assert_eq!(stored, token(state.path()).unwrap());
    }
}
