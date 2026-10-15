//! The `download` and `resume` commands: prepare storage, run the engine,
//! drive the live display, and leave resume state behind.

use std::path::PathBuf;
use std::pin::pin;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use npeer_core::session::{
    Engine, EngineOptions, EngineSummary, Event, StateStore, StateUpdate, StopReason, StoredStatus,
    TorrentState, unix_now,
};
use npeer_core::storage::Storage;
use npeer_core::{FileSelection, Metainfo};

use crate::commands::verify::check_pieces;
use crate::commands::{Ctx, Source, parse_source, resolve_source};
use crate::files::FileChoice;
use crate::ui::{self, DownloadUi};
use crate::units;

/// `npeer download <sources...>`
pub async fn run(
    ctx: &Ctx,
    sources: &[String],
    explicit_output: Option<PathBuf>,
    choice: Option<FileChoice>,
) -> Result<ExitCode> {
    if choice.is_some() && sources.len() > 1 {
        bail!(
            "--only and --skip pick files of one torrent at a time; \
             give a single torrent or magnet link"
        );
    }

    // Validate every source up front: a typo in the third argument must
    // fail now, not after the first download has run for an hour.
    let parsed: Vec<Source> = sources
        .iter()
        .map(|input| parse_source(input))
        .collect::<Result<_>>()?;

    let mut paused = false;
    for (index, source) in parsed.iter().enumerate() {
        if index > 0 {
            println!();
        }
        let (metainfo, doc) = resolve_source(ctx, source).await?;
        ctx.store.save_torrent_file(metainfo.info_hash, &doc)?;

        match download_one(ctx, metainfo, explicit_output.clone(), choice.as_ref()).await? {
            RunOutcome::Complete => {}
            RunOutcome::Paused => {
                paused = true;
                if index + 1 < sources.len() {
                    println!(
                        "Skipping the remaining {} download(s).",
                        sources.len() - index - 1
                    );
                }
                break;
            }
        }
    }
    Ok(if paused {
        ExitCode::from(130)
    } else {
        ExitCode::SUCCESS
    })
}

/// `npeer resume <id>` / `npeer resume --all`
pub async fn resume(
    ctx: &Ctx,
    id: Option<&str>,
    all: bool,
    choice: Option<FileChoice>,
) -> Result<ExitCode> {
    if choice.is_some() && id.is_none() {
        bail!("name the torrent to pick files from: npeer resume <id> --only 1,3");
    }
    let targets: Vec<TorrentState> = if all {
        ctx.store
            .list()?
            .into_iter()
            .filter(|s| {
                matches!(s.status, StoredStatus::Active | StoredStatus::Paused)
                    && s.progress() < 1.0
            })
            .collect()
    } else if let Some(id) = id {
        vec![ctx.store.resolve(id)?]
    } else {
        // Plain `npeer resume`: show what can be resumed and how.
        return show_resumable(ctx);
    };

    if targets.is_empty() {
        println!("Nothing to resume.");
        return Ok(ExitCode::SUCCESS);
    }

    for (index, state) in targets.iter().enumerate() {
        if index > 0 {
            println!();
        }
        let Some(doc) = ctx.store.load_torrent_file(state.info_hash)? else {
            println!(
                "· {}: stored metadata is missing; run `npeer download` with the \
                 original torrent or magnet link",
                state.name
            );
            continue;
        };
        let metainfo = Arc::new(Metainfo::from_torrent_bytes(&doc)?);
        let output_dir = Some(state.output_dir.clone());
        match download_one(ctx, metainfo, output_dir, choice.as_ref()).await? {
            RunOutcome::Complete => {}
            RunOutcome::Paused => return Ok(ExitCode::from(130)),
        }
    }
    Ok(ExitCode::SUCCESS)
}

enum RunOutcome {
    Complete,
    Paused,
}

