//! Choosing which files of a torrent to download.
//!
//! Pieces do not line up with files, so a piece is needed as soon as any of
//! its bytes belong to a chosen file. The bytes such a piece carries for a
//! skipped neighbour are kept out of the download folder by [`Storage`].
//!
//! [`Storage`]: crate::storage::Storage

use crate::metainfo::Metainfo;

/// Which files of a torrent to download, by their position in the torrent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSelection {
    wanted: Vec<bool>,
}

impl FileSelection {
    /// Every file.
    pub fn all(file_count: usize) -> Self {
        FileSelection {
            wanted: vec![true; file_count],
        }
    }

    /// Just the given files (0-based; unknown indices are ignored).
    pub fn only(file_count: usize, files: &[u32]) -> Self {
        let mut selection = FileSelection {
            wanted: vec![false; file_count],
        };
        selection.set(files, true);
        selection
    }

    /// Every file except the given ones (0-based; unknown indices are ignored).
    pub fn skipping(file_count: usize, files: &[u32]) -> Self {
        let mut selection = Self::all(file_count);
        selection.set(files, false);
        selection
    }

    fn set(&mut self, files: &[u32], wanted: bool) {
        for &index in files {
            if let Some(slot) = self.wanted.get_mut(index as usize) {
                *slot = wanted;
            }
        }
    }

    pub fn file_count(&self) -> usize {
        self.wanted.len()
    }

    pub fn is_wanted(&self, index: usize) -> bool {
        self.wanted.get(index).copied().unwrap_or(false)
    }

    pub fn is_everything(&self) -> bool {
        self.wanted.iter().all(|&wanted| wanted)
    }

    pub fn wanted_count(&self) -> usize {
        self.wanted.iter().filter(|&&wanted| wanted).count()
    }

    /// 0-based indices of the skipped files, in torrent order.
    pub fn skipped(&self) -> Vec<u32> {
        (0..self.wanted.len() as u32)
            .filter(|&index| !self.wanted[index as usize])
            .collect()
    }

    /// Total size of the chosen files.
    pub fn selected_length(&self, metainfo: &Metainfo) -> u64 {
        metainfo
            .files
            .iter()
            .enumerate()
            .filter(|(index, _)| self.is_wanted(*index))
            .map(|(_, file)| file.length)
            .sum()
    }

    /// For every piece, how many of its bytes belong to chosen files.
    /// Zero means the piece is not needed at all.
    pub fn piece_weights(&self, metainfo: &Metainfo) -> Vec<u64> {
        let mut weights = vec![0u64; metainfo.piece_count() as usize];
        for (index, file) in metainfo.files.iter().enumerate() {
            if !self.is_wanted(index) {
                continue;
            }
            let file_end = file.offset + file.length;
            for piece in metainfo.file_pieces(index) {
                let start = u64::from(piece) * metainfo.piece_length;
                let end = start + metainfo.piece_size(piece);
                weights[piece as usize] += end.min(file_end) - start.max(file.offset);
            }
        }
        weights
    }

    /// Pieces touching a file that one selection downloads and the other
    /// skips. Their bytes are stored differently under each, so data
    /// recorded under one cannot be trusted under the other.
    pub fn changed_pieces(&self, other: &FileSelection, metainfo: &Metainfo) -> Vec<bool> {
        let mut changed = vec![false; metainfo.piece_count() as usize];
        for index in 0..metainfo.files.len() {
            if self.is_wanted(index) != other.is_wanted(index) {
                for piece in metainfo.file_pieces(index) {
                    changed[piece as usize] = true;
                }
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metainfo::test_support::multi_file_meta;

    #[test]
    fn everything_weighs_whole_pieces() {
        let meta = multi_file_meta(&[10, 22, 8], 16);
        let all = FileSelection::all(3);
        assert!(all.is_everything());
        assert_eq!(all.piece_weights(&meta), vec![16, 16, 8]);
        assert_eq!(all.selected_length(&meta), 40);
    }

    #[test]
    fn skipped_files_only_count_where_pieces_are_shared() {
        // Files: [0,10) [10,32) [32,40); pieces: [0,16) [16,32) [32,40).
        let meta = multi_file_meta(&[10, 22, 8], 16);
        let selection = FileSelection::skipping(3, &[1]);
        assert_eq!(selection.skipped(), vec![1]);
        assert_eq!(selection.wanted_count(), 2);
        // Piece 0 is needed for file 0's 10 bytes; piece 1 holds only file 1.
        assert_eq!(selection.piece_weights(&meta), vec![10, 0, 8]);
        assert_eq!(selection.selected_length(&meta), 18);
    }

    #[test]
    fn only_and_skipping_are_complements() {
        let only = FileSelection::only(4, &[1, 3, 99]);
        let skipping = FileSelection::skipping(4, &[0, 2]);
        assert_eq!(only, skipping);
        assert!(!only.is_wanted(0));
        assert!(only.is_wanted(3));
        assert!(!only.is_wanted(99), "out of range is never wanted");
    }

    #[test]
    fn changed_pieces_cover_every_piece_of_a_changed_file() {
        let meta = multi_file_meta(&[10, 22, 8], 16);
        let before = FileSelection::all(3);
        let after = FileSelection::skipping(3, &[1]);
        assert_eq!(
            after.changed_pieces(&before, &meta),
            vec![true, true, false]
        );
        assert_eq!(before.changed_pieces(&before, &meta), vec![false; 3]);
    }
}
