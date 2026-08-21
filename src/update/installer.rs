//! How the running Rymd was installed, and how to replace it.
//!
//! This module owns the single mapping from an installation to the release
//! asset that updates it. `github.rs` never guesses asset names with
//! substring checks; it asks [`InstallKind::asset_name`] for one exact file
//! name and looks that up in the release.
//!
//! The naming scheme is documented in `docs/RELEASING.md` and produced by
//! `.github/workflows/release.yml`. Change all three together.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};

/// The only CPU architecture Rymd currently ships binaries for.
const SUPPORTED_ARCH: &str = "x86_64";

/// How this copy of Rymd was installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallKind {
    /// Installed by the Inno Setup installer. Updates re-run the installer.
    WindowsInstaller,
    /// A loose `rymd.exe`. Updates swap the binary next to itself.
    WindowsPortable,
    /// Running from an AppImage. Updates replace the AppImage file.
    LinuxAppImage,
    /// A distribution package, a tarball, or a local build. Never touched;
    /// the user is pointed at the release page instead.
    Managed,
}

impl InstallKind {
    /// Detect the installation kind of the running process.
    pub fn detect() -> Self {
        if cfg!(windows) {
            match std::env::current_exe() {
                Ok(exe) if in_program_files(&exe) => InstallKind::WindowsInstaller,
                Ok(_) => InstallKind::WindowsPortable,
                Err(_) => InstallKind::Managed,
            }
        } else if cfg!(target_os = "linux") {
            // The AppImage runtime exports the path of the running image.
            match std::env::var_os("APPIMAGE").map(PathBuf::from) {
                Some(p) if p.is_file() => InstallKind::LinuxAppImage,
                _ => InstallKind::Managed,
            }
        } else {
            InstallKind::Managed
        }
    }

    /// The exact release asset that updates this installation, or `None`
    /// when Rymd cannot update itself here.
    ///
    /// `tag` is the release tag (`v0.2.0`), `arch` is a value of
    /// [`std::env::consts::ARCH`].
    pub fn asset_name(self, tag: &str, arch: &str) -> Option<String> {
        if arch != SUPPORTED_ARCH {
            return None;
        }
        let name = match self {
            InstallKind::WindowsInstaller => format!("rymd-{tag}-windows-x86_64-setup.exe"),
            InstallKind::WindowsPortable => format!("rymd-{tag}-windows-x86_64.exe"),
            InstallKind::LinuxAppImage => format!("rymd-{tag}-linux-x86_64.AppImage"),
            InstallKind::Managed => return None,
        };
        Some(name)
    }

    /// Short description used when Rymd cannot update itself.
    pub fn manual_reason(self) -> &'static str {
        match self {
            InstallKind::Managed => {
                "Rymd was installed by a package manager or built from source, \
                 so it will not replace itself."
            }
            _ => "No release artifact matches this platform.",
        }
    }
}

/// Replaces the installation with a downloaded, already verified artifact.
///
/// `install` starts the takeover (an installer process, or a swapped
/// binary plus a fresh Rymd) and returns. The caller must then quit: the
/// old process must be gone before the new one takes over.
pub trait UpdateInstaller {
    fn can_self_update(&self) -> bool;
    fn install(&self, artifact: &Path) -> Result<()>;
}

/// The installer for the running process.
pub fn for_current_install(kind: InstallKind) -> Box<dyn UpdateInstaller> {
    match kind {
        InstallKind::WindowsInstaller => Box::new(WindowsInstallerUpdate),
        InstallKind::WindowsPortable => Box::new(BinarySwap::current_exe()),
        InstallKind::LinuxAppImage => Box::new(BinarySwap::app_image()),
        InstallKind::Managed => Box::new(NoSelfUpdate),
    }
}

/// Installations Rymd must not modify.
struct NoSelfUpdate;

impl UpdateInstaller for NoSelfUpdate {
    fn can_self_update(&self) -> bool {
        false
    }

    fn install(&self, _artifact: &Path) -> Result<()> {
        bail!("This installation of Rymd cannot update itself.")
    }
}

/// Hands the downloaded Inno Setup installer the job. Windows asks for
/// elevation there if Rymd lives in a system directory, so Rymd itself
/// never runs elevated. The installer relaunches Rymd when it finishes.
struct WindowsInstallerUpdate;

impl UpdateInstaller for WindowsInstallerUpdate {
    fn can_self_update(&self) -> bool {
        true
    }

