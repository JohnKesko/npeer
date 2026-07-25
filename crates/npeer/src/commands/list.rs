//! `npeer list` and `npeer status`: what npeer knows about.

use anyhow::Result;
use npeer_core::session::{StoredStatus, TorrentState};

use crate::commands::Ctx;
use crate::ui;
use crate::units;

pub fn list(ctx: &Ctx) -> Result<()> {
    let states = ctx.store.list()?;
    if states.is_empty() {
        println!("No torrents yet. Start one with `npeer download <torrent|magnet>`.");
        return Ok(());
    }

    let rows: Vec<Vec<String>> = states
        .iter()
        .map(|state| {
            vec![
                short_id(state),
                units::truncate_name(&state.name, 48),
                units::bytes(state.total_length),
                format!("{:.1}%", state.progress() * 100.0),
                status_word(state).to_owned(),
            ]
        })
        .collect();
    print!(
        "{}",
        ui::table(&["id", "name", "size", "done", "status"], &rows)
    );
    Ok(())
}

pub fn status(ctx: &Ctx, id: Option<&str>) -> Result<()> {
    let states = match id {
        Some(id) => vec![ctx.store.resolve(id)?],
        None => ctx.store.list()?,
    };
    if states.is_empty() {
        println!("No torrents yet. Start one with `npeer download <torrent|magnet>`.");
        return Ok(());
    }

    for (index, state) in states.iter().enumerate() {
        if index > 0 {
            println!();
        }
        print_status(state);
    }
    Ok(())
}

fn print_status(state: &TorrentState) {
    let have = state.have_pieces().iter().filter(|&&h| h).count();
    println!("{}", state.name);
    let row = |label: &str, value: String| println!("  {label:<11} {value}");
    row("id", short_id(state));
    row(
        "status",
        format!(
            "{} ({:.1}% of {}, {have}/{} pieces)",
            status_word(state),
            state.progress() * 100.0,
            units::bytes(state.total_length),
            state.piece_count,
        ),
    );
    row("output", state.output_dir.display().to_string());
    let ratio = state.uploaded as f64 / state.total_length.max(1) as f64;
    row(
        "transferred",
        format!(
            "down {} · up {} (ratio {ratio:.2})",
            units::bytes(state.downloaded),
            units::bytes(state.uploaded),
        ),
    );
    row("added", local_time(state.added_at));
    if let Some(completed) = state.completed_at {
        row("completed", local_time(completed));
    }
    match state.status {
        StoredStatus::Active if state.progress() < 1.0 => {
            row("resume", format!("npeer resume {}", short_id(state)));
        }
        StoredStatus::Paused | StoredStatus::Stopped => {
            row("resume", format!("npeer resume {}", short_id(state)));
        }
        _ => {}
    }
}

fn short_id(state: &TorrentState) -> String {
    state.info_hash.to_hex()[..8].to_owned()
}

fn status_word(state: &TorrentState) -> &'static str {
    match state.status {
        StoredStatus::Active => {
            if state.progress() >= 1.0 {
                "active"
            } else {
                "incomplete"
            }
        }
        StoredStatus::Paused => "paused",
        StoredStatus::Stopped => "stopped",
        StoredStatus::Completed => "completed",
    }
}

fn local_time(unix: u64) -> String {
    chrono::DateTime::from_timestamp(unix as i64, 0)
        .map(|utc| {
            utc.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "-".to_owned())
}
