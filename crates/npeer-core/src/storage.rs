//! On-disk storage: maps the torrent's contiguous byte space onto one or
//! more files, exactly as laid out in the metainfo.
//!
//! Blocks are written straight to their final location as they arrive and
//! verified by reading the piece back, so memory use stays constant no
//! matter how large pieces are. All methods are synchronous; async callers
//! wrap them in `spawn_blocking`.
//!
//! When only some files are chosen, skipped files are never created. A
//! piece shared between a chosen file and a skipped one must still be
//! downloaded whole to pass its hash check, so the skipped file's share of
//! it goes into a small part file kept outside the download folder.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{Error, Result};
use crate::metainfo::Metainfo;
use crate::selection::FileSelection;

/// One file on disk: a chosen torrent file, or a part file.
struct Target {
    path: PathBuf,
    length: u64,
    /// Holds leftovers of a skipped file rather than real content.
    is_part: bool,
    handle: Mutex<Option<File>>,
}

/// A run of torrent bytes stored contiguously in one target.
struct Segment {
    /// Offset within the torrent's byte space.
    start: u64,
    length: u64,
    target: usize,
    target_offset: u64,
}

pub struct Storage {
    targets: Vec<Target>,
    /// Sorted by `start` and never overlapping. Bytes outside every segment
    /// belong only to pieces nobody asked for.
    segments: Vec<Segment>,
    /// Every file of the torrent, chosen or not.
    file_paths: Vec<PathBuf>,
    total_length: u64,
    /// The directory downloads land in; single-file torrents put the file
    /// directly here, multi-file torrents create `<root>/<name>/...`.
    content_path: PathBuf,
    selection: FileSelection,
    selected_length: u64,
    /// Bytes of chosen files in each piece; zero for pieces not needed.
    piece_weights: Vec<u64>,
}

impl Storage {
    /// Plan the on-disk layout of every file without touching the disk.
    pub fn plan(output_dir: &Path, metainfo: &Metainfo) -> Self {
        let everything = FileSelection::all(metainfo.files.len());
        // Nothing is skipped, so no part file ever lands in `parts_dir`.
        Self::plan_selected(output_dir, metainfo, &everything, output_dir)
    }

    /// Plan the layout for just the chosen files. Part files for skipped
    /// neighbours go in `parts_dir`, one per file, named by its index.
    pub fn plan_selected(
        output_dir: &Path,
        metainfo: &Metainfo,
        selection: &FileSelection,
        parts_dir: &Path,
    ) -> Self {
        let base = if metainfo.multi_file {
            output_dir.join(&metainfo.name)
        } else {
            output_dir.to_path_buf()
        };
        let piece_weights = selection.piece_weights(metainfo);
        let piece_needed = |piece: u32| piece_weights[piece as usize] > 0;

        let mut targets = Vec::new();
        let mut segments = Vec::new();
        let mut add_target = |path: PathBuf, length: u64, is_part: bool| {
            targets.push(Target {
                path,
                length,
                is_part,
                handle: Mutex::new(None),
            });
            targets.len() - 1
        };

        for (index, file) in metainfo.files.iter().enumerate() {
            if selection.is_wanted(index) {
                let target = add_target(base.join(&file.path), file.length, false);
                if file.length > 0 {
                    segments.push(Segment {
                        start: file.offset,
                        length: file.length,
                        target,
                        target_offset: 0,
                    });
                }
                continue;
            }

            // A skipped file can only share its first and last piece with
            // chosen files. Its part file keeps the head at offset 0 and the
            // tail right after it, a layout that depends on the piece grid
            // alone, so it survives changes to which other files are chosen.
            let pieces = metainfo.file_pieces(index);
            if pieces.is_empty() {
                continue;
            }
            let file_end = file.offset + file.length;
            let head_end = file_end.min(u64::from(pieces.start + 1) * metainfo.piece_length);
            let tail_start = head_end.max(u64::from(pieces.end - 1) * metainfo.piece_length);
            let head_length = head_end - file.offset;
            let tail_length = file_end - tail_start;
            let need_head = piece_needed(pieces.start);
            let need_tail = tail_length > 0 && piece_needed(pieces.end - 1);
            if !need_head && !need_tail {
                continue;
            }
            let part_length = if need_tail {
                head_length + tail_length
            } else {
                head_length
            };
            let target = add_target(parts_dir.join(format!("{index}.part")), part_length, true);
            if need_head {
                segments.push(Segment {
                    start: file.offset,
                    length: head_length,
                    target,
                    target_offset: 0,
                });
            }
            if need_tail {
                segments.push(Segment {
                    start: tail_start,
                    length: tail_length,
                    target,
                    target_offset: head_length,
                });
            }
        }
        debug_assert!(segments.is_sorted_by_key(|s| s.start));

        let content_path = if metainfo.multi_file {
            base.clone()
        } else {
            base.join(&metainfo.files[0].path)
        };
        Storage {
            targets,
            segments,
            file_paths: metainfo.files.iter().map(|f| base.join(&f.path)).collect(),
            total_length: metainfo.total_length,
            content_path,
            selection: selection.clone(),
            selected_length: selection.selected_length(metainfo),
            piece_weights,
        }
    }

