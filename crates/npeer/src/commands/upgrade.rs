//! `npeer upgrade`: replace the running binary with a newer release.
//!
//! Two channels. Stable (the default) is GitHub's "latest" release, which
//! never includes drafts or pre-releases. Beta (`--beta`) is the newest
//! published release of any kind, so it also moves on to a stable release
//! once that is newer than the beta. Running the stable channel on a beta
//! offers to switch back, even though stable is then the older version.
//!
//! The flow: find the target version, confirm, download the archive for
//! this platform, verify it against the release's SHA256SUMS.txt, then swap
//! the running executable in place.

use std::fmt;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use npeer_core::session::{StateStore, TorrentState};
use serde::Deserialize;
use sha2::Digest;

use crate::config::{self, ProgressMode};
use crate::ui::Phase;
use crate::units;

const REPO: &str = "JohnKesko/npeer";

pub async fn run(
    progress: ProgressMode,
    check_only: bool,
    yes: bool,
    beta: bool,
) -> Result<ExitCode> {
    let current = Version::parse(env!("CARGO_PKG_VERSION")).expect("crate version is well-formed");

    let phase = Phase::start(progress, "Checking for updates");
    let found = if beta {
        newest_release().await
    } else {
        latest_stable_release().await
    };
    let target = match found {
        Ok(release) => release,
        Err(err) => {
            phase.fail("Could not check for updates");
            return Err(err);
        }
    };

    let plan = plan(current, target.version, beta);
    match plan {
        Plan::UpToDate => {
            let note = if beta { "; there is no newer beta" } else { "" };
            phase.done(Some(&format!("npeer is up to date ({current}){note}")));
            return Ok(ExitCode::SUCCESS);
        }
        Plan::Upgrade => phase.done(Some(&format!(
            "Update available: {current} → {}",
            target.version
        ))),
        Plan::SwitchBack => phase.done(Some(&format!(
            "You are on beta {current}. The newest stable version is {}.",
            target.version
        ))),
    }

    if check_only {
        match plan {
            Plan::SwitchBack => println!("Run `npeer upgrade` to switch back to it."),
            _ if beta => println!("Run `npeer upgrade --beta` to install it."),
            _ => println!("Run `npeer upgrade` to install it."),
        }
        return Ok(ExitCode::SUCCESS);
    }

    let Some(asset) = release_asset() else {
        bail!(
            "no pre-built binaries are published for this platform — update \
             from source instead:\n  cargo install --git https://github.com/{REPO} npeer --force"
        );
    };

    // Fail before downloading if the binary cannot be replaced anyway.
    let exe = ensure_replaceable()?;

    let question = if plan == Plan::SwitchBack {
        warn_about_newer_downloads(target.version);
        format!(
            "Switch back to npeer {}? It is older than the beta you have.",
            target.version
        )
    } else {
        format!("Download and install npeer {}?", target.version)
    };
    if !yes && !confirm(&question)? {
        println!("Not upgrading.");
        return Ok(ExitCode::SUCCESS);
    }

    let phase = Phase::start(progress, "Downloading");
    let (archive, sums) = match download_release_files(&target.tag, asset).await {
        Ok(files) => files,
        Err(err) => {
            phase.fail("Download failed");
            return Err(err);
        }
    };
    let expected = checksum_for(&sums, asset)
        .with_context(|| format!("SHA256SUMS.txt has no entry for {asset}"))?;
    let actual = hex::encode(sha2::Sha256::digest(&archive));
    if actual != expected {
        phase.fail("Checksum mismatch");
        bail!(
            "the downloaded file does not match the published checksum — \
             not installing it. Try again later; if this keeps happening, \
             something between you and GitHub is altering downloads."
        );
    }
    phase.done(Some(&format!(
        "Downloaded {asset} ({}, checksum OK)",
        units::bytes(archive.len() as u64)
    )));

    let phase = Phase::start(progress, "Installing");
    match install(&exe, &archive) {
        Ok(()) => {
            phase.done(Some(&format!(
                "npeer is now {} (replaced {})",
                target.version,
                exe.display()
            )));
            Ok(ExitCode::SUCCESS)
        }
        Err(err) => {
            phase.fail("Install failed");
            Err(err)
        }
    }
}

/// A release version: `0.2.0`, or a beta of it like `0.2.0-beta.3`.
/// Field order gives the version order; every beta of 0.2.0 sorts before
/// 0.2.0 itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    stage: Stage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Stage {
    Beta(u64),
    Stable,
}

