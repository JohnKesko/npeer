//! Parsing of `.torrent` files and info dictionaries (BEP 3, BEP 12).

use std::path::{Component, Path, PathBuf};

use sha1::{Digest, Sha1};

use crate::bencode::{self, Value};
use crate::error::{Error, Result};
use crate::types::InfoHash;

/// One file inside a torrent. `path` is always relative and sanitised.
#[derive(Clone, Debug)]
pub struct FileEntry {
    pub path: PathBuf,
    pub length: u64,
    /// Offset of this file within the torrent's contiguous byte space.
    pub offset: u64,
}

/// Fully parsed torrent metadata.
#[derive(Clone, Debug)]
pub struct Metainfo {
    pub info_hash: InfoHash,
    /// Suggested name: the file name for single-file torrents, the directory
    /// name for multi-file torrents.
    pub name: String,
    pub piece_length: u64,
    pub pieces: Vec<[u8; 20]>,
    /// Files in torrent order. Single-file torrents have exactly one entry
    /// whose path is the torrent name.
    pub files: Vec<FileEntry>,
    /// True when the torrent spans a directory of files.
    pub multi_file: bool,
    pub total_length: u64,
    /// Tracker URLs grouped in tiers (BEP 12). A plain `announce` key becomes
    /// a single one-element tier.
    pub trackers: Vec<Vec<String>>,
    /// The raw bencoded `info` dictionary, kept for magnet persistence.
    pub info_bytes: Vec<u8>,
    pub private: bool,
    pub comment: Option<String>,
    pub created_by: Option<String>,
}

impl Metainfo {
    /// Parse a complete `.torrent` document.
    pub fn from_torrent_bytes(bytes: &[u8]) -> Result<Self> {
        let root = bencode::decode(bytes).map_err(invalid)?;
        let root = root
            .as_dict()
            .ok_or_else(|| torrent_err("root is not a dictionary"))?;

        let mut trackers: Vec<Vec<String>> = Vec::new();
        if let Some(list) = root
            .get(b"announce-list".as_slice())
            .and_then(Value::as_list)
        {
            for tier in list {
                let urls: Vec<String> = tier
                    .as_list()
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                if !urls.is_empty() {
                    trackers.push(urls);
                }
            }
        }
        if trackers.is_empty()
            && let Some(announce) = root.get(b"announce".as_slice()).and_then(Value::as_str)
        {
            trackers.push(vec![announce.to_owned()]);
        }

        let info_span = bencode::top_level_value_span(bytes, b"info").map_err(invalid)?;
        let info_bytes = bytes[info_span].to_vec();

        let comment = root
            .get(b"comment".as_slice())
            .and_then(Value::as_str)
            .map(str::to_owned);
        let created_by = root
            .get(b"created by".as_slice())
            .and_then(Value::as_str)
            .map(str::to_owned);

        let mut metainfo = Self::from_info_bytes(&info_bytes, trackers)?;
        metainfo.comment = comment;
        metainfo.created_by = created_by;
        Ok(metainfo)
    }

    /// Parse a raw `info` dictionary, e.g. metadata fetched for a magnet link.
    pub fn from_info_bytes(info_bytes: &[u8], trackers: Vec<Vec<String>>) -> Result<Self> {
        let info_hash = InfoHash(Sha1::digest(info_bytes).into());
        let info = bencode::decode(info_bytes).map_err(invalid)?;
        let info = info
            .as_dict()
            .ok_or_else(|| torrent_err("info is not a dictionary"))?;

        let name = info
            .get(b"name".as_slice())
            .and_then(Value::as_bytes)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| info_hash.to_hex());

        let piece_length = info
            .get(b"piece length".as_slice())
            .and_then(Value::as_int)
            .filter(|&n| n > 0)
            .ok_or_else(|| torrent_err("missing or invalid `piece length`"))?
            as u64;

        let pieces_raw = info
            .get(b"pieces".as_slice())
            .and_then(Value::as_bytes)
            .ok_or_else(|| torrent_err("missing `pieces`"))?;
        if pieces_raw.is_empty() || pieces_raw.len() % 20 != 0 {
            return Err(torrent_err("`pieces` is not a sequence of 20-byte hashes"));
        }
        let pieces: Vec<[u8; 20]> = pieces_raw.as_chunks::<20>().0.to_vec();

