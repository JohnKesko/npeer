//! Persisted per-torrent state, kept separate from downloaded content.
//!
//! Layout inside the application data directory:
//!
//! ```text
//! <data>/torrents/<info-hash>.json     resume state
//! <data>/torrents/<info-hash>.torrent  copy of the metadata
//! <data>/torrents/<info-hash>.parts/   leftovers of skipped files
//! ```
//!
//! The `.torrent` copy means magnet downloads survive restarts without
//! re-fetching metadata, and files are never mixed into the download folder.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::metainfo::Metainfo;
use crate::selection::FileSelection;
use crate::types::InfoHash;

pub const STATE_VERSION: u32 = 1;

/// Lifecycle of a known torrent, as persisted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoredStatus {
    /// Incomplete; picked up by automatic resume.
    Active,
    /// Paused by the user; still included in `resume --all`.
    Paused,
    /// Stopped by the user; only resumes when named explicitly.
    Stopped,
    Completed,
}

/// Everything needed to resume a torrent later.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TorrentState {
    pub version: u32,
    pub info_hash: InfoHash,
    pub name: String,
    pub output_dir: PathBuf,
    pub total_length: u64,
    pub piece_count: u32,
    /// Hex-encoded verified-piece bitfield (wire format).
    pub have: String,
    pub downloaded: u64,
    pub uploaded: u64,
    pub status: StoredStatus,
    /// Unix timestamps (seconds).
    pub added_at: u64,
    pub completed_at: Option<u64>,
    /// Which files to download; absent when it is every file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<StoredSelection>,
}

/// A choice of files that leaves some out.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredSelection {
    /// 0-based indices of the files not downloaded.
    pub skipped_files: Vec<u32>,
    /// Hex-encoded bitfield of the pieces the chosen files need.
    pub pieces: String,
    /// Total size of the chosen files.
    pub length: u64,
}

impl TorrentState {
    pub fn have_pieces(&self) -> Vec<bool> {
        decode_bits(&self.have, self.piece_count)
    }

    pub fn set_have_pieces(&mut self, have: &[bool]) {
        self.have = encode_bits(have);
    }

    /// Remember which files are downloaded.
    pub fn set_selection(&mut self, selection: &FileSelection, metainfo: &Metainfo) {
        self.selection = (!selection.is_everything()).then(|| {
            let wanted: Vec<bool> = selection
                .piece_weights(metainfo)
                .iter()
                .map(|&weight| weight > 0)
                .collect();
            StoredSelection {
                skipped_files: selection.skipped(),
                pieces: encode_bits(&wanted),
                length: selection.selected_length(metainfo),
            }
        });
    }

    /// The remembered file choice for a torrent with `file_count` files.
    pub fn file_selection(&self, file_count: usize) -> FileSelection {
        match &self.selection {
            Some(stored) => FileSelection::skipping(file_count, &stored.skipped_files),
            None => FileSelection::all(file_count),
        }
    }

    /// Size of what is being downloaded: the chosen files, or everything.
    pub fn selected_length(&self) -> u64 {
        self.selection
            .as_ref()
            .map_or(self.total_length, |stored| stored.length)
    }

    /// Pieces the chosen files need.
    pub fn wanted_pieces(&self) -> Vec<bool> {
        match &self.selection {
            Some(stored) => decode_bits(&stored.pieces, self.piece_count),
            None => vec![true; self.piece_count as usize],
        }
    }

    /// Verified pieces among the wanted ones, and how many are wanted.
    pub fn piece_progress(&self) -> (usize, usize) {
        let wanted = self.wanted_pieces();
        let have = self
            .have_pieces()
            .iter()
            .zip(&wanted)
            .filter(|&(&have, &wanted)| have && wanted)
            .count();
        (have, wanted.iter().filter(|&&w| w).count())
    }

    pub fn progress(&self) -> f64 {
        match self.piece_progress() {
            (_, 0) => 0.0,
            (have, wanted) => have as f64 / wanted as f64,
        }
    }
}

/// Wire-format bitfield (high bit of byte 0 is piece 0) as hex.
fn encode_bits(bits: &[bool]) -> String {
    let mut bytes = vec![0u8; bits.len().div_ceil(8)];
    for (index, &set) in bits.iter().enumerate() {
        if set {
            bytes[index / 8] |= 0x80 >> (index % 8);
        }
    }
    hex::encode(bytes)
}

fn decode_bits(hex_bits: &str, count: u32) -> Vec<bool> {
    let bytes = hex::decode(hex_bits).unwrap_or_default();
    (0..count as usize)
        .map(|index| {
            bytes
                .get(index / 8)
                .is_some_and(|byte| byte & (0x80 >> (index % 8)) != 0)
        })
        .collect()
}