impl Version {
    /// Parse "0.2.0", "v0.2.0" or "v0.2.0-beta.3"; anything else is `None`.
    fn parse(text: &str) -> Option<Version> {
        let text = text.trim().trim_start_matches('v');
        let (numbers, stage) = match text.split_once('-') {
            Some((numbers, suffix)) => (
                numbers,
                Stage::Beta(suffix.strip_prefix("beta.")?.parse().ok()?),
            ),
            None => (text, Stage::Stable),
        };
        let mut parts = numbers.split('.');
        let version = Version {
            major: parts.next()?.parse().ok()?,
            minor: parts.next()?.parse().ok()?,
            patch: parts.next()?.parse().ok()?,
            stage,
        };
        parts.next().is_none().then_some(version)
    }

    fn is_beta(self) -> bool {
        matches!(self.stage, Stage::Beta(_))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        match self.stage {
            Stage::Beta(number) => write!(f, "-beta.{number}"),
            Stage::Stable => Ok(()),
        }
    }
}

/// A published release: its git tag and the version that tag names.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Release {
    tag: String,
    version: Version,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plan {
    UpToDate,
    Upgrade,
    /// On a beta, asked for stable, and stable is older: go back to it.
    SwitchBack,
}

/// What to do, given the newest release on the chosen channel.
fn plan(current: Version, target: Version, beta: bool) -> Plan {
    if target > current {
        Plan::Upgrade
    } else if current.is_beta() && !beta {
        Plan::SwitchBack
    } else {
        Plan::UpToDate
    }
}

/// The current stable release, found by following GitHub's `latest`
/// redirect — no API token, no rate limits, and pre-releases are never
/// offered.
async fn latest_stable_release() -> Result<Release> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("npeer/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .context("could not build the HTTP client")?;
    let response = client
        .get(format!("https://github.com/{REPO}/releases/latest"))
        .send()
        .await
        .context("could not reach github.com")?;
    let location = response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .context("GitHub did not answer with a release redirect")?;
    let tag =
        tag_from_location(location).context("GitHub's redirect did not name a release tag")?;
    let version =
        Version::parse(&tag).with_context(|| format!("unexpected release tag `{tag}`"))?;
    Ok(Release { tag, version })
}

fn tag_from_location(location: &str) -> Option<String> {
    let (_, tag) = location.rsplit_once("/tag/")?;
    let tag = tag.trim_matches('/');
    (!tag.is_empty()).then(|| tag.to_owned())
}

/// One entry of GitHub's release list.
#[derive(Deserialize)]
struct ListedRelease {
    tag_name: String,
    draft: bool,
}

/// The newest published release, beta or stable, from GitHub's release
/// list (newest first, so recent betas are always on the first page).
async fn newest_release() -> Result<Release> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("npeer/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .context("could not build the HTTP client")?;
    let response = client
        .get(format!(
            "https://api.github.com/repos/{REPO}/releases?per_page=30"
        ))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("could not reach github.com")?;
    let status = response.status();
    if status.as_u16() == 403 || status.as_u16() == 429 {
        bail!("GitHub is limiting how often it can be asked; try again in an hour");
    }
    if !status.is_success() {
        bail!("GitHub's release list answered with HTTP {status}");
    }
    let body = response
        .bytes()
        .await
        .context("could not read GitHub's release list")?;
    let listed: Vec<ListedRelease> =
        serde_json::from_slice(&body).context("GitHub's release list could not be read")?;
    newest_of(&listed).context("GitHub lists no npeer releases")
}

fn newest_of(listed: &[ListedRelease]) -> Option<Release> {
    listed
        .iter()
        .filter(|release| !release.draft)
        .filter_map(|release| {
            Some(Release {
                tag: release.tag_name.clone(),
                version: Version::parse(&release.tag_name)?,
            })
        })
        .max_by_key(|release| release.version)
}

/// Before going back to an older version: list unfinished downloads that a
/// newer npeer saved, since the older one may not continue them correctly.
fn warn_about_newer_downloads(target: Version) {
    let Some(store) = config::default_data_dir()
        .ok()
        .and_then(|dir| StateStore::open(&dir).ok())
    else {
        return;
    };
    let at_risk: Vec<TorrentState> = store
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter(|state| state.progress() < 1.0 && saved_by_newer(state, target))
        .collect();
    if at_risk.is_empty() {
        return;
    }
    println!(
        "These unfinished downloads were saved by a newer npeer and may not \
         continue correctly on {target}:"
    );
    for state in &at_risk {
        println!("  {} ({})", state.name, &state.info_hash.to_hex()[..8]);
    }
    println!("Finish them first, or remove them and start them again after switching.");
}

fn saved_by_newer(state: &TorrentState, target: Version) -> bool {
    state
        .saved_by
        .as_deref()
        .and_then(Version::parse)
        .is_some_and(|saved_by| saved_by > target)
}

