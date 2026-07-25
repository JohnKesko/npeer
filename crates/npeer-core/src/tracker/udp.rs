//! UDP tracker announces (BEP 15).
//!
//! Most magnet links in the wild only carry `udp://` trackers, so this is
//! required for real-world use even though the original implementation only
//! spoke HTTP.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::UdpSocket;

use crate::error::{Error, Result};

use super::{AnnounceParams, AnnounceResponse, parse_compact_v4};

const PROTOCOL_ID: u64 = 0x0417_2710_1980;
const ACTION_CONNECT: u32 = 0;
const ACTION_ANNOUNCE: u32 = 1;
const ACTION_ERROR: u32 = 3;

/// Per-attempt timeouts. BEP 15 suggests 15s doubling; two brisk attempts
/// keep the UI responsive while still tolerating one dropped datagram.
const ATTEMPT_TIMEOUTS: [Duration; 2] = [Duration::from_secs(6), Duration::from_secs(12)];

pub(super) async fn announce(url: &str, params: &AnnounceParams) -> Result<AnnounceResponse> {
    let target = resolve(url).await?;
    let socket = UdpSocket::bind(if target.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })
    .await
    .map_err(|e| Error::Tracker(format!("{url}: binding UDP socket: {e}")))?;
    socket
        .connect(target)
        .await
        .map_err(|e| Error::Tracker(format!("{url}: {e}")))?;

    let mut last_error = None;
    for timeout in ATTEMPT_TIMEOUTS {
        match attempt(&socket, params, timeout).await {
            Ok(response) => return Ok(response),
            Err(err) => last_error = Some(err),
        }
    }
    Err(match last_error {
        Some(Error::Tracker(msg)) => Error::Tracker(format!("{url}: {msg}")),
        Some(other) => other,
        None => Error::Tracker(format!("{url}: announce failed")),
    })
}

async fn attempt(
    socket: &UdpSocket,
    params: &AnnounceParams,
    timeout: Duration,
) -> Result<AnnounceResponse> {
    // Step 1: connect request -> connection id.
    let transaction_id: u32 = rand::random();
    let mut connect_request = Vec::with_capacity(16);
    connect_request.extend_from_slice(&PROTOCOL_ID.to_be_bytes());
    connect_request.extend_from_slice(&ACTION_CONNECT.to_be_bytes());
    connect_request.extend_from_slice(&transaction_id.to_be_bytes());

    let reply = exchange(socket, &connect_request, timeout).await?;
    if reply.len() < 16 {
        return Err(tracker_err("connect response too short"));
    }
    check_header(&reply, ACTION_CONNECT, transaction_id)?;
    let connection_id = u64::from_be_bytes(reply[8..16].try_into().unwrap());

    // Step 2: announce request.
    let transaction_id: u32 = rand::random();
    let mut request = Vec::with_capacity(98);
    request.extend_from_slice(&connection_id.to_be_bytes());
    request.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
    request.extend_from_slice(&transaction_id.to_be_bytes());
    request.extend_from_slice(params.info_hash.as_bytes());
    request.extend_from_slice(params.peer_id.as_bytes());
    request.extend_from_slice(&params.downloaded.to_be_bytes());
    request.extend_from_slice(&params.left.to_be_bytes());
    request.extend_from_slice(&params.uploaded.to_be_bytes());
    request.extend_from_slice(&params.event.as_udp_value().to_be_bytes());
    request.extend_from_slice(&0u32.to_be_bytes()); // IP: default (sender address)
    request.extend_from_slice(&params.key.to_be_bytes());
    request.extend_from_slice(&(params.num_want as i32).to_be_bytes());
    request.extend_from_slice(&params.port.to_be_bytes());

    let reply = exchange(socket, &request, timeout).await?;
    if reply.len() < 20 {
        return Err(tracker_err("announce response too short"));
    }
    check_header(&reply, ACTION_ANNOUNCE, transaction_id)?;

    let interval = u32::from_be_bytes(reply[8..12].try_into().unwrap());
    let leechers = u32::from_be_bytes(reply[12..16].try_into().unwrap());
    let seeders = u32::from_be_bytes(reply[16..20].try_into().unwrap());
    let peers = parse_compact_v4(&reply[20..]);

    Ok(AnnounceResponse {
        interval: Duration::from_secs(u64::from(interval.max(60))),
        min_interval: None,
        peers,
        seeders: Some(seeders),
        leechers: Some(leechers),
    })
}

