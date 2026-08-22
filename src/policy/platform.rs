use crate::error::{GroveError, Result};

pub const MINIMUM_GIT: (u32, u32) = (2, 47);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl GitVersion {
    pub fn parse(output: &[u8]) -> Result<GitVersion> {
        let text = String::from_utf8_lossy(output);
        let mut words = text.split_whitespace();
        let rest = match (words.next(), words.next(), words.next()) {
            (Some("git"), Some("version"), Some(rest)) => rest,
            _ => return Err(GroveError::failure("cannot parse `git --version` output")),
        };
        let mut parts = rest.split('.');
        let mut number = |name: &str| -> Result<u32> {
            parts.next().and_then(|p| p.parse().ok()).ok_or_else(|| {
                GroveError::failure(format!("cannot parse the {name} of `git --version`"))
            })
        };
        let major = number("major")?;
        let minor = number("minor")?;
        let patch = match parts.next() {
            Some(patch) => patch
                .parse()
                .map_err(|_| GroveError::failure("cannot parse the patch of `git --version`"))?,
            None => 0,
        };
        Ok(GitVersion {
            major,
            minor,
            patch,
        })
    }

    pub fn at_least(&self, major: u32, minor: u32) -> bool {
        (self.major, self.minor) >= (major, minor)
    }
}

/// The `gh`/`glab` versions measured when `--create` was specified — the
/// version actually measured against, not the oldest one that might work,
/// mirroring [`MINIMUM_GIT`]'s own pattern.
pub const MINIMUM_GH: (u32, u32) = (2, 97);
pub const MINIMUM_GLAB: (u32, u32) = (1, 114);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl ProviderVersion {
    /// `"gh version X.Y.Z (...)"`.
    pub fn parse_gh(output: &[u8]) -> Result<Self> {
        let text = String::from_utf8_lossy(output);
        let mut words = text.split_whitespace();
        let rest = match (words.next(), words.next(), words.next()) {
            (Some("gh"), Some("version"), Some(rest)) => rest,
            _ => return Err(GroveError::failure("cannot parse `gh --version` output")),
        };
        Self::parse_dotted(rest, "gh")
    }

    /// `"glab X.Y.Z (...)"`.
    pub fn parse_glab(output: &[u8]) -> Result<Self> {
        let text = String::from_utf8_lossy(output);
        let mut words = text.split_whitespace();
        let rest = match (words.next(), words.next()) {
            (Some("glab"), Some(rest)) => rest,
            _ => return Err(GroveError::failure("cannot parse `glab --version` output")),
        };
        Self::parse_dotted(rest, "glab")
    }

    fn parse_dotted(rest: &str, program: &str) -> Result<Self> {
        let mut parts = rest.split('.');
        let mut number = |name: &str| -> Result<u32> {
            parts.next().and_then(|p| p.parse().ok()).ok_or_else(|| {
                GroveError::failure(format!("cannot parse the {name} of `{program} --version`"))
            })
        };
        let major = number("major")?;
        let minor = number("minor")?;
        let patch = match parts.next() {
            Some(patch) => patch.parse().map_err(|_| {
                GroveError::failure(format!("cannot parse the patch of `{program} --version`"))
            })?,
            None => 0,
        };
        Ok(ProviderVersion {
            major,
            minor,
            patch,
        })
    }

    pub fn at_least(&self, major: u32, minor: u32) -> bool {
        (self.major, self.minor) >= (major, minor)
    }
}

pub fn check_platform() -> Result<()> {
    if !cfg!(target_os = "linux") || !cfg!(target_pointer_width = "64") {
        return Err(GroveError::usage("git-grove supports 64-bit Linux only")
            .with_detail("this build targets a platform the layout has never been verified on"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ordinary_version_output() {
        let v = GitVersion::parse(b"git version 2.47.3\n").unwrap();
        assert_eq!((v.major, v.minor, v.patch), (2, 47, 3));
    }

    #[test]
    fn parses_vendor_suffixed_version() {
        let v = GitVersion::parse(b"git version 2.43.0.windows.1\n").unwrap();
        assert_eq!((v.major, v.minor, v.patch), (2, 43, 0));
    }

    #[test]
    fn rejects_unparsable_output() {
        let err = GitVersion::parse(b"not a version").unwrap_err();
        assert_eq!(err.class, crate::error::ExitClass::Failure);
    }

    #[test]
    fn rejects_nonnumeric_patch() {
        let err = GitVersion::parse(b"git version 2.47.not-a-version\n").unwrap_err();
        assert_eq!(err.class, crate::error::ExitClass::Failure);
    }

    #[test]
    fn rejects_invalid_version_prefix() {
        let err = GitVersion::parse(b"anything else 2.47.3\n").unwrap_err();
        assert_eq!(err.class, crate::error::ExitClass::Failure);
    }

    #[test]
    fn parses_measured_gh_version_output() {
        let v = ProviderVersion::parse_gh(
            b"gh version 2.97.0 (2026-07-31)\nhttps://github.com/cli/cli/releases/tag/v2.97.0\n",
        )
        .unwrap();
        assert_eq!((v.major, v.minor, v.patch), (2, 97, 0));
    }

    #[test]
    fn parses_measured_glab_version_output() {
        let v = ProviderVersion::parse_glab(b"glab 1.114.0 (4d7c6cda7)\n").unwrap();
        assert_eq!((v.major, v.minor, v.patch), (1, 114, 0));
    }

    #[test]
    fn rejects_unparsable_gh_output() {
        let err = ProviderVersion::parse_gh(b"not a version").unwrap_err();
        assert_eq!(err.class, crate::error::ExitClass::Failure);
    }

    #[test]
    fn rejects_unparsable_glab_output() {
        let err = ProviderVersion::parse_glab(b"not a version").unwrap_err();
        assert_eq!(err.class, crate::error::ExitClass::Failure);
    }

    #[test]
    fn provider_version_at_least_compares_major_minor_only() {
        let old = ProviderVersion {
            major: 2,
            minor: 96,
            patch: 9,
        };
        let floor = ProviderVersion {
            major: 2,
            minor: 97,
            patch: 0,
        };
        assert!(!old.at_least(MINIMUM_GH.0, MINIMUM_GH.1));
        assert!(floor.at_least(MINIMUM_GH.0, MINIMUM_GH.1));
        assert!(ProviderVersion {
            major: 2,
            minor: 97,
            patch: 99,
        }
        .at_least(MINIMUM_GH.0, MINIMUM_GH.1));
    }

    #[test]
    fn compares_against_the_minimum() {
        let old = GitVersion {
            major: 2,
            minor: 43,
            patch: 0,
        };
        let new = GitVersion {
            major: 2,
            minor: 47,
            patch: 3,
        };
        assert!(!old.at_least(MINIMUM_GIT.0, MINIMUM_GIT.1));
        assert!(new.at_least(MINIMUM_GIT.0, MINIMUM_GIT.1));
        assert!(GitVersion {
            major: 3,
            minor: 0,
            patch: 0,
        }
        .at_least(2, 47));
    }
}
