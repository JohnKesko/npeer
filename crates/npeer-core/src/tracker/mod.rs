//! Tracker communication: HTTP(S) announces (BEP 3) and UDP announces (BEP 15).

mod http;
mod udp;

use std::net::SocketAddr;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::types::{InfoHash, PeerId};

/// Progress values reported to the tracker with each announce.
#[derive(Clone, Copy, Debug)]
pub struct AnnounceParams {
    pub info_hash: InfoHash,
    pub peer_id: PeerId,
    /// Port we tell peers to reach us on.
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub event: AnnounceEvent,
    /// How many peers we would like back.
    pub num_want: u32,
    /// Random per-session key so trackers can match announces across IPs.
    pub key: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnounceEvent {
    None,
    Started,
    Completed,
    Stopped,
}

impl AnnounceEvent {
    fn as_query_value(self) -> Option<&'static str> {
        match self {
            AnnounceEvent::None => None,
            AnnounceEvent::Started => Some("started"),
            AnnounceEvent::Completed => Some("completed"),
            AnnounceEvent::Stopped => Some("stopped"),
        }
    }

    fn as_udp_value(self) -> u32 {
        match self {
            AnnounceEvent::None => 0,
            AnnounceEvent::Completed => 1,
            AnnounceEvent::Started => 2,
            AnnounceEvent::Stopped => 3,
        }
    }
}

/// What a tracker told us.
#[derive(Clone, Debug)]
pub struct AnnounceResponse {
    /// How long to wait before the next regular announce.
    pub interval: Duration,
    pub min_interval: Option<Duration>,
    pub peers: Vec<SocketAddr>,
    /// Seeder count for the whole swarm, if the tracker reports it.
    pub seeders: Option<u32>,
    /// Leecher count for the whole swarm, if the tracker reports it.
    pub leechers: Option<u32>,
}

/// Announce to a single tracker URL, dispatching on its scheme.
pub async fn announce(url: &str, params: &AnnounceParams) -> Result<AnnounceResponse> {
    let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
    match scheme.as_str() {
        "http" | "https" => http::announce(url, params).await,
        "udp" => udp::announce(url, params).await,
        other => Err(Error::Tracker(format!(
            "unsupported tracker scheme `{other}` in {url}"
        ))),
    }
}

/// Parse the compact IPv4 peer format: 6 bytes per peer (4 IP + 2 port).
pub(crate) fn parse_compact_v4(bytes: &[u8]) -> Vec<SocketAddr> {
    bytes
        .as_chunks::<6>()
        .0
        .iter()
        .map(|c| {
            let ip = std::net::Ipv4Addr::new(c[0], c[1], c[2], c[3]);
            let port = u16::from_be_bytes([c[4], c[5]]);
            SocketAddr::from((ip, port))
        })
        .filter(|addr| addr.port() != 0)
        .collect()
}

/// Parse the compact IPv6 peer format: 18 bytes per peer (16 IP + 2 port).
pub(crate) fn parse_compact_v6(bytes: &[u8]) -> Vec<SocketAddr> {
    bytes
        .as_chunks::<18>()
        .0
        .iter()
        .map(|c| {
            let ip_bytes: [u8; 16] = c[..16].try_into().unwrap();
            let ip = std::net::Ipv6Addr::from(ip_bytes);
            let port = u16::from_be_bytes([c[16], c[17]]);
            SocketAddr::from((ip, port))
        })
        .filter(|addr| addr.port() != 0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_v4_parsing() {
        let bytes = [127, 0, 0, 1, 0x1A, 0xE1, 10, 0, 0, 2, 0, 80];
        let peers = parse_compact_v4(&bytes);
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].to_string(), "127.0.0.1:6881");
        assert_eq!(peers[1].to_string(), "10.0.0.2:80");
    }

    #[test]
    fn compact_v4_skips_zero_ports_and_partial_chunks() {
        let bytes = [127, 0, 0, 1, 0, 0, 1, 2, 3];
        assert!(parse_compact_v4(&bytes).is_empty());
    }

    #[test]
    fn compact_v6_parsing() {
        let mut bytes = [0u8; 18];
        bytes[15] = 1; // ::1
        bytes[16..18].copy_from_slice(&6881u16.to_be_bytes());
        let peers = parse_compact_v6(&bytes);
        assert_eq!(peers[0].to_string(), "[::1]:6881");
    }
}
