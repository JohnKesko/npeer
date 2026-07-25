//! Fetching torrent metadata for magnet links over the peer protocol
//! (BEP 9 / BEP 10), following the flow proven in the original
//! implementation: handshake with the extension bit, swap extension
//! handshakes, request metadata pieces, verify against the info hash.
//!
//! Improvements for real-world torrents: several peers are tried
//! concurrently, and metadata larger than 16 KiB is fetched piece by piece.

use std::net::SocketAddr;
use std::time::Duration;

use sha1::{Digest, Sha1};
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::peer::extension::{
    self, HANDSHAKE_ID, METADATA_PIECE_SIZE, MetadataMessage, OUR_UT_METADATA_ID,
};
use crate::peer::{self, Message};
use crate::types::{InfoHash, PeerId};

/// Overall attempt budget per peer.
const PER_PEER_TIMEOUT: Duration = Duration::from_secs(25);
/// How many peers to try at once.
const CONCURRENT_ATTEMPTS: usize = 4;
/// Refuse absurd metadata sizes announced by hostile peers.
const MAX_METADATA_SIZE: u64 = 32 * 1024 * 1024;

/// Fetch the raw `info` dictionary from any of `peers`.
pub async fn fetch_from_peers(
    peers: &[SocketAddr],
    info_hash: InfoHash,
    our_peer_id: PeerId,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    if peers.is_empty() {
        return Err(Error::Metadata("no peers to ask".into()));
    }

    let mut last_error: Option<Error> = None;
    for batch in peers.chunks(CONCURRENT_ATTEMPTS) {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let mut attempts = tokio::task::JoinSet::new();
        for &addr in batch {
            attempts.spawn(tokio::time::timeout(
                PER_PEER_TIMEOUT,
                fetch_from_one_peer(addr, info_hash, our_peer_id),
            ));
        }
        while let Some(joined) = attempts.join_next().await {
            match joined {
                Ok(Ok(Ok(metadata))) => return Ok(metadata),
                Ok(Ok(Err(err))) => last_error = Some(err),
                Ok(Err(_elapsed)) => {
                    last_error = Some(Error::Metadata("peer timed out".into()));
                }
                Err(join_err) => last_error = Some(Error::Other(join_err.to_string())),
            }
        }
    }

    Err(last_error.unwrap_or_else(|| Error::Metadata("every peer refused".into())))
}

async fn fetch_from_one_peer(
    addr: SocketAddr,
    info_hash: InfoHash,
    our_peer_id: PeerId,
) -> Result<Vec<u8>> {
    let (stream, handshake) = peer::connect(addr, info_hash, our_peer_id).await?;
    if !handshake.supports_extensions {
        return Err(Error::Metadata(format!(
            "{addr} does not support extensions"
        )));
    }
    let (mut reader, mut writer) = stream.into_split();

    // Extension handshake: we do not know the metadata size yet.
    peer::write_message(
        &mut writer,
        &Message::Extended {
            id: HANDSHAKE_ID,
            payload: extension::build_handshake(None),
        },
    )
    .await?;

    // Wait for the peer's extension handshake, skipping unrelated traffic.
    let (their_metadata_id, metadata_size) = loop {
        match peer::read_message(&mut reader).await? {
            Message::Extended {
                id: HANDSHAKE_ID,
                payload,
            } => {
                let theirs = extension::parse_handshake(&payload)?;
                let id = theirs
                    .ut_metadata_id
                    .ok_or_else(|| Error::Metadata(format!("{addr} does not offer ut_metadata")))?;
                let size = theirs.metadata_size.filter(|&s| s > 0).ok_or_else(|| {
                    Error::Metadata(format!("{addr} did not report a metadata size"))
                })?;
                if size > MAX_METADATA_SIZE {
                    return Err(Error::Metadata(format!(
                        "{addr} claims implausible metadata size {size}"
                    )));
                }
                break (id, size);
            }
            _other => continue,
        }
    };

    // Request every metadata piece; peers answer with our advertised id.
    let piece_count = metadata_size.div_ceil(METADATA_PIECE_SIZE as u64) as u32;
    let mut metadata = vec![0u8; metadata_size as usize];
    let mut received = vec![false; piece_count as usize];
    let mut received_count = 0u32;

    for piece in 0..piece_count {
        peer::write_message(
            &mut writer,
            &Message::Extended {
                id: their_metadata_id,
                payload: extension::build_metadata_request(piece),
            },
        )
        .await?;
    }

    while received_count < piece_count {
        match peer::read_message(&mut reader).await? {
            Message::Extended {
                id: OUR_UT_METADATA_ID,
                payload,
            } => match extension::parse_metadata_message(&payload)? {
                MetadataMessage::Data { piece, data, .. } => {
                    let index = piece as usize;
                    if index >= received.len() || received[index] {
                        continue;
                    }
                    let start = index * METADATA_PIECE_SIZE;
                    let end = (start + METADATA_PIECE_SIZE).min(metadata.len());
                    if data.len() != end - start {
                        return Err(Error::Metadata(format!(
                            "{addr} sent a bad metadata piece size"
                        )));
                    }
                    metadata[start..end].copy_from_slice(&data);
                    received[index] = true;
                    received_count += 1;
                }
                MetadataMessage::Reject { .. } => {
                    return Err(Error::Metadata(format!("{addr} rejected our request")));
                }
                MetadataMessage::Request { .. } => continue,
            },
            _other => continue,
        }
    }

    // The whole point: the metadata must hash to the magnet's info hash.
    let digest: [u8; 20] = Sha1::digest(&metadata).into();
    if digest != info_hash.0 {
        return Err(Error::Metadata(format!(
            "{addr} sent metadata failing verification"
        )));
    }
    Ok(metadata)
}

