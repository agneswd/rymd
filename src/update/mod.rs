//! Self-update against the GitHub Releases of `agneswd/rymd`.
//!
//! The shell only ever sees [`UpdateStatus`], [`UpdateInfo`] and
//! [`UpdateState`]. GitHub, semver and platform install details stay in the
//! submodules.
//!
//! Every entry point here blocks; callers run them on a background thread.
//! Nothing in this module touches GPUI.

pub mod github;
pub mod installer;
pub mod version;

use std::path::PathBuf;
use std::sync::Arc;

use semver::Version;

pub use github::{Download, UpdateInfo};
pub use installer::InstallKind;
pub use version::CURRENT;

/// Set `RYMD_NO_UPDATE_CHECK=1` to keep Rymd entirely offline.
const OPT_OUT: &str = "RYMD_NO_UPDATE_CHECK";

/// Result of one check against GitHub.
#[derive(Debug)]
pub enum UpdateStatus {
    UpToDate,
    UpdateAvailable(Box<UpdateInfo>),
    /// The check could not be completed. The reason is shown for a manual
    /// check and swallowed at startup.
    Unavailable(String),
}

/// Where the updater is in its lifecycle. One value, owned by the shell.
pub enum UpdateState {
    Idle,
    Checking,
    Available(Box<UpdateInfo>),
    Downloading {
        info: Box<UpdateInfo>,
        downloaded: u64,
        total: Option<u64>,
        download: Arc<Download>,
    },
    Ready {
        info: Box<UpdateInfo>,
        artifact: PathBuf,
    },
    Installing,
    Failed(String),
}

impl UpdateState {
    /// True while a check or a download is in flight, so the UI can refuse
    /// to start a second one.
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            UpdateState::Checking | UpdateState::Downloading { .. } | UpdateState::Installing
        )
    }
}

/// Versions the user answered "Later" to during this run.
///
/// Startup offers a version at most once per launch. A manual check always
/// shows the dialog, because the user asked for it.
#[derive(Default)]
pub struct SessionDismissals(Vec<Version>);

impl SessionDismissals {
    pub fn dismiss(&mut self, version: &Version) {
        if !self.0.contains(version) {
            self.0.push(version.clone());
        }
    }

    /// Whether a silent startup check may raise the dialog for `version`.
    pub fn should_auto_offer(&self, version: &Version) -> bool {
        !self.0.contains(version)
    }
}