        let (files, multi_file) = parse_files(info, &name)?;
        let total_length: u64 = files.iter().map(|f| f.length).sum();
        if total_length == 0 {
            return Err(torrent_err("torrent contains no data"));
        }

        let expected_pieces = total_length.div_ceil(piece_length);
        if pieces.len() as u64 != expected_pieces {
            return Err(torrent_err(&format!(
                "piece count mismatch: {} hashes for {} pieces of data",
                pieces.len(),
                expected_pieces
            )));
        }

        let private = info
            .get(b"private".as_slice())
            .and_then(Value::as_int)
            .unwrap_or(0)
            == 1;

        Ok(Metainfo {
            info_hash,
            name,
            piece_length,
            pieces,
            files,
            multi_file,
            total_length,
            trackers,
            info_bytes: info_bytes.to_vec(),
            private,
            comment: None,
            created_by: None,
        })
    }

    pub fn piece_count(&self) -> u32 {
        self.pieces.len() as u32
    }

    /// Byte length of piece `index`; the last piece is usually shorter.
    pub fn piece_size(&self, index: u32) -> u64 {
        let start = u64::from(index) * self.piece_length;
        (self.total_length - start).min(self.piece_length)
    }

    /// Every tracker URL, flattened across tiers in order.
    pub fn all_trackers(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        self.trackers
            .iter()
            .flatten()
            .filter(|url| seen.insert((*url).clone()))
            .cloned()
            .collect()
    }
}

fn parse_files(
    info: &std::collections::BTreeMap<Vec<u8>, Value>,
    name: &str,
) -> Result<(Vec<FileEntry>, bool)> {
    if let Some(length) = info.get(b"length".as_slice()).and_then(Value::as_int) {
        if length < 0 {
            return Err(torrent_err("file length is negative"));
        }
        let path = sanitise_component(name)
            .ok_or_else(|| torrent_err("torrent name is not a usable file name"))?;
        return Ok((
            vec![FileEntry {
                path: PathBuf::from(path),
                length: length as u64,
                offset: 0,
            }],
            false,
        ));
    }

    let file_list = info
        .get(b"files".as_slice())
        .and_then(Value::as_list)
        .ok_or_else(|| torrent_err("info has neither `length` nor `files`"))?;

    let mut files = Vec::with_capacity(file_list.len());
    let mut offset = 0u64;
    for entry in file_list {
        let length = entry
            .get(b"length")
            .and_then(Value::as_int)
            .filter(|&n| n >= 0)
            .ok_or_else(|| torrent_err("file entry has no valid `length`"))?
            as u64;
        let path_parts = entry
            .get(b"path")
            .and_then(Value::as_list)
            .ok_or_else(|| torrent_err("file entry has no `path`"))?;

        let mut path = PathBuf::new();
        for part in path_parts {
            let part = part
                .as_bytes()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .ok_or_else(|| torrent_err("file path element is not a string"))?;
            let clean = sanitise_component(&part)
                .ok_or_else(|| torrent_err(&format!("unsafe path element `{part}`")))?;
            path.push(clean);
        }
        if path.as_os_str().is_empty() {
            return Err(torrent_err("file entry has an empty path"));
        }

        files.push(FileEntry {
            path,
            length,
            offset,
        });
        offset += length;
    }

    if files.is_empty() {
        return Err(torrent_err("`files` list is empty"));
    }
    Ok((files, true))
}

/// Reject path elements that could escape the download directory.
fn sanitise_component(part: &str) -> Option<String> {
    let trimmed = part.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return None;
    }
    // A single component must not smuggle in separators or drive prefixes.
    let path = Path::new(trimmed);
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(c)), None) => Some(c.to_string_lossy().into_owned()),
        _ => None,
    }
}

fn torrent_err(msg: &str) -> Error {
    Error::InvalidTorrent(msg.to_owned())
}

