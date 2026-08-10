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
        let rest = text
            .split_whitespace()
            .nth(2)
            .ok_or_else(|| GroveError::failure("cannot parse `git --version` output"))?;
        let mut parts = rest.split('.');
        let mut number = |name: &str| -> Result<u32> {
            parts.next().and_then(|p| p.parse().ok()).ok_or_else(|| {
                GroveError::failure(format!("cannot parse the {name} of `git --version`"))
            })
        };
        let major = number("major")?;
        let minor = number("minor")?;
        let patch = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
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
