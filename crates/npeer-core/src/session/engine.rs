//! The download engine: one running torrent session.
//!
//! A coordinator task owns tracker scheduling, the peer address book and
//! lifecycle decisions. Each connected peer runs in its own task and talks
//! to the shared [`PiecePicker`], [`Storage`] and [`TransferStats`]. The
//! wire behaviour (handshake, interested/unchoke, 16 KiB block requests,
//! per-piece SHA-1 verification) matches the proven implementation, extended
//! with multiple peers, pipelining, endgame mode and uploading.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{Notify, Semaphore, broadcast, mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::metainfo::Metainfo;
use crate::peer::extension::{self, HANDSHAKE_ID, MetadataMessage, OUR_UT_METADATA_ID};
use crate::peer::{self, Message};
use crate::picker::{BlockRef, PeerBitfield, PiecePicker, Received};
use crate::ratelimit::RateLimiter;
use crate::stats::TransferStats;
use crate::storage::Storage;
use crate::tracker::{self, AnnounceEvent, AnnounceParams};
use crate::types::PeerId;
use crate::verify;

/// Largest block a remote peer may request from us.
const MAX_SERVED_BLOCK: u32 = 128 * 1024;
/// Peers we are willing to upload to at once.
const UNCHOKE_SLOTS: usize = 4;
/// Consider a peer snubbed after this long with requests outstanding.
const SNUB_TIMEOUT: Duration = Duration::from_secs(40);
/// Give up on a silent peer after this long.
const IDLE_TIMEOUT: Duration = Duration::from_secs(150);
/// Drop a discovered address after this many failed dials.
const MAX_DIAL_ATTEMPTS: u32 = 5;

/// Tunables for one engine run. The CLI builds this from its configuration.
#[derive(Clone, Debug)]
pub struct EngineOptions {
    /// Port reported to trackers (we do not accept incoming connections yet).
    pub listen_port: u16,
    /// Maximum concurrent peer connections.
    pub max_peers: usize,
    /// Cap on remembered peer addresses.
    pub max_discovered: usize,
    pub download_limit: Option<u64>,
    pub upload_limit: Option<u64>,
    pub seeding: SeedingPolicy,
    /// Blocks kept in flight per peer.
    pub pipeline_depth: usize,
    /// Historical totals from previous sessions, for ratio accounting.
    pub prior_downloaded: u64,
    pub prior_uploaded: u64,
}

impl Default for EngineOptions {
    fn default() -> Self {
        EngineOptions {
            listen_port: 6881,
            max_peers: 40,
            max_discovered: 500,
            download_limit: None,
            upload_limit: None,
            seeding: SeedingPolicy::default(),
            pipeline_depth: 16,
            prior_downloaded: 0,
            prior_uploaded: 0,
        }
    }
}

/// What to do once a download completes.
#[derive(Clone, Copy, Debug)]
pub struct SeedingPolicy {
    pub enabled: bool,
    /// Stop seeding at this upload/size ratio; 0 disables the check.
    pub max_ratio: f64,
    /// Stop seeding after this many minutes; 0 disables the check.
    pub max_minutes: u64,
}

impl Default for SeedingPolicy {
    fn default() -> Self {
        SeedingPolicy {
            enabled: false,
            max_ratio: 1.0,
            max_minutes: 0,
        }
    }
}

/// Engine-side phase of a running torrent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnginePhase {
    Downloading,
    Seeding,
}

/// Live progress, published a few times per second on a watch channel.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub phase: EnginePhase,
    pub total_bytes: u64,
    pub verified_bytes: u64,
    pub downloaded_session: u64,
    pub uploaded_session: u64,
    pub download_rate: u64,
    pub upload_rate: u64,
    pub average_download_rate: u64,
    pub peak_download_rate: u64,
    pub connected_peers: u32,
    pub connected_seeders: u32,
    pub discovered_peers: u32,
    pub swarm_seeders: Option<u32>,
    pub swarm_leechers: Option<u32>,
    pub pieces_have: u32,
    pub piece_count: u32,
    /// Pieces that failed their hash check and were downloaded again.
    pub hash_failures: u64,
    pub endgame: bool,
    pub elapsed: Duration,
}

/// Discrete happenings the UI may want to surface.
#[derive(Clone, Debug)]
pub enum Event {
    TrackerOk { url: String, peers_found: usize },
    TrackerFailed { url: String, error: String },
    PieceFailed { piece: u32 },
    Completed,
    SeedingStarted,
}

/// Why the engine stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// Download finished (and seeding, if enabled, ran to its limits).
    Complete,
    /// `stop()` was called, e.g. by Ctrl+C.
    Cancelled,
}

/// Final accounting returned when the engine stops.
#[derive(Clone, Debug)]
pub struct EngineSummary {
    pub stop_reason: StopReason,
    pub complete: bool,
    pub elapsed: Duration,
    pub downloaded: u64,
    pub uploaded: u64,
    pub wasted: u64,
    pub average_rate: u64,
    pub peak_rate: u64,
    /// All-time upload / content size.
    pub ratio: f64,
    pub seeded: bool,
    pub have: Vec<bool>,
}

