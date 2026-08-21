use crate::error::{GroveError, Result};
#[cfg(feature = "failpoints")]
use std::ffi::OsStr;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailpointMode {
    Error(u64),
    Kill(u64),
    Count,
}

#[derive(Clone, Debug)]
pub struct Checkpoints {
    next: u64,
    mode: Option<FailpointMode>,
    total: u64,
}

impl Checkpoints {
    pub fn disabled() -> Self {
        Self {
            next: 1,
            mode: None,
            total: 0,
        }
    }

    pub fn from_env() -> Result<Self> {
        #[cfg(feature = "failpoints")]
        {
            Self::from_value(std::env::var_os("GIT_GROVE_FAILPOINT").as_deref())
        }
        #[cfg(not(feature = "failpoints"))]
        {
            Ok(Self::disabled())
        }
    }

    #[cfg(feature = "failpoints")]
    fn from_value(value: Option<&OsStr>) -> Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        let mode = match value.map(OsStrExt::as_bytes) {
            None => None,
            Some(b"count") => Some(FailpointMode::Count),
            Some(value) => {
                let separator = value.iter().position(|byte| *byte == b':').ok_or_else(|| {
                    GroveError::usage("GIT_GROVE_FAILPOINT must be error:N, kill:N, or count")
                })?;
                let (kind, number_with_separator) = value.split_at(separator);
                let number = &number_with_separator[1..];
                let number = std::str::from_utf8(number)
                    .ok()
                    .and_then(|number| number.parse::<u64>().ok())
                    .filter(|number| *number > 0)
                    .ok_or_else(|| {
                        GroveError::usage("failpoint checkpoint N must be a positive u64")
                    })?;
                Some(match kind {
                    b"error" => FailpointMode::Error(number),
                    b"kill" => FailpointMode::Kill(number),
                    _ => {
                        return Err(GroveError::usage(
                            "GIT_GROVE_FAILPOINT must be error:N, kill:N, or count",
                        ))
                    }
                })
            }
        };
        Ok(Self {
            next: 1,
            mode,
            total: 0,
        })
    }

    pub fn checkpoint(&mut self) -> Result<()> {
        crate::transaction::signal::check_interrupted()?;
        let checkpoint = self.next;
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| GroveError::failure("failure checkpoint counter overflow"))?;
        self.total = checkpoint;
        match self.mode {
            Some(FailpointMode::Error(target)) if checkpoint == target => {
                Err(GroveError::needs_decision(format!(
                    "injected failure after checkpoint {checkpoint}"
                )))
            }
            Some(FailpointMode::Kill(target)) if checkpoint == target => {
                rustix::process::kill_process(
                    rustix::process::getpid(),
                    rustix::process::Signal::KILL,
                )
                .map_err(|error| {
                    GroveError::failure(format!("cannot inject SIGKILL at checkpoint: {error}"))
                })?;
                unreachable!("SIGKILL cannot return")
            }
            None
            | Some(FailpointMode::Count)
            | Some(FailpointMode::Error(_))
            | Some(FailpointMode::Kill(_)) => Ok(()),
        }
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn is_counting(&self) -> bool {
        self.mode == Some(FailpointMode::Count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_checkpoints_count_without_failing() {
        let mut checkpoints = Checkpoints::disabled();
        checkpoints.checkpoint().unwrap();
        checkpoints.checkpoint().unwrap();
        assert_eq!(checkpoints.total(), 2);
        assert!(!checkpoints.is_counting());
    }

    #[cfg(feature = "failpoints")]
    #[test]
    fn feature_parser_accepts_only_the_documented_grammar() {
        for (value, expected) in [
            ("error:1", FailpointMode::Error(1)),
            ("kill:42", FailpointMode::Kill(42)),
            ("count", FailpointMode::Count),
        ] {
            assert_eq!(
                Checkpoints::from_value(Some(OsStr::new(value)))
                    .unwrap()
                    .mode,
                Some(expected)
            );
        }
        for invalid in [
            "error:0",
            "error:-1",
            "error:18446744073709551616",
            "error",
            "error:1:2",
            "unknown:1",
            "junk",
            "",
        ] {
            assert!(
                Checkpoints::from_value(Some(OsStr::new(invalid))).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[cfg(feature = "failpoints")]
    #[test]
    fn error_mode_fails_immediately_after_the_selected_checkpoint() {
        let mut checkpoints = Checkpoints::from_value(Some(OsStr::new("error:2"))).unwrap();
        checkpoints.checkpoint().unwrap();
        assert!(checkpoints.checkpoint().is_err());
        assert_eq!(checkpoints.total(), 2);
    }

    #[cfg(not(feature = "failpoints"))]
    #[test]
    fn featureless_build_has_no_active_failpoint_mode() {
        assert!(Checkpoints::from_env().unwrap().mode.is_none());
    }
}
