//! The only part of Rymd that talks to the network.
//!
//! Everything here treats GitHub's answers as untrusted input: JSON is
//! parsed into a handful of named fields, asset names must match exactly
//! what [`InstallKind::asset_name`] asked for, and download URLs must be
//! HTTPS on a GitHub host. Nothing is ever executed from this module.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use super::installer::InstallKind;
use super::version::{Channel, parse_tag};

/// The repository the updater treats as the source of truth.
pub const REPO: &str = "agneswd/rymd";

/// Checksum file published next to every release artifact.
pub const CHECKSUM_ASSET: &str = "SHA256SUMS";

const API_RELEASES: &str = "https://api.github.com/repos/agneswd/rymd/releases?per_page=20";
const RELEASE_PAGE: &str = "https://github.com/agneswd/rymd/releases/latest";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// Release listings are small. Anything larger is not a release listing.
const MAX_JSON_BYTES: u64 = 4 * 1024 * 1024;
/// Generous ceiling for a single Rymd artifact.
const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

// ---- API shapes --------------------------------------------------------
//
// Only the fields the updater actually uses are deserialized.

#[derive(Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

/// A release that is newer than the running build.
#[derive(Clone, Debug)]
pub struct UpdateInfo {
    pub version: Version,
    pub tag: String,
    pub name: String,
    pub page_url: String,
    pub published_at: Option<String>,
    /// The artifact for this installation, absent when Rymd cannot update
    /// itself here (unsupported architecture, package-managed install).
    pub asset: Option<Asset>,
    /// Expected SHA-256 of `asset`, taken from the release `SHA256SUMS`.
    pub sha256: Option<String>,
}

impl UpdateInfo {
    /// Whether "Update now" can do the whole job without the browser.
    pub fn is_installable(&self) -> bool {
        self.asset.is_some() && self.sha256.is_some()
    }
}

/// Parse a releases listing. Malformed JSON is an error, never a silent
/// empty list, so a manual check can report something useful.
pub fn parse_releases(json: &str) -> Result<Vec<Release>> {
    serde_json::from_str(json).context("GitHub returned an unexpected release listing")
}

/// The newest release this build is allowed to be offered, or `None` when
/// the running version is already current.
///
/// Drafts are always ignored. Prereleases are ignored unless the running
/// build is itself a prerelease.
pub fn select_release<'a>(
    releases: &'a [Release],
    current: &Version,
    channel: Channel,
) -> Option<(&'a Release, Version)> {
    releases
        .iter()
        .filter(|r| !(r.draft || r.prerelease && channel == Channel::Stable))
        .filter_map(|r| parse_tag(&r.tag_name).map(|v| (r, v)))
        .filter(|(_, v)| channel.accepts(v))
        .max_by(|(_, a), (_, b)| a.cmp(b))
        .filter(|(_, v)| v > current)
}

/// Find the artifact for `kind` in a release, by exact name.
pub fn select_asset<'a>(
    release: &'a Release,
    tag: &str,
    kind: InstallKind,
    arch: &str,
) -> Option<&'a Asset> {
    let want = kind.asset_name(tag, arch)?;
    release.assets.iter().find(|a| a.name == want)
}

/// Look up one file's digest in a `sha256sum` style listing.
///
/// Lines look like `<64 hex>  <name>`; anything else is skipped.
pub fn checksum_for(checksums: &str, asset_name: &str) -> Option<String> {
    checksums.lines().find_map(|line| {
        let (digest, name) = line.split_once(char::is_whitespace)?;
        let name = name.trim_start_matches(['*', ' ']).trim();
        if name != asset_name || digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit())
        {
            return None;
        }
        Some(digest.to_ascii_lowercase())
    })
}

/// Build the update description for a chosen release. Pure: takes the
/// already fetched checksum listing so it can be tested without a network.
pub fn describe(
    release: &Release,
    version: Version,
    kind: InstallKind,
    arch: &str,
    checksums: Option<&str>,
) -> UpdateInfo {
    let asset = select_asset(release, &release.tag_name, kind, arch).cloned();
    let sha256 = asset
        .as_ref()
        .zip(checksums)
        .and_then(|(a, sums)| checksum_for(sums, &a.name));
    UpdateInfo {
        tag: release.tag_name.clone(),
        name: release
            .name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| format!("Rymd {version}")),
        page_url: if release.html_url.starts_with("https://github.com/") {
            release.html_url.clone()
        } else {
            RELEASE_PAGE.to_string()
        },
        published_at: release.published_at.clone(),
        version,
        asset,
        sha256,
    }
}