/// Periodic resume-state callback: the engine stays persistence-agnostic.
pub type StateHook = Arc<dyn Fn(StateUpdate) + Send + Sync>;

#[derive(Clone, Debug)]
pub struct StateUpdate {
    pub have: Vec<bool>,
    pub downloaded_session: u64,
    pub uploaded_session: u64,
    pub complete: bool,
}

/// Handle to a running engine.
pub struct Engine {
    cancel: CancellationToken,
    progress: watch::Receiver<Snapshot>,
    events: Option<mpsc::UnboundedReceiver<Event>>,
    coordinator: JoinHandle<Result<EngineSummary>>,
}

impl Engine {
    /// Start downloading/seeding. `storage` must already be opened and
    /// `have` must describe which pieces are verified on disk.
    pub fn start(
        metainfo: Arc<Metainfo>,
        storage: Arc<Storage>,
        have: Vec<bool>,
        options: EngineOptions,
        state_hook: Option<StateHook>,
    ) -> Engine {
        let cancel = CancellationToken::new();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let picker = PiecePicker::new(
            metainfo.piece_count(),
            metainfo.piece_length,
            metainfo.total_length,
            have,
        );
        let initial_snapshot = Snapshot {
            phase: if picker.is_complete() {
                EnginePhase::Seeding
            } else {
                EnginePhase::Downloading
            },
            total_bytes: metainfo.total_length,
            verified_bytes: picker.verified_bytes(),
            downloaded_session: 0,
            uploaded_session: 0,
            download_rate: 0,
            upload_rate: 0,
            average_download_rate: 0,
            peak_download_rate: 0,
            connected_peers: 0,
            connected_seeders: 0,
            discovered_peers: 0,
            swarm_seeders: None,
            swarm_leechers: None,
            pieces_have: picker.have_count(),
            piece_count: picker.piece_count(),
            hash_failures: 0,
            endgame: false,
            elapsed: Duration::ZERO,
        };
        let (progress_tx, progress_rx) = watch::channel(initial_snapshot);

        let shared = Arc::new(Shared {
            metainfo,
            storage,
            picker: Mutex::new(picker),
            stats: TransferStats::new(),
            dl_limit: RateLimiter::new(options.download_limit),
            ul_limit: RateLimiter::new(options.upload_limit),
            notices: broadcast::channel(512).0,
            peers: Mutex::new(HashMap::new()),
            unchoke_slots: Arc::new(Semaphore::new(UNCHOKE_SLOTS)),
            peer_id: PeerId::generate(),
            events: events_tx,
            completion: Notify::new(),
            options: options.clone(),
        });

        let coordinator = tokio::spawn(coordinate(shared, cancel.clone(), progress_tx, state_hook));

        Engine {
            cancel,
            progress: progress_rx,
            events: Some(events_rx),
            coordinator,
        }
    }

    pub fn progress(&self) -> watch::Receiver<Snapshot> {
        self.progress.clone()
    }

    /// The event stream; can only be taken once.
    pub fn take_events(&mut self) -> Option<mpsc::UnboundedReceiver<Event>> {
        self.events.take()
    }

    /// Ask the engine to stop gracefully (state is saved, trackers told).
    pub fn stop(&self) {
        self.cancel.cancel();
    }

