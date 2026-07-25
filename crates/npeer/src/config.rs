//! Configuration: a TOML file, environment variables, and CLI flags,
//! merged in that order of increasing precedence.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use npeer_core::session::SeedingPolicy;
use serde::{Deserialize, Serialize};

use crate::units;

/// The written-out configuration file, all fields optional with defaults.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ConfigFile {
    pub general: GeneralSection,
    pub network: NetworkSection,
    pub limits: LimitsSection,
    pub seeding: SeedingSection,
    pub ui: UiSection,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GeneralSection {
    /// Download directory; platform Downloads folder when unset.
    pub download_dir: Option<PathBuf>,
    /// Re-check data on disk before resuming a download.
    pub verify_on_resume: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NetworkSection {
    /// Port reported to trackers.
    pub listen_port: u16,
    /// Peer connections per torrent.
    pub max_peers: usize,
    /// Reserved for future releases; enabling them has no effect yet.
    pub dht: bool,
    pub pex: bool,
    pub lsd: bool,
    pub utp: bool,
}

impl Default for NetworkSection {
    fn default() -> Self {
        NetworkSection {
            listen_port: 6881,
            max_peers: 40,
            dht: false,
            pex: false,
            lsd: false,
            utp: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LimitsSection {
    /// e.g. "5MiB", "500KiB", "unlimited".
    pub download: String,
    pub upload: String,
}

impl Default for LimitsSection {
    fn default() -> Self {
        LimitsSection {
            download: "unlimited".into(),
            upload: "unlimited".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SeedingSection {
    /// Keep seeding after a download completes.
    pub enabled: bool,
    /// Stop at this upload/size ratio; 0 = no limit.
    pub max_ratio: f64,
    /// Stop after this many minutes of seeding; 0 = no limit.
    pub max_minutes: u64,
}

impl Default for SeedingSection {
    fn default() -> Self {
        SeedingSection {
            enabled: false,
            max_ratio: 1.0,
            max_minutes: 0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UiSection {
    /// Live progress display: "auto", "always" or "never".
    pub progress: String,
}

impl Default for UiSection {
    fn default() -> Self {
        UiSection {
            progress: "auto".into(),
        }
    }
}

/// Fully resolved settings a command actually runs with.
#[derive(Clone, Debug)]
pub struct Settings {
    pub download_dir: PathBuf,
    pub data_dir: PathBuf,
    pub verify_on_resume: bool,
    pub listen_port: u16,
    pub max_peers: usize,
    pub download_limit: Option<u64>,
    pub upload_limit: Option<u64>,
    pub seeding: SeedingPolicy,
    pub progress: ProgressMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressMode {
    Auto,
    Always,
    Never,
}

/// Per-command overrides collected from CLI flags. `None` = not given.
#[derive(Clone, Debug, Default)]
pub struct Overrides {
    pub output: Option<PathBuf>,
    pub download_limit: Option<Option<u64>>,
    pub upload_limit: Option<Option<u64>>,
    pub port: Option<u16>,
    pub max_peers: Option<usize>,
    pub seed: Option<bool>,
    pub verify_on_resume: Option<bool>,
}

/// Platform config file location, e.g. `~/Library/Application Support/npeer/config.toml`.
pub fn default_config_path() -> Result<PathBuf> {
    let dirs = directories_next::ProjectDirs::from("", "", "npeer")
        .context("could not determine your configuration directory")?;
    Ok(dirs.config_dir().join("config.toml"))
}

/// Platform application-data location for state, resume data and metadata.
pub fn default_data_dir() -> Result<PathBuf> {
    let dirs = directories_next::ProjectDirs::from("", "", "npeer")
        .context("could not determine your application data directory")?;
    Ok(dirs.data_dir().to_path_buf())
}

/// The user's Downloads folder, falling back to the home directory.
pub fn default_download_dir() -> PathBuf {
    if let Some(dirs) = directories_next::UserDirs::new() {
        if let Some(downloads) = dirs.download_dir() {
            return downloads.to_path_buf();
        }
        return dirs.home_dir().join("Downloads");
    }
    PathBuf::from(".")
}

/// Load the config file (the explicit `--config` path must exist; the
/// default path is allowed to be absent). Returns the file and its path.
pub fn load_file(explicit: Option<&Path>) -> Result<(ConfigFile, PathBuf)> {
    let (path, must_exist) = match explicit {
        Some(path) => (path.to_path_buf(), true),
        None => (default_config_path()?, false),
    };
    let file = match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text).with_context(|| format!("in {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !must_exist => ConfigFile::default(),
        Err(e) => bail!("could not read {}: {e}", path.display()),
    };
    Ok((file, path))
}

pub fn parse(text: &str) -> Result<ConfigFile> {
    toml::from_str(text).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Resolve the effective settings: defaults <- file <- environment <- CLI.
pub fn resolve(
    file: &ConfigFile,
    overrides: &Overrides,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Settings> {
    let parse_limit_setting = |what: &str, text: &str| {
        units::parse_limit(text).map_err(|e| anyhow::anyhow!("{what}: {e}"))
    };

    // File layer.
    let mut download_dir = file
        .general
        .download_dir
        .clone()
        .unwrap_or_else(default_download_dir);
    let mut verify_on_resume = file.general.verify_on_resume;
    let mut listen_port = file.network.listen_port;
    let mut max_peers = file.network.max_peers;
    let mut download_limit = parse_limit_setting("limits.download", &file.limits.download)?;
    let mut upload_limit = parse_limit_setting("limits.upload", &file.limits.upload)?;
    let mut seeding_enabled = file.seeding.enabled;
    let progress = match file.ui.progress.as_str() {
        "auto" => ProgressMode::Auto,
        "always" => ProgressMode::Always,
        "never" => ProgressMode::Never,
        other => bail!("ui.progress: `{other}` is not one of auto, always, never"),
    };

    for reserved in [
        ("network.dht", file.network.dht),
        ("network.pex", file.network.pex),
        ("network.lsd", file.network.lsd),
        ("network.utp", file.network.utp),
    ] {
        if reserved.1 {
            tracing::warn!("{} is not implemented yet and has no effect", reserved.0);
        }
    }

    // Environment layer.
    if let Some(dir) = env("NPEER_DOWNLOAD_DIR") {
        download_dir = PathBuf::from(dir);
    }
    if let Some(value) = env("NPEER_LIMIT_DOWN") {
        download_limit = parse_limit_setting("NPEER_LIMIT_DOWN", &value)?;
    }
    if let Some(value) = env("NPEER_LIMIT_UP") {
        upload_limit = parse_limit_setting("NPEER_LIMIT_UP", &value)?;
    }
    if let Some(value) = env("NPEER_PORT") {
        listen_port = value.parse().context("NPEER_PORT is not a valid port")?;
    }
    if let Some(value) = env("NPEER_MAX_PEERS") {
        max_peers = value.parse().context("NPEER_MAX_PEERS is not a number")?;
    }
    if let Some(value) = env("NPEER_SEED") {
        seeding_enabled = matches!(value.as_str(), "1" | "true" | "yes");
    }

    // CLI layer.
    if let Some(dir) = &overrides.output {
        download_dir = dir.clone();
    }
    if let Some(value) = overrides.download_limit {
        download_limit = value;
    }
    if let Some(value) = overrides.upload_limit {
        upload_limit = value;
    }
    if let Some(port) = overrides.port {
        listen_port = port;
    }
    if let Some(value) = overrides.max_peers {
        max_peers = value;
    }
    if let Some(value) = overrides.seed {
        seeding_enabled = value;
    }
    if let Some(value) = overrides.verify_on_resume {
        verify_on_resume = value;
    }

    if max_peers == 0 {
        bail!("max_peers must be at least 1");
    }

    Ok(Settings {
        download_dir,
        data_dir: default_data_dir()?,
        verify_on_resume,
        listen_port,
        max_peers,
        download_limit,
        upload_limit,
        seeding: SeedingPolicy {
            enabled: seeding_enabled,
            max_ratio: file.seeding.max_ratio,
            max_minutes: file.seeding.max_minutes,
        },
        progress,
    })
}

/// The commented template written by `npeer config init`.
pub const CONFIG_TEMPLATE: &str = r#"# npeer configuration
# Values here are defaults; command-line flags and NPEER_* environment
# variables override them. Run `npeer config validate` after editing.

[general]
# Where downloads are saved. Defaults to your Downloads folder.
# download_dir = "/path/to/downloads"

# Re-check data already on disk before resuming a download.
verify_on_resume = false

[network]
# Port reported to trackers.
listen_port = 6881

# Peer connections per torrent.
max_peers = 40

# Reserved for future releases; enabling them has no effect yet.
dht = false
pex = false
lsd = false
utp = false

[limits]
# Bandwidth caps per second: "5MiB", "500KiB", "8MB", or "unlimited".
download = "unlimited"
upload = "unlimited"

[seeding]
# Keep seeding after a download completes.
enabled = false

# Stop seeding at this upload ratio (uploaded / torrent size). 0 = no limit.
max_ratio = 1.0

# Stop seeding after this many minutes. 0 = no limit.
max_minutes = 0

[ui]
# Live progress display: "auto" (only on a terminal), "always", or "never".
progress = "auto"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn template_parses_to_defaults() {
        let file = parse(CONFIG_TEMPLATE).unwrap();
        let settings = resolve(&file, &Overrides::default(), no_env).unwrap();
        assert_eq!(settings.listen_port, 6881);
        assert_eq!(settings.max_peers, 40);
        assert_eq!(settings.download_limit, None);
        assert!(!settings.seeding.enabled);
        assert_eq!(settings.progress, ProgressMode::Auto);
    }

    #[test]
    fn rejects_unknown_keys() {
        let err = parse("[general]\ntypo_key = 1\n").unwrap_err();
        assert!(err.to_string().contains("typo_key"));
    }

    #[test]
    fn rejects_bad_values() {
        let file = parse("[limits]\ndownload = \"very fast\"\n").unwrap();
        assert!(resolve(&file, &Overrides::default(), no_env).is_err());
        let file = parse("[ui]\nprogress = \"sometimes\"\n").unwrap();
        assert!(resolve(&file, &Overrides::default(), no_env).is_err());
    }

    #[test]
    fn precedence_env_beats_file_cli_beats_env() {
        let file = parse("[limits]\ndownload = \"1MiB\"\n[network]\nlisten_port = 7000\n").unwrap();

        let env = |key: &str| match key {
            "NPEER_LIMIT_DOWN" => Some("2MiB".to_owned()),
            "NPEER_PORT" => Some("7001".to_owned()),
            _ => None,
        };

        // Env over file.
        let settings = resolve(&file, &Overrides::default(), env).unwrap();
        assert_eq!(settings.download_limit, Some(2 * 1024 * 1024));
        assert_eq!(settings.listen_port, 7001);

        // CLI over env.
        let overrides = Overrides {
            download_limit: Some(Some(3 * 1024 * 1024)),
            port: Some(7002),
            ..Overrides::default()
        };
        let settings = resolve(&file, &overrides, env).unwrap();
        assert_eq!(settings.download_limit, Some(3 * 1024 * 1024));
        assert_eq!(settings.listen_port, 7002);
    }

    #[test]
    fn seeding_policy_flows_through() {
        let file = parse("[seeding]\nenabled = true\nmax_ratio = 2.5\nmax_minutes = 90\n").unwrap();
        let settings = resolve(&file, &Overrides::default(), no_env).unwrap();
        assert!(settings.seeding.enabled);
        assert!((settings.seeding.max_ratio - 2.5).abs() < f64::EPSILON);
        assert_eq!(settings.seeding.max_minutes, 90);
    }
}
