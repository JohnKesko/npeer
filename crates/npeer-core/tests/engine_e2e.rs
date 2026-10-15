//! End-to-end engine test over real sockets: a local seeder speaks the peer
//! wire protocol, a fake UDP tracker (BEP 15) hands out its address, and the
//! engine downloads, verifies and completes — including pause/resume.

use std::sync::Arc;
use std::time::Duration;

use sha1::{Digest, Sha1};
use tokio::net::{TcpListener, UdpSocket};

use npeer_core::FileSelection;
use npeer_core::metainfo::Metainfo;
use npeer_core::peer::{Message, read_message, write_message};
use npeer_core::session::{Engine, EngineOptions, StopReason};
use npeer_core::storage::Storage;

/// Build a multi-file torrent document over `data` split into two files.
fn build_test_torrent(data: &[u8], piece_length: usize, announce: &str) -> Vec<u8> {
    let first_len = data.len() * 2 / 3;
    let second_len = data.len() - first_len;
    let mut pieces = Vec::new();
    for chunk in data.chunks(piece_length) {
        pieces.extend_from_slice(&Sha1::digest(chunk));
    }

    let mut info = Vec::new();
    info.extend_from_slice(b"d5:filesl");
    info.extend_from_slice(format!("d6:lengthi{first_len}e4:pathl5:a.binee").as_bytes());
    info.extend_from_slice(format!("d6:lengthi{second_len}e4:pathl3:sub5:b.bineee").as_bytes());
    info.extend_from_slice(b"4:name8:testdata12:piece length");
    info.extend_from_slice(format!("i{piece_length}e").as_bytes());
    info.extend_from_slice(format!("6:pieces{}:", pieces.len()).as_bytes());
    info.extend_from_slice(&pieces);
    info.extend_from_slice(b"e");

    let mut doc = Vec::new();
    doc.extend_from_slice(format!("d8:announce{}:{announce}", announce.len()).as_bytes());
    doc.extend_from_slice(b"4:info");
    doc.extend_from_slice(&info);
    doc.extend_from_slice(b"e");
    doc
}

/// A minimal seeder: full bitfield, unchokes on interest, serves blocks.
async fn run_seeder(listener: TcpListener, metainfo: Arc<Metainfo>, data: Arc<Vec<u8>>) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let metainfo = metainfo.clone();
        let data = data.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let mut handshake = [0u8; 68];
            if socket.read_exact(&mut handshake).await.is_err() {
                return;
            }
            assert_eq!(&handshake[28..48], metainfo.info_hash.as_bytes());
            handshake[48..68].copy_from_slice(b"-SD0001-seederseeder");
            if socket.write_all(&handshake).await.is_err() {
                return;
            }

            let (mut reader, mut writer) = socket.split();
            let piece_count = metainfo.piece_count() as usize;
            let mut bitfield = vec![0u8; piece_count.div_ceil(8)];
            for piece in 0..piece_count {
                bitfield[piece / 8] |= 0x80 >> (piece % 8);
            }
            if write_message(&mut writer, &Message::Bitfield(bitfield))
                .await
                .is_err()
            {
                return;
            }

            loop {
                let message = match read_message(&mut reader).await {
                    Ok(message) => message,
                    Err(_) => return, // engine disconnected
                };
                match message {
                    Message::Interested => {
                        write_message(&mut writer, &Message::Unchoke).await.ok();
                    }
                    Message::Request {
                        index,
                        begin,
                        length,
                    } => {
                        let start =
                            index as usize * metainfo.piece_length as usize + begin as usize;
                        let end = start + length as usize;
                        let block = data[start..end].to_vec();
                        write_message(
                            &mut writer,
                            &Message::Piece {
                                index,
                                begin,
                                data: block,
                            },
                        )
                        .await
                        .ok();
                    }
                    _ => {}
                }
            }
        });
    }
}