    /// Where the downloaded content lives: the file itself for single-file
    /// torrents, the directory for multi-file torrents.
    pub fn content_path(&self) -> &Path {
        &self.content_path
    }

    /// The files this layout downloads.
    pub fn selection(&self) -> &FileSelection {
        &self.selection
    }

    /// Total size of the chosen files.
    pub fn selected_length(&self) -> u64 {
        self.selected_length
    }

    /// Bytes of chosen files in each piece; zero for pieces not needed.
    pub fn piece_weights(&self) -> &[u64] {
        &self.piece_weights
    }

    /// Which pieces the chosen files need.
    pub fn wanted_pieces(&self) -> Vec<bool> {
        self.piece_weights
            .iter()
            .map(|&weight| weight > 0)
            .collect()
    }

    /// True if any of the chosen files already exist on disk.
    pub fn any_file_exists(&self) -> bool {
        self.targets
            .iter()
            .any(|target| !target.is_part && target.path.exists())
    }

    /// Create directories and open every chosen file (plus part files),
    /// sizing each to its final length.
    pub fn open_all(&self) -> Result<()> {
        for target in &self.targets {
            if let Some(parent) = target.path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| Error::Storage(format!("creating {}: {e}", parent.display())))?;
            }
            let handle = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&target.path)
                .map_err(|e| Error::Storage(format!("opening {}: {e}", target.path.display())))?;
            let current = handle
                .metadata()
                .map_err(|e| Error::Storage(format!("{}: {e}", target.path.display())))?
                .len();
            if current != target.length {
                handle.set_len(target.length).map_err(|e| {
                    Error::Storage(format!("allocating {}: {e}", target.path.display()))
                })?;
            }
            *target.handle.lock().unwrap() = Some(handle);
        }
        Ok(())
    }

    /// Write `data` at `offset` in torrent byte space, spanning files as needed.
    pub fn write_at(&self, offset: u64, data: &[u8]) -> Result<()> {
        self.for_each_span(offset, data.len() as u64, |target, target_offset, range| {
            let mut guard = target.handle.lock().unwrap();
            let handle = guard.as_mut().ok_or_else(|| storage_closed(&target.path))?;
            handle.seek(SeekFrom::Start(target_offset))?;
            handle.write_all(&data[range])?;
            Ok(())
        })
    }

    /// Read `buf.len()` bytes from `offset` in torrent byte space.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let len = buf.len() as u64;
        self.for_each_span(offset, len, |target, target_offset, range| {
            let mut guard = target.handle.lock().unwrap();
            let handle = guard.as_mut().ok_or_else(|| storage_closed(&target.path))?;
            handle.seek(SeekFrom::Start(target_offset))?;
            handle.read_exact(&mut buf[range])?;
            Ok(())
        })
    }

    /// Flush and fsync every open file.
    pub fn sync_all(&self) -> Result<()> {
        for target in &self.targets {
            let guard = target.handle.lock().unwrap();
            if let Some(handle) = guard.as_ref() {
                handle.sync_all().map_err(|e| {
                    Error::Storage(format!("syncing {}: {e}", target.path.display()))
                })?;
            }
        }
        Ok(())
    }

    /// Delete every file of this torrent, chosen or not, plus its directory
    /// if now empty. Part files belong to npeer's state and are left alone.
    pub fn delete_files(&self) -> Result<()> {
        for target in &self.targets {
            *target.handle.lock().unwrap() = None;
        }
        for path in &self.file_paths {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(Error::Storage(format!("deleting {}: {e}", path.display())));
                }
            }
        }
        if self.content_path.is_dir() {
            // Only removes empty directories; anything user-added survives.
            let _ = remove_empty_dirs(&self.content_path);
        }
        Ok(())
    }

    /// Map a byte range onto the targets that store it. Fails if any byte
    /// is not stored, which only happens inside pieces nobody asked for.
    fn for_each_span(
        &self,
        offset: u64,
        length: u64,
        mut visit: impl FnMut(&Target, u64, std::ops::Range<usize>) -> Result<()>,
    ) -> Result<()> {
        let end = offset + length;
        if end > self.total_length {
            return Err(Error::Storage(format!(
                "range {offset}+{length} is outside the torrent ({} bytes)",
                self.total_length
            )));
        }
        let first = self
            .segments
            .partition_point(|segment| segment.start + segment.length <= offset);
        let mut cursor = offset;
        for segment in &self.segments[first..] {
            if cursor == end || segment.start > cursor {
                break;
            }
            let span_end = end.min(segment.start + segment.length);
            let range = (cursor - offset) as usize..(span_end - offset) as usize;
            let target_offset = segment.target_offset + (cursor - segment.start);
            visit(&self.targets[segment.target], target_offset, range)?;
            cursor = span_end;
        }
        if cursor < end {
            return Err(Error::Storage(format!(
                "bytes {cursor}..{end} belong to skipped files and are not stored"
            )));
        }
        Ok(())
    }
}