/// The release asset built for the running platform, if one is published.
fn release_asset() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("npeer-aarch64-apple-darwin.tar.gz")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("npeer-x86_64-apple-darwin.tar.gz")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("npeer-aarch64-unknown-linux-musl.tar.gz")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("npeer-x86_64-unknown-linux-musl.tar.gz")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("npeer-x86_64-pc-windows-msvc.zip")
    } else {
        None
    }
}

async fn download_release_files(tag: &str, asset: &str) -> Result<(Vec<u8>, String)> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("npeer/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .context("could not build the HTTP client")?;
    let fetch = async |name: &str| -> Result<Vec<u8>> {
        let url = format!("https://github.com/{REPO}/releases/download/{tag}/{name}");
        let response = client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("could not download {name}"))?;
        if !response.status().is_success() {
            bail!("downloading {name} failed with HTTP {}", response.status());
        }
        Ok(response
            .bytes()
            .await
            .with_context(|| format!("could not read {name}"))?
            .to_vec())
    };
    let archive = fetch(asset).await?;
    let sums = String::from_utf8(fetch("SHA256SUMS.txt").await?)
        .context("SHA256SUMS.txt is not valid text")?;
    Ok((archive, sums))
}

/// The published checksum for `asset`, from shasum-style "hex  name" lines.
fn checksum_for(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, name) = line.split_once(char::is_whitespace)?;
        let name = name.trim().trim_start_matches('*');
        (name == asset).then(|| hash.to_ascii_lowercase())
    })
}

/// Where the running binary lives — after proving we may replace it.
fn ensure_replaceable() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("could not locate the running npeer binary")?;
    let dir = exe
        .parent()
        .context("the npeer binary has no parent directory")?;
    let probe = dir.join(".npeer-upgrade-probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(exe.clone())
        }
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => bail!(
            "npeer cannot write to {} — run the upgrade with elevated \
             rights, for example:  sudo npeer upgrade",
            dir.display()
        ),
        Err(err) => {
            Err(err).with_context(|| format!("could not test writing to {}", dir.display()))
        }
    }
}

/// Unpack the new binary next to the old one and swap it in atomically.
fn install(exe: &std::path::Path, archive: &[u8]) -> Result<()> {
    let binary = extract_binary(archive)?;
    let dir = exe.parent().context("no parent directory")?;
    let staged = dir.join(".npeer-upgrade-new");
    std::fs::write(&staged, &binary)
        .with_context(|| format!("could not write {}", staged.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
            .context("could not mark the new binary as executable")?;
    }
    let result = self_replace::self_replace(&staged)
        .context("could not replace the running binary with the new one");
    let _ = std::fs::remove_file(&staged);
    result
}

#[cfg(unix)]
fn extract_binary(archive: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut entries = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in entries
        .entries()
        .context("the download is not a tar.gz archive")?
    {
        let mut entry = entry.context("broken entry in the downloaded archive")?;
        if entry
            .path()?
            .file_name()
            .is_some_and(|name| name == "npeer")
        {
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .context("could not read the npeer binary from the archive")?;
            return Ok(bytes);
        }
    }
    bail!("the downloaded archive does not contain the npeer binary")
}

#[cfg(windows)]
fn extract_binary(archive: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))
        .context("the download is not a zip archive")?;
    for index in 0..zip.len() {
        let mut file = zip.by_index(index)?;
        if file.name().ends_with("npeer.exe") {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .context("could not read npeer.exe from the archive")?;
            return Ok(bytes);
        }
    }
    bail!("the downloaded archive does not contain npeer.exe")
}