/// A stateless BEP 15 tracker: answers connects and announces forever.
async fn run_fake_tracker(socket: UdpSocket, peer_addr: std::net::SocketAddr) {
    let peer_port = peer_addr.port();
    let mut buf = [0u8; 1024];
    loop {
        let Ok((len, from)) = socket.recv_from(&mut buf).await else {
            return;
        };
        if len >= 16 && buf[0..8] == 0x0417_2710_1980_u64.to_be_bytes() {
            // Connect request.
            let mut reply = Vec::new();
            reply.extend_from_slice(&0u32.to_be_bytes()); // action: connect
            reply.extend_from_slice(&buf[12..16]); // transaction id
            reply.extend_from_slice(&0x1122_3344_5566_7788u64.to_be_bytes());
            socket.send_to(&reply, from).await.ok();
        } else if len >= 98 {
            // Announce request.
            let mut reply = Vec::new();
            reply.extend_from_slice(&1u32.to_be_bytes()); // action: announce
            reply.extend_from_slice(&buf[12..16]); // transaction id
            reply.extend_from_slice(&1800u32.to_be_bytes()); // interval
            reply.extend_from_slice(&0u32.to_be_bytes()); // leechers
            reply.extend_from_slice(&1u32.to_be_bytes()); // seeders
            reply.extend_from_slice(&[127, 0, 0, 1]);
            reply.extend_from_slice(&peer_port.to_be_bytes());
            socket.send_to(&reply, from).await.ok();
        }
    }
}

struct Swarm {
    metainfo: Arc<Metainfo>,
    data: Arc<Vec<u8>>,
}

/// Boot the seeder + tracker pair and parse the resulting metainfo.
async fn start_swarm(piece_length: usize, total: usize) -> Swarm {
    let data: Arc<Vec<u8>> = Arc::new((0..total).map(|i| (i * 31 % 251) as u8).collect());

    let seeder_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let seeder_addr = seeder_listener.local_addr().unwrap();

    let tracker_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let tracker_port = tracker_socket.local_addr().unwrap().port();
    tokio::spawn(run_fake_tracker(tracker_socket, seeder_addr));

    let announce = format!("udp://127.0.0.1:{tracker_port}/announce");
    let doc = build_test_torrent(&data, piece_length, &announce);
    let metainfo = Arc::new(Metainfo::from_torrent_bytes(&doc).unwrap());
    tokio::spawn(run_seeder(seeder_listener, metainfo.clone(), data.clone()));

    Swarm { metainfo, data }
}

fn engine_options() -> EngineOptions {
    EngineOptions {
        max_peers: 4,
        ..EngineOptions::default()
    }
}

#[tokio::test]
async fn engine_downloads_from_local_seeder() {
    let swarm = start_swarm(32 * 1024, 200_000).await; // 7 pieces, short last piece
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(Storage::plan(dir.path(), &swarm.metainfo));
    storage.open_all().unwrap();

    let have = vec![false; swarm.metainfo.piece_count() as usize];
    let engine = Engine::start(
        swarm.metainfo.clone(),
        storage.clone(),
        have,
        engine_options(),
        None,
    );

    let summary = tokio::time::timeout(Duration::from_secs(30), engine.wait())
        .await
        .expect("download should finish quickly")
        .unwrap();

    assert!(summary.complete);
    assert_eq!(summary.stop_reason, StopReason::Complete);
    assert_eq!(summary.downloaded, swarm.data.len() as u64);
    assert!(summary.have.iter().all(|&h| h));

    // Byte-for-byte identical, across both files.
    let mut on_disk = vec![0u8; swarm.data.len()];
    storage.read_at(0, &mut on_disk).unwrap();
    assert_eq!(&on_disk, swarm.data.as_ref());

    let first_len = swarm.data.len() * 2 / 3;
    let a = std::fs::read(dir.path().join("testdata/a.bin")).unwrap();
    let b = std::fs::read(dir.path().join("testdata/sub/b.bin")).unwrap();
    assert_eq!(a, swarm.data[..first_len]);
    assert_eq!(b, swarm.data[first_len..]);
}

