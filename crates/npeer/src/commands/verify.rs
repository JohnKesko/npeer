//! `npeer verify`: re-check downloaded data against the piece hashes.

use std::pin::pin;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use npeer_core::session::{StoredStatus, unix_now};
use npeer_core::storage::Storage;
use npeer_core::{Metainfo, verify};
use tokio_util::sync::CancellationToken;

use crate::commands::Ctx;
use crate::ui::VerifyUi;
use crate::units;

pub async fn run(ctx: &Ctx, id: &str) -> Result<()> {
    let mut state = ctx.store.resolve(id)?;
    let doc = ctx
        .store
        .load_torrent_file(state.info_hash)?
        .with_context(|| format!("stored metadata for {} is missing", state.name))?;
    let metainfo = Arc::new(Metainfo::from_torrent_bytes(&doc)?);

    let storage = Arc::new(Storage::plan(&state.output_dir, &metainfo));
    if !storage.any_file_exists() {
        bail!(
            "no files of {} exist in {} yet",
            state.name,
            state.output_dir.display()
        );
    }
    storage.open_all()?;

    println!(
        "Checking {} against {} pieces...",
        state.name,
        metainfo.piece_count()
    );
    let verify_ui = VerifyUi::new(ctx.settings.progress, metainfo.piece_count());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut task = pin!(verify::verify_pieces(
        storage.clone(),
        metainfo.clone(),
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

    let good = have.iter().filter(|&&h| h).count() as u32;
    let total = metainfo.piece_count();
    let verified_bytes: u64 = (0..total)
        .filter(|&p| have[p as usize])
        .map(|p| metainfo.piece_size(p))
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
            units::bytes(metainfo.total_length),
        );
        println!(
            "  Repair the rest with: npeer resume {}",
            &state.info_hash.to_hex()[..8]
        );
    }
    Ok(())
}