fn storage_closed(path: &Path) -> Error {
    Error::Storage(format!("{} is not open", path.display()))
}

fn remove_empty_dirs(dir: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            let _ = remove_empty_dirs(&path);
        }
    }
    fs::remove_dir(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metainfo::{Metainfo, test_support};

    /// Multi-file metainfo: a.bin (10 bytes) + b.bin (22 bytes), pieces of 16.
    fn multi_file_meta() -> Metainfo {
        use sha1::{Digest, Sha1};
        let hashes: Vec<u8> = (0..2)
            .flat_map(|_| Sha1::digest([0u8; 16]).to_vec())
            .collect();
        let mut info = Vec::new();
        info.extend_from_slice(b"d5:filesl");
        info.extend_from_slice(b"d6:lengthi10e4:pathl5:a.binee");
        info.extend_from_slice(b"d6:lengthi22e4:pathl5:b.bineee");
        info.extend_from_slice(b"4:name3:dir12:piece lengthi16e");
        info.extend_from_slice(format!("6:pieces{}:", hashes.len()).as_bytes());
        info.extend_from_slice(&hashes);
        info.extend_from_slice(b"e");
        Metainfo::from_info_bytes(&info, vec![]).unwrap()
    }

    #[test]
    fn writes_across_file_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::plan(dir.path(), &multi_file_meta());
        storage.open_all().unwrap();

        // 16 bytes starting at offset 5 span both files (5..10 and 10..21).
        let data: Vec<u8> = (0..16).collect();
        storage.write_at(5, &data).unwrap();

        let mut back = vec![0u8; 16];
        storage.read_at(5, &mut back).unwrap();
        assert_eq!(back, data);

        // The bytes really landed in the right files.
        let a = fs::read(dir.path().join("dir/a.bin")).unwrap();
        let b = fs::read(dir.path().join("dir/b.bin")).unwrap();
        assert_eq!(a.len(), 10);
        assert_eq!(b.len(), 22);
        assert_eq!(&a[5..10], &data[..5]);
        assert_eq!(&b[..11], &data[5..]);
    }

    #[test]
    fn rejects_out_of_range_access() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::plan(dir.path(), &multi_file_meta());
        storage.open_all().unwrap();
        assert!(storage.write_at(30, &[0u8; 10]).is_err());
    }

    #[test]
    fn preallocates_to_final_size() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::plan(dir.path(), &multi_file_meta());
        storage.open_all().unwrap();
        assert_eq!(
            fs::metadata(dir.path().join("dir/a.bin")).unwrap().len(),
            10
        );
        assert_eq!(
            fs::metadata(dir.path().join("dir/b.bin")).unwrap().len(),
            22
        );
    }

    #[test]
    fn delete_files_removes_content() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::plan(dir.path(), &multi_file_meta());
        storage.open_all().unwrap();
        assert!(storage.any_file_exists());
        storage.delete_files().unwrap();
        assert!(!storage.any_file_exists());
        assert!(!dir.path().join("dir").exists());
    }

    /// Files [0,10) [10,32) [32,40) in pieces [0,16) [16,32) [32,40),
    /// with the middle file skipped.
    fn skip_middle(dir: &Path) -> (Metainfo, Storage) {
        let meta = test_support::multi_file_meta(&[10, 22, 8], 16);
        let selection = FileSelection::skipping(3, &[1]);
        let storage = Storage::plan_selected(dir, &meta, &selection, &dir.join("parts"));
        (meta, storage)
    }

    #[test]
    fn skipped_files_are_never_created() {
        let dir = tempfile::tempdir().unwrap();
        let (_, storage) = skip_middle(dir.path());
        storage.open_all().unwrap();
        assert!(dir.path().join("dir/f0.bin").exists());
        assert!(dir.path().join("dir/f2.bin").exists());
        assert!(!dir.path().join("dir/f1.bin").exists());
        assert_eq!(storage.selected_length(), 18);
        assert_eq!(storage.wanted_pieces(), vec![true, false, true]);
    }

    #[test]
    fn shared_piece_keeps_the_skipped_share_in_a_part_file() {
        let dir = tempfile::tempdir().unwrap();
        let (_, storage) = skip_middle(dir.path());
        storage.open_all().unwrap();

        // Piece 0 is needed for f0 and also carries 6 bytes of skipped f1.
        let piece: Vec<u8> = (100..116).collect();
        storage.write_at(0, &piece).unwrap();
        let mut back = vec![0u8; 16];
        storage.read_at(0, &mut back).unwrap();
        assert_eq!(back, piece);

        assert_eq!(
            fs::read(dir.path().join("dir/f0.bin")).unwrap(),
            piece[..10]
        );
        // f1's part file holds just its head: its tail piece is not needed.
        let part = fs::read(dir.path().join("parts/1.part")).unwrap();
        assert_eq!(part, piece[10..]);
    }

    #[test]
    fn pieces_of_skipped_files_alone_are_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let (_, storage) = skip_middle(dir.path());
        storage.open_all().unwrap();
        // Piece 1 lies entirely inside the skipped file.
        let error = storage.write_at(16, &[0u8; 16]).unwrap_err();
        assert!(error.to_string().contains("skipped"), "got: {error}");
        assert!(storage.read_at(20, &mut [0u8; 4]).is_err());
    }

    #[test]
    fn part_layout_ignores_other_files_choices() {
        // Whatever else is chosen, a skipped file's head and tail land at
        // the same offsets of its part file.
        let dir = tempfile::tempdir().unwrap();
        let meta = test_support::multi_file_meta(&[10, 22, 8], 16);
        let parts = dir.path().join("parts");
        let a = Storage::plan_selected(dir.path(), &meta, &FileSelection::only(3, &[0]), &parts);
        a.open_all().unwrap();
        a.write_at(0, &[7u8; 16]).unwrap();
        drop(a);

        let b = Storage::plan_selected(dir.path(), &meta, &FileSelection::only(3, &[0, 2]), &parts);
        b.open_all().unwrap();
        let mut back = [0u8; 16];
        b.read_at(0, &mut back).unwrap();
        assert_eq!(back, [7u8; 16]);
    }

    #[test]
    fn delete_files_leaves_part_files_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (_, storage) = skip_middle(dir.path());
        storage.open_all().unwrap();
        storage.delete_files().unwrap();
        assert!(!dir.path().join("dir").exists());
        assert!(dir.path().join("parts/1.part").exists());
    }
}