    /// A handle that can stop the engine after `wait()` has consumed it.
    pub fn stopper(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Wait for the engine to finish and collect the final summary.
    pub async fn wait(self) -> Result<EngineSummary> {
        self.coordinator
            .await
            .map_err(|e| Error::Other(format!("engine task failed: {e}")))?
    }
}

/// Everything peer tasks share.
struct Shared {
    metainfo: Arc<Metainfo>,
    storage: Arc<Storage>,
    picker: Mutex<PiecePicker>,
    stats: TransferStats,
    dl_limit: RateLimiter,
    ul_limit: RateLimiter,
    /// Broadcast of Have / endgame-Cancel notices to every peer task.
    notices: broadcast::Sender<Notice>,
    peers: Mutex<HashMap<u64, PeerView>>,
    unchoke_slots: Arc<Semaphore>,
    peer_id: PeerId,
    events: mpsc::UnboundedSender<Event>,
    /// Signalled by the peer task that verifies the last piece.
    completion: Notify,
    options: EngineOptions,
}

#[derive(Clone, Copy, Debug)]
enum Notice {
    Have(u32),
    CancelBlock { piece: u32, begin: u32 },
}

struct PeerView {
    #[allow(dead_code)]
    addr: SocketAddr,
    is_seeder: bool,
}

/// Per-address dialing state.
struct AddrInfo {
    busy: bool,
    attempts: u32,
    next_try: Instant,
}

struct TrackerSlot {
    url: String,
    next_at: Instant,
    in_flight: bool,
    started_sent: bool,
    ok_once: bool,
    failures: u32,
    pending_event: Option<AnnounceEvent>,
}

async fn coordinate(
    shared: Arc<Shared>,
    cancel: CancellationToken,
    progress_tx: watch::Sender<Snapshot>,
    state_hook: Option<StateHook>,
) -> Result<EngineSummary> {
    let started = Instant::now();
    let announce_key: u32 = rand::random();

    let mut book: HashMap<SocketAddr, AddrInfo> = HashMap::new();
    let mut trackers: Vec<TrackerSlot> = shared
        .metainfo
        .all_trackers()
        .into_iter()
        .map(|url| TrackerSlot {
            url,
            next_at: started,
            in_flight: false,
            started_sent: false,
            ok_once: false,
            failures: 0,
            pending_event: None,
        })
        .collect();

    let mut peer_tasks: JoinSet<(u64, SocketAddr, Result<()>)> = JoinSet::new();
    let mut announce_tasks: JoinSet<(usize, Result<tracker::AnnounceResponse>)> = JoinSet::new();
    let mut next_peer_key: u64 = 1;
    let mut swarm_seeders: Option<u32> = None;
    let mut swarm_leechers: Option<u32> = None;

    let mut complete = { shared.picker.lock().unwrap().is_complete() };
    if complete && !shared.options.seeding.enabled {
        // Already done and not asked to seed: nothing to run.
        let have = { shared.picker.lock().unwrap().have_vec() };
        return Ok(EngineSummary {
            stop_reason: StopReason::Complete,
            complete: true,
            elapsed: Duration::ZERO,
            downloaded: 0,
            uploaded: 0,
            wasted: 0,
            average_rate: 0,
            peak_rate: 0,
            ratio: shared.options.prior_uploaded as f64
                / shared.metainfo.total_length.max(1) as f64,
            seeded: false,
            have,
        });
    }
    let mut phase = if complete {
        EnginePhase::Seeding
    } else {
        EnginePhase::Downloading
    };
    let mut seeding_since: Option<Instant> = complete.then(Instant::now);
    let stop_reason;

    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut ticks_since_save: u32 = 0;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                stop_reason = StopReason::Cancelled;
                break;
            }

            _ = shared.completion.notified() => {
                if !complete {
                    complete = true;
                    handle_completion(&shared, &mut trackers, &state_hook).await;
                    if shared.options.seeding.enabled {
                        phase = EnginePhase::Seeding;
                        seeding_since = Some(Instant::now());
                        let _ = shared.events.send(Event::SeedingStarted);
                    } else {
                        stop_reason = StopReason::Complete;
                        break;
                    }
                }
            }

            _ = ticker.tick() => {
                publish_snapshot(
                    &shared, &progress_tx, phase, started,
                    book.len(), swarm_seeders, swarm_leechers,
                );

                // Completion can race the Notify permit; double-check here.
                if !complete && shared.picker.lock().unwrap().is_complete() {
                    shared.completion.notify_one();
                }

                // Seeding limits.
                if complete && phase == EnginePhase::Seeding
                    && seeding_limits_reached(&shared, seeding_since)
                {
                    stop_reason = StopReason::Complete;
                    break;
                }

                // Keep the swarm full.
                dial_peers(&shared, &cancel, &mut book, &mut peer_tasks, &mut next_peer_key);

                // Announce to due trackers.
                schedule_announces(&shared, &mut trackers, &mut announce_tasks, announce_key, complete);

                // Save resume state every 30 seconds.
                ticks_since_save += 1;
                if ticks_since_save >= 60 {
                    ticks_since_save = 0;
                    run_state_hook(&shared, &state_hook, complete);
                }
            }

            Some(joined) = peer_tasks.join_next(), if !peer_tasks.is_empty() => {
                if let Ok((_, addr, result)) = joined
                    && let Some(info) = book.get_mut(&addr)
                {
                    info.busy = false;
                    match result {
                        Ok(()) => {
                            info.attempts = 0;
                            info.next_try = Instant::now() + Duration::from_secs(120);
                        }
                        Err(_) => {
                            info.attempts += 1;
                            let backoff = 15u64.saturating_mul(1 << info.attempts.min(5));
                            info.next_try = Instant::now() + Duration::from_secs(backoff.min(600));
                        }
                    }
                }
            }

            Some(joined) = announce_tasks.join_next(), if !announce_tasks.is_empty() => {
                if let Ok((slot_index, result)) = joined {
                    apply_announce_result(
                        &shared, &mut trackers, slot_index, result,
                        &mut book, &mut swarm_seeders, &mut swarm_leechers,
                    );
                }
            }
        }
    }

    // Graceful shutdown: stop peers, flush data, tell trackers, save state.
    cancel.cancel();
    let drain = async { while peer_tasks.join_next().await.is_some() {} };
    if tokio::time::timeout(Duration::from_secs(5), drain)
        .await
        .is_err()
    {
        peer_tasks.abort_all();
    }

    {
        let storage = shared.storage.clone();
        let _ = tokio::task::spawn_blocking(move || storage.sync_all()).await;
    }
    run_state_hook(&shared, &state_hook, complete);
    announce_stopped(&shared, &trackers, announce_key).await;

    let elapsed = started.elapsed();
    let downloaded = shared.stats.downloaded();
    let uploaded = shared.stats.uploaded();
    let all_uploaded = shared.options.prior_uploaded + uploaded;
    let have = { shared.picker.lock().unwrap().have_vec() };
    Ok(EngineSummary {
        stop_reason,
        complete,
        elapsed,
        downloaded,
        uploaded,
        wasted: shared.stats.wasted(),
        average_rate: if elapsed.as_secs() > 0 {
            downloaded / elapsed.as_secs()
        } else {
            downloaded
        },
        peak_rate: shared.stats.peak_download_rate(),
        ratio: all_uploaded as f64 / shared.metainfo.total_length.max(1) as f64,
        seeded: seeding_since.is_some(),
        have,
    })
}

