//! Transfer accounting: totals, rolling speeds, and peaks.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Window used for "current speed" readings.
const SPEED_WINDOW: Duration = Duration::from_secs(4);

/// Shared, thread-safe transfer counters for one torrent session.
#[derive(Default)]
pub struct TransferStats {
    downloaded: AtomicU64,
    uploaded: AtomicU64,
    wasted: AtomicU64,
    download_window: SpeedWindow,
    upload_window: SpeedWindow,
    peak_download: AtomicU64,
}

impl TransferStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_download(&self, bytes: u64) {
        self.downloaded.fetch_add(bytes, Ordering::Relaxed);
        self.download_window.record(bytes);
    }

    pub fn record_upload(&self, bytes: u64) {
        self.uploaded.fetch_add(bytes, Ordering::Relaxed);
        self.upload_window.record(bytes);
    }

    /// Bytes received but thrown away (duplicates, failed pieces).
    pub fn record_wasted(&self, bytes: u64) {
        self.wasted.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn downloaded(&self) -> u64 {
        self.downloaded.load(Ordering::Relaxed)
    }

    pub fn uploaded(&self) -> u64 {
        self.uploaded.load(Ordering::Relaxed)
    }

    pub fn wasted(&self) -> u64 {
        self.wasted.load(Ordering::Relaxed)
    }

    /// Current download speed in bytes/second; also updates the peak.
    pub fn download_rate(&self) -> u64 {
        let rate = self.download_window.rate();
        self.peak_download.fetch_max(rate, Ordering::Relaxed);
        rate
    }

    pub fn upload_rate(&self) -> u64 {
        self.upload_window.rate()
    }

    pub fn peak_download_rate(&self) -> u64 {
        self.peak_download.load(Ordering::Relaxed)
    }
}

/// Rolling byte counter over a short window.
#[derive(Default)]
struct SpeedWindow {
    samples: Mutex<VecDeque<(Instant, u64)>>,
}

impl SpeedWindow {
    fn record(&self, bytes: u64) {
        let mut samples = self.samples.lock().unwrap();
        samples.push_back((Instant::now(), bytes));
    }

    fn rate(&self) -> u64 {
        let mut samples = self.samples.lock().unwrap();
        let now = Instant::now();
        while samples
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > SPEED_WINDOW)
        {
            samples.pop_front();
        }
        if samples.is_empty() {
            return 0;
        }
        let total: u64 = samples.iter().map(|(_, b)| b).sum();
        // Divide by the full window once it has had time to fill, otherwise
        // by the observed span, so early readings are not inflated.
        let span = now
            .duration_since(samples.front().unwrap().0)
            .max(Duration::from_millis(250));
        let span = span.min(SPEED_WINDOW);
        (total as f64 / span.as_secs_f64()) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_accumulate() {
        let stats = TransferStats::new();
        stats.record_download(100);
        stats.record_download(50);
        stats.record_upload(30);
        stats.record_wasted(7);
        assert_eq!(stats.downloaded(), 150);
        assert_eq!(stats.uploaded(), 30);
        assert_eq!(stats.wasted(), 7);
    }

    #[test]
    fn rate_reflects_recent_bytes() {
        let stats = TransferStats::new();
        stats.record_download(1_000_000);
        let rate = stats.download_rate();
        assert!(rate > 0);
        assert_eq!(stats.peak_download_rate(), rate);
    }

    #[test]
    fn rate_is_zero_when_idle() {
        let stats = TransferStats::new();
        assert_eq!(stats.download_rate(), 0);
        assert_eq!(stats.upload_rate(), 0);
    }
}
