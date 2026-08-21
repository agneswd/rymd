//! Version parsing and release-channel rules for the updater.
//!
//! GitHub tags carry a leading `v` (`v0.2.0`); Cargo does not. Everything
//! outside this module works with parsed [`Version`] values, never with
//! version strings, so comparison is always semver-correct
//! (`0.10.0 > 0.9.0`, not lexicographic).

use semver::Version;

/// The version of the running build, taken from Cargo at compile time.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// Which releases a build is willing to be offered.
///
/// A stable build is only ever offered stable releases. A prerelease build
/// (`0.3.0-rc.1`) also accepts prereleases, so testers keep getting them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Prerelease,
}

impl Channel {
    /// The channel a build belongs to, decided by its own version.
    pub fn of(version: &Version) -> Self {
        if version.pre.is_empty() {
            Channel::Stable
        } else {
            Channel::Prerelease
        }
    }

    pub fn accepts(self, candidate: &Version) -> bool {
        candidate.pre.is_empty() || self == Channel::Prerelease
    }
}

/// Parse a release tag such as `v0.2.0` or `0.2.0` into a [`Version`].
///
/// Returns `None` for tags that are not plain semver, so a stray tag in the
/// repository can never be offered as an update.
pub fn parse_tag(tag: &str) -> Option<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}

/// The version of the running build. Panics only if Cargo.toml is not semver,
/// which the release workflow rejects.
pub fn current() -> Version {
    Version::parse(CURRENT).expect("CARGO_PKG_VERSION is valid semver")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_leading_v() {
        assert_eq!(parse_tag("v0.2.0"), Some(Version::new(0, 2, 0)));
        assert_eq!(parse_tag("0.2.0"), Some(Version::new(0, 2, 0)));
    }

    #[test]
    fn rejects_non_semver_tags() {
        assert!(parse_tag("nightly").is_none());
        assert!(parse_tag("v1.2").is_none());
        assert!(parse_tag("").is_none());
    }

    #[test]
    fn compares_by_semver_not_lexicographically() {
        assert!(parse_tag("v0.10.0") > parse_tag("v0.9.0"));
        assert!(parse_tag("v1.0.0") > parse_tag("v0.99.0"));
        assert!(parse_tag("v0.2.0") > parse_tag("v0.2.0-rc.1"));
    }

    #[test]
    fn stable_builds_reject_prereleases() {
        let stable = Version::new(0, 1, 0);
        let ch = Channel::of(&stable);
        assert_eq!(ch, Channel::Stable);
        assert!(ch.accepts(&Version::new(0, 2, 0)));
        assert!(!ch.accepts(&parse_tag("v0.3.0-beta.1").unwrap()));
    }

    #[test]
    fn prerelease_builds_accept_both() {
        let ch = Channel::of(&parse_tag("v0.3.0-rc.1").unwrap());
        assert_eq!(ch, Channel::Prerelease);
        assert!(ch.accepts(&Version::new(0, 3, 0)));
        assert!(ch.accepts(&parse_tag("v0.3.0-rc.2").unwrap()));
    }

    #[test]
    fn the_running_version_comes_from_cargo() {
        assert_eq!(current(), Version::parse(CURRENT).unwrap());
        assert_eq!(parse_tag(&format!("v{CURRENT}")), Some(current()));
    }
}
