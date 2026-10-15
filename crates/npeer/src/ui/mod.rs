//! Terminal presentation: phase spinners, the live download display, and
//! the completion summary.
//!
//! Everything degrades gracefully: when stderr/stdout is not a terminal (or
//! `ui.progress = "never"`), spinners become plain lines and the live
//! display becomes an occasional one-line status print, so logs and pipes
//! stay readable.

use std::io::IsTerminal;
use std::path::Path;
use std::time::{Duration, Instant};

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use npeer_core::session::{EnginePhase, EngineSummary, Snapshot};

use crate::config::ProgressMode;
use crate::units;

/// Are fancy terminal visuals appropriate right now?
pub fn fancy(mode: ProgressMode) -> bool {
    match mode {
        ProgressMode::Always => true,
        ProgressMode::Never => false,
        ProgressMode::Auto => std::io::stdout().is_terminal() && std::io::stderr().is_terminal(),
    }
}

/// A transient step with a spinner: reading metadata, contacting trackers,
/// handshaking, and so on.
pub struct Phase {
    spinner: Option<spinners::Spinner>,
    text: String,
}

impl Phase {
    pub fn start(mode: ProgressMode, text: &str) -> Phase {
        if fancy(mode) {
            let spinner = spinners::Spinner::new(spinners::Spinners::Dots, text.to_owned());
            Phase {
                spinner: Some(spinner),
                text: text.to_owned(),
            }
        } else {
            println!("{text}...");
            Phase {
                spinner: None,
                text: text.to_owned(),
            }
        }
    }

    /// Finish successfully, optionally replacing the message.
    pub fn done(mut self, message: Option<&str>) {
        let final_text = message.unwrap_or(&self.text).to_owned();
        match self.spinner.as_mut() {
            Some(spinner) => spinner.stop_and_persist("✓", final_text),
            None => println!("✓ {final_text}"),
        }
        self.spinner = None;
    }

    /// Finish with a failure message.
    pub fn fail(mut self, message: &str) {
        match self.spinner.as_mut() {
            Some(spinner) => spinner.stop_and_persist("✗", message.to_owned()),
            None => eprintln!("✗ {message}"),
        }
        self.spinner = None;
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        // A dropped phase (early return / error path) stops the animation
        // without pretending it succeeded.
        if let Some(spinner) = self.spinner.as_mut() {
            spinner.stop_and_persist("·", self.text.clone());
        }
    }
}

/// The live, in-place download display.
pub struct DownloadUi {
    fancy: Option<FancyUi>,
    /// Plain mode: rate-limited one-line prints.
    last_plain_print: Instant,
    name: String,
    dest: String,
    /// ` · cap 3.00 MiB/s` when a limit is set, empty otherwise.
    cap_down: String,
    cap_up: String,
    paused: bool,
}

struct FancyUi {
    multi: MultiProgress,
    title: ProgressBar,
    destination: ProgressBar,
    bar: ProgressBar,
    down_line: ProgressBar,
    up_line: ProgressBar,
    peers: ProgressBar,
    pieces: ProgressBar,
    timing: ProgressBar,
    /// Blank lines separating the groups; kept alive so they stay drawn.
    _spacers: Vec<ProgressBar>,
}

impl DownloadUi {
    pub fn new(
        mode: ProgressMode,
        name: &str,
        destination: &Path,
        total: u64,
        download_limit: Option<u64>,
        upload_limit: Option<u64>,
    ) -> DownloadUi {
        let cap = |limit: Option<u64>| {
            limit
                .map(|l| format!(" · cap {}", units::rate(l)))
                .unwrap_or_default()
        };
        let fancy_ui = fancy(mode).then(|| {
            let multi = MultiProgress::new();
            let line = |template: &str| {
                let bar = multi.add(ProgressBar::no_length());
                bar.set_style(ProgressStyle::with_template(template).unwrap());
                bar
            };
            // Bars that are never updated are never drawn, so spacers get
            // one initial message to make their blank line appear.
            let spacer = || {
                let bar = line("{msg}");
                bar.set_message(" ");
                bar
            };

            let title = multi.add(ProgressBar::no_length());
            title.set_style(ProgressStyle::with_template("{spinner:.green} {msg}").unwrap());
            title.enable_steady_tick(Duration::from_millis(120));
            let destination_line = line("{msg}");

            let spacer_1 = spacer();
            let bar = multi.add(ProgressBar::new(total.max(1)));
            // The percentage lives in the message, floored, so the bar
            // never claims 100% while pieces are still missing.
            bar.set_style(
                ProgressStyle::with_template("[{bar:38.cyan/blue}] {msg}")
                    .unwrap()
                    .progress_chars("=> "),
            );
            let spacer_2 = spacer();

            let down_line = line("{msg}");
            let up_line = line("{msg}");
            let spacer_3 = spacer();

            let peers = line("{msg}");
            let pieces = line("{msg}");
            let timing = line("{msg}");
            FancyUi {
                multi,
                title,
                destination: destination_line,
                bar,
                down_line,
                up_line,
                peers,
                pieces,
                timing,
                _spacers: vec![spacer_1, spacer_2, spacer_3],
            }
        });

        DownloadUi {
            fancy: fancy_ui,
            last_plain_print: Instant::now() - Duration::from_secs(60),
            name: name.to_owned(),
            dest: destination.display().to_string(),
            cap_down: cap(download_limit),
            cap_up: cap(upload_limit),
            paused: false,
        }
    }