    fn install(&self, artifact: &Path) -> Result<()> {
        // The path is passed as an argument, never through a shell.
        Command::new(artifact)
            .args(["/VERYSILENT", "/NORESTART", "/SUPPRESSMSGBOXES"])
            .spawn()
            .context("failed to start the Rymd installer")?;
        Ok(())
    }
}

/// Swaps a single self-contained file (a portable `rymd.exe` or an
/// AppImage) and restarts it.
///
/// The running file is never written through. It is moved aside first, so
/// the open executable image stays valid until the process exits.
struct BinarySwap {
    target: Option<PathBuf>,
}

impl BinarySwap {
    fn current_exe() -> Self {
        Self {
            target: std::env::current_exe().ok(),
        }
    }

    fn app_image() -> Self {
        Self {
            target: std::env::var_os("APPIMAGE").map(PathBuf::from),
        }
    }
}

impl UpdateInstaller for BinarySwap {
    fn can_self_update(&self) -> bool {
        self.target.is_some()
    }

    fn install(&self, artifact: &Path) -> Result<()> {
        let target = self
            .target
            .as_deref()
            .context("cannot locate the running Rymd executable")?;

        set_executable(artifact)?;

        // Stage next to the target so the final step is an atomic rename on
        // the same filesystem, and so a full disk fails before anything moves.
        let staged = target.with_extension("rymd-update");
        let _ = std::fs::remove_file(&staged);
        move_file(artifact, &staged)
            .with_context(|| format!("failed to stage the update at {}", staged.display()))?;

        // Windows refuses to replace a running image, but it does allow
        // renaming it away. The leftover is cleaned up on the next launch.
        let parked = target.with_extension("rymd-old");
        let _ = std::fs::remove_file(&parked);
        if cfg!(windows) {
            std::fs::rename(target, &parked)
                .with_context(|| format!("failed to move {} aside", target.display()))?;
        }

        if let Err(e) = std::fs::rename(&staged, target) {
            // Put the old build back rather than leaving no executable.
            if cfg!(windows) {
                let _ = std::fs::rename(&parked, target);
            }
            let _ = std::fs::remove_file(&staged);
            return Err(e).with_context(|| format!("failed to install {}", target.display()));
        }

        Command::new(target)
            .spawn()
            .with_context(|| format!("failed to restart {}", target.display()))?;
        Ok(())
    }
}

/// Delete the executable a previous Windows update moved aside. Best
/// effort: if the file is still locked it is retried on the next launch.
pub fn clean_stale_backup() {
    if !cfg!(windows) {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("rymd-old"));
    }
}

/// Rename, falling back to copy when the temporary file is on another
/// filesystem (the usual case for `/tmp` versus a home directory).
fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) => {
            std::fs::copy(from, to)?;
            let _ = std::fs::remove_file(from);
            Ok(())
        }
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("failed to make {} executable", path.display()))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
fn in_program_files(exe: &Path) -> bool {
    ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
        .iter()
        .filter_map(|k| std::env::var_os(k))
        .any(|root| exe.starts_with(PathBuf::from(root)))
}

#[cfg(not(windows))]
fn in_program_files(_exe: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_names_match_the_release_scheme() {
        assert_eq!(
            InstallKind::WindowsInstaller.asset_name("v0.2.0", "x86_64"),
            Some("rymd-v0.2.0-windows-x86_64-setup.exe".into())
        );
        assert_eq!(
            InstallKind::WindowsPortable.asset_name("v0.2.0", "x86_64"),
            Some("rymd-v0.2.0-windows-x86_64.exe".into())
        );
        assert_eq!(
            InstallKind::LinuxAppImage.asset_name("v0.2.0", "x86_64"),
            Some("rymd-v0.2.0-linux-x86_64.AppImage".into())
        );
    }

    #[test]
    fn unsupported_architecture_has_no_asset() {
        for kind in [
            InstallKind::WindowsInstaller,
            InstallKind::WindowsPortable,
            InstallKind::LinuxAppImage,
        ] {
            assert_eq!(kind.asset_name("v0.2.0", "aarch64"), None);
        }
    }

    #[test]
    fn package_managed_installs_are_never_replaced() {
        assert_eq!(InstallKind::Managed.asset_name("v0.2.0", "x86_64"), None);
        assert!(!for_current_install(InstallKind::Managed).can_self_update());
    }
}