fn publish_snapshot(
    shared: &Arc<Shared>,
    progress_tx: &watch::Sender<Snapshot>,
    phase: EnginePhase,
    started: Instant,
    discovered: usize,
    swarm_seeders: Option<u32>,
    swarm_leechers: Option<u32>,
) {
    let (verified, pieces_have, piece_count, endgame) = {
        let picker = shared.picker.lock().unwrap();
        (
            picker.verified_bytes(),
            picker.have_count(),
            picker.piece_count(),
            picker.in_endgame(),
        )
    };
    let (connected, seeders) = {
        let peers = shared.peers.lock().unwrap();
        let seeders = peers.values().filter(|p| p.is_seeder).count() as u32;
        (peers.len() as u32, seeders)
    };
    let elapsed = started.elapsed();
    let downloaded = shared.stats.downloaded();

    let snapshot = Snapshot {
        phase,
        total_bytes: shared.metainfo.total_length,
        verified_bytes: verified,
        downloaded_session: downloaded,
        uploaded_session: shared.stats.uploaded(),
        download_rate: shared.stats.download_rate(),
        upload_rate: shared.stats.upload_rate(),
        average_download_rate: if elapsed.as_secs() > 0 {
            downloaded / elapsed.as_secs()
        } else {
            0
        },
        peak_download_rate: shared.stats.peak_download_rate(),
        connected_peers: connected,
        connected_seeders: seeders,
        discovered_peers: discovered as u32,
        swarm_seeders,
        swarm_leechers,
        pieces_have,
        piece_count,
        hash_failures: shared.stats.hash_failures(),
        endgame,
        elapsed,
    };
    let _ = progress_tx.send(snapshot);
}

fn run_state_hook(shared: &Arc<Shared>, state_hook: &Option<StateHook>, complete: bool) {
    if let Some(hook) = state_hook {
        let have = { shared.picker.lock().unwrap().have_vec() };
        hook(StateUpdate {
            have,
            downloaded_session: shared.stats.downloaded(),
            uploaded_session: shared.stats.uploaded(),
            complete,
        });
    }
}

fn seeding_limits_reached(shared: &Arc<Shared>, seeding_since: Option<Instant>) -> bool {
    let policy = shared.options.seeding;
    let all_uploaded = shared.options.prior_uploaded + shared.stats.uploaded();
    let ratio = all_uploaded as f64 / shared.metainfo.total_length.max(1) as f64;
    if policy.max_ratio > 0.0 && ratio >= policy.max_ratio {
        return true;
    }
    if policy.max_minutes > 0
        && seeding_since
            .is_some_and(|s| s.elapsed() >= Duration::from_secs(policy.max_minutes * 60))
    {
        return true;
    }
    false
}

async fn handle_completion(
    shared: &Arc<Shared>,
    trackers: &mut [TrackerSlot],
    state_hook: &Option<StateHook>,
) {
    let storage = shared.storage.clone();
    let _ = tokio::task::spawn_blocking(move || storage.sync_all()).await;
    let _ = shared.events.send(Event::Completed);
    run_state_hook(shared, state_hook, true);
    for slot in trackers.iter_mut() {
        if slot.ok_once {
            slot.pending_event = Some(AnnounceEvent::Completed);
            slot.next_at = Instant::now();
        }
    }
}

fn announce_params(shared: &Arc<Shared>, event: AnnounceEvent, key: u32) -> AnnounceParams {
    let left = {
        let picker = shared.picker.lock().unwrap();
        picker.remaining_bytes()
    };
    AnnounceParams {
        info_hash: shared.metainfo.info_hash,
        peer_id: shared.peer_id,
        port: shared.options.listen_port,
        uploaded: shared.stats.uploaded(),
        downloaded: shared.stats.downloaded(),
        left,
        event,
        num_want: 80,
        key,
    }
}

fn schedule_announces(
    shared: &Arc<Shared>,
    trackers: &mut [TrackerSlot],
    announce_tasks: &mut JoinSet<(usize, Result<tracker::AnnounceResponse>)>,
    announce_key: u32,
    _complete: bool,
) {
    let now = Instant::now();
    for (index, slot) in trackers.iter_mut().enumerate() {
        if slot.in_flight || now < slot.next_at {
            continue;
        }
        let event = slot.pending_event.take().unwrap_or(if slot.started_sent {
            AnnounceEvent::None
        } else {
            AnnounceEvent::Started
        });
        slot.in_flight = true;
        let params = announce_params(shared, event, announce_key);
        let url = slot.url.clone();
        announce_tasks.spawn(async move {
            let result = tracker::announce(&url, &params).await;
            (index, result)
        });
    }
}

