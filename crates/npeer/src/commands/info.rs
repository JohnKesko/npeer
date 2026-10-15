//! `npeer info`: show torrent metadata.

use anyhow::Result;
use npeer_core::Metainfo;

use crate::commands::{Ctx, Source, parse_source, resolve_source};
use crate::{ui, units};

/// Files listed before `--files` is needed to see the rest.
const SHOWN: usize = 20;

pub async fn run(ctx: &Ctx, input: &str, all_files: bool) -> Result<()> {
    let source = parse_source(input)?;
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
        row("Files", metainfo.files.len().to_string());
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

    if metainfo.multi_file {
        print_files(&metainfo, &source, input, all_files);
    }
    Ok(())
}

/// The numbered file list, set apart from the torrent's own details so the
/// numbers read as npeer's and not as part of the file names.
fn print_files(metainfo: &Metainfo, source: &Source, input: &str, all_files: bool) {
    let count = metainfo.files.len();
    let shown = if all_files { count } else { count.min(SHOWN) };
    let rows: Vec<Vec<String>> = metainfo
        .files
        .iter()
        .take(shown)
        .enumerate()
        .map(|(index, file)| {
            vec![
                (index + 1).to_string(),
                units::bytes(file.length),
                file.path.display().to_string(),
            ]
        })
        .collect();

    println!();
    print!("{}", ui::table(&["#", "size", "file"], &rows));
    if count > shown {
        println!(
            "… and {} more files (list them all with --files)",
            count - shown
        );
    }

    println!();
    println!("Download only some files by their number:");
    for example in pick_examples(source, input, count) {
        println!("  {example}");
    }
}

/// Ready-to-run `--only` / `--skip` commands for this torrent.
fn pick_examples(source: &Source, input: &str, file_count: usize) -> [String; 2] {
    let target = match source {
        Source::Magnet(_) => "<magnet link>".to_owned(),
        Source::Torrent(_) if input.contains(char::is_whitespace) => format!("'{input}'"),
        Source::Torrent(_) => input.to_owned(),
    };
    let first_few = if file_count >= 3 { "1-3" } else { "1" };
    [
        format!("npeer download {target} --only {first_few}"),
        format!("npeer download {target} --skip {file_count}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use npeer_core::{InfoHash, MagnetLink};
    use std::path::PathBuf;

    #[test]
    fn examples_use_real_numbers_and_a_runnable_path() {
        let file = Source::Torrent(PathBuf::from("wired-cd.torrent"));
        assert_eq!(
            pick_examples(&file, "wired-cd.torrent", 18),
            [
                "npeer download wired-cd.torrent --only 1-3",
                "npeer download wired-cd.torrent --skip 18",
            ]
        );

        let spaced = Source::Torrent(PathBuf::from("my album.torrent"));
        let [only, skip] = pick_examples(&spaced, "my album.torrent", 2);
        assert_eq!(only, "npeer download 'my album.torrent' --only 1");
        assert_eq!(skip, "npeer download 'my album.torrent' --skip 2");
    }

    #[test]
    fn magnet_examples_use_a_placeholder() {
        let magnet = Source::Magnet(MagnetLink {
            info_hash: InfoHash([1; 20]),
            name: None,
            trackers: vec![],
        });
        let [only, _] = pick_examples(&magnet, "magnet:?xt=urn:btih:0101", 5);
        assert_eq!(only, "npeer download <magnet link> --only 1-3");
    }
}