/// File-based store for torrent states and metadata copies.
#[derive(Clone, Debug)]
pub struct StateStore {
    torrents_dir: PathBuf,
}

impl StateStore {
    /// Open (creating if needed) the store under the given data directory.
    pub fn open(data_dir: &Path) -> Result<Self> {
        let torrents_dir = data_dir.join("torrents");
        fs::create_dir_all(&torrents_dir)
            .map_err(|e| Error::Storage(format!("creating {}: {e}", torrents_dir.display())))?;
        Ok(StateStore { torrents_dir })
    }

    fn state_path(&self, info_hash: InfoHash) -> PathBuf {
        self.torrents_dir.join(format!("{info_hash}.json"))
    }

    pub fn torrent_file_path(&self, info_hash: InfoHash) -> PathBuf {
        self.torrents_dir.join(format!("{info_hash}.torrent"))
    }

    /// Where skipped files keep the bytes they share with chosen ones.
    pub fn parts_dir(&self, info_hash: InfoHash) -> PathBuf {
        self.torrents_dir.join(format!("{info_hash}.parts"))
    }

    /// Persist state atomically (write to a temp file, then rename).
    pub fn save(&self, state: &TorrentState) -> Result<()> {
        let path = self.state_path(state.info_hash);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(state)
            .map_err(|e| Error::Other(format!("encoding state: {e}")))?;
        fs::write(&tmp, json)
            .map_err(|e| Error::Storage(format!("writing {}: {e}", tmp.display())))?;
        fs::rename(&tmp, &path)
            .map_err(|e| Error::Storage(format!("saving {}: {e}", path.display())))?;
        Ok(())
    }

    /// Store a copy of the torrent metadata document.
    pub fn save_torrent_file(&self, info_hash: InfoHash, torrent_bytes: &[u8]) -> Result<()> {
        let path = self.torrent_file_path(info_hash);
        fs::write(&path, torrent_bytes)
            .map_err(|e| Error::Storage(format!("writing {}: {e}", path.display())))?;
        Ok(())
    }

    pub fn load(&self, info_hash: InfoHash) -> Result<Option<TorrentState>> {
        let path = self.state_path(info_hash);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::Storage(format!("reading {}: {e}", path.display()))),
        };
        let state: TorrentState = serde_json::from_slice(&bytes)
            .map_err(|e| Error::Storage(format!("{} is corrupt: {e}", path.display())))?;
        Ok(Some(state))
    }

    pub fn load_torrent_file(&self, info_hash: InfoHash) -> Result<Option<Vec<u8>>> {
        let path = self.torrent_file_path(info_hash);
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::Storage(format!("reading {}: {e}", path.display()))),
        }
    }

    /// All known torrents, sorted by when they were added.
    pub fn list(&self) -> Result<Vec<TorrentState>> {
        let mut states = Vec::new();
        let entries = fs::read_dir(&self.torrents_dir)
            .map_err(|e| Error::Storage(format!("reading state directory: {e}")))?;
        for entry in entries {
            let path = entry.map_err(|e| Error::Storage(e.to_string()))?.path();
            if path.extension().is_some_and(|ext| ext == "json")
                && let Ok(bytes) = fs::read(&path)
                && let Ok(state) = serde_json::from_slice::<TorrentState>(&bytes)
            {
                states.push(state);
            }
        }
        states.sort_by_key(|s| s.added_at);
        Ok(states)
    }

    /// Remove a torrent's state, metadata copy and part files.
    pub fn remove(&self, info_hash: InfoHash) -> Result<()> {
        let parts = self.parts_dir(info_hash);
        match fs::remove_dir_all(&parts) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(Error::Storage(format!("removing {}: {e}", parts.display())));
            }
        }
        for path in [
            self.state_path(info_hash),
            self.torrent_file_path(info_hash),
        ] {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(Error::Storage(format!("removing {}: {e}", path.display())));
                }
            }
        }
        Ok(())
    }

    /// Resolve a user-supplied id: an info-hash prefix (4+ hex chars) or a
    /// case-insensitive name fragment. Must match exactly one torrent.
    pub fn resolve(&self, id: &str) -> Result<TorrentState> {
        let states = self.list()?;
        let id_lower = id.to_ascii_lowercase();

        let mut matches: Vec<&TorrentState> =
            if id_lower.len() >= 4 && id_lower.chars().all(|c| c.is_ascii_hexdigit()) {
                states
                    .iter()
                    .filter(|s| s.info_hash.to_hex().starts_with(&id_lower))
                    .collect()
            } else {
                Vec::new()
            };
        if matches.is_empty() {
            matches = states
                .iter()
                .filter(|s| s.name.to_ascii_lowercase().contains(&id_lower))
                .collect();
        }

        match matches.len() {
            0 => Err(Error::Other(format!(
                "no torrent matches `{id}` (see `npeer list`)"
            ))),
            1 => Ok(matches[0].clone()),
            n => Err(Error::Other(format!(
                "`{id}` is ambiguous: it matches {n} torrents (see `npeer list`)"
            ))),
        }
    }
}