fn confirm(question: &str) -> Result<bool> {
    print!("{question} [y/N] ");
    std::io::stdout().flush().ok();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("could not read the answer")?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn versions_parse_and_compare() {
        assert_eq!(v("v0.1.2").to_string(), "0.1.2");
        assert_eq!(v("0.10.0").to_string(), "0.10.0");
        assert_eq!(v("v0.2.0-beta.3").to_string(), "0.2.0-beta.3");
        assert!(Version::parse("v0.1").is_none());
        assert!(Version::parse("v0.1.2-rc1").is_none());
        assert!(Version::parse("v0.2.0-beta").is_none());
        assert!(Version::parse("main").is_none());

        assert!(v("v0.2.0") > v("v0.1.9"));
        assert!(v("v0.1.10") > v("v0.1.2"));
        // Betas come after the previous release and before their own.
        assert!(v("0.2.0-beta.1") > v("0.1.3"));
        assert!(v("0.2.0-beta.10") > v("0.2.0-beta.9"));
        assert!(v("0.2.0") > v("0.2.0-beta.10"));
        assert!(!v("0.2.0").is_beta());
        assert!(v("0.2.0-beta.1").is_beta());
    }

    #[test]
    fn stable_channel_upgrades_or_switches_back() {
        // Plain stable upgrade, and nothing to do.
        assert_eq!(plan(v("0.1.3"), v("0.2.0"), false), Plan::Upgrade);
        assert_eq!(plan(v("0.2.0"), v("0.2.0"), false), Plan::UpToDate);
        // A beta user gets the stable release once it is out...
        assert_eq!(plan(v("0.2.0-beta.3"), v("0.2.0"), false), Plan::Upgrade);
        // ...and can go back to an older stable before then.
        assert_eq!(plan(v("0.2.0-beta.3"), v("0.1.3"), false), Plan::SwitchBack);
    }

    #[test]
    fn beta_channel_takes_the_newest_of_anything() {
        assert_eq!(plan(v("0.1.3"), v("0.2.0-beta.1"), true), Plan::Upgrade);
        assert_eq!(
            plan(v("0.2.0-beta.1"), v("0.2.0-beta.2"), true),
            Plan::Upgrade
        );
        assert_eq!(plan(v("0.2.0-beta.2"), v("0.2.0"), true), Plan::Upgrade);
        assert_eq!(
            plan(v("0.2.0-beta.2"), v("0.2.0-beta.2"), true),
            Plan::UpToDate
        );
        assert_eq!(plan(v("0.1.3"), v("0.1.3"), true), Plan::UpToDate);
    }

    #[test]
    fn newest_release_skips_drafts_and_unknown_tags() {
        let listed: Vec<ListedRelease> = serde_json::from_str(
            r#"[
                {"tag_name": "v0.3.0", "draft": true},
                {"tag_name": "nightly", "draft": false},
                {"tag_name": "v0.2.0-beta.2", "draft": false},
                {"tag_name": "v0.1.3", "draft": false},
                {"tag_name": "v0.2.0-beta.10", "draft": false}
            ]"#,
        )
        .unwrap();
        let newest = newest_of(&listed).unwrap();
        assert_eq!(newest.tag, "v0.2.0-beta.10");
        assert_eq!(newest.version, v("0.2.0-beta.10"));
        assert!(newest_of(&[]).is_none());
    }

    #[test]
    fn only_newer_saves_are_at_risk_when_going_back() {
        let state = |saved_by: Option<&str>| TorrentState {
            version: 1,
            info_hash: npeer_core::InfoHash([1; 20]),
            name: "x".into(),
            output_dir: PathBuf::from("/tmp"),
            total_length: 1,
            piece_count: 1,
            have: String::new(),
            downloaded: 0,
            uploaded: 0,
            status: npeer_core::session::StoredStatus::Active,
            added_at: 0,
            completed_at: None,
            selection: None,
            saved_by: saved_by.map(str::to_owned),
        };
        let target = v("0.1.3");
        assert!(saved_by_newer(&state(Some("0.2.0-beta.1")), target));
        assert!(!saved_by_newer(&state(Some("0.1.3")), target));
        assert!(
            !saved_by_newer(&state(None), target),
            "from before saved_by"
        );
    }

    #[test]
    fn release_tag_comes_from_the_redirect() {
        assert_eq!(
            tag_from_location("https://github.com/JohnKesko/npeer/releases/tag/v0.1.1"),
            Some("v0.1.1".to_owned())
        );
        assert_eq!(
            tag_from_location("https://github.com/JohnKesko/npeer/releases"),
            None
        );
    }

    #[test]
    fn checksums_are_found_by_asset_name() {
        let sums = "abc123  npeer-aarch64-apple-darwin.tar.gz\n\
                    DEF456  npeer-x86_64-pc-windows-msvc.zip\n";
        assert_eq!(
            checksum_for(sums, "npeer-aarch64-apple-darwin.tar.gz").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            checksum_for(sums, "npeer-x86_64-pc-windows-msvc.zip").as_deref(),
            Some("def456"),
        );
        assert!(checksum_for(sums, "other.tar.gz").is_none());
    }

    #[test]
    fn this_platform_has_a_published_binary() {
        // Development happens on platforms we release for; if this fails,
        // the release matrix and release_asset() have drifted apart.
        assert!(release_asset().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn extracts_the_binary_from_a_release_shaped_archive() {
        use flate2::write::GzEncoder;
        let mut tar_bytes = Vec::new();
        {
            let encoder = GzEncoder::new(&mut tar_bytes, flate2::Compression::fast());
            let mut builder = tar::Builder::new(encoder);
            let content = b"fake binary";
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(
                    &mut header,
                    "npeer-aarch64-apple-darwin/npeer",
                    &content[..],
                )
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        assert_eq!(extract_binary(&tar_bytes).unwrap(), b"fake binary");
        assert!(extract_binary(b"not an archive").is_err());
    }
}