/// Assemble a complete `.torrent` document from tracker tiers and the raw
/// `info` dictionary, splicing the original bytes so the info hash is
/// preserved exactly.
pub fn build_torrent_file(tracker_tiers: &[Vec<String>], info_bytes: &[u8]) -> Vec<u8> {
    let mut doc = Vec::with_capacity(info_bytes.len() + 256);
    doc.push(b'd');
    // Keys must appear in sorted order: announce, announce-list, info.
    if let Some(first) = tracker_tiers.iter().flatten().next() {
        doc.extend_from_slice(format!("8:announce{}:{first}", first.len()).as_bytes());
    }
    if !tracker_tiers.is_empty() {
        doc.extend_from_slice(b"13:announce-listl");
        for tier in tracker_tiers {
            doc.push(b'l');
            for url in tier {
                doc.extend_from_slice(format!("{}:{url}", url.len()).as_bytes());
            }
            doc.push(b'e');
        }
        doc.push(b'e');
    }
    doc.extend_from_slice(b"4:info");
    doc.extend_from_slice(info_bytes);
    doc.push(b'e');
    doc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metainfo::{Metainfo, test_support};
    use tokio::net::TcpListener;

    #[test]
    fn built_torrent_file_round_trips() {
        let info = test_support::info_dict("x.bin", 16, 16, &Sha1::digest([0u8; 16]));
        let tiers = vec![
            vec!["udp://a:1/ann".to_owned()],
            vec!["http://b/ann".to_owned()],
        ];
        let doc = build_torrent_file(&tiers, &info);

        let meta = Metainfo::from_torrent_bytes(&doc).unwrap();
        assert_eq!(meta.name, "x.bin");
        assert_eq!(meta.trackers, tiers);
        assert_eq!(meta.info_bytes, info);
        // Hash over the spliced bytes must match hashing the raw info dict.
        assert_eq!(meta.info_hash.0, <[u8; 20]>::from(Sha1::digest(&info)));
    }

    /// End-to-end: a fake peer serves 2-piece metadata over a real socket.
    #[tokio::test]
    async fn fetches_multi_piece_metadata() {
        // Build an info dict larger than one metadata piece (16 KiB).
        let name = "m".repeat(20_000);
        let info = test_support::info_dict(&name, 16, 16, &Sha1::digest([0u8; 16]));
        assert!(info.len() > METADATA_PIECE_SIZE);
        let info_hash = InfoHash(Sha1::digest(&info).into());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let info_for_peer = info.clone();

        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut socket, _) = listener.accept().await.unwrap();

            // Handshake.
            let mut hs = [0u8; 68];
            socket.read_exact(&mut hs).await.unwrap();
            hs[48..68].copy_from_slice(b"-FK0001-fakefakefake");
            socket.write_all(&hs).await.unwrap();

            // Their extension handshake, then ours (we pick id 3).
            let (mut reader, mut writer) = socket.split();
            loop {
                let msg = peer::read_message(&mut reader).await.unwrap();
                if let Message::Extended { id: 0, .. } = msg {
                    break;
                }
            }
            let mut payload = b"d1:md11:ut_metadatai3ee13:metadata_sizei".to_vec();
            payload.extend_from_slice(info_for_peer.len().to_string().as_bytes());
            payload.extend_from_slice(b"ee");
            peer::write_message(&mut writer, &Message::Extended { id: 0, payload })
                .await
                .unwrap();

            // Serve metadata requests.
            let mut served = 0;
            while served < 2 {
                let msg = peer::read_message(&mut reader).await.unwrap();
                if let Message::Extended { id: 3, payload } = msg
                    && let MetadataMessage::Request { piece } =
                        extension::parse_metadata_message(&payload).unwrap()
                {
                    let start = piece as usize * METADATA_PIECE_SIZE;
                    let end = (start + METADATA_PIECE_SIZE).min(info_for_peer.len());
                    let reply = extension::build_metadata_data(
                        piece,
                        info_for_peer.len() as u64,
                        &info_for_peer[start..end],
                    );
                    // Data messages go out under *our* advertised id (1).
                    peer::write_message(
                        &mut writer,
                        &Message::Extended {
                            id: OUR_UT_METADATA_ID,
                            payload: reply,
                        },
                    )
                    .await
                    .unwrap();
                    served += 1;
                }
            }
        });

        let fetched = fetch_from_peers(
            &[addr],
            info_hash,
            PeerId::generate(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(fetched, info);
    }
}