/// `npeer resume` without arguments: list unfinished torrents with their
/// ids, so nobody has to guess where ids come from.
fn show_resumable(ctx: &Ctx) -> Result<ExitCode> {
    let unfinished: Vec<TorrentState> = ctx
        .store
        .list()?
        .into_iter()
        .filter(|s| s.progress() < 1.0)
        .collect();

    if unfinished.is_empty() {
        println!("Nothing to resume — every torrent npeer knows about is finished.");
        return Ok(ExitCode::SUCCESS);
    }

    println!("Unfinished downloads:\n");
    let rows: Vec<Vec<String>> = unfinished
        .iter()
        .map(|s| {
            vec![
                s.info_hash.to_hex()[..8].to_owned(),
                crate::units::truncate_name(&s.name, 48),
                format!("{:.1}%", s.progress() * 100.0),
            ]
        })
        .collect();
    print!("{}", ui::table(&["id", "name", "done"], &rows));
    println!();
    println!("Resume one:  npeer resume <id or part of the name>");
    println!("Resume all:  npeer resume --all");
    println!("Remove one:  npeer remove <id>   (--delete-files also deletes its files)");
    Ok(ExitCode::SUCCESS)
}

async fn download_one(
    ctx: &Ctx,
    metainfo: Arc<Metainfo>,
    explicit_output: Option<PathBuf>,
    choice: Option<&FileChoice>,
) -> Result<RunOutcome> {
    let settings = &ctx.settings;
    let previous = ctx.store.load(metainfo.info_hash)?;

    let output_dir = explicit_output
        .or_else(|| previous.as_ref().map(|s| s.output_dir.clone()))
        .unwrap_or_else(|| settings.download_dir.clone());

    // Download the newly chosen files, or keep the earlier choice.
    let file_count = metainfo.files.len();
    let earlier = previous.as_ref().map(|s| s.file_selection(file_count));
    let selection = match choice {
        Some(choice) => choice.apply(&metainfo)?,
        None => earlier
            .clone()
            .unwrap_or_else(|| FileSelection::all(file_count)),
    };

    let storage = Arc::new(Storage::plan_selected(
        &output_dir,
        &metainfo,
        &selection,
        &ctx.store.parts_dir(metainfo.info_hash),
    ));
    let files_present = storage.any_file_exists();
    let wanted = storage.wanted_pieces();
    let wanted_count = wanted.iter().filter(|&&w| w).count();
    // Pieces touching a file whose choice changed are stored differently
    // now, so what was recorded about them no longer counts.
    let changed = match &earlier {
        Some(earlier) => selection.changed_pieces(earlier, &metainfo),
        None => vec![false; wanted.len()],
    };

    // Figure out what we already have.
    let mut have: Vec<bool> = match &previous {
        Some(state) if state.piece_count == metainfo.piece_count() && files_present => state
            .have_pieces()
            .iter()
            .zip(wanted.iter().zip(&changed))
            .map(|(&have, (&wanted, &changed))| have && wanted && !changed)
            .collect(),
        _ => vec![false; metainfo.piece_count() as usize],
    };
    let had_state = previous.is_some();
    let already_have = have.iter().filter(|&&h| h).count();

    storage
        .open_all()
        .with_context(|| format!("could not prepare files in {}", output_dir.display()))?;

    if !selection.is_everything() {
        let note = if choice.is_none() {
            " · chosen earlier, change with --only or --skip"
        } else {
            ""
        };
        println!(
            "✓ Selected {} of {file_count} files ({}){note}",
            selection.wanted_count(),
            units::bytes(storage.selected_length())
        );
    }

    // Check data on disk: all of it when asked to, or when files exist
    // without usable state; otherwise just the pieces whose storage changed.
    let check_all = files_present && (settings.verify_on_resume || !had_state || already_have == 0);
    let check: Vec<bool> = wanted
        .iter()
        .zip(&changed)
        .map(|(&wanted, &changed)| files_present && wanted && (check_all || changed))
        .collect();
    if check.contains(&true) {
        let found = check_pieces(settings.progress, &storage, &metainfo, check.clone()).await?;
        for (piece, &checked) in check.iter().enumerate() {
            if checked {
                have[piece] = found[piece];
            }
        }
        let good = found.iter().filter(|&&ok| ok).count();
        if check_all && good > 0 {
            println!("✓ Verified existing data: {good}/{wanted_count} pieces intact");
        }
    }

    let complete_already = have
        .iter()
        .zip(&wanted)
        .all(|(&have, &wanted)| have || !wanted);
    if complete_already && !settings.seeding.enabled {
        save_state_snapshot(
            ctx,
            &metainfo,
            &output_dir,
            &have,
            &selection,
            &previous,
            StoredStatus::Completed,
        );
        println!(
            "✓ {} is already complete: {}",
            metainfo.name,
            storage.content_path().display()
        );
        return Ok(RunOutcome::Complete);
    }

    // Persist immediately so `npeer list` knows about this torrent.
    let template = save_state_snapshot(
        ctx,
        &metainfo,
        &output_dir,
        &have,
        &selection,
        &previous,
        StoredStatus::Active,
    );

    let options = EngineOptions {
        listen_port: settings.listen_port,
        max_peers: settings.max_peers,
        max_discovered: 500,
        download_limit: settings.download_limit,
        upload_limit: settings.upload_limit,
        seeding: settings.seeding,
        pipeline_depth: 16,
        prior_downloaded: template.downloaded,
        prior_uploaded: template.uploaded,
    };

    let state_hook = make_state_hook(ctx.store.clone(), template.clone());
    let engine = Engine::start(
        metainfo.clone(),
        storage.clone(),
        have,
        options.clone(),
        Some(state_hook),
    );

    let summary = drive_ui(ctx, &metainfo, &storage, &options, engine).await?;

    // Final state write reflecting the outcome.
    let final_status = if summary.complete {
        StoredStatus::Completed
    } else {
        StoredStatus::Active
    };
    let mut final_state = template.clone();
    final_state.set_have_pieces(&summary.have);
    final_state.downloaded = template.downloaded + summary.downloaded;
    final_state.uploaded = template.uploaded + summary.uploaded;
    final_state.status = final_status;
    if summary.complete && final_state.completed_at.is_none() {
        final_state.completed_at = Some(unix_now());
    }
    ctx.store.save(&final_state)?;

    let size = if selection.is_everything() {
        units::bytes(storage.selected_length())
    } else {
        format!(
            "{} ({} of {file_count} files)",
            units::bytes(storage.selected_length()),
            selection.wanted_count()
        )
    };
    ui::print_summary(
        &metainfo.name,
        storage.content_path(),
        &summary,
        &size,
        wanted_count,
        settings.seeding.enabled,
    );

    if summary.stop_reason == StopReason::Cancelled {
        let id = &metainfo.info_hash.to_hex()[..8];
        println!("\nResume any time with: npeer resume {id}");
        return Ok(RunOutcome::Paused);
    }
    Ok(RunOutcome::Complete)
}

