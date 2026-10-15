//! `npeer verify`: re-check downloaded data against the piece hashes.

use std::pin::pin;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use npeer_core::session::{StoredStatus, unix_now};
use npeer_core::storage::Storage;
use npeer_core::{Metainfo, verify};
use tokio_util::sync::CancellationToken;

use crate::commands::Ctx;
use crate::config::ProgressMode;
use crate::ui::VerifyUi;
use crate::units;

pub async fn run(ctx: &Ctx, id: &str) -> Result<()> {
    let mut state = ctx.store.resolve(id)?;
    let doc = ctx
        .store
        .load_torrent_file(state.info_hash)?
        .with_context(|| format!("stored metadata for {} is missing", state.name))?;
    let metainfo = Arc::new(Metainfo::from_torrent_bytes(&doc)?);

    let selection = state.file_selection(metainfo.files.len());
    let storage = Arc::new(Storage::plan_selected(
        &state.output_dir,
        &metainfo,
        &selection,
        &ctx.store.parts_dir(state.info_hash),
    ));
    if !storage.any_file_exists() {
        bail!(
            "no files of {} exist in {} yet",
            state.name,
            state.output_dir.display()
        );
    }
    storage.open_all()?;

    let wanted = storage.wanted_pieces();
    let total = wanted.iter().filter(|&&w| w).count();
    println!("Checking {} against {total} pieces...", state.name);
    let have = check_pieces(ctx.settings.progress, &storage, &metainfo, wanted).await?;

    let good = have.iter().filter(|&&h| h).count();
    let verified_bytes: u64 = have
        .iter()
        .zip(storage.piece_weights())
        .filter(|&(&have, _)| have)
        .map(|(_, &weight)| weight)
        .sum();

    state.set_have_pieces(&have);
    state.status = if good == total {
        if state.completed_at.is_none() {
            state.completed_at = Some(unix_now());
        }
        StoredStatus::Completed
    } else {
        StoredStatus::Active
    };
    ctx.store.save(&state)?;

    if good == total {
        println!(
            "✓ All {total} pieces OK ({}).",
            units::bytes(verified_bytes)
        );
    } else {
        println!(
            "· {good}/{total} pieces OK ({} of {}).",
            units::bytes(verified_bytes),
            units::bytes(storage.selected_length()),
        );
        println!(
            "  Repair the rest with: npeer resume {}",
            &state.info_hash.to_hex()[..8]
        );
    }
    Ok(())
}

/// Hash-check the pieces marked in `check` with a progress display.
pub async fn check_pieces(
    progress: ProgressMode,
    storage: &Arc<Storage>,
    metainfo: &Arc<Metainfo>,
    check: Vec<bool>,
) -> Result<Vec<bool>> {
    let count = check.iter().filter(|&&c| c).count() as u32;
    let verify_ui = VerifyUi::new(progress, count);
    // The verify callback must be 'static, so progress goes via a channel.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut task = pin!(verify::verify_pieces(
        storage.clone(),
        metainfo.clone(),
        check,
        CancellationToken::new(),
        move |checked, total| {
            let _ = tx.send((checked, total));
        },
    ));
    let have = loop {
        tokio::select! {
            result = &mut task => break result?,
            Some((checked, _)) = rx.recv() => verify_ui.update(checked),
        }
    };
    verify_ui.finish();
    Ok(have)
}
