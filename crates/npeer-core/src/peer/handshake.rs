//! The 68-byte BitTorrent handshake.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::{Error, Result};
use crate::types::{InfoHash, PeerId};

const PROTOCOL: &[u8; 19] = b"BitTorrent protocol";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// What the remote peer sent back in its handshake.
#[derive(Clone, Copy, Debug)]
pub struct Handshake {
    pub peer_id: PeerId,
    /// Extension protocol support (BEP 10), needed for magnet metadata.
    pub supports_extensions: bool,
}

/// Connect to `addr` and exchange handshakes.
///
/// Layout (68 bytes): 1 byte protocol length, 19 bytes protocol string,
/// 8 reserved bytes (bit 20 = extension protocol), 20 bytes info hash,
/// 20 bytes peer id.
pub async fn connect(
    addr: SocketAddr,
    info_hash: InfoHash,
    our_peer_id: PeerId,
) -> Result<(TcpStream, Handshake)> {
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| Error::Timeout(format!("connecting to {addr}")))?
        .map_err(|e| Error::Protocol(format!("connecting to {addr}: {e}")))?;
    let _ = stream.set_nodelay(true);

    tokio::time::timeout(HANDSHAKE_TIMEOUT, exchange(stream, info_hash, our_peer_id))
        .await
        .map_err(|_| Error::Timeout(format!("handshaking with {addr}")))?
}

async fn exchange(
    mut stream: TcpStream,
    info_hash: InfoHash,
    our_peer_id: PeerId,
) -> Result<(TcpStream, Handshake)> {
    let mut ours = Vec::with_capacity(68);
    ours.push(19u8);
    ours.extend_from_slice(PROTOCOL);
    let mut reserved = [0u8; 8];
    reserved[5] |= 0x10; // we always advertise extension protocol support
    ours.extend_from_slice(&reserved);
    ours.extend_from_slice(info_hash.as_bytes());
    ours.extend_from_slice(our_peer_id.as_bytes());
    stream.write_all(&ours).await?;
    stream.flush().await?;

    let mut theirs = [0u8; 68];
    stream.read_exact(&mut theirs).await?;

    if theirs[0] != 19 || &theirs[1..20] != PROTOCOL {
        return Err(Error::protocol(
            "peer does not speak the BitTorrent protocol",
        ));
    }
    if theirs[28..48] != info_hash.0 {
        return Err(Error::protocol("peer answered for a different torrent"));
    }

    let supports_extensions = theirs[25] & 0x10 != 0;
    let peer_id = PeerId(theirs[48..68].try_into().unwrap());
    Ok((
        stream,
        Handshake {
            peer_id,
            supports_extensions,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn handshake_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let info_hash = InfoHash([9; 20]);

        // Fake remote peer: echo a valid handshake with the extension bit set.
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut incoming = [0u8; 68];
            socket.read_exact(&mut incoming).await.unwrap();
            assert_eq!(incoming[0], 19);
            assert_eq!(&incoming[28..48], &[9; 20]);
            assert_eq!(incoming[25] & 0x10, 0x10, "we must advertise extensions");

            let mut reply = incoming;
            reply[48..68].copy_from_slice(b"-XX0000-remotepeerid");
            socket.write_all(&reply).await.unwrap();
        });

        let (_stream, handshake) = connect(addr, info_hash, PeerId::generate()).await.unwrap();
        assert!(handshake.supports_extensions);
        assert_eq!(&handshake.peer_id.0, b"-XX0000-remotepeerid");
    }

    #[tokio::test]
    async fn rejects_wrong_info_hash() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut incoming = [0u8; 68];
            socket.read_exact(&mut incoming).await.unwrap();
            incoming[28..48].copy_from_slice(&[0xEE; 20]); // different torrent
            socket.write_all(&incoming).await.unwrap();
        });

        let err = connect(addr, InfoHash([9; 20]), PeerId::generate())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("different torrent"));
    }
}
