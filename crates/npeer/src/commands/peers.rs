//! `npeer peers`: one-shot tracker query showing reachable peers.

use anyhow::{Result, bail};

use crate::commands::{
    Ctx, Source, announce_all, discovery_params, merge_peers, parse_source, resolve_source,
};
use crate::ui::{self, Phase};

pub async fn run(ctx: &Ctx, source: &str) -> Result<()> {
    let source = parse_source(source)?;

    // A torrent file carries its trackers; magnets carry their own list.
    let (info_hash, trackers, name) = match &source {
        Source::Magnet(link) if !link.trackers.is_empty() => {
            (link.info_hash, link.trackers.clone(), link.display_name())
        }
        Source::Magnet(link) => return Err(crate::commands::no_trackers_error(link)),
        Source::Torrent(_) => {
            let (metainfo, _) = resolve_source(ctx, &source).await?;
            let trackers = metainfo.all_trackers();
            if trackers.is_empty() {
                bail!("this torrent lists no trackers");
            }
            (metainfo.info_hash, trackers, metainfo.name.clone())
        }
    };

    let phase = Phase::start(ctx.settings.progress, "Contacting trackers");
    let params = discovery_params(&ctx.settings, info_hash);
    let results = announce_all(&trackers, &params).await;
    let peers = merge_peers(&results);
    phase.done(Some(&format!("{}: {} unique peers", name, peers.len())));

    println!();
    let mut rows = Vec::new();
    for (url, result) in &results {
        match result {
            Ok(response) => {
                let swarm = match (response.seeders, response.leechers) {
                    (Some(s), Some(l)) => format!("{s} seeds / {l} peers"),
                    _ => "-".to_owned(),
                };
                rows.push(vec![
                    url.clone(),
                    format!("{} peers", response.peers.len()),
                    swarm,
                ]);
            }
            Err(err) => rows.push(vec![url.clone(), "failed".into(), err.to_string()]),
        }
    }
    print!("{}", ui::table(&["tracker", "returned", "swarm"], &rows));

    if !peers.is_empty() {
        println!();
        for peer in &peers {
            println!("{peer}");
        }
    }
    Ok(())
}
