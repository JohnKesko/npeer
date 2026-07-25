//! On-disk storage: maps the torrent's contiguous byte space onto one or
//! more files, exactly as laid out in the metainfo.
//!
//! Blocks are written straight to their final location as they arrive and
//! verified by reading the piece back, so memory use stays constant no
//! matter how large pieces are. All methods are synchronous; async callers
//! wrap them in `spawn_blocking`.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{Error, Result};
use crate::metainfo::Metainfo;

struct StorageFile {
    path: PathBuf,
    /// Start of this file within the torrent's byte space.
    offset: u64,
    length: u64,
    handle: Mutex<Option<File>>,
}

pub struct Storage {
    files: Vec<StorageFile>,
    total_length: u64,
    /// The directory downloads land in; single-file torrents put the file
    /// directly here, multi-file torrents create `<root>/<name>/...`.
    content_path: PathBuf,
}

impl Storage {
    /// Plan the on-disk layout under `output_dir` without touching the disk.
    pub fn plan(output_dir: &Path, metainfo: &Metainfo) -> Self {
        let base = if metainfo.multi_file {
            output_dir.join(&metainfo.name)
        } else {
            output_dir.to_path_buf()
        };
        let files = metainfo
            .files
            .iter()
            .map(|f| StorageFile {
                path: base.join(&f.path),
                offset: f.offset,
                length: f.length,
                handle: Mutex::new(None),
            })
            .collect();
        let content_path = if metainfo.multi_file {
            base
        } else {
            base.join(&metainfo.files[0].path)
        };
        Storage {
            files,
            total_length: metainfo.total_length,
            content_path,
        }
    }

    /// Where the downloaded content lives: the file itself for single-file
    /// torrents, the directory for multi-file torrents.
    pub fn content_path(&self) -> &Path {
        &self.content_path
    }

    /// True if any of the target files already exist on disk.
    pub fn any_file_exists(&self) -> bool {
        self.files.iter().any(|f| f.path.exists())
    }

    /// Create directories and open every file, sizing it to its final length.
    pub fn open_all(&self) -> Result<()> {
        for file in &self.files {
            if let Some(parent) = file.path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| Error::Storage(format!("creating {}: {e}", parent.display())))?;
            }
            let handle = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&file.path)
                .map_err(|e| Error::Storage(format!("opening {}: {e}", file.path.display())))?;
            let current = handle
                .metadata()
                .map_err(|e| Error::Storage(format!("{}: {e}", file.path.display())))?
                .len();
            if current != file.length {
                handle.set_len(file.length).map_err(|e| {
                    Error::Storage(format!("allocating {}: {e}", file.path.display()))
                })?;
            }
            *file.handle.lock().unwrap() = Some(handle);
        }
        Ok(())
    }

    /// Write `data` at `offset` in torrent byte space, spanning files as needed.
    pub fn write_at(&self, offset: u64, data: &[u8]) -> Result<()> {
        self.for_each_span(offset, data.len() as u64, |file, file_offset, range| {
            let mut guard = file.handle.lock().unwrap();
            let handle = guard.as_mut().ok_or_else(|| storage_closed(&file.path))?;
            handle.seek(SeekFrom::Start(file_offset))?;
            handle.write_all(&data[range])?;
            Ok(())
        })
    }

    /// Read `buf.len()` bytes from `offset` in torrent byte space.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let len = buf.len() as u64;
        self.for_each_span(offset, len, |file, file_offset, range| {
            let mut guard = file.handle.lock().unwrap();
            let handle = guard.as_mut().ok_or_else(|| storage_closed(&file.path))?;
            handle.seek(SeekFrom::Start(file_offset))?;
            handle.read_exact(&mut buf[range])?;
            Ok(())
        })
    }

    /// Flush and fsync every open file.
    pub fn sync_all(&self) -> Result<()> {
        for file in &self.files {
            let guard = file.handle.lock().unwrap();
            if let Some(handle) = guard.as_ref() {
                handle
                    .sync_all()
                    .map_err(|e| Error::Storage(format!("syncing {}: {e}", file.path.display())))?;
            }
        }
        Ok(())
    }

    /// Delete every file of this torrent, plus its directory if now empty.
    pub fn delete_files(&self) -> Result<()> {
        for file in &self.files {
            *file.handle.lock().unwrap() = None;
            match fs::remove_file(&file.path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(Error::Storage(format!(
                        "deleting {}: {e}",
                        file.path.display()
                    )));
                }
            }
        }
        if self.content_path.is_dir() {
            // Only removes empty directories; anything user-added survives.
            let _ = remove_empty_dirs(&self.content_path);
        }
        Ok(())
    }

    /// Map a byte range onto the files it touches.
    fn for_each_span(
        &self,
        offset: u64,
        length: u64,
        mut visit: impl FnMut(&StorageFile, u64, std::ops::Range<usize>) -> Result<()>,
    ) -> Result<()> {
        if offset + length > self.total_length {
            return Err(Error::Storage(format!(
                "range {offset}+{length} is outside the torrent ({} bytes)",
                self.total_length
            )));
        }
        let mut consumed = 0u64;
        for file in &self.files {
            let file_end = file.offset + file.length;
            if file_end <= offset || file.offset >= offset + length {
                continue;
            }
            let span_start = offset.max(file.offset);
            let span_end = (offset + length).min(file_end);
            let range = (span_start - offset) as usize..(span_end - offset) as usize;
            visit(file, span_start - file.offset, range)?;
            consumed += span_end - span_start;
        }
        debug_assert_eq!(consumed, length);
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
    use crate::metainfo::Metainfo;

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
}
