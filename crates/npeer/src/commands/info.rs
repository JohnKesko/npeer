//! `npeer info`: show torrent metadata.

use anyhow::Result;

use crate::commands::{Ctx, parse_source, resolve_source};
use crate::units;

pub async fn run(ctx: &Ctx, source: &str) -> Result<()> {
    let source = parse_source(source)?;
    let (metainfo, _) = resolve_source(ctx, &source).await?;

    println!();
    let row = |label: &str, value: String| println!("{label:<12} {value}");
    row("Name", metainfo.name.clone());
    row("Info hash", metainfo.info_hash.to_hex());
    row(
        "Size",
        format!(
            "{} ({} pieces of {})",
            units::bytes(metainfo.total_length),
            metainfo.piece_count(),
            units::bytes(metainfo.piece_length),
        ),
    );

    if metainfo.multi_file {
        row("Files", format!("{}", metainfo.files.len()));
        const SHOWN: usize = 20;
        for file in metainfo.files.iter().take(SHOWN) {
            println!("  {:<9} {}", units::bytes(file.length), file.path.display());
        }
        if metainfo.files.len() > SHOWN {
            println!("  … and {} more files", metainfo.files.len() - SHOWN);
        }
    } else {
        row("Files", format!("1 ({})", metainfo.files[0].path.display()));
    }

    let trackers = metainfo.all_trackers();
    if trackers.is_empty() {
        row("Trackers", "none".into());
    } else {
        row("Trackers", trackers[0].clone());
        for tracker in &trackers[1..] {
            println!("{:<12} {tracker}", "");
        }
    }

    row(
        "Private",
        if metainfo.private { "yes" } else { "no" }.into(),
    );
    if let Some(comment) = &metainfo.comment {
        row("Comment", comment.clone());
    }
    if let Some(created_by) = &metainfo.created_by {
        row("Created by", created_by.clone());
    }
    Ok(())
}
