//! Human-friendly formatting and parsing of sizes, speeds and durations.

use std::time::Duration;

/// Format a byte count with binary units: `1.50 MiB`.
pub fn bytes(value: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if value < 1024 {
        return format!("{value} B");
    }
    let mut size = value as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if size >= 100.0 {
        format!("{size:.0} {}", UNITS[unit])
    } else if size >= 10.0 {
        format!("{size:.1} {}", UNITS[unit])
    } else {
        format!("{size:.2} {}", UNITS[unit])
    }
}

/// Format a transfer rate: `8.42 MiB/s`.
pub fn rate(bytes_per_sec: u64) -> String {
    format!("{}/s", bytes(bytes_per_sec))
}

/// Format an optional bandwidth limit.
pub fn limit(limit: Option<u64>) -> String {
    match limit {
        Some(l) => rate(l),
        None => "unlimited".to_owned(),
    }
}

/// Format a duration compactly: `58s`, `1m 12s`, `2h 03m`.
pub fn duration(d: Duration) -> String {
    let total = d.as_secs();
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

/// Estimated time remaining given a rate; `None` when unknowable.
pub fn eta(remaining_bytes: u64, rate: u64) -> Option<Duration> {
    if remaining_bytes == 0 {
        return Some(Duration::ZERO);
    }
    if rate == 0 {
        return None;
    }
    let secs = remaining_bytes.div_ceil(rate);
    (secs < 100 * 24 * 3600).then(|| Duration::from_secs(secs))
}

/// Local wall-clock time `eta` from now: `14:32`, or `Thu 14:32` if far out.
pub fn completion_time(eta: Duration) -> String {
    let when = chrono::Local::now() + chrono::Duration::seconds(eta.as_secs() as i64);
    if eta >= Duration::from_secs(20 * 3600) {
        when.format("%a %H:%M").to_string()
    } else {
        when.format("%H:%M").to_string()
    }
}

/// Parse a bandwidth value: `5MiB`, `500KiB`, `1.5M`, `8mb`, `unlimited`.
///
/// Binary units (`K`, `KiB`, `M`, `MiB`, ...) multiply by 1024; decimal units
/// (`KB`, `MB`, `GB`) by 1000. A trailing `/s` is accepted. `unlimited`,
/// `none`, `off` and `0` all mean no limit.
pub fn parse_limit(input: &str) -> Result<Option<u64>, String> {
    let text = input.trim().to_ascii_lowercase();
    let text = text.strip_suffix("/s").unwrap_or(&text);
    if matches!(text, "unlimited" | "none" | "off" | "0") {
        return Ok(None);
    }

    let split = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let (number_part, unit_part) = text.split_at(split);
    let number: f64 = number_part
        .parse()
        .map_err(|_| format!("`{input}` is not a valid bandwidth value"))?;
    if number < 0.0 || !number.is_finite() {
        return Err(format!("`{input}` is not a valid bandwidth value"));
    }

    let multiplier: u64 = match unit_part.trim() {
        "" | "b" => 1,
        "k" | "kib" => 1024,
        "m" | "mib" => 1024 * 1024,
        "g" | "gib" => 1024 * 1024 * 1024,
        "kb" => 1000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        other => {
            return Err(format!(
                "unknown unit `{other}` in `{input}` (try 5MiB, 500KiB or unlimited)"
            ));
        }
    };

    let value = (number * multiplier as f64).round() as u64;
    Ok((value > 0).then_some(value))
}

/// Shorten a name for narrow displays.
pub fn truncate_name(name: &str, max: usize) -> String {
    if name.chars().count() <= max {
        return name.to_owned();
    }
    let kept: String = name.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1024), "1.00 KiB");
        assert_eq!(bytes(1536), "1.50 KiB");
        assert_eq!(bytes(10 * 1024 * 1024), "10.0 MiB");
        assert_eq!(bytes(700 * 1024 * 1024), "700 MiB");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024), "3.00 GiB");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(duration(Duration::from_secs(9)), "9s");
        assert_eq!(duration(Duration::from_secs(72)), "1m 12s");
        assert_eq!(duration(Duration::from_secs(7395)), "2h 03m");
    }

    #[test]
    fn eta_handles_edges() {
        assert_eq!(eta(0, 0), Some(Duration::ZERO));
        assert_eq!(eta(100, 0), None);
        assert_eq!(eta(100, 50), Some(Duration::from_secs(2)));
        assert_eq!(eta(u64::MAX, 1), None, "absurd ETAs are hidden");
    }

    #[test]
    fn parses_limits() {
        assert_eq!(parse_limit("unlimited"), Ok(None));
        assert_eq!(parse_limit("0"), Ok(None));
        assert_eq!(parse_limit("off"), Ok(None));
        assert_eq!(parse_limit("5MiB"), Ok(Some(5 * 1024 * 1024)));
        assert_eq!(parse_limit("5MiB/s"), Ok(Some(5 * 1024 * 1024)));
        assert_eq!(parse_limit("500KiB"), Ok(Some(500 * 1024)));
        assert_eq!(parse_limit("1.5m"), Ok(Some(1_572_864)));
        assert_eq!(parse_limit("8MB"), Ok(Some(8_000_000)));
        assert_eq!(parse_limit("1024"), Ok(Some(1024)));
        assert!(parse_limit("fast").is_err());
        assert!(parse_limit("5xb").is_err());
    }

    #[test]
    fn truncates_names() {
        assert_eq!(truncate_name("short", 10), "short");
        assert_eq!(truncate_name("a-very-long-torrent-name", 10), "a-very-lo…");
    }
}
