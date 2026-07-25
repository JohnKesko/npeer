//! Persisted per-torrent state, kept separate from downloaded content.
//!
//! Layout inside the application data directory:
//!
//! ```text
//! <data>/torrents/<info-hash>.json     resume state
//! <data>/torrents/<info-hash>.torrent  copy of the metadata
//! ```
//!
//! The `.torrent` copy means magnet downloads survive restarts without
//! re-fetching metadata, and files are never mixed into the download folder.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
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
}

impl TorrentState {
    pub fn have_pieces(&self) -> Vec<bool> {
        let bytes = hex::decode(&self.have).unwrap_or_default();
        (0..self.piece_count as usize)
            .map(|p| {
                bytes
                    .get(p / 8)
                    .is_some_and(|byte| byte & (0x80 >> (p % 8)) != 0)
            })
            .collect()
    }

    pub fn set_have_pieces(&mut self, have: &[bool]) {
        let mut bytes = vec![0u8; have.len().div_ceil(8)];
        for (piece, &has) in have.iter().enumerate() {
            if has {
                bytes[piece / 8] |= 0x80 >> (piece % 8);
            }
        }
        self.have = hex::encode(bytes);
    }

    pub fn progress(&self) -> f64 {
        if self.piece_count == 0 {
            return 0.0;
        }
        let have = self.have_pieces().iter().filter(|&&h| h).count();
        have as f64 / f64::from(self.piece_count)
    }
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

    /// Remove a torrent's state and metadata copy.
    pub fn remove(&self, info_hash: InfoHash) -> Result<()> {
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
