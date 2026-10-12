//! Working out who a request came from, behind a trusted proxy or not.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};

use axum::{
    extract::{ConnectInfo, FromRequestParts},
    http::{HeaderMap, request::Parts},
};

use crate::util::net::is_public;

// The peer address, when there is one. A Unix connection has none, so this reads the extension
// directly rather than extracting ConnectInfo, and never rejects. One handler serves both.
#[derive(Debug, Clone, Copy)]
/// The connecting peer's address, or `None` over a Unix socket, which has no peer address.
pub struct PeerAddr(
    /// The address, absent on a Unix socket.
    pub Option<SocketAddr>,
);

impl PeerAddr {
    /// Who an internal API request came from: the connecting address, or loopback over the
    /// socket, where the kernel has already decided who may connect.
    ///
    /// Forwarded headers are never read here. `TRUSTED_PROXY` names the proxy in front of the
    /// callback, and a caller of the internal API that could set one would pick its own rate
    /// limit and the address its audit trail keeps.
    pub fn ip(self) -> IpAddr {
        self.0.map_or(IpAddr::from([127, 0, 0, 1]), |addr| addr.ip().to_canonical())
    }
}

impl<S: Send + Sync> FromRequestParts<S> for PeerAddr {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(PeerAddr(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(addr)| *addr),
        ))
    }
}

/// Who a callback came from, behind a reverse proxy or not.
///
/// The first address in `X-Forwarded-For` is believed only when the immediate peer is in
/// `trusted_proxies` **and** that address is publicly routable. Anything else falls back to the
/// peer, so a caller cannot pick its own address by setting a header, and a private one never
/// reaches the logs or the allowlist. `X-Real-IP` is never read: the shipped proxy block
/// overwrites `X-Forwarded-For` and leaves `X-Real-IP` as the caller sent it.
///
/// An IPv4 address written as IPv6, as a listener on `[::]` sees every IPv4 peer, is read as the
/// IPv4 address it is, so it matches the trusted proxies and the callback allowlist as written.
pub fn client_ip(headers: &HeaderMap, peer: SocketAddr, trusted_proxies: &[IpAddr]) -> IpAddr {
    let peer_ip = peer.ip().to_canonical();
    if !trusted_proxies.contains(&peer_ip) {
        return peer_ip;
    }

    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .and_then(|first| first.trim().parse::<IpAddr>().ok())
        .map(|ip| ip.to_canonical())
        .filter(|ip| is_public(*ip))
        .unwrap_or(peer_ip)
}