/// Current Unix time in seconds.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state(hash_byte: u8, name: &str) -> TorrentState {
        let mut state = TorrentState {
            version: STATE_VERSION,
            info_hash: InfoHash([hash_byte; 20]),
            name: name.to_owned(),
            output_dir: PathBuf::from("/downloads"),
            total_length: 100,
            piece_count: 3,
            have: String::new(),
            downloaded: 0,
            uploaded: 0,
            status: StoredStatus::Active,
            added_at: unix_now(),
            completed_at: None,
            selection: None,
        };
        state.set_have_pieces(&[true, false, true]);
        state
    }

    #[test]
    fn bitfield_round_trips() {
        let state = sample_state(1, "a");
        assert_eq!(state.have_pieces(), vec![true, false, true]);
        assert!((state.progress() - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn selection_round_trips_and_drives_progress() {
        use crate::metainfo::test_support::multi_file_meta;
        // Files [0,10) [10,32) [32,40) in pieces of 16; skip the middle one.
        let meta = multi_file_meta(&[10, 22, 8], 16);
        let mut state = sample_state(3, "dir");
        state.total_length = 40;

        state.set_selection(&FileSelection::skipping(3, &[1]), &meta);
        assert_eq!(state.selected_length(), 18);
        assert_eq!(state.wanted_pieces(), vec![true, false, true]);
        assert_eq!(state.file_selection(3), FileSelection::skipping(3, &[1]));
        // Pieces 0 and 2 are had: both wanted pieces, so it is complete.
        assert_eq!(state.piece_progress(), (2, 2));
        assert!((state.progress() - 1.0).abs() < 1e-9);

        // Survives a save and load, and choosing everything clears it.
        let json = serde_json::to_string(&state).unwrap();
        let loaded: TorrentState = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.selected_length(), 18);
        state.set_selection(&FileSelection::all(3), &meta);
        assert!(state.selection.is_none());
        assert!(!serde_json::to_string(&state).unwrap().contains("selection"));
    }

    #[test]
    fn state_without_selection_still_loads() {
        // State files written before file selection existed.
        let mut value = serde_json::to_value(sample_state(4, "old")).unwrap();
        value.as_object_mut().unwrap().remove("selection");
        let state: TorrentState = serde_json::from_value(value).unwrap();
        assert!(state.selection.is_none());
        assert_eq!(state.selected_length(), state.total_length);
    }

    #[test]
    fn remove_deletes_part_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(dir.path()).unwrap();
        let state = sample_state(0xCC, "parts");
        store.save(&state).unwrap();
        let parts = store.parts_dir(state.info_hash);
        fs::create_dir_all(&parts).unwrap();
        fs::write(parts.join("1.part"), b"x").unwrap();

        store.remove(state.info_hash).unwrap();
        assert!(!parts.exists());
    }

    #[test]
    fn save_load_list_remove() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(dir.path()).unwrap();

        let state = sample_state(0xAA, "ubuntu.iso");
        store.save(&state).unwrap();
        store
            .save_torrent_file(state.info_hash, b"fake-torrent")
            .unwrap();

        let loaded = store.load(state.info_hash).unwrap().unwrap();
        assert_eq!(loaded.name, "ubuntu.iso");
        assert_eq!(
            store.load_torrent_file(state.info_hash).unwrap().unwrap(),
            b"fake-torrent"
        );
        assert_eq!(store.list().unwrap().len(), 1);

        store.remove(state.info_hash).unwrap();
        assert!(store.load(state.info_hash).unwrap().is_none());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn resolve_by_prefix_and_name() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(dir.path()).unwrap();
        store.save(&sample_state(0xAA, "ubuntu.iso")).unwrap();
        store.save(&sample_state(0xBB, "debian.iso")).unwrap();

        assert_eq!(store.resolve("aaaa").unwrap().name, "ubuntu.iso");
        assert_eq!(store.resolve("debian").unwrap().name, "debian.iso");
        assert!(store.resolve("iso").is_err(), "ambiguous");
        assert!(store.resolve("zzzz").is_err(), "no match");
    }

    #[test]
    fn missing_state_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(dir.path()).unwrap();
        assert!(store.load(InfoHash([9; 20])).unwrap().is_none());
    }
}