    /// Terminal width right now, with a sane floor for tiny windows.
    fn width() -> usize {
        let (_, columns) = console::Term::stderr().size();
        (columns as usize).max(40)
    }

    /// Show a line above the live display (survives redraws).
    pub fn note(&self, text: &str) {
        match &self.fancy {
            Some(ui) => {
                let _ = ui.multi.println(text);
            }
            None => println!("{text}"),
        }
    }

    pub fn set_paused(&mut self) {
        self.paused = true;
    }

    /// Redraw from the latest engine snapshot.
    pub fn render(&mut self, snapshot: &Snapshot) {
        let status = if self.paused {
            "Pausing".to_owned()
        } else {
            match snapshot.phase {
                EnginePhase::Seeding => "Seeding".to_owned(),
                EnginePhase::Downloading if snapshot.connected_peers == 0 => {
                    "Connecting".to_owned()
                }
                EnginePhase::Downloading if snapshot.endgame => "Downloading (endgame)".to_owned(),
                EnginePhase::Downloading => "Downloading".to_owned(),
            }
        };
        let remaining = snapshot.total_bytes.saturating_sub(snapshot.verified_bytes);
        let eta = units::eta(remaining, snapshot.download_rate);

        match &self.fancy {
            Some(ui) => {
                // Clamp every line to the terminal width: overlong lines
                // wrap, which breaks in-place redrawing and floods the
                // scrollback with stale copies.
                let width = Self::width();
                let fit = |text: String, reserved: usize| {
                    units::truncate_name(&text, width.saturating_sub(reserved))
                };

                ui.title
                    .set_message(fit(format!("{status}: {}", self.name), 3));
                ui.destination
                    .set_message(fit(format!("To: {}", self.dest), 1));
                ui.bar.set_length(snapshot.total_bytes.max(1));
                ui.bar.set_position(snapshot.verified_bytes);
                // Integer division floors: 99.6% shows as 99%, and 100%
                // appears only when every byte is verified.
                let percent = snapshot.verified_bytes * 100 / snapshot.total_bytes.max(1);
                ui.bar.set_message(format!(
                    "{percent:>3}%  {} / {}",
                    units::bytes(snapshot.verified_bytes),
                    units::bytes(snapshot.total_bytes)
                ));
                ui.down_line.set_message(fit(
                    format!(
                        "down: {} (avg {}){}",
                        units::rate(snapshot.download_rate),
                        units::rate(snapshot.average_download_rate),
                        self.cap_down,
                    ),
                    1,
                ));
                ui.up_line.set_message(fit(
                    format!("up: {}{}", units::rate(snapshot.upload_rate), self.cap_up),
                    1,
                ));
                let swarm = match (snapshot.swarm_seeders, snapshot.swarm_leechers) {
                    (Some(seeders), Some(leechers)) => {
                        format!(" · swarm {seeders} seeds / {leechers} peers")
                    }
                    _ => String::new(),
                };
                ui.peers.set_message(fit(
                    format!(
                        "peers {} ({} seeds) · discovered {}{swarm}",
                        snapshot.connected_peers,
                        snapshot.connected_seeders,
                        snapshot.discovered_peers,
                    ),
                    1,
                ));
                let redownloaded = if snapshot.hash_failures > 0 {
                    format!(" · re-downloaded: {}", snapshot.hash_failures)
                } else {
                    String::new()
                };
                ui.pieces.set_message(fit(
                    format!(
                        "verified: {}/{} pieces{redownloaded}",
                        snapshot.pieces_have, snapshot.piece_count,
                    ),
                    1,
                ));
                let eta_text = match eta {
                    Some(eta) if snapshot.phase == EnginePhase::Downloading => format!(
                        " · eta {} · done ~{}",
                        units::duration(eta),
                        units::completion_time(eta)
                    ),
                    _ => String::new(),
                };
                ui.timing.set_message(fit(
                    format!("elapsed {}{eta_text}", units::duration(snapshot.elapsed)),
                    1,
                ));
            }
            None => {
                // Plain mode: one line every few seconds.
                if self.last_plain_print.elapsed() < Duration::from_secs(5) {
                    return;
                }
                self.last_plain_print = Instant::now();
                let percent = if snapshot.total_bytes > 0 {
                    snapshot.verified_bytes as f64 / snapshot.total_bytes as f64 * 100.0
                } else {
                    0.0
                };
                let eta_text = eta
                    .map(|e| format!(", eta {}", units::duration(e)))
                    .unwrap_or_default();
                let redownloaded = if snapshot.hash_failures > 0 {
                    format!(", {} re-downloaded", snapshot.hash_failures)
                } else {
                    String::new()
                };
                println!(
                    "{status}: {percent:.1}% ({} / {}) at {}, {} peers{redownloaded}{eta_text}",
                    units::bytes(snapshot.verified_bytes),
                    units::bytes(snapshot.total_bytes),
                    units::rate(snapshot.download_rate),
                    snapshot.connected_peers,
                );
            }
        }
    }