fn invalid(err: Error) -> Error {
    match err {
        Error::Bencode(msg) => Error::InvalidTorrent(msg),
        other => other,
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    /// Build a minimal single-file torrent document for tests.
    pub fn single_file_torrent(
        announce: &str,
        name: &str,
        piece_length: u64,
        data: &[u8],
    ) -> Vec<u8> {
        use sha1::{Digest, Sha1};
        let mut pieces = Vec::new();
        for chunk in data.chunks(piece_length as usize) {
            pieces.extend_from_slice(&Sha1::digest(chunk));
        }
        let mut doc = Vec::new();
        doc.extend_from_slice(b"d");
        doc.extend_from_slice(format!("8:announce{}:{}", announce.len(), announce).as_bytes());
        doc.extend_from_slice(b"4:info");
        doc.extend_from_slice(info_dict(name, piece_length, data.len() as u64, &pieces).as_slice());
        doc.extend_from_slice(b"e");
        doc
    }

    pub fn info_dict(name: &str, piece_length: u64, length: u64, pieces: &[u8]) -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(b"d");
        info.extend_from_slice(format!("6:lengthi{length}e").as_bytes());
        info.extend_from_slice(format!("4:name{}:{}", name.len(), name).as_bytes());
        info.extend_from_slice(format!("12:piece lengthi{piece_length}e").as_bytes());
        info.extend_from_slice(format!("6:pieces{}:", pieces.len()).as_bytes());
        info.extend_from_slice(pieces);
        info.extend_from_slice(b"e");
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_file_torrent() {
        let data = vec![7u8; 40];
        let doc = test_support::single_file_torrent(
            "http://tracker.example/announce",
            "a.bin",
            16,
            &data,
        );
        let meta = Metainfo::from_torrent_bytes(&doc).unwrap();

        assert_eq!(meta.name, "a.bin");
        assert_eq!(meta.total_length, 40);
        assert_eq!(meta.piece_length, 16);
        assert_eq!(meta.piece_count(), 3);
        assert_eq!(meta.piece_size(0), 16);
        assert_eq!(meta.piece_size(2), 8, "last piece is the remainder");
        assert!(!meta.multi_file);
        assert_eq!(meta.files.len(), 1);
        assert_eq!(
            meta.trackers,
            vec![vec!["http://tracker.example/announce".to_owned()]]
        );
        assert_eq!(meta.info_hash.to_hex().len(), 40);
    }

    #[test]
    fn parses_multi_file_torrent() {
        use sha1::{Digest, Sha1};
        // Two files, 10 + 22 bytes, piece length 16 -> 2 pieces.
        let total = 32u64;
        let piece_hashes: Vec<u8> = (0..2)
            .flat_map(|_| Sha1::digest([0u8; 16]).to_vec())
            .collect();
        let mut info = Vec::new();
        info.extend_from_slice(b"d5:filesl");
        info.extend_from_slice(b"d6:lengthi10e4:pathl5:a.txtee");
        info.extend_from_slice(b"d6:lengthi22e4:pathl3:sub5:b.txteee");
        info.extend_from_slice(b"4:name3:dir12:piece lengthi16e");
        info.extend_from_slice(format!("6:pieces{}:", piece_hashes.len()).as_bytes());
        info.extend_from_slice(&piece_hashes);
        info.extend_from_slice(b"e");

        let meta = Metainfo::from_info_bytes(&info, vec![]).unwrap();
        assert!(meta.multi_file);
        assert_eq!(meta.total_length, total);
        assert_eq!(meta.files.len(), 2);
        assert_eq!(meta.files[0].path, PathBuf::from("a.txt"));
        assert_eq!(meta.files[1].path, PathBuf::from("sub").join("b.txt"));
        assert_eq!(meta.files[1].offset, 10);
    }

    #[test]
    fn rejects_path_traversal() {
        use sha1::{Digest, Sha1};
        let hash = Sha1::digest([0u8; 4]);
        let mut info = Vec::new();
        info.extend_from_slice(b"d5:filesl");
        info.extend_from_slice(b"d6:lengthi4e4:pathl2:..6:pwned!ee");
        info.extend_from_slice(b"e4:name3:dir12:piece lengthi16e");
        info.extend_from_slice(format!("6:pieces{}:", hash.len()).as_bytes());
        info.extend_from_slice(&hash);
        info.extend_from_slice(b"e");

        let err = Metainfo::from_info_bytes(&info, vec![]).unwrap_err();
        assert!(err.to_string().contains("unsafe path"), "got: {err}");
    }

    #[test]
    fn rejects_piece_count_mismatch() {
        let data = vec![1u8; 32];
        let mut doc = test_support::single_file_torrent("http://t/a", "x", 16, &data);
        // Corrupt: claim a bigger length than the pieces cover.
        let pos = doc.windows(12).position(|w| w == b"6:lengthi32e").unwrap();
        doc[pos..pos + 12].copy_from_slice(b"6:lengthi99e");
        assert!(Metainfo::from_torrent_bytes(&doc).is_err());
    }
}
