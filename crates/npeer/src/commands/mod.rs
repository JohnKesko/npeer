//! Command implementations.

pub mod config_cmd;
pub mod decode;
pub mod download;
pub mod info;
pub mod list;
pub mod manage;
pub mod peers;
pub mod verify;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use npeer_core::session::StateStore;
use npeer_core::tracker::{AnnounceEvent, AnnounceParams, AnnounceResponse};
use npeer_core::{MagnetLink, Metainfo, PeerId, magnet, metadata, tracker};
use tokio_util::sync::CancellationToken;

use crate::config::Settings;
use crate::ui::Phase;
use crate::units;

/// Shared context handed to every command.
pub struct Ctx {
    pub settings: Settings,
    pub store: StateStore,
}

impl Ctx {
    pub fn new(settings: Settings) -> Result<Ctx> {
        let store = StateStore::open(&settings.data_dir)
            .context("could not open npeer's data directory")?;
        Ok(Ctx { settings, store })
    }
}

/// What the user gave us on the command line.
#[derive(Debug)]
pub enum Source {
    Torrent(PathBuf),
    Magnet(MagnetLink),
}

pub fn parse_source(input: &str) -> Result<Source> {
    if magnet::is_magnet_uri(input) {
        return Ok(Source::Magnet(MagnetLink::parse(input)?));
    }
    let path = PathBuf::from(input);
    if path.is_dir() {
        // A directory argument is almost always a mistyped destination.
        bail!(
            "`{input}` is a directory, not a torrent file — to choose where \
             downloads go, use:  -o {input}"
        );
    }
    if !path.exists() {
        if input.contains("://") {
            bail!("`{input}` is not a magnet link or a local torrent file");
        }
        bail!("torrent file not found: {input}");
    }
    Ok(Source::Torrent(path))
}

/// Resolve a source to full metadata, plus the torrent document bytes.
///
/// Magnet links go through tracker discovery and the BEP 9 metadata
/// exchange; the result is cached in the state store so later runs skip
/// the network round trip.
pub async fn resolve_source(ctx: &Ctx, source: &Source) -> Result<(Arc<Metainfo>, Vec<u8>)> {
    match source {
        Source::Torrent(path) => {
            let phase = Phase::start(ctx.settings.progress, "Reading torrent");
            let bytes = std::fs::read(path)
                .with_context(|| format!("could not read {}", path.display()))?;
            let metainfo = match Metainfo::from_torrent_bytes(&bytes) {
                Ok(metainfo) => metainfo,
                Err(err) => {
                    phase.fail(&format!("{} is not a valid torrent", path.display()));
                    return Err(err.into());
                }
            };
            phase.done(Some(&format!(
                "Read {} ({}, {} pieces)",
                metainfo.name,
                units::bytes(metainfo.total_length),
                metainfo.piece_count()
            )));
            Ok((Arc::new(metainfo), bytes))
        }
        Source::Magnet(link) => resolve_magnet(ctx, link).await,
    }
}

async fn resolve_magnet(ctx: &Ctx, link: &MagnetLink) -> Result<(Arc<Metainfo>, Vec<u8>)> {
    // Cached metadata from an earlier run?
    if let Some(bytes) = ctx.store.load_torrent_file(link.info_hash)? {
        let phase = Phase::start(ctx.settings.progress, "Resolving magnet");
        match Metainfo::from_torrent_bytes(&bytes) {
            Ok(metainfo) => {
                phase.done(Some(&format!(
                    "Resolved {} (cached metadata)",
                    metainfo.name
                )));
                return Ok((Arc::new(metainfo), bytes));
            }
            Err(_) => phase.fail("Cached metadata was unreadable; fetching again"),
        }
    }

    if link.trackers.is_empty() {
        return Err(no_trackers_error(link));
    }

    // Ask the trackers for peers.
    let phase = Phase::start(ctx.settings.progress, "Contacting trackers");
    let params = discovery_params(&ctx.settings, link.info_hash);
    let results = announce_all(&link.trackers, &params).await;
    let ok_count = results.iter().filter(|(_, r)| r.is_ok()).count();
    let peers = merge_peers(&results);
    if peers.is_empty() {
        let detail = results
            .iter()
            .filter_map(|(url, r)| r.as_ref().err().map(|e| format!("  {url}: {e}")))
            .collect::<Vec<_>>()
            .join("\n");
        phase.fail("No peers found");
        bail!("no tracker returned any peers:\n{detail}");
    }
    phase.done(Some(&format!(
        "Found {} peers from {ok_count} of {} trackers",
        peers.len(),
        link.trackers.len()
    )));

    // Fetch the info dictionary from the swarm.
    let phase = Phase::start(
        ctx.settings.progress,
        &format!(
            "Fetching metadata from the swarm ({} peers to try)",
            peers.len()
        ),
    );
    let info_bytes = match metadata::fetch_from_peers(
        &peers,
        link.info_hash,
        PeerId::generate(),
        &CancellationToken::new(),
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(err) => {
            phase.fail("Could not fetch metadata");
            return Err(err.into());
        }
    };

    let tiers = link.tracker_tiers();
    let doc = metadata::build_torrent_file(&tiers, &info_bytes);
    let metainfo = Metainfo::from_info_bytes(&info_bytes, tiers)?;
    ctx.store.save_torrent_file(link.info_hash, &doc)?;
    phase.done(Some(&format!(
        "Resolved {} ({}, {} pieces)",
        metainfo.name,
        units::bytes(metainfo.total_length),
        metainfo.piece_count()
    )));
    Ok((Arc::new(metainfo), doc))
}

