//! Hash-checking data that is already on disk.

use std::sync::Arc;

use sha1::{Digest, Sha1};
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::metainfo::Metainfo;
use crate::storage::Storage;

/// Re-hash the pieces marked in `check` and report which ones are good;
/// pieces not checked are reported as missing.
///
/// `progress` is called with (checked, total) as pieces complete. Runs on the
/// blocking pool; cancellation is checked between pieces.
pub async fn verify_pieces(
    storage: Arc<Storage>,
    metainfo: Arc<Metainfo>,
    check: Vec<bool>,
    cancel: CancellationToken,
    progress: impl Fn(u32, u32) + Send + 'static,
) -> Result<Vec<bool>> {
    let piece_count = metainfo.piece_count();
    assert_eq!(check.len(), piece_count as usize);
    let total = check.iter().filter(|&&c| c).count() as u32;
    tokio::task::spawn_blocking(move || {
        let mut have = vec![false; piece_count as usize];
        let mut buffer = vec![0u8; metainfo.piece_length as usize];
        let mut checked = 0;

        for piece in (0..piece_count).filter(|&p| check[p as usize]) {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let size = metainfo.piece_size(piece) as usize;
            let offset = u64::from(piece) * metainfo.piece_length;
            let ok = match storage.read_at(offset, &mut buffer[..size]) {
                Ok(()) => {
                    let digest: [u8; 20] = Sha1::digest(&buffer[..size]).into();
                    digest == metainfo.pieces[piece as usize]
                }
                // Unreadable regions (e.g. missing file) just mean "not here yet".
                Err(_) => false,
            };
            have[piece as usize] = ok;
            checked += 1;
            progress(checked, total);
        }
        Ok(have)
    })
    .await
    .map_err(|e| Error::Other(format!("verification task failed: {e}")))?
}

/// Hash one piece already on disk, used after downloading it.
pub fn verify_single_piece(storage: &Storage, metainfo: &Metainfo, piece: u32) -> Result<bool> {
    let size = metainfo.piece_size(piece) as usize;
    let mut buffer = vec![0u8; size];
    let offset = u64::from(piece) * metainfo.piece_length;
    storage.read_at(offset, &mut buffer)?;
    let digest: [u8; 20] = Sha1::digest(&buffer).into();
    Ok(digest == metainfo.pieces[piece as usize])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metainfo::test_support::single_file_torrent;

    #[tokio::test]
    async fn detects_good_and_bad_pieces() {
        let data: Vec<u8> = (0..40u8).collect();
        let doc = single_file_torrent("http://t/a", "v.bin", 16, &data);
        let metainfo = Arc::new(Metainfo::from_torrent_bytes(&doc).unwrap());

        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(Storage::plan(dir.path(), &metainfo));
        storage.open_all().unwrap();
        storage.write_at(0, &data).unwrap();
        // Corrupt one byte inside piece 1 (bytes 16..32).
        storage.write_at(20, &[0xFF]).unwrap();

        let have = verify_pieces(
            storage.clone(),
            metainfo.clone(),
            vec![true; 3],
            CancellationToken::new(),
            |_, _| {},
        )
        .await
        .unwrap();
        assert_eq!(have, vec![true, false, true]);

        // Unchecked pieces count as missing, however good they are.
        let have = verify_pieces(
            storage.clone(),
            metainfo.clone(),
            vec![false, true, true],
            CancellationToken::new(),
            |_, _| {},
        )
        .await
        .unwrap();
        assert_eq!(have, vec![false, false, true]);
        assert!(verify_single_piece(&storage, &metainfo, 0).unwrap());
        assert!(!verify_single_piece(&storage, &metainfo, 1).unwrap());
    }

    #[tokio::test]
    async fn cancellation_stops_verification() {
        let data = vec![0u8; 64];
        let doc = single_file_torrent("http://t/a", "v.bin", 16, &data);
        let metainfo = Arc::new(Metainfo::from_torrent_bytes(&doc).unwrap());
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(Storage::plan(dir.path(), &metainfo));
        storage.open_all().unwrap();

        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = verify_pieces(storage, metainfo, vec![true; 4], cancel, |_, _| {}).await;
        assert!(matches!(result, Err(Error::Cancelled)));
    }
}