/// Whether update checks are allowed at all in this process.
pub fn checks_enabled() -> bool {
    !matches!(
        std::env::var(OPT_OUT).as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// Check GitHub for a release newer than `current`.
///
/// Blocking, and never panics: any failure becomes
/// [`UpdateStatus::Unavailable`] with a message fit to show a user who
/// asked for the check.
pub fn check_for_update(current: &Version) -> UpdateStatus {
    if !checks_enabled() {
        return UpdateStatus::Unavailable("Update checks are disabled.".into());
    }
    let kind = InstallKind::detect();
    match github::fetch_update(current, kind, std::env::consts::ARCH) {
        Ok(None) => UpdateStatus::UpToDate,
        Ok(Some(info)) => UpdateStatus::UpdateAvailable(Box::new(info)),
        Err(e) => UpdateStatus::Unavailable(format!("{e:#}")),
    }
}

/// Download and verify the artifact for an offered update.
///
/// Blocking. `on_progress` is called on this thread; it must be cheap.
pub fn download(
    info: &UpdateInfo,
    handle: &Arc<Download>,
    on_progress: impl FnMut(u64, Option<u64>),
) -> anyhow::Result<PathBuf> {
    github::download_verified(info, handle, on_progress)
}

/// Start the platform takeover for a verified artifact. The caller must
/// quit Rymd as soon as this returns.
pub fn install(artifact: &std::path::Path) -> anyhow::Result<()> {
    installer::for_current_install(InstallKind::detect()).install(artifact)
}

#[cfg(test)]
mod tests {
    use super::*;
    use github::Asset;

    fn info() -> Box<UpdateInfo> {
        Box::new(UpdateInfo {
            version: Version::new(0, 2, 0),
            tag: "v0.2.0".into(),
            name: "Rymd 0.2.0".into(),
            page_url: "https://github.com/agneswd/rymd/releases/tag/v0.2.0".into(),
            published_at: None,
            asset: Some(Asset {
                name: "rymd-v0.2.0-linux-x86_64.AppImage".into(),
                browser_download_url: "https://github.com/x".into(),
                size: 10,
            }),
            sha256: Some("a".repeat(64)),
        })
    }

    #[test]
    fn busy_states_block_a_second_check() {
        assert!(!UpdateState::Idle.is_busy());
        assert!(UpdateState::Checking.is_busy());
        assert!(!UpdateState::Available(info()).is_busy());
        assert!(UpdateState::Installing.is_busy());
        assert!(!UpdateState::Failed("x".into()).is_busy());
    }

    #[test]
    fn the_offered_version_survives_every_step() {
        // Available -> Downloading -> Ready must all describe one release,
        // so the artifact that gets installed is the one that was offered.
        let states = [
            UpdateState::Available(info()),
            UpdateState::Downloading {
                info: info(),
                downloaded: 5,
                total: Some(10),
                download: Arc::new(Download::default()),
            },
            UpdateState::Ready {
                info: info(),
                artifact: PathBuf::from("/tmp/x"),
            },
        ];
        for state in &states {
            let info = match state {
                UpdateState::Available(i)
                | UpdateState::Downloading { info: i, .. }
                | UpdateState::Ready { info: i, .. } => i,
                _ => unreachable!(),
            };
            assert_eq!(info.version, Version::new(0, 2, 0));
            assert!(info.asset.as_ref().unwrap().name.contains(&info.tag));
        }
    }

    #[test]
    fn later_hides_the_same_version_for_the_rest_of_the_session() {
        let (v2, v3) = (Version::new(0, 2, 0), Version::new(0, 3, 0));
        let mut seen = SessionDismissals::default();
        assert!(seen.should_auto_offer(&v2));

        seen.dismiss(&v2);
        assert!(!seen.should_auto_offer(&v2));
        // Dismissing twice is harmless, and a later release still gets offered.
        seen.dismiss(&v2);
        assert!(!seen.should_auto_offer(&v2));
        assert!(seen.should_auto_offer(&v3));
    }

    /// The release workflow templates the tag into asset names with this
    /// expression, so the fixed part of every name can be compared against
    /// what the updater asks GitHub for.
    const WORKFLOW_TAG: &str = "${{ env.RYMD_VERSION }}";
    const WORKFLOW: &str = include_str!("../../.github/workflows/release.yml");

    #[test]
    fn the_release_workflow_builds_the_assets_the_updater_asks_for() {
        for kind in [
            InstallKind::WindowsInstaller,
            InstallKind::WindowsPortable,
            InstallKind::LinuxAppImage,
        ] {
            let name = kind.asset_name(WORKFLOW_TAG, "x86_64").unwrap();
            assert!(
                WORKFLOW.contains(&name),
                "release.yml does not produce {name}, which the updater needs for {kind:?}"
            );
        }
        assert!(WORKFLOW.contains(github::CHECKSUM_ASSET));
    }

    #[test]
    fn the_release_workflow_pins_the_tag_to_the_cargo_version() {
        // The updater is only correct while a tag cannot outrun Cargo.toml.
        assert!(
            WORKFLOW.contains("Verify the tag matches the Cargo version"),
            "release.yml no longer checks the Cargo version against the tag"
        );
    }

    #[test]
    fn checks_can_be_switched_off() {
        // SAFETY: single-threaded test, and the variable is only read here.
        unsafe { std::env::set_var(OPT_OUT, "1") };
        assert!(!checks_enabled());
        assert!(matches!(
            check_for_update(&Version::new(0, 1, 0)),
            UpdateStatus::Unavailable(_)
        ));
        unsafe { std::env::remove_var(OPT_OUT) };
        assert!(checks_enabled());
    }
}
