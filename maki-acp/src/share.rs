//! Opening a listener for a session that is already running, so a window on
//! another machine can watch it without the agent restarting.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::Path;

/// Client to server: listen on the LAN for this session, answering with the
/// link and the token another machine needs.
pub const SHARE_METHOD: &str = "maki/share";
/// Client to server: stop the listener this connection opened.
pub const UNSHARE_METHOD: &str = "maki/unshare";
/// Answers a share request that could not bind or read the token.
pub const SHARE_FAILED_CODE: i32 = -32013;
/// Answers a share request from a connection that only watches the session.
pub const NOT_OWNER_CODE: i32 = -32014;
pub const NOT_OWNER_MESSAGE: &str = "only the connection that owns the session can share it";
/// The token a client presents: the one the environment overrides, then the
/// one stored for this machine.
pub const TOKEN_ENV: &str = "MAKIMA_LAN_TOKEN";
/// The scheme a client puts in front of {host}:{port}.
pub const LINK_SCHEME: &str = "lan://";

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

/// What a remote window sets as its agent address.
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
    fn a_token_is_read_from_the_state_directory() {
        let state = tempfile::tempdir().unwrap();
        let stored = token(state.path()).unwrap();
        assert_eq!(stored.len(), 32);
        assert_eq!(stored, token(state.path()).unwrap());
    }

    #[test]
    fn reachable_replaces_a_wildcard_with_a_route_address() {
        let reachable = reachable((Ipv4Addr::UNSPECIFIED, 4000).into());
        assert_eq!(reachable.port(), 4000);
        assert!(!reachable.ip().is_unspecified());
    }
}
