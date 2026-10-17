//! `npeer info`: show torrent metadata.

use std::sync::Arc;

use anyhow::{Context, Result};
use npeer_core::Metainfo;

use crate::commands::{Ctx, Source, parse_source, resolve_source};
use crate::{ui, units};

/// Files listed before `--files` is needed to see the rest.
const SHOWN: usize = 20;

pub async fn run(ctx: &Ctx, input: &str, all_files: bool) -> Result<()> {
    // A torrent file or magnet link, or else a torrent npeer already knows.
    let (metainfo, pick_command) = match parse_source(input) {
        Ok(source) => {
            let (metainfo, _) = resolve_source(ctx, &source).await?;
            (metainfo, download_command(&source, input))
        }
        Err(not_a_source) => match known_torrent(ctx, input) {
            Ok(found) => found,
            // A path-looking input is a missing file, not a mistyped id.
            Err(_) if looks_like_path(input) => return Err(not_a_source),
            Err(not_known) => return Err(not_known),
        },
    };

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
        print_files(&metainfo, &pick_command, all_files);
    }
    Ok(())
}

/// A torrent from `npeer list`, by id or part of its name, read from the
/// copy npeer keeps. Files are picked for it with `npeer resume`.
fn known_torrent(ctx: &Ctx, id: &str) -> Result<(Arc<Metainfo>, String)> {
    let state = ctx.store.resolve(id)?;
    let doc = ctx
        .store
        .load_torrent_file(state.info_hash)?
        .with_context(|| format!("stored metadata for {} is missing", state.name))?;
    let metainfo = Metainfo::from_torrent_bytes(&doc)?;
    let command = format!("npeer resume {}", &state.info_hash.to_hex()[..8]);
    Ok((Arc::new(metainfo), command))
}

fn looks_like_path(input: &str) -> bool {
    input.contains(['/', '\\']) || input.ends_with(".torrent")
}

/// The numbered file list, set apart from the torrent's own details so the
/// numbers read as npeer's and not as part of the file names.
fn print_files(metainfo: &Metainfo, pick_command: &str, all_files: bool) {
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
    for example in pick_examples(pick_command, count) {
        println!("  {example}");
    }
}

/// The command that downloads `source`, as the user would retype it.
fn download_command(source: &Source, input: &str) -> String {
    let target = match source {
        Source::Magnet(_) => "<magnet link>".to_owned(),
        Source::Torrent(_) if input.contains(char::is_whitespace) => format!("'{input}'"),
        Source::Torrent(_) => input.to_owned(),
    };
    format!("npeer download {target}")
}

/// Ready-to-run `--only` / `--skip` commands for this torrent.
fn pick_examples(command: &str, file_count: usize) -> [String; 2] {
    let first_few = if file_count >= 3 { "1-3" } else { "1" };
    [
        format!("{command} --only {first_few}"),
        format!("{command} --skip {file_count}"),
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
        let command = download_command(&file, "wired-cd.torrent");
        assert_eq!(
            pick_examples(&command, 18),
            [
                "npeer download wired-cd.torrent --only 1-3",
                "npeer download wired-cd.torrent --skip 18",
            ]
        );

        let spaced = Source::Torrent(PathBuf::from("my album.torrent"));
        let command = download_command(&spaced, "my album.torrent");
        let [only, skip] = pick_examples(&command, 2);
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
        let command = download_command(&magnet, "magnet:?xt=urn:btih:0101");
        let [only, _] = pick_examples(&command, 5);
        assert_eq!(only, "npeer download <magnet link> --only 1-3");
    }

    #[test]
    fn known_torrents_pick_files_with_resume() {
        let [only, skip] = pick_examples("npeer resume 5b1f0c9e", 6);
        assert_eq!(only, "npeer resume 5b1f0c9e --only 1-3");
        assert_eq!(skip, "npeer resume 5b1f0c9e --skip 6");
        assert!(looks_like_path("~/Downloads/x.torrent"));
        assert!(looks_like_path("x.torrent"));
        assert!(!looks_like_path("5b1f0c9e"));
        assert!(!looks_like_path("nature"));
    }
}
