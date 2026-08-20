pub mod inventory;
pub mod preflight;

use crate::error::{GroveError, Result};
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdoptAction {
    Fresh,
    Continue,
    Abort,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdoptArgs {
    pub path: Option<PathBuf>,
    pub remote: Option<OsString>,
    pub default_branch: Option<OsString>,
    pub action: AdoptAction,
}

impl AdoptArgs {
    pub fn fresh(path: Option<PathBuf>) -> Self {
        Self {
            path,
            remote: None,
            default_branch: None,
            action: AdoptAction::Fresh,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.action != AdoptAction::Fresh
            && (self.remote.is_some() || self.default_branch.is_some())
        {
            return Err(GroveError::usage(
                "--continue/--abort cannot be combined with --remote or --default-branch",
            ));
        }
        for (description, value) in [
            ("remote name", self.remote.as_deref()),
            ("default branch", self.default_branch.as_deref()),
        ] {
            if value.is_some_and(|value| value.as_bytes().starts_with(b"-")) {
                return Err(GroveError::usage(format!(
                    "the {description} must not begin with '-'"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_arguments_are_mutually_exclusive_with_fresh_decisions() {
        let mut args = AdoptArgs::fresh(None);
        args.action = AdoptAction::Continue;
        args.remote = Some(OsString::from("origin"));
        assert!(args.validate().is_err());
        args.remote = None;
        args.default_branch = Some(OsString::from("main"));
        assert!(args.validate().is_err());
    }

    #[test]
    fn option_shaped_remote_and_branch_names_are_rejected_before_git() {
        for remote in [true, false] {
            let mut args = AdoptArgs::fresh(None);
            if remote {
                args.remote = Some(OsString::from("-remote"));
            } else {
                args.default_branch = Some(OsString::from("-branch"));
            }
            assert!(args.validate().is_err());
        }
    }
}