/// Error for magnets without trackers. A magnet that also lost its display
/// name almost always means the shell ate everything after the first `&`,
/// so point at the missing quotes rather than at DHT.
pub fn no_trackers_error(link: &MagnetLink) -> anyhow::Error {
    if link.name.is_none() {
        anyhow::anyhow!(
            "this magnet link lists no trackers — it was probably cut short by \
             your shell, which treats `&` specially.\n\
             Wrap the whole link in quotes:  npeer download 'magnet:?xt=...'"
        )
    } else {
        anyhow::anyhow!(
            "this magnet link lists no trackers, and npeer does not support \
             DHT-only magnets yet"
        )
    }
}

/// Parameters for one-shot discovery announces (`peers`, magnet resolution).
pub fn discovery_params(settings: &Settings, info_hash: npeer_core::InfoHash) -> AnnounceParams {
    AnnounceParams {
        info_hash,
        peer_id: PeerId::generate(),
        port: settings.listen_port,
        uploaded: 0,
        downloaded: 0,
        // Unknown until we have metadata; any non-zero value marks us a leecher.
        left: 1 << 20,
        event: AnnounceEvent::None,
        num_want: 80,
        key: rand_key(),
    }
}

fn rand_key() -> u32 {
    // Derived from the random peer id; avoids pulling rand into the CLI.
    let id = PeerId::generate();
    u32::from_be_bytes([id.0[16], id.0[17], id.0[18], id.0[19]])
}

/// Announce to every tracker concurrently.
pub async fn announce_all(
    trackers: &[String],
    params: &AnnounceParams,
) -> Vec<(String, npeer_core::Result<AnnounceResponse>)> {
    let mut tasks = tokio::task::JoinSet::new();
    for (index, url) in trackers.iter().enumerate() {
        let url = url.clone();
        let params = *params;
        tasks.spawn(async move {
            let result = tracker::announce(&url, &params).await;
            (index, url, result)
        });
    }
    let mut results: Vec<Option<(String, npeer_core::Result<AnnounceResponse>)>> =
        (0..trackers.len()).map(|_| None).collect();
    while let Some(joined) = tasks.join_next().await {
        if let Ok((index, url, result)) = joined {
            results[index] = Some((url, result));
        }
    }
    results.into_iter().flatten().collect()
}

/// Unique peer addresses across all announce results, in first-seen order.
pub fn merge_peers(results: &[(String, npeer_core::Result<AnnounceResponse>)]) -> Vec<SocketAddr> {
    let mut seen = std::collections::HashSet::new();
    let mut peers = Vec::new();
    for (_, result) in results {
        if let Ok(response) = result {
            for &addr in &response.peers {
                if seen.insert(addr) {
                    peers.push(addr);
                }
            }
        }
    }
    peers
}

#[cfg(test)]
mod tests {
    use super::*;
    use npeer_core::InfoHash;

    fn magnet(name: Option<&str>, trackers: Vec<String>) -> MagnetLink {
        MagnetLink {
            info_hash: InfoHash([7; 20]),
            name: name.map(str::to_owned),
            trackers,
        }
    }

    #[test]
    fn truncated_magnet_error_suggests_quoting() {
        // A shell-truncated paste keeps only `xt`: no name, no trackers.
        let error = no_trackers_error(&magnet(None, vec![]));
        let text = error.to_string();
        assert!(
            text.contains("quotes"),
            "should hint at shell quoting: {text}"
        );
        assert!(text.contains("npeer download 'magnet:?xt=...'"));
    }

    #[test]
    fn trackerless_magnet_with_name_mentions_dht() {
        // The link arrived whole (it has a display name); the real problem
        // is that npeer cannot do DHT-only magnets yet.
        let error = no_trackers_error(&magnet(Some("some name"), vec![]));
        let text = error.to_string();
        assert!(
            text.contains("DHT"),
            "should explain the DHT limitation: {text}"
        );
        assert!(!text.contains("quotes"));
    }

    #[test]
    fn parse_source_routes_magnets_and_files() {
        let uri = "magnet:?xt=urn:btih:2A4FD5505BF534993AF015918CA75EB3E19EB532";
        assert!(matches!(parse_source(uri).unwrap(), Source::Magnet(_)));

        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_string_lossy().into_owned();
        assert!(matches!(parse_source(&path).unwrap(), Source::Torrent(_)));

        let error = parse_source("missing.torrent").unwrap_err().to_string();
        assert!(error.contains("not found"));
    }

    #[test]
    fn parse_source_rejects_directories_with_an_output_hint() {
        // A stray directory argument almost always means the user wanted
        // `-o`; the error must say so instead of failing mid-queue later.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_string_lossy().into_owned();
        let error = parse_source(&path).unwrap_err().to_string();
        assert!(error.contains("is a directory"), "got: {error}");
        assert!(error.contains("-o"), "should hint at -o: {error}");
    }
}
