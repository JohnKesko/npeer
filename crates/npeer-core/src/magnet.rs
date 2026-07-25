//! Magnet link parsing (BEP 9).

use std::str::FromStr;

use crate::error::{Error, Result};
use crate::types::InfoHash;

/// A parsed `magnet:` URI.
#[derive(Clone, Debug)]
pub struct MagnetLink {
    pub info_hash: InfoHash,
    /// Display name (`dn`), if present.
    pub name: Option<String>,
    /// Tracker URLs (`tr`), in the order they appeared.
    pub trackers: Vec<String>,
}

impl MagnetLink {
    pub fn parse(uri: &str) -> Result<Self> {
        let url = url::Url::parse(uri).map_err(|e| magnet_err(&format!("not a valid URI: {e}")))?;
        if url.scheme() != "magnet" {
            return Err(magnet_err("URI does not start with `magnet:`"));
        }

        let mut info_hash = None;
        let mut name = None;
        let mut trackers = Vec::new();

        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "xt" => {
                    if let Some(hash_text) = value.strip_prefix("urn:btih:") {
                        info_hash = Some(InfoHash::from_str(hash_text)?);
                    } else if value.starts_with("urn:btmh:") {
                        return Err(magnet_err(
                            "BitTorrent v2 magnet links (`urn:btmh`) are not supported yet",
                        ));
                    }
                }
                "dn" => name = Some(value.into_owned()),
                "tr" => trackers.push(value.into_owned()),
                _ => {}
            }
        }

        let info_hash =
            info_hash.ok_or_else(|| magnet_err("missing `xt=urn:btih:<info-hash>` parameter"))?;
        Ok(MagnetLink {
            info_hash,
            name,
            trackers,
        })
    }

    /// Tracker tiers in the shape `Metainfo` uses: one tier per `tr` entry.
    pub fn tracker_tiers(&self) -> Vec<Vec<String>> {
        self.trackers.iter().map(|t| vec![t.clone()]).collect()
    }

    /// Best-effort human name for display before metadata is known.
    pub fn display_name(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.info_hash.to_hex())
    }
}

/// True if the string looks like a magnet link rather than a file path.
pub fn is_magnet_uri(input: &str) -> bool {
    input
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("magnet:")
}

fn magnet_err(msg: &str) -> Error {
    Error::InvalidMagnet(msg.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "magnet:?xt=urn:btih:ad42ce8109f54c99613ce38f9b4d87e70f24a165&dn=magnet1.gif&tr=http%3A%2F%2Fbittorrent-test-tracker.codecrafters.io%2Fannounce";

    #[test]
    fn parses_reference_link() {
        let magnet = MagnetLink::parse(SAMPLE).unwrap();
        assert_eq!(
            magnet.info_hash.to_hex(),
            "ad42ce8109f54c99613ce38f9b4d87e70f24a165"
        );
        assert_eq!(magnet.name.as_deref(), Some("magnet1.gif"));
        assert_eq!(
            magnet.trackers,
            vec!["http://bittorrent-test-tracker.codecrafters.io/announce".to_owned()]
        );
    }

    #[test]
    fn parses_multiple_trackers() {
        let uri = "magnet:?xt=urn:btih:ad42ce8109f54c99613ce38f9b4d87e70f24a165\
                   &tr=udp%3A%2F%2Fa.example%3A6969&tr=https%3A%2F%2Fb.example%2Fannounce";
        let magnet = MagnetLink::parse(uri).unwrap();
        assert_eq!(magnet.trackers.len(), 2);
        assert_eq!(magnet.tracker_tiers().len(), 2);
        assert!(magnet.name.is_none());
    }

    #[test]
    fn parses_real_world_link_shape() {
        // Uppercase hash, percent-encoded display name, many trackers,
        // some without an /announce path — as pasted from a torrent site.
        let uri = "magnet:?xt=urn:btih:2A4FD5505BF534993AF015918CA75EB3E19EB532\
                   &dn=Some%20Linux%20ISO%20v1.0\
                   &tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337\
                   &tr=udp%3A%2F%2Fopen.stealth.si%3A80%2Fannounce\
                   &tr=udp%3A%2F%2Fexodus.desync.com%3A6969";
        let magnet = MagnetLink::parse(uri).unwrap();
        assert_eq!(
            magnet.info_hash.to_hex(),
            "2a4fd5505bf534993af015918ca75eb3e19eb532"
        );
        assert_eq!(magnet.name.as_deref(), Some("Some Linux ISO v1.0"));
        assert_eq!(magnet.trackers.len(), 3);
        assert_eq!(magnet.trackers[0], "udp://tracker.opentrackr.org:1337");
    }

    #[test]
    fn shell_truncated_link_still_parses_but_loses_trackers() {
        // What arrives when the shell eats everything after the first `&`.
        let uri = "magnet:?xt=urn:btih:2A4FD5505BF534993AF015918CA75EB3E19EB532";
        let magnet = MagnetLink::parse(uri).unwrap();
        assert!(magnet.trackers.is_empty());
        assert!(magnet.name.is_none());
    }

    #[test]
    fn parses_base32_hash() {
        // Same hash as SAMPLE, base32-encoded.
        let uri = "magnet:?xt=urn:btih:VVBM5AIJ6VGJSYJ44OHZWTMH44HSJILF";
        let magnet = MagnetLink::parse(uri).unwrap();
        assert_eq!(
            magnet.info_hash.to_hex(),
            "ad42ce8109f54c99613ce38f9b4d87e70f24a165"
        );
    }

    #[test]
    fn rejects_bad_links() {
        assert!(MagnetLink::parse("http://example.com").is_err());
        assert!(MagnetLink::parse("magnet:?dn=missing-hash").is_err());
        assert!(MagnetLink::parse("magnet:?xt=urn:btih:tooshort").is_err());
    }

    #[test]
    fn detects_magnet_uris() {
        assert!(is_magnet_uri(SAMPLE));
        assert!(is_magnet_uri("  MAGNET:?xt=urn:btih:x"));
        assert!(!is_magnet_uri("./file.torrent"));
    }
}
