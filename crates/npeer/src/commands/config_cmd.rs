//! `npeer config`: init, show, path, validate.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::{self, Settings};
use crate::units;

pub fn init(explicit: Option<&Path>, force: bool) -> Result<()> {
    let path = match explicit {
        Some(path) => path.to_path_buf(),
        None => config::default_config_path()?,
    };
    if path.exists() && !force {
        bail!(
            "{} already exists (use `npeer config init --force` to overwrite it)",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    std::fs::write(&path, config::CONFIG_TEMPLATE)
        .with_context(|| format!("could not write {}", path.display()))?;
    println!("Wrote {}", path.display());
    Ok(())
}

pub fn show(settings: &Settings, config_path: &Path) -> Result<()> {
    let source = if config_path.exists() {
        config_path.display().to_string()
    } else {
        format!(
            "{} (not created yet — using defaults)",
            config_path.display()
        )
    };

    let row = |label: &str, value: String| println!("{label:<18} {value}");
    row("config file", source);
    row("data directory", settings.data_dir.display().to_string());
    row("download dir", settings.download_dir.display().to_string());
    row("listen port", settings.listen_port.to_string());
    row("max peers", settings.max_peers.to_string());
    row("download limit", units::limit(settings.download_limit));
    row("upload limit", units::limit(settings.upload_limit));
    row(
        "seeding",
        if settings.seeding.enabled {
            let ratio = if settings.seeding.max_ratio > 0.0 {
                format!("up to ratio {:.2}", settings.seeding.max_ratio)
            } else {
                "no ratio limit".to_owned()
            };
            let time = if settings.seeding.max_minutes > 0 {
                format!(", at most {} min", settings.seeding.max_minutes)
            } else {
                String::new()
            };
            format!("on ({ratio}{time})")
        } else {
            "off".to_owned()
        },
    );
    row("verify on resume", settings.verify_on_resume.to_string());
    Ok(())
}

pub fn path(explicit: Option<&Path>) -> Result<()> {
    let path = match explicit {
        Some(path) => path.to_path_buf(),
        None => config::default_config_path()?,
    };
    println!("{}", path.display());
    Ok(())
}

pub fn validate(explicit: Option<&Path>) -> Result<()> {
    let (file, path) = config::load_file(explicit)?;
    if !path.exists() {
        println!(
            "No config file at {} — defaults are in use. Create one with `npeer config init`.",
            path.display()
        );
        return Ok(());
    }
    // Value-level checks (limits, enums) happen during resolution.
    config::resolve(&file, &Default::default(), |key| std::env::var(key).ok())?;
    println!("✓ {} is valid.", path.display());
    Ok(())
}