fn apply_announce_result(
    shared: &Arc<Shared>,
    trackers: &mut [TrackerSlot],
    slot_index: usize,
    result: Result<tracker::AnnounceResponse>,
    book: &mut HashMap<SocketAddr, AddrInfo>,
    swarm_seeders: &mut Option<u32>,
    swarm_leechers: &mut Option<u32>,
) {
    let Some(slot) = trackers.get_mut(slot_index) else {
        return;
    };
    slot.in_flight = false;
    match result {
        Ok(response) => {
            slot.started_sent = true;
            slot.ok_once = true;
            slot.failures = 0;
            let interval = response
                .interval
                .max(response.min_interval.unwrap_or(Duration::from_secs(30)))
                .clamp(Duration::from_secs(30), Duration::from_secs(3600));
            slot.next_at = Instant::now() + interval;

            if response.seeders.is_some() {
                *swarm_seeders = response.seeders;
            }
            if response.leechers.is_some() {
                *swarm_leechers = response.leechers;
            }

            let peers_found = response.peers.len();
            let now = Instant::now();
            for addr in response.peers {
                if book.len() >= shared.options.max_discovered {
                    break;
                }
                book.entry(addr).or_insert(AddrInfo {
                    busy: false,
                    attempts: 0,
                    next_try: now,
                });
            }
            let _ = shared.events.send(Event::TrackerOk {
                url: slot.url.clone(),
                peers_found,
            });
            tracing::debug!(url = %slot.url, peers_found, "tracker announce ok");
        }
        Err(err) => {
            slot.failures += 1;
            let backoff = 30u64.saturating_mul(1 << slot.failures.min(4)).min(900);
            slot.next_at = Instant::now() + Duration::from_secs(backoff);
            let _ = shared.events.send(Event::TrackerFailed {
                url: slot.url.clone(),
                error: err.to_string(),
            });
            tracing::debug!(url = %slot.url, error = %err, "tracker announce failed");
        }
    }
}

async fn announce_stopped(shared: &Arc<Shared>, trackers: &[TrackerSlot], announce_key: u32) {
    let mut goodbyes = JoinSet::new();
    for slot in trackers.iter().filter(|s| s.ok_once) {
        let url = slot.url.clone();
        let params = announce_params(shared, AnnounceEvent::Stopped, announce_key);
        goodbyes.spawn(async move {
            let _ = tracker::announce(&url, &params).await;
        });
    }
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while goodbyes.join_next().await.is_some() {}
    })
    .await;
}

fn dial_peers(
    shared: &Arc<Shared>,
    cancel: &CancellationToken,
    book: &mut HashMap<SocketAddr, AddrInfo>,
    peer_tasks: &mut JoinSet<(u64, SocketAddr, Result<()>)>,
    next_peer_key: &mut u64,
) {
    let now = Instant::now();
    let free_slots = shared.options.max_peers.saturating_sub(peer_tasks.len());
    if free_slots == 0 {
        return;
    }
    let mut candidates: Vec<SocketAddr> = book
        .iter()
        .filter(|(_, info)| !info.busy && info.attempts < MAX_DIAL_ATTEMPTS && now >= info.next_try)
        .map(|(addr, _)| *addr)
        .collect();
    candidates.sort();

    for addr in candidates.into_iter().take(free_slots) {
        let info = book.get_mut(&addr).unwrap();
        info.busy = true;
        let key = *next_peer_key;
        *next_peer_key += 1;
        let shared = shared.clone();
        let cancel = cancel.clone();
        peer_tasks.spawn(async move {
            let result = run_peer(shared, cancel, key, addr).await;
            (key, addr, result)
        });
    }
}

