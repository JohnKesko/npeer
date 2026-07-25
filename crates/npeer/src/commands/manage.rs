//! `npeer pause`, `stop` and `remove`: manage stored torrents.
//!
//! npeer runs one download per invocation, so these commands act on stored
//! state: `pause` keeps a torrent eligible for `resume --all`, `stop` parks
//! it until resumed by name, and `remove` forgets it entirely. A download
//! that is currently running in another terminal is paused with Ctrl+C.

use std::io::Write;

use anyhow::{Context, Result, bail};
use npeer_core::Metainfo;
use npeer_core::session::StoredStatus;
use npeer_core::storage::Storage;

use crate::commands::Ctx;

pub fn pause(ctx: &Ctx, id: &str) -> Result<()> {
    set_status(ctx, id, StoredStatus::Paused, "Paused")
}

pub fn stop(ctx: &Ctx, id: &str) -> Result<()> {
    set_status(ctx, id, StoredStatus::Stopped, "Stopped")
}

fn set_status(ctx: &Ctx, id: &str, status: StoredStatus, verb: &str) -> Result<()> {
    let mut state = ctx.store.resolve(id)?;
    if state.status == StoredStatus::Completed {
        bail!(
            "{} is already completed; use `npeer remove` to forget it",
            state.name
        );
    }
    state.status = status;
    ctx.store.save(&state)?;
    let hint = match status {
        StoredStatus::Paused => "it will resume with `npeer resume --all`",
        _ => "resume it explicitly with `npeer resume`",
    };
    println!("{verb} {} — {hint}.", state.name);
    println!(
        "Note: a download running in another terminal keeps running; pause it there with Ctrl+C."
    );
    Ok(())
}

pub fn remove(ctx: &Ctx, id: &str, delete_files: bool, assume_yes: bool) -> Result<()> {
    let state = ctx.store.resolve(id)?;

    if delete_files && !assume_yes {
        print!(
            "Delete the downloaded files of `{}` in {}? [y/N] ",
            state.name,
            state.output_dir.display()
        );
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .context("could not read your answer")?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Nothing removed.");
            return Ok(());
        }
    }

    if delete_files {
        match ctx.store.load_torrent_file(state.info_hash)? {
            Some(doc) => {
                let metainfo = Metainfo::from_torrent_bytes(&doc)?;
                let storage = Storage::plan(&state.output_dir, &metainfo);
                storage.delete_files()?;
                println!("Deleted files of {}.", state.name);
            }
            None => {
                println!(
                    "Stored metadata for {} is missing; its files were left in place.",
                    state.name
                );
            }
        }
    }

    ctx.store.remove(state.info_hash)?;
    println!("Removed {} from npeer.", state.name);
    if !delete_files {
        println!(
            "Downloaded files were kept in {}.",
            state.output_dir.display()
        );
    }
    Ok(())
}
