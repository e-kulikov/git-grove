use crate::error::{GroveError, Result};
use crate::fsx::held::HeldDirectory;
use rustix::fs::{flock, FlockOperation};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LockMode {
    Shared,
    Exclusive,
}

#[derive(Debug)]
pub struct GroveLock {
    directory: HeldDirectory,
    mode: LockMode,
    command: String,
    path: PathBuf,
}

impl GroveLock {
    pub fn acquire_path(path: &Path, mode: LockMode, command: &str) -> Result<Self> {
        let directory = HeldDirectory::open(path)?;
        Self::acquire(directory, mode, command)
    }

    pub fn acquire(directory: HeldDirectory, mode: LockMode, command: &str) -> Result<Self> {
        let operation = match mode {
            LockMode::Shared => FlockOperation::NonBlockingLockShared,
            LockMode::Exclusive => FlockOperation::NonBlockingLockExclusive,
        };
        flock(&directory.file, operation).map_err(|error| {
            GroveError::needs_decision(format!(
                "another git-grove command holds the repository lock at {}",
                directory.path().display()
            ))
            .with_detail(format!("cannot start {command}: {error}"))
        })?;
        let path = directory.path().to_path_buf();
        Ok(Self {
            directory,
            mode,
            command: command.to_owned(),
            path,
        })
    }

    pub fn directory(&self) -> &HeldDirectory {
        &self.directory
    }

    pub fn mode(&self) -> LockMode {
        self.mode
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_shared_succeeds_and_exclusive_contention_refuses() {
        let root = tempfile::tempdir().unwrap();
        let first = GroveLock::acquire_path(root.path(), LockMode::Shared, "first").unwrap();
        let second = GroveLock::acquire_path(root.path(), LockMode::Shared, "second").unwrap();
        assert_eq!(first.mode(), LockMode::Shared);
        drop(second);
        assert!(GroveLock::acquire_path(root.path(), LockMode::Exclusive, "exclusive").is_err());
    }

    #[test]
    fn exclusive_exclusive_refuses() {
        let root = tempfile::tempdir().unwrap();
        let _first = GroveLock::acquire_path(root.path(), LockMode::Exclusive, "first").unwrap();
        assert!(GroveLock::acquire_path(root.path(), LockMode::Exclusive, "second").is_err());
    }
}
