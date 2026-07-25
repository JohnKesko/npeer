//! Small strongly-typed values used across the engine.

use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};
use crate::util;

/// SHA-1 hash of the bencoded `info` dictionary. Identifies a torrent.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InfoHash(pub [u8; 20]);

impl InfoHash {
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Parse from a 40-character hex string.
    pub fn from_hex(s: &str) -> Result<Self> {
        let bytes = hex::decode(s).map_err(|_| bad_hash(s))?;
        let arr: [u8; 20] = bytes.try_into().map_err(|_| bad_hash(s))?;
        Ok(InfoHash(arr))
    }

    /// Parse from a 32-character base32 string (older magnet links, BEP 9).
    pub fn from_base32(s: &str) -> Result<Self> {
        let bytes = util::base32_decode(s).ok_or_else(|| bad_hash(s))?;
        let arr: [u8; 20] = bytes.try_into().map_err(|_| bad_hash(s))?;
        Ok(InfoHash(arr))
    }
}

fn bad_hash(s: &str) -> Error {
    Error::InvalidMagnet(format!("`{s}` is not a valid info hash"))
}

impl FromStr for InfoHash {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.len() {
            40 => Self::from_hex(s),
            32 => Self::from_base32(s),
            _ => Err(bad_hash(s)),
        }
    }
}

impl fmt::Display for InfoHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for InfoHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl serde::Serialize for InfoHash {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> serde::Deserialize<'de> for InfoHash {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        InfoHash::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// Our 20-byte peer id, in the Azureus convention: `-NP<version>-` + random.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PeerId(pub [u8; 20]);

impl PeerId {
    /// Generate a fresh peer id for this session.
    pub fn generate() -> Self {
        let mut id = [0u8; 20];
        let prefix = client_prefix();
        id[..prefix.len()].copy_from_slice(prefix.as_bytes());
        let random = util::random_alnum(20 - prefix.len());
        id[prefix.len()..].copy_from_slice(random.as_bytes());
        PeerId(id)
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }
}

/// `-NP0100-` style prefix derived from the crate version (e.g. 0.1.0 -> 0100).
fn client_prefix() -> String {
    let mut parts = env!("CARGO_PKG_VERSION").split('.');
    let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let patch: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    format!("-NP{}{}{}0-", major % 10, minor % 10, patch % 10)
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.0))
    }
}

impl fmt::Debug for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_hash_hex_round_trip() {
        let h = InfoHash::from_hex("d69f91e6b2ae4c542468d1073a71d4ea13879a7f").unwrap();
        assert_eq!(h.to_hex(), "d69f91e6b2ae4c542468d1073a71d4ea13879a7f");
        assert_eq!(h.to_string().len(), 40);
    }

    #[test]
    fn info_hash_rejects_garbage() {
        assert!(InfoHash::from_str("not-a-hash").is_err());
        assert!(InfoHash::from_str("").is_err());
        assert!(InfoHash::from_hex("zz").is_err());
    }

    #[test]
    fn peer_id_shape() {
        let id = PeerId::generate();
        assert_eq!(id.0.len(), 20);
        assert!(id.0.starts_with(b"-NP"));
        assert_eq!(id.0[7], b'-');
    }
}