// ---- networking --------------------------------------------------------

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .user_agent(format!(
            "Rymd/{} (+https://github.com/{REPO})",
            super::version::CURRENT
        ))
        .https_only(true)
        .http_status_as_error(false)
        .build()
        .new_agent()
}

fn get(agent: &ureq::Agent, url: &str) -> Result<ureq::http::Response<ureq::Body>> {
    let res = agent
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .context("could not reach GitHub")?;

    let status = res.status().as_u16();
    if status == 403 || status == 429 {
        let exhausted = res
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            == Some("0");
        if exhausted {
            bail!("GitHub is rate limiting update checks. Try again later.");
        }
        bail!("GitHub refused the update check (HTTP {status}).");
    }
    if status == 404 {
        bail!("No Rymd releases were found.");
    }
    if !(200..300).contains(&status) {
        bail!("GitHub returned HTTP {status}.");
    }
    Ok(res)
}

/// Fetch the release listing and its checksum file.
///
/// Blocking. Callers run this off the UI thread.
pub fn fetch_update(current: &Version, kind: InstallKind, arch: &str) -> Result<Option<UpdateInfo>> {
    let agent = agent(REQUEST_TIMEOUT);
    let body = get(&agent, API_RELEASES)?
        .body_mut()
        .with_config()
        .limit(MAX_JSON_BYTES)
        .read_to_string()
        .context("could not read the GitHub release listing")?;

    let releases = parse_releases(&body)?;
    let Some((release, version)) = select_release(&releases, current, Channel::of(current)) else {
        return Ok(None);
    };

    // Only fetch checksums when an artifact for this install actually exists.
    let checksums = match select_asset(release, &release.tag_name, kind, arch) {
        Some(_) => release
            .assets
            .iter()
            .find(|a| a.name == CHECKSUM_ASSET)
            .and_then(|a| fetch_checksums(&agent, &a.browser_download_url).ok()),
        None => None,
    };

    Ok(Some(describe(
        release,
        version,
        kind,
        arch,
        checksums.as_deref(),
    )))
}

fn fetch_checksums(agent: &ureq::Agent, url: &str) -> Result<String> {
    check_download_url(url)?;
    Ok(get(agent, url)?
        .body_mut()
        .with_config()
        .limit(64 * 1024)
        .read_to_string()?)
}

/// Reject anything that is not an HTTPS URL on a GitHub host, whatever the
/// release metadata claims.
pub fn check_download_url(url: &str) -> Result<()> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| anyhow!("release asset URL is not HTTPS"))?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = host.split('@').next_back().unwrap_or_default();
    let ok = matches!(host, "github.com" | "api.github.com")
        || host.ends_with(".github.com")
        || host.ends_with(".githubusercontent.com");
    if !ok {
        bail!("release asset is not hosted on GitHub");
    }
    Ok(())
}

/// Shared state for one download: bytes so far, and the cancel flag.
#[derive(Default)]
pub struct Download {
    pub cancelled: AtomicBool,
}

