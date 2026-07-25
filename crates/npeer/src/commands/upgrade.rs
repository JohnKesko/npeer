//! `npeer upgrade`: replace the running binary with the latest release.
//!
//! One channel only — GitHub's "latest" release, which never includes
//! drafts or pre-releases. The flow: discover the newest version, confirm,
//! download the archive for this platform, verify it against the release's
//! SHA256SUMS.txt, then swap the running executable in place.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use sha2::Digest;

use crate::config::ProgressMode;
use crate::ui::Phase;
use crate::units;

const REPO: &str = "JohnKesko/npeer";

pub async fn run(progress: ProgressMode, check_only: bool, yes: bool) -> Result<ExitCode> {
    let current_text = env!("CARGO_PKG_VERSION");
    let current = parse_version(current_text).expect("crate version is well-formed");

    let phase = Phase::start(progress, "Checking for updates");
    let latest_tag = match latest_release_tag().await {
        Ok(tag) => tag,
        Err(err) => {
            phase.fail("Could not check for updates");
            return Err(err);
        }
    };
    let latest = parse_version(&latest_tag)
        .with_context(|| format!("unexpected release tag `{latest_tag}`"))?;
    let latest_text = latest_tag.trim_start_matches('v').to_owned();

    if latest <= current {
        phase.done(Some(&format!("npeer is up to date ({current_text})")));
        return Ok(ExitCode::SUCCESS);
    }
    phase.done(Some(&format!(
        "Update available: {current_text} → {latest_text}"
    )));

    if check_only {
        println!("Run `npeer upgrade` to install it.");
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

    if !yes && !confirm(&format!("Download and install npeer {latest_text}?"))? {
        println!("Not upgrading.");
        return Ok(ExitCode::SUCCESS);
    }

    let phase = Phase::start(progress, "Downloading");
    let (archive, sums) = match download_release_files(asset).await {
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
                "npeer is now {latest_text} (replaced {})",
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

/// `(major, minor, patch)` from "0.1.2" or "v0.1.2".
fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.trim().trim_start_matches('v').split('.');
    let version = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(version)
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

/// Find the newest release tag by following GitHub's `latest` redirect —
/// no API token, no rate limits, and pre-releases are never offered.
async fn latest_release_tag() -> Result<String> {
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
    tag_from_location(location).context("GitHub's redirect did not name a release tag")
}

fn tag_from_location(location: &str) -> Option<String> {
    let (_, tag) = location.rsplit_once("/tag/")?;
    let tag = tag.trim_matches('/');
    (!tag.is_empty()).then(|| tag.to_owned())
}

async fn download_release_files(asset: &str) -> Result<(Vec<u8>, String)> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("npeer/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .context("could not build the HTTP client")?;
    let fetch = async |name: &str| -> Result<Vec<u8>> {
        let url = format!("https://github.com/{REPO}/releases/latest/download/{name}");
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

    #[test]
    fn versions_parse_and_compare() {
        assert_eq!(parse_version("v0.1.2"), Some((0, 1, 2)));
        assert_eq!(parse_version("0.10.0"), Some((0, 10, 0)));
        assert!(parse_version("v0.1").is_none());
        assert!(parse_version("v0.1.2-rc1").is_none());
        assert!(parse_version("main").is_none());
        // Tuple ordering is the version ordering.
        assert!(parse_version("v0.2.0") > parse_version("v0.1.9"));
        assert!(parse_version("v0.1.10") > parse_version("v0.1.2"));
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