async fn exchange(socket: &UdpSocket, request: &[u8], timeout: Duration) -> Result<Vec<u8>> {
    socket
        .send(request)
        .await
        .map_err(|e| tracker_err(&format!("send: {e}")))?;
    let mut buffer = vec![0u8; 4096];
    let received = tokio::time::timeout(timeout, socket.recv(&mut buffer))
        .await
        .map_err(|_| tracker_err("no response"))?
        .map_err(|e| tracker_err(&format!("recv: {e}")))?;
    buffer.truncate(received);
    Ok(buffer)
}

fn check_header(reply: &[u8], expected_action: u32, expected_transaction: u32) -> Result<()> {
    let action = u32::from_be_bytes(reply[0..4].try_into().unwrap());
    let transaction = u32::from_be_bytes(reply[4..8].try_into().unwrap());
    if transaction != expected_transaction {
        return Err(tracker_err("transaction id mismatch"));
    }
    if action == ACTION_ERROR {
        let message = String::from_utf8_lossy(&reply[8..]);
        return Err(tracker_err(&format!("tracker replied: {message}")));
    }
    if action != expected_action {
        return Err(tracker_err(&format!("unexpected action {action}")));
    }
    Ok(())
}

/// `udp://host:port/...` -> resolved socket address (IPv4 preferred).
async fn resolve(url: &str) -> Result<SocketAddr> {
    let parsed =
        url::Url::parse(url).map_err(|e| Error::Tracker(format!("{url}: invalid URL: {e}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| Error::Tracker(format!("{url}: missing host")))?;
    let port = parsed
        .port()
        .ok_or_else(|| Error::Tracker(format!("{url}: missing port")))?;

    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| Error::Tracker(format!("{url}: DNS lookup failed: {e}")))?
        .collect();
    addresses
        .iter()
        .find(|a| a.is_ipv4())
        .or(addresses.first())
        .copied()
        .ok_or_else(|| Error::Tracker(format!("{url}: host has no addresses")))
}

fn tracker_err(msg: &str) -> Error {
    Error::Tracker(msg.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::AnnounceEvent;
    use crate::types::{InfoHash, PeerId};

    /// A tiny in-process UDP tracker implementing the BEP 15 handshake.
    async fn fake_tracker(socket: UdpSocket, peers: Vec<u8>) {
        let mut buf = [0u8; 1024];
        // Connect round.
        let (len, from) = socket.recv_from(&mut buf).await.unwrap();
        assert!(len >= 16);
        assert_eq!(&buf[0..8], &PROTOCOL_ID.to_be_bytes());
        let txid = &buf[12..16].to_vec();
        let mut reply = Vec::new();
        reply.extend_from_slice(&ACTION_CONNECT.to_be_bytes());
        reply.extend_from_slice(txid);
        reply.extend_from_slice(&0xDEADBEEFu64.to_be_bytes());
        socket.send_to(&reply, from).await.unwrap();

        // Announce round.
        let (len, from) = socket.recv_from(&mut buf).await.unwrap();
        assert_eq!(len, 98);
        assert_eq!(&buf[0..8], &0xDEADBEEFu64.to_be_bytes());
        let txid = &buf[12..16].to_vec();
        let mut reply = Vec::new();
        reply.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
        reply.extend_from_slice(txid);
        reply.extend_from_slice(&1800u32.to_be_bytes());
        reply.extend_from_slice(&3u32.to_be_bytes()); // leechers
        reply.extend_from_slice(&9u32.to_be_bytes()); // seeders
        reply.extend_from_slice(&peers);
        socket.send_to(&reply, from).await.unwrap();
    }

    #[tokio::test]
    async fn announces_against_fake_tracker() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = server.local_addr().unwrap().port();
        let peers = vec![127, 0, 0, 1, 0x1A, 0xE1];
        tokio::spawn(fake_tracker(server, peers));

        let params = AnnounceParams {
            info_hash: InfoHash([1; 20]),
            peer_id: PeerId(*b"-NP0100-abcdefghijkl"),
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: 100,
            event: AnnounceEvent::Started,
            num_want: 50,
            key: 42,
        };
        let response = announce(&format!("udp://127.0.0.1:{port}/announce"), &params)
            .await
            .unwrap();
        assert_eq!(response.seeders, Some(9));
        assert_eq!(response.leechers, Some(3));
        assert_eq!(response.peers[0].to_string(), "127.0.0.1:6881");
        assert_eq!(response.interval, Duration::from_secs(1800));
    }
}