/// Run the live display until the engine stops; returns its summary.
async fn drive_ui(
    ctx: &Ctx,
    metainfo: &Arc<Metainfo>,
    storage: &Arc<Storage>,
    options: &EngineOptions,
    mut engine: Engine,
) -> Result<EngineSummary> {
    let progress = engine.progress();
    let mut events = engine.take_events().expect("events taken once");
    let stopper = engine.stopper();

    // The title already names the download; the To: line shows just the
    // directory so the name does not appear twice.
    let target_dir = storage
        .content_path()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| storage.content_path().to_path_buf());
    let mut ui = DownloadUi::new(
        ctx.settings.progress,
        &metainfo.name,
        &target_dir,
        storage.selected_length(),
        options.download_limit,
        options.upload_limit,
    );

    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(250));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut events_open = true;
    let mut interrupts: u32 = 0;
    let mut wait = pin!(engine.wait());
    // A dropped SSH connection or closed terminal sends SIGHUP; treat it
    // like Ctrl+C so the download pauses cleanly instead of dying mid-write.
    let mut hangup = pin!(hangup_signal());
    let mut hangup_seen = false;

    let summary = loop {
        tokio::select! {
            result = &mut wait => break result?,

            _ = tokio::signal::ctrl_c() => {
                interrupts += 1;
                if interrupts == 1 {
                    ui.set_paused();
                    ui.note("Pausing — saving state (press Ctrl+C again to force quit)");
                    stopper.cancel();
                } else {
                    std::process::exit(130);
                }
            }

            _ = &mut hangup, if !hangup_seen => {
                hangup_seen = true;
                ui.set_paused();
                tracing::info!("hangup received; pausing and saving state");
                stopper.cancel();
            }

            _ = ticker.tick() => {
                ui.render(&progress.borrow());
            }

            event = events.recv(), if events_open => {
                match event {
                    Some(Event::PieceFailed { piece }) => {
                        // Shown as the re-downloaded counter in the live
                        // display; per-piece messages would flood it.
                        tracing::info!(piece, "piece failed verification; downloading again");
                    }
                    Some(Event::SeedingStarted) => {
                        ui.note("Download complete — now seeding");
                    }
                    Some(Event::TrackerFailed { url, error }) => {
                        tracing::info!(%url, %error, "tracker announce failed");
                    }
                    Some(Event::TrackerOk { .. }) | Some(Event::Completed) => {}
                    None => events_open = false,
                }
            }
        }
    };

    ui.finish();
    Ok(summary)
}