#[tokio::test]
async fn engine_downloads_only_chosen_files() {
    let swarm = start_swarm(32 * 1024, 200_000).await; // a.bin ends inside piece 4
    let dir = tempfile::tempdir().unwrap();
    let parts = dir.path().join("parts");
    let skip_b = FileSelection::skipping(2, &[1]);
    let storage = Arc::new(Storage::plan_selected(
        dir.path(),
        &swarm.metainfo,
        &skip_b,
        &parts,
    ));
    storage.open_all().unwrap();

    let first_len = swarm.data.len() * 2 / 3;
    let have = vec![false; swarm.metainfo.piece_count() as usize];
    let engine = Engine::start(
        swarm.metainfo.clone(),
        storage.clone(),
        have,
        engine_options(),
        None,
    );
    assert_eq!(engine.progress().borrow().total_bytes, first_len as u64);

    let summary = tokio::time::timeout(Duration::from_secs(30), engine.wait())
        .await
        .expect("download should finish quickly")
        .unwrap();
    assert!(summary.complete);
    assert_eq!(summary.stop_reason, StopReason::Complete);

    // Just the pieces holding a.bin, including the one it shares with b.bin.
    let wanted_pieces = first_len.div_ceil(32 * 1024);
    assert_eq!(summary.have.iter().filter(|&&h| h).count(), wanted_pieces);
    assert!(summary.downloaded < swarm.data.len() as u64);

    let a = std::fs::read(dir.path().join("testdata/a.bin")).unwrap();
    assert_eq!(a, swarm.data[..first_len]);
    assert!(!dir.path().join("testdata/sub/b.bin").exists());
    assert!(
        parts.join("1.part").exists(),
        "b.bin's share of the shared piece"
    );
}

#[tokio::test]
async fn engine_pauses_and_resumes() {
    let swarm = start_swarm(16 * 1024, 160_000).await; // 10 pieces
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(Storage::plan(dir.path(), &swarm.metainfo));
    storage.open_all().unwrap();

    // First session: throttled so the pause lands mid-download.
    let have = vec![false; swarm.metainfo.piece_count() as usize];
    let throttled = EngineOptions {
        download_limit: Some(48 * 1024),
        ..engine_options()
    };
    let engine = Engine::start(
        swarm.metainfo.clone(),
        storage.clone(),
        have,
        throttled,
        None,
    );
    let stopper = engine.stopper();
    let mut progress = engine.progress();
    let waiter = tokio::spawn(engine.wait());

    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            progress.changed().await.unwrap();
            let pieces_have = progress.borrow().pieces_have;
            if pieces_have > 0 {
                break;
            }
        }
    })
    .await
    .expect("should verify at least one piece");
    stopper.cancel();

    let summary = tokio::time::timeout(Duration::from_secs(10), waiter)
        .await
        .expect("shutdown should be prompt")
        .unwrap()
        .unwrap();
    assert_eq!(summary.stop_reason, StopReason::Cancelled);
    let resumed_from: u32 = summary.have.iter().filter(|&&h| h).count() as u32;
    assert!(resumed_from > 0);

    // Second session: resume from the saved bitfield and finish.
    let engine = Engine::start(
        swarm.metainfo.clone(),
        storage.clone(),
        summary.have.clone(),
        engine_options(),
        None,
    );
    let summary = tokio::time::timeout(Duration::from_secs(30), engine.wait())
        .await
        .expect("resume should finish")
        .unwrap();
    assert!(summary.complete);
    // Only the missing pieces were downloaded the second time.
    assert!(
        summary.downloaded <= swarm.data.len() as u64,
        "resume must not re-download everything"
    );

    let mut on_disk = vec![0u8; swarm.data.len()];
    storage.read_at(0, &mut on_disk).unwrap();
    assert_eq!(&on_disk, swarm.data.as_ref());
}

#[tokio::test]
async fn engine_returns_immediately_when_complete_without_seeding() {
    let swarm = start_swarm(32 * 1024, 64 * 1024).await;
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(Storage::plan(dir.path(), &swarm.metainfo));
    storage.open_all().unwrap();
    storage.write_at(0, &swarm.data).unwrap();

    let have = vec![true; swarm.metainfo.piece_count() as usize];
    let engine = Engine::start(
        swarm.metainfo.clone(),
        storage,
        have,
        engine_options(),
        None,
    );
    let summary = tokio::time::timeout(Duration::from_secs(5), engine.wait())
        .await
        .expect("must not hang when there is nothing to do")
        .unwrap();
    assert!(summary.complete);
    assert_eq!(summary.stop_reason, StopReason::Complete);
}