/// One connected peer: the request/receive loop.
async fn run_peer(
    shared: Arc<Shared>,
    cancel: CancellationToken,
    key: u64,
    addr: SocketAddr,
) -> Result<()> {
    let piece_count = shared.metainfo.piece_count();

    let (stream, handshake) = tokio::select! {
        result = peer::connect(addr, shared.metainfo.info_hash, shared.peer_id) => result?,
        _ = cancel.cancelled() => return Ok(()),
    };
    tracing::debug!(%addr, peer_id = %handshake.peer_id, "peer connected");

    let (reader, mut writer) = stream.into_split();

    // Subscribe before registering so no Have notice is missed.
    let mut notices = shared.notices.subscribe();
    shared.peers.lock().unwrap().insert(
        key,
        PeerView {
            addr,
            is_seeder: false,
        },
    );

    // A dedicated reader task keeps message framing safe under select!.
    let (message_tx, mut message_rx) = mpsc::channel::<Result<Message>>(64);
    let reader_task = tokio::spawn(async move {
        let mut reader = reader;
        loop {
            let read = tokio::time::timeout(IDLE_TIMEOUT, peer::read_message(&mut reader)).await;
            let outcome = match read {
                Ok(Ok(message)) => message_tx.send(Ok(message)).await.is_ok(),
                Ok(Err(err)) => {
                    let _ = message_tx.send(Err(err)).await;
                    false
                }
                Err(_) => {
                    let _ = message_tx
                        .send(Err(Error::Timeout("waiting for peer".into())))
                        .await;
                    false
                }
            };
            if !outcome {
                break;
            }
        }
    });

    let result = drive_peer(
        &shared,
        &cancel,
        key,
        &mut writer,
        &mut message_rx,
        &mut notices,
        handshake.supports_extensions,
        piece_count,
    )
    .await;

    reader_task.abort();
    shared.peers.lock().unwrap().remove(&key);
    // The bitfield contribution is removed inside drive_peer's owner; claims
    // always need releasing here in case it bailed early.
    shared.picker.lock().unwrap().release_claims(key);

    if let Err(ref err) = result {
        tracing::debug!(%addr, error = %err, "peer disconnected");
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn drive_peer(
    shared: &Arc<Shared>,
    cancel: &CancellationToken,
    key: u64,
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    messages: &mut mpsc::Receiver<Result<Message>>,
    notices: &mut broadcast::Receiver<Notice>,
    peer_supports_extensions: bool,
    piece_count: u32,
) -> Result<()> {
    let mut remote_bits = PeerBitfield::new(piece_count);
    let mut bits_counted = false;
    let mut remote_choked_us = true;
    let mut remote_interested = false;
    let mut we_interested = false;
    let mut their_metadata_id: Option<u8> = None;
    let mut unchoke_permit: Option<tokio::sync::OwnedSemaphorePermit> = None;
    let mut in_flight: Vec<BlockRef> = Vec::new();
    let mut last_progress = Instant::now();

    // Extension handshake (lets peers fetch metadata from us, BEP 9).
    if peer_supports_extensions {
        let payload = extension::build_handshake(Some(shared.metainfo.info_bytes.len() as u64));
        peer::write_message(
            writer,
            &Message::Extended {
                id: HANDSHAKE_ID,
                payload,
            },
        )
        .await?;
    }

    // Tell the peer what we already have.
    {
        let bitfield = {
            let picker = shared.picker.lock().unwrap();
            (picker.have_count() > 0).then(|| picker.bitfield_bytes())
        };
        if let Some(bits) = bitfield {
            peer::write_message(writer, &Message::Bitfield(bits)).await?;
        }
    }

    let mut keepalive = tokio::time::interval(Duration::from_secs(60));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut watchdog = tokio::time::interval(Duration::from_secs(10));
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let result: Result<()> = loop {
        tokio::select! {
            _ = cancel.cancelled() => break Ok(()),

            received = messages.recv() => {
                let message = match received {
                    Some(Ok(message)) => message,
                    Some(Err(err)) => break Err(err),
                    None => break Err(Error::protocol("connection closed")),
                };
                if let Err(err) = handle_message(
                    shared, key, writer, message,
                    &mut remote_bits, &mut bits_counted, &mut remote_choked_us,
                    &mut remote_interested, &mut we_interested,
                    &mut their_metadata_id, &mut unchoke_permit,
                    &mut in_flight, &mut last_progress,
                ).await {
                    break Err(err);
                }
            }

            notice = notices.recv() => {
                match notice {
                    Ok(Notice::Have(piece)) => {
                        peer::write_message(writer, &Message::Have(piece)).await?;
                        update_interest(shared, writer, &remote_bits, &mut we_interested).await?;
                        // Both sides complete: nothing left to exchange.
                        if remote_bits.is_complete()
                            && shared.picker.lock().unwrap().is_complete()
                        {
                            break Ok(());
                        }
                    }
                    Ok(Notice::CancelBlock { piece, begin }) => {
                        if let Some(position) = in_flight
                            .iter()
                            .position(|b| b.piece == piece && b.begin == begin)
                        {
                            let block = in_flight.remove(position);
                            peer::write_message(writer, &Message::Cancel {
                                index: block.piece, begin: block.begin, length: block.length,
                            }).await?;
                        }
                    }
                    // Falling behind on notices only costs some duplicates.
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break Ok(()),
                }
            }

            _ = keepalive.tick() => {
                peer::write_message(writer, &Message::KeepAlive).await?;
            }

            _ = watchdog.tick() => {
                if !in_flight.is_empty() && last_progress.elapsed() > SNUB_TIMEOUT {
                    break Err(Error::protocol("peer stopped sending pieces"));
                }
                // Interest or unchoke slots may have changed while idle.
                update_interest(shared, writer, &remote_bits, &mut we_interested).await?;
                if remote_interested && unchoke_permit.is_none()
                    && let Ok(permit) = shared.unchoke_slots.clone().try_acquire_owned()
                {
                    unchoke_permit = Some(permit);
                    peer::write_message(writer, &Message::Unchoke).await?;
                }
                if !remote_choked_us {
                    top_up_requests(shared, key, writer, &remote_bits, &mut in_flight).await?;
                }
            }
        }
    };

    // Remove this peer's availability contribution.
    if bits_counted {
        shared.picker.lock().unwrap().peer_gone(key, &remote_bits);
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn handle_message(
    shared: &Arc<Shared>,
    key: u64,
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    message: Message,
    remote_bits: &mut PeerBitfield,
    bits_counted: &mut bool,
    remote_choked_us: &mut bool,
    remote_interested: &mut bool,
    we_interested: &mut bool,
    their_metadata_id: &mut Option<u8>,
    unchoke_permit: &mut Option<tokio::sync::OwnedSemaphorePermit>,
    in_flight: &mut Vec<BlockRef>,
    last_progress: &mut Instant,
) -> Result<()> {
    match message {
        Message::KeepAlive | Message::Unknown { .. } | Message::Cancel { .. } => {}

        Message::Bitfield(bytes) => {
            // Only the first bitfield counts toward availability.
            if !*bits_counted {
                remote_bits.set_from_bytes(&bytes);
                let mut picker = shared.picker.lock().unwrap();
                picker.peer_has_bitfield(remote_bits);
                *bits_counted = true;
            }
            let is_seeder = remote_bits.is_complete();
            if let Some(view) = shared.peers.lock().unwrap().get_mut(&key) {
                view.is_seeder = is_seeder;
            }
            if is_seeder && shared.picker.lock().unwrap().is_complete() {
                return Err(Error::protocol("both sides are seeds"));
            }
            update_interest(shared, writer, remote_bits, we_interested).await?;
        }

        Message::Have(piece) => {
            if !remote_bits.has(piece) {
                remote_bits.set(piece);
                if *bits_counted {
                    shared.picker.lock().unwrap().peer_has_piece(piece);
                } else {
                    // Haves before any bitfield: start counting now.
                    let mut picker = shared.picker.lock().unwrap();
                    picker.peer_has_bitfield(remote_bits);
                    *bits_counted = true;
                }
            }
            if remote_bits.is_complete()
                && let Some(view) = shared.peers.lock().unwrap().get_mut(&key)
            {
                view.is_seeder = true;
            }
            update_interest(shared, writer, remote_bits, we_interested).await?;
            if !*remote_choked_us {
                top_up_requests(shared, key, writer, remote_bits, in_flight).await?;
            }
        }

        Message::Choke => {
            *remote_choked_us = true;
            shared.picker.lock().unwrap().release_claims(key);
            in_flight.clear();
        }

        Message::Unchoke => {
            *remote_choked_us = false;
            *last_progress = Instant::now();
            top_up_requests(shared, key, writer, remote_bits, in_flight).await?;
        }

        Message::Interested => {
            *remote_interested = true;
            if unchoke_permit.is_none()
                && let Ok(permit) = shared.unchoke_slots.clone().try_acquire_owned()
            {
                *unchoke_permit = Some(permit);
                peer::write_message(writer, &Message::Unchoke).await?;
            }
        }

        Message::NotInterested => {
            *remote_interested = false;
            if unchoke_permit.take().is_some() {
                peer::write_message(writer, &Message::Choke).await?;
            }
        }

        Message::Piece { index, begin, data } => {
            *last_progress = Instant::now();
            in_flight.retain(|b| !(b.piece == index && b.begin == begin));
            shared.dl_limit.acquire(data.len() as u64).await;

            if !valid_block(shared, index, begin, data.len()) {
                shared.stats.record_wasted(data.len() as u64);
                return Err(Error::protocol("peer sent a malformed block"));
            }

            // Write first, then account, so verification reads real bytes.
            let offset = u64::from(index) * shared.metainfo.piece_length + u64::from(begin);
            let length = data.len() as u64;
            {
                let storage = shared.storage.clone();
                tokio::task::spawn_blocking(move || storage.write_at(offset, &data))
                    .await
                    .map_err(|e| Error::Other(format!("write task failed: {e}")))??;
            }
            let received = {
                let mut picker = shared.picker.lock().unwrap();
                picker.block_received(key, index, begin, length as u32)
            };
            match received {
                Received::Accepted { piece_complete } => {
                    shared.stats.record_download(length);
                    let endgame = { shared.picker.lock().unwrap().in_endgame() };
                    if endgame {
                        let _ = shared.notices.send(Notice::CancelBlock {
                            piece: index,
                            begin,
                        });
                    }
                    if piece_complete {
                        finish_piece(shared, key, index).await?;
                    }
                }
                Received::Duplicate | Received::Ignored => {
                    shared.stats.record_wasted(length);
                }
            }
            top_up_requests(shared, key, writer, remote_bits, in_flight).await?;
        }

        Message::Request {
            index,
            begin,
            length,
        } => {
            serve_block(
                shared,
                writer,
                unchoke_permit.is_some(),
                index,
                begin,
                length,
            )
            .await?;
        }

        Message::Extended {
            id: HANDSHAKE_ID,
            payload,
        } => {
            if let Ok(theirs) = extension::parse_handshake(&payload) {
                *their_metadata_id = theirs.ut_metadata_id;
            }
        }

        Message::Extended {
            id: OUR_UT_METADATA_ID,
            payload,
        } => {
            serve_metadata(shared, writer, *their_metadata_id, &payload).await?;
        }

        Message::Extended { .. } => {}
    }
    Ok(())
}

fn valid_block(shared: &Arc<Shared>, piece: u32, begin: u32, len: usize) -> bool {
    let piece_count = shared.metainfo.piece_count();
    if piece >= piece_count || len == 0 {
        return false;
    }
    let piece_size = shared.metainfo.piece_size(piece);
    u64::from(begin) + len as u64 <= piece_size
}

/// Hash-check a completed piece and update the world.
async fn finish_piece(shared: &Arc<Shared>, key: u64, piece: u32) -> Result<()> {
    let ok = {
        let storage = shared.storage.clone();
        let metainfo = shared.metainfo.clone();
        tokio::task::spawn_blocking(move || verify::verify_single_piece(&storage, &metainfo, piece))
            .await
            .map_err(|e| Error::Other(format!("verify task failed: {e}")))??
    };

    if ok {
        let complete = {
            let mut picker = shared.picker.lock().unwrap();
            picker.piece_verified(piece);
            picker.is_complete()
        };
        let _ = shared.notices.send(Notice::Have(piece));
        if complete {
            shared.completion.notify_one();
        }
    } else {
        let contributors = {
            let mut picker = shared.picker.lock().unwrap();
            picker.piece_failed(piece)
        };
        shared
            .stats
            .record_wasted(shared.metainfo.piece_size(piece));
        shared.stats.record_hash_failure();
        let _ = shared.events.send(Event::PieceFailed { piece });
        tracing::debug!(piece, "piece failed hash check; re-downloading");
        // If this peer was the only contributor, it is feeding us garbage.
        if contributors == [key] {
            return Err(Error::protocol("peer sent data failing hash verification"));
        }
    }
    Ok(())
}

async fn top_up_requests(
    shared: &Arc<Shared>,
    key: u64,
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    remote_bits: &PeerBitfield,
    in_flight: &mut Vec<BlockRef>,
) -> Result<()> {
    let depth = shared.options.pipeline_depth.max(1);
    if in_flight.len() >= depth {
        return Ok(());
    }
    let picked = {
        let mut picker = shared.picker.lock().unwrap();
        picker.pick_blocks(key, remote_bits, depth - in_flight.len())
    };
    for block in picked {
        peer::write_message(
            writer,
            &Message::Request {
                index: block.piece,
                begin: block.begin,
                length: block.length,
            },
        )
        .await?;
        in_flight.push(block);
    }
    Ok(())
}

async fn update_interest(
    shared: &Arc<Shared>,
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    remote_bits: &PeerBitfield,
    we_interested: &mut bool,
) -> Result<()> {
    let interesting = { shared.picker.lock().unwrap().is_interesting(remote_bits) };
    if interesting != *we_interested {
        *we_interested = interesting;
        let message = if interesting {
            Message::Interested
        } else {
            Message::NotInterested
        };
        peer::write_message(writer, &message).await?;
    }
    Ok(())
}

/// Serve one block to an unchoked peer.
async fn serve_block(
    shared: &Arc<Shared>,
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    unchoked: bool,
    index: u32,
    begin: u32,
    length: u32,
) -> Result<()> {
    if !unchoked {
        return Ok(()); // requests from choked peers are simply ignored
    }
    if length == 0 || length > MAX_SERVED_BLOCK {
        return Err(Error::protocol("peer requested an unreasonable block size"));
    }
    let have = { shared.picker.lock().unwrap().has_piece(index) };
    if !have || !valid_block(shared, index, begin, length as usize) {
        return Ok(());
    }

    shared.ul_limit.acquire(u64::from(length)).await;
    let offset = u64::from(index) * shared.metainfo.piece_length + u64::from(begin);
    let data = {
        let storage = shared.storage.clone();
        tokio::task::spawn_blocking(move || {
            let mut buffer = vec![0u8; length as usize];
            storage.read_at(offset, &mut buffer).map(|()| buffer)
        })
        .await
        .map_err(|e| Error::Other(format!("read task failed: {e}")))??
    };
    peer::write_message(writer, &Message::Piece { index, begin, data }).await?;
    shared.stats.record_upload(u64::from(length));
    Ok(())
}

/// Answer a metadata request (BEP 9) from our stored info dictionary.
async fn serve_metadata(
    shared: &Arc<Shared>,
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    their_metadata_id: Option<u8>,
    payload: &[u8],
) -> Result<()> {
    let Some(their_id) = their_metadata_id else {
        return Ok(());
    };
    let Ok(MetadataMessage::Request { piece }) = extension::parse_metadata_message(payload) else {
        return Ok(());
    };

    let info = &shared.metainfo.info_bytes;
    let start = piece as usize * extension::METADATA_PIECE_SIZE;
    let reply = if start < info.len() {
        let end = (start + extension::METADATA_PIECE_SIZE).min(info.len());
        extension::build_metadata_data(piece, info.len() as u64, &info[start..end])
    } else {
        extension::build_metadata_reject(piece)
    };
    peer::write_message(
        writer,
        &Message::Extended {
            id: their_id,
            payload: reply,
        },
    )
    .await?;
    Ok(())
}