    /// Remove the live display (before printing the summary).
    pub fn finish(self) {
        if let Some(ui) = &self.fancy {
            let _ = ui.multi.clear();
        }
    }
}

/// A progress bar for hash-checking, shared by resume and `verify`.
pub struct VerifyUi {
    bar: Option<ProgressBar>,
}

impl VerifyUi {
    pub fn new(mode: ProgressMode, piece_count: u32) -> VerifyUi {
        if fancy(mode) {
            let bar = ProgressBar::new(u64::from(piece_count));
            bar.set_style(
                ProgressStyle::with_template("  Verifying [{bar:38.cyan/blue}] {pos}/{len} pieces")
                    .unwrap()
                    .progress_chars("=> "),
            );
            VerifyUi { bar: Some(bar) }
        } else {
            let pieces = if piece_count == 1 { "piece" } else { "pieces" };
            println!("Verifying {piece_count} {pieces}...");
            VerifyUi { bar: None }
        }
    }

    pub fn update(&self, checked: u32) {
        if let Some(bar) = &self.bar {
            bar.set_position(u64::from(checked));
        }
    }

    pub fn finish(self) {
        if let Some(bar) = &self.bar {
            bar.finish_and_clear();
        }
    }
}

/// Final report once a download run ends.
pub fn print_summary(
    name: &str,
    content_path: &Path,
    summary: &EngineSummary,
    size: &str,
    piece_count: usize,
    seeding_configured: bool,
) {
    if summary.complete {
        println!("\n✓ Downloaded {name}");
    } else {
        let have = summary.have.iter().filter(|&&h| h).count();
        println!("\n· Paused {name} ({have}/{piece_count} pieces downloaded)");
    }

    let row = |label: &str, value: String| println!("  {label:<14} {value}");
    row("saved to", content_path.display().to_string());
    row("size", size.to_owned());
    row("time", units::duration(summary.elapsed));
    row(
        "average speed",
        format!(
            "{} (peak {})",
            units::rate(summary.average_rate),
            units::rate(summary.peak_rate)
        ),
    );
    let wasted = if summary.wasted > 0 {
        format!(" ({} wasted)", units::bytes(summary.wasted))
    } else {
        String::new()
    };
    row(
        "downloaded",
        format!("{}{wasted}", units::bytes(summary.downloaded)),
    );
    row(
        "uploaded",
        format!(
            "{} (ratio {:.2})",
            units::bytes(summary.uploaded),
            summary.ratio
        ),
    );
    if summary.complete {
        row("verification", format!("all {piece_count} pieces OK"));
        let seeding = if summary.seeded {
            "yes".to_owned()
        } else if seeding_configured {
            "no".to_owned()
        } else {
            "off (enable with --seed or [seeding] in the config)".to_owned()
        };
        row("seeding", seeding);
    }
}

/// Simple aligned text table for `list` and `peers`.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
    }
    let render_row = |cells: &[String]| -> String {
        let mut line = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            let pad = widths[i].saturating_sub(cell.chars().count());
            line.push_str(cell);
            if i + 1 < cells.len() {
                line.push_str(&" ".repeat(pad));
            }
        }
        line
    };

    let mut out = String::new();
    let header_cells: Vec<String> = headers.iter().map(|h| h.to_uppercase()).collect();
    out.push_str(&render_row(&header_cells));
    out.push('\n');
    for row in rows {
        out.push_str(&render_row(row));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_aligns_columns() {
        let rendered = table(
            &["id", "name"],
            &[
                vec!["ab12".into(), "debian.iso".into()],
                vec!["cd".into(), "x".into()],
            ],
        );
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines[0], "ID    NAME");
        assert_eq!(lines[1], "ab12  debian.iso");
        assert_eq!(lines[2], "cd    x");
    }
}