/// Resolves when the process receives SIGHUP (Unix only; never on Windows).
async fn hangup_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::hangup()) {
            Ok(mut hangups) => {
                hangups.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    }
    #[cfg(not(unix))]
    std::future::pending::<()>().await;
}

/// Save the current picture of a torrent, reusing history from any
/// previous state. Returns the state used as the template for hooks.
fn save_state_snapshot(
    ctx: &Ctx,
    metainfo: &Metainfo,
    output_dir: &std::path::Path,
    have: &[bool],
    selection: &FileSelection,
    previous: &Option<TorrentState>,
    status: StoredStatus,
) -> TorrentState {
    let mut state = TorrentState {
        version: npeer_core::session::STATE_VERSION,
        info_hash: metainfo.info_hash,
        name: metainfo.name.clone(),
        output_dir: output_dir.to_path_buf(),
        total_length: metainfo.total_length,
        piece_count: metainfo.piece_count(),
        have: String::new(),
        downloaded: previous.as_ref().map(|s| s.downloaded).unwrap_or(0),
        uploaded: previous.as_ref().map(|s| s.uploaded).unwrap_or(0),
        status,
        added_at: previous
            .as_ref()
            .map(|s| s.added_at)
            .unwrap_or_else(unix_now),
        completed_at: previous.as_ref().and_then(|s| s.completed_at),
        selection: None,
    };
    state.set_have_pieces(have);
    state.set_selection(selection, metainfo);
    // Choosing more files can turn a finished torrent unfinished again.
    if state.progress() < 1.0 {
        state.completed_at = None;
    }
    if status == StoredStatus::Completed && state.completed_at.is_none() {
        state.completed_at = Some(unix_now());
    }
    if let Err(err) = ctx.store.save(&state) {
        tracing::warn!(error = %err, "could not save torrent state");
    }
    state
}

/// Periodic engine callback that keeps resume state fresh on disk.
fn make_state_hook(store: StateStore, template: TorrentState) -> npeer_core::session::StateHook {
    let template = Mutex::new(template);
    Arc::new(move |update: StateUpdate| {
        let mut state = template.lock().unwrap();
        state.set_have_pieces(&update.have);
        let base_downloaded = state.downloaded;
        let base_uploaded = state.uploaded;
        // The template keeps pre-session totals; sessions add on top.
        let mut snapshot = state.clone();
        snapshot.downloaded = base_downloaded + update.downloaded_session;
        snapshot.uploaded = base_uploaded + update.uploaded_session;
        if update.complete {
            snapshot.status = StoredStatus::Completed;
            if snapshot.completed_at.is_none() {
                snapshot.completed_at = Some(unix_now());
            }
            state.completed_at = snapshot.completed_at;
        }
        if let Err(err) = store.save(&snapshot) {
            tracing::warn!(error = %err, "could not save torrent state");
        }
    })
}