impl Download {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

/// Download `info`'s artifact into a fresh temporary directory, verify its
/// SHA-256, and return the file.
///
/// Blocking. `on_progress` is called from this thread with the running byte
/// count; it is expected to coalesce. The file is deleted unless
/// verification succeeds, so a caller can never install a partial download.
pub fn download_verified(
    info: &UpdateInfo,
    download: &Arc<Download>,
    on_progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf> {
    let asset = info.asset.as_ref().context("no artifact for this platform")?;
    let expected = info
        .sha256
        .as_ref()
        .context("the release has no SHA256SUMS entry for this artifact")?;
    check_download_url(&asset.browser_download_url)?;

    // The name embeds the release tag, so an artifact from another version
    // cannot be substituted for the one that was offered.
    if !asset.name.contains(&info.tag) {
        bail!("release artifact does not belong to {}", info.tag);
    }

    let dir = tempfile::Builder::new()
        .prefix("rymd-update-")
        .tempdir()
        .context("could not create a temporary directory")?;
    let path = dir.path().join(&asset.name);

    let agent = agent(DOWNLOAD_TIMEOUT);
    let mut res = get(&agent, &asset.browser_download_url)?;
    let total = res
        .body()
        .content_length()
        .or(Some(asset.size).filter(|s| *s > 0));
    if total.is_some_and(|t| t > MAX_ARTIFACT_BYTES) {
        bail!("release artifact is implausibly large");
    }

    let digest = {
        let mut file = std::fs::File::create(&path)
            .with_context(|| format!("cannot write {}", path.display()))?;
        let mut reader = res
            .body_mut()
            .with_config()
            .limit(MAX_ARTIFACT_BYTES)
            .reader();
        stream_hashed(&mut reader, &mut file, total, download, on_progress)?
    };

    if !digest.eq_ignore_ascii_case(expected) {
        bail!("the downloaded update failed its checksum check");
    }

    // Keep the private temporary directory until the installer has used
    // the file. The OS reclaims it; Rymd never leaves one behind after a
    // successful install because the artifact is moved out of it.
    let _ = dir.keep();
    Ok(path)
}

/// Copy `reader` into `out`, hashing as it goes, and return the SHA-256 as
/// lowercase hex.
///
/// Fails on cancellation and on a short read against a known
/// `total`, so a truncated transfer can never be mistaken for a complete
/// one before the checksum is even considered.
fn stream_hashed(
    reader: &mut impl Read,
    out: &mut impl Write,
    total: Option<u64>,
    download: &Arc<Download>,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 128 * 1024];
    let mut seen: u64 = 0;
    loop {
        if download.cancelled.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let n = reader.read(&mut buf).context("download interrupted")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n]).context("cannot write the update")?;
        seen += n as u64;
        on_progress(seen, total);
    }
    out.flush()?;
    if total.is_some_and(|t| t != seen) {
        bail!("download was incomplete: got {seen} of {} bytes", total.unwrap());
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Verify a file already on disk against an expected digest.
pub fn verify_file(path: &Path, expected: &str) -> Result<()> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    if !hex(&hasher.finalize()).eq_ignore_ascii_case(expected) {
        bail!("the downloaded update failed its checksum check");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = include_str!("testdata/releases.json");

    fn releases() -> Vec<Release> {
        parse_releases(LISTING).unwrap()
    }

    #[test]
    fn picks_the_newest_stable_release() {
        let rs = releases();
        let (r, v) = select_release(&rs, &Version::new(0, 1, 0), Channel::Stable).unwrap();
        assert_eq!(r.tag_name, "v0.10.0");
        assert_eq!(v, Version::new(0, 10, 0));
    }

    #[test]
    fn skips_drafts_and_prereleases_on_stable() {
        let rs = releases();
        let picked = select_release(&rs, &Version::new(0, 1, 0), Channel::Stable).unwrap();
        assert!(!picked.0.draft && !picked.0.prerelease);
    }

    #[test]
    fn prerelease_channel_sees_prereleases() {
        let rs = releases();
        let (r, _) = select_release(&rs, &Version::new(0, 1, 0), Channel::Prerelease).unwrap();
        assert_eq!(r.tag_name, "v0.11.0-rc.1");
    }

    #[test]
    fn drafts_are_never_offered() {
        let rs = releases();
        assert!(rs.iter().any(|r| r.draft));
        for channel in [Channel::Stable, Channel::Prerelease] {
            let (r, _) = select_release(&rs, &Version::new(0, 1, 0), channel).unwrap();
            assert!(!r.draft);
        }
    }

    #[test]
    fn current_version_newer_than_every_release() {
        let rs = releases();
        assert!(select_release(&rs, &Version::new(9, 0, 0), Channel::Stable).is_none());
        // Equal to the newest release is also "nothing to do".
        assert!(select_release(&rs, &Version::new(0, 10, 0), Channel::Stable).is_none());
    }

    #[test]
    fn matches_the_windows_and_linux_assets() {
        let rs = releases();
        let (r, _) = select_release(&rs, &Version::new(0, 1, 0), Channel::Stable).unwrap();
        let pick = |k| select_asset(r, &r.tag_name, k, "x86_64").map(|a| a.name.as_str());
        assert_eq!(
            pick(InstallKind::WindowsInstaller),
            Some("rymd-v0.10.0-windows-x86_64-setup.exe")
        );
        assert_eq!(
            pick(InstallKind::WindowsPortable),
            Some("rymd-v0.10.0-windows-x86_64.exe")
        );
        assert_eq!(
            pick(InstallKind::LinuxAppImage),
            Some("rymd-v0.10.0-linux-x86_64.AppImage")
        );
    }

    #[test]
    fn unsupported_architecture_matches_nothing() {
        let rs = releases();
        let (r, _) = select_release(&rs, &Version::new(0, 1, 0), Channel::Stable).unwrap();
        assert!(select_asset(r, &r.tag_name, InstallKind::LinuxAppImage, "aarch64").is_none());
    }

    #[test]
    fn missing_asset_leaves_the_update_uninstallable() {
        let rs = releases();
        // v0.9.0 in the fixture ships no Windows installer.
        let old = rs.iter().find(|r| r.tag_name == "v0.9.0").unwrap();
        let info = describe(
            old,
            Version::new(0, 9, 0),
            InstallKind::WindowsInstaller,
            "x86_64",
            None,
        );
        assert!(info.asset.is_none());
        assert!(!info.is_installable());
    }

    #[test]
    fn malformed_api_response_is_an_error() {
        assert!(parse_releases("not json").is_err());
        assert!(parse_releases(r#"{"message":"Not Found"}"#).is_err());
        assert!(parse_releases("[]").unwrap().is_empty());
    }

    #[test]
    fn reads_checksums_by_exact_name() {
        let sums = "\
aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111  rymd-v0.10.0-linux-x86_64.AppImage
bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222 *rymd-v0.10.0-windows-x86_64.exe
garbage line
";
        assert_eq!(
            checksum_for(sums, "rymd-v0.10.0-linux-x86_64.AppImage").as_deref(),
            Some("aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111")
        );
        assert_eq!(
            checksum_for(sums, "rymd-v0.10.0-windows-x86_64.exe").as_deref(),
            Some("bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222")
        );
        assert_eq!(checksum_for(sums, "rymd-v0.10.0-linux-x86_64.tar.gz"), None);
    }

    #[test]
    fn describe_attaches_the_matching_checksum() {
        let rs = releases();
        let (r, v) = select_release(&rs, &Version::new(0, 1, 0), Channel::Stable).unwrap();
        let sums = format!(
            "{}  rymd-v0.10.0-linux-x86_64.AppImage\n",
            "c".repeat(64)
        );
        let info = describe(r, v, InstallKind::LinuxAppImage, "x86_64", Some(&sums));
        assert_eq!(info.sha256.as_deref(), Some("c".repeat(64).as_str()));
        assert!(info.is_installable());
        assert_eq!(info.page_url, "https://github.com/agneswd/rymd/releases/tag/v0.10.0");
    }

    fn stream(data: &[u8], total: Option<u64>) -> Result<String> {
        let mut out = Vec::new();
        stream_hashed(
            &mut std::io::Cursor::new(data),
            &mut out,
            total,
            &Arc::new(Download::default()),
            |_, _| {},
        )
    }

    /// sha256("hello")
    const HELLO: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn a_complete_download_hashes_to_the_expected_digest() {
        assert_eq!(stream(b"hello", Some(5)).unwrap(), HELLO);
        // An unknown length is still accepted; the checksum decides.
        assert_eq!(stream(b"hello", None).unwrap(), HELLO);
    }

    #[test]
    fn an_incomplete_download_is_rejected_before_the_checksum() {
        let err = stream(b"hel", Some(5)).unwrap_err().to_string();
        assert!(err.contains("incomplete"), "{err}");
    }

    #[test]
    fn a_cancelled_download_stops() {
        let handle = Arc::new(Download::default());
        handle.cancel();
        let mut out = Vec::new();
        let err = stream_hashed(
            &mut std::io::Cursor::new(b"hello"),
            &mut out,
            Some(5),
            &handle,
            |_, _| {},
        )
        .unwrap_err()
        .to_string();
        assert_eq!(err, "cancelled");
        assert!(out.is_empty());
    }

    #[test]
    fn progress_reports_every_chunk_read() {
        let data = vec![7u8; 300 * 1024];
        let mut seen = Vec::new();
        let mut out = Vec::new();
        stream_hashed(
            &mut std::io::Cursor::new(&data),
            &mut out,
            Some(data.len() as u64),
            &Arc::new(Download::default()),
            |n, total| {
                assert_eq!(total, Some(300 * 1024));
                seen.push(n);
            },
        )
        .unwrap();
        assert_eq!(seen.last(), Some(&(300 * 1024)));
        assert!(seen.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn checksum_mismatch_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifact");
        std::fs::write(&path, b"hello").unwrap();
        // sha256("hello")
        let good = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(verify_file(&path, good).is_ok());
        assert!(verify_file(&path, &"0".repeat(64)).is_err());
    }

    #[test]
    fn only_github_hosted_downloads_are_accepted() {
        assert!(check_download_url("https://github.com/agneswd/rymd/releases/download/x").is_ok());
        assert!(check_download_url("https://objects.githubusercontent.com/x").is_ok());
        assert!(check_download_url("http://github.com/x").is_err());
        assert!(check_download_url("https://evil.example.com/x").is_err());
        assert!(check_download_url("https://github.com.evil.example/x").is_err());
        assert!(check_download_url("https://github.com@evil.example/x").is_err());
    }
}

