//! Session management: the running engine plus persisted torrent state.

pub mod engine;
pub mod state;

pub use engine::{
    Engine, EngineOptions, EnginePhase, EngineSummary, Event, SeedingPolicy, Snapshot, StateHook,
    StateUpdate, StopReason,
};
pub use state::{STATE_VERSION, StateStore, StoredStatus, TorrentState, unix_now};
