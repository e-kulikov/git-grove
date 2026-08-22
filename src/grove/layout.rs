use crate::error::{GroveError, Result};
use crate::fsx;
use crate::fsx::held::open_directory_at;
use bstr::ByteSlice;
use rustix::fs::{fstat, mkdirat, openat, statat, AtFlags, Mode, OFlags, CWD};
use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

pub const POINTER_CONTENTS: &str = "gitdir: ./.bare\n";
pub const RESERVED: &[&str] = &[".bare", ".git", "AGENTS.md", "CLAUDE.md"];

fn escaped_path(path: &Path) -> String {
    path.as_os_str().as_bytes().escape_bytes().to_string()
}

pub fn write_pointer(root: &Path) -> Result<()> {
    fsx::write_atomic(&root.join(".git"), POINTER_CONTENTS.as_bytes())
}

pub fn write_pointer_if_absent(root: &Path) -> Result<bool> {
    fsx::write_atomic_if_absent(&root.join(".git"), POINTER_CONTENTS.as_bytes())
}

/// A lexically contained, vacant worktree path tied to an open grove root.
///
/// Call [`Self::validate_vacant`] immediately before passing [`Self::path`] to
/// a mutating Git command.
#[derive(Debug)]
pub struct ValidatedWorktreePath {
    root_directory: File,
    root_path: PathBuf,
    relative: PathBuf,
}

impl ValidatedWorktreePath {
    pub fn path(&self) -> PathBuf {
        self.root_path.join(&self.relative)
    }

    pub fn relative(&self) -> &Path {
        &self.relative
    }

    /// Repeat the descriptor-relative symlink and vacancy checks.
    pub fn validate_vacant(&self) -> Result<()> {
        self.validate_vacant_with_post_walk(|| {})
    }

    fn validate_vacant_with_post_walk(&self, post_walk: impl FnOnce()) -> Result<()> {
        validate_root_identity(&self.root_directory, &self.root_path)?;
        validate_vacant_from(&self.root_directory, &self.path(), &self.relative)?;
        post_walk();
        validate_root_identity(&self.root_directory, &self.root_path)
    }

    /// Create only missing parent directories, relative to the held grove
    /// descriptor, and finish by repeating the vacancy check.
    pub fn create_parent_directories(&self) -> Result<()> {
        validate_root_identity(&self.root_directory, &self.root_path)?;
        let mut current = self.root_directory.try_clone().map_err(|error| {
            GroveError::failure(format!(
                "cannot duplicate grove directory descriptor: {error}"
            ))
        })?;
        let mut components = self.relative.iter().peekable();
        while let Some(component) = components.next() {
            if components.peek().is_none() {
                break;
            }
            current = match open_directory_at(&current, component) {
                Ok(directory) => directory,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match mkdirat(&current, component, Mode::from_raw_mode(0o755)) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(error) => {
                            return Err(GroveError::failure(format!(
                                "cannot create a worktree parent under {}: {error}",
                                escaped_path(&self.root_path)
                            )))
                        }
                    }
                    open_directory_at(&current, component).map_err(|error| {
                        GroveError::usage(format!(
                            "{} has a non-directory or symlinked ancestor: {error}",
                            escaped_path(&self.path())
                        ))
                    })?
                }
                Err(error) => {
                    return Err(GroveError::usage(format!(
                        "{} has a non-directory or symlinked ancestor: {error}",
                        escaped_path(&self.path())
                    )))
                }
            };
        }
        self.validate_vacant()
    }
}

/// Refuse a candidate path that is inside, or equal to, `root` — the inverse
/// of [`contained_worktree_path`], which validates the *opposite* direction.
/// Both paths are canonicalised first, so a relative or symlinked candidate
/// cannot slip past a lexical comparison.
pub fn assert_outside_grove(root: &Path, candidate: &Path) -> Result<()> {
    let root = root.canonicalize().map_err(|error| {
        GroveError::failure(format!(
            "cannot canonicalise grove root {}: {error}",
            escaped_path(root)
        ))
    })?;
    let candidate_canonical = candidate.canonicalize().map_err(|error| {
        GroveError::failure(format!(
            "cannot canonicalise {}: {error}",
            escaped_path(candidate)
        ))
    })?;
    if candidate_canonical == root || candidate_canonical.starts_with(&root) {
        return Err(GroveError::failure(format!(
            "{} must be outside the grove root {}",
            escaped_path(candidate),
            escaped_path(&root)
        )));
    }
    Ok(())
}

pub fn contained_worktree_path(root: &Path, requested: &Path) -> Result<PathBuf> {
    Ok(validate_worktree_path(root, requested)?.path())
}

/// Validate a worktree path and retain the grove directory descriptor so later
/// callers can safely create parents and perform a final pre-mutation check.
pub fn validate_worktree_path(root: &Path, requested: &Path) -> Result<ValidatedWorktreePath> {
    let relative = contained_relative_path(root, requested)?;
    let root_directory = openat(
        CWD,
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| {
        GroveError::usage(format!(
            "cannot open grove root {} without following symlinks: {error}",
            escaped_path(root)
        ))
    })?;
    let validated = ValidatedWorktreePath {
        root_directory,
        root_path: root.to_path_buf(),
        relative,
    };
    validated.validate_vacant()?;
    Ok(validated)
}

/// Validate a worktree path using a caller-held grove root descriptor.
pub fn validate_worktree_path_at(
    root_directory: &File,
    root: &Path,
    requested: &Path,
) -> Result<ValidatedWorktreePath> {
    let relative = contained_relative_path(root, requested)?;
    let validated = ValidatedWorktreePath {
        root_directory: root_directory.try_clone().map_err(|error| {
            GroveError::failure(format!(
                "cannot duplicate grove directory descriptor: {error}"
            ))
        })?,
        root_path: root.to_path_buf(),
        relative,
    };
    validated.validate_vacant()?;
    Ok(validated)
}

fn contained_relative_path(root: &Path, requested: &Path) -> Result<PathBuf> {
    let relative = if requested.is_absolute() {
        requested.strip_prefix(root).map_err(|_| {
            GroveError::usage(format!("{} is outside the grove", escaped_path(requested)))
                .with_detail("worktrees must live under the grove root")
        })?
    } else {
        requested
    };

    validate_relative_worktree_path(relative)
}

/// Validate path policy before the grove root exists.
pub fn validate_relative_worktree_path(requested: &Path) -> Result<PathBuf> {
    if requested.is_absolute() {
        return Err(GroveError::usage(format!(
            "{} must be relative to the grove root",
            escaped_path(requested)
        )));
    }

    let mut cleaned = PathBuf::new();
    for component in requested.components() {
        match component {
            Component::Normal(part) => {
                if RESERVED.iter().any(|reserved| part == OsStr::new(reserved)) {
                    return Err(GroveError::usage(format!(
                        "{} is reserved by the grove layout",
                        escaped_path(Path::new(part))
                    )));
                }
                if part.as_bytes().starts_with(b".grove-adopt-") {
                    return Err(GroveError::usage(
                        "that name is reserved for adoption transactions",
                    ));
                }
                cleaned.push(part);
            }
            _ => {
                return Err(GroveError::usage(format!(
                    "{} must be a plain path inside the grove",
                    escaped_path(requested)
                )))
            }
        }
    }
    if cleaned.as_os_str().is_empty() {
        return Err(GroveError::usage(
            "the worktree path must be a strict descendant of the grove root",
        ));
    }

    Ok(cleaned)
}

fn validate_vacant_from(root: &File, absolute: &Path, relative: &Path) -> Result<()> {
    let mut current = root.try_clone().map_err(|error| {
        GroveError::failure(format!(
            "cannot duplicate grove directory descriptor: {error}"
        ))
    })?;
    let mut components = relative.iter().peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            return match statat(&current, component, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(_) => Err(GroveError::needs_decision(format!(
                    "{} already exists",
                    escaped_path(absolute)
                ))
                .with_detail(
                    "pass an explicit directory argument to place the worktree elsewhere",
                )),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(GroveError::failure(format!(
                    "cannot inspect {}: {error}",
                    escaped_path(absolute)
                ))),
            };
        }

        current = match open_directory_at(&current, component) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(GroveError::usage(format!(
                    "{} has a non-directory or symlinked ancestor: {error}",
                    escaped_path(absolute)
                )))
            }
        };
    }
    unreachable!("contained paths always have a final component")
}

fn validate_root_identity(root: &File, root_path: &Path) -> Result<()> {
    let held = fstat(root).map_err(|error| {
        GroveError::failure(format!("cannot inspect held grove directory: {error}"))
    })?;
    let named = statat(CWD, root_path, AtFlags::SYMLINK_NOFOLLOW).map_err(|error| {
        GroveError::usage(format!(
            "grove root {} changed while preparing the worktree: {error}",
            escaped_path(root_path)
        ))
    })?;
    if held.st_dev != named.st_dev || held.st_ino != named.st_ino {
        return Err(GroveError::usage(format!(
            "grove root {} changed while preparing the worktree",
            escaped_path(root_path)
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    fn root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".bare")).unwrap();
        dir
    }

    #[test]
    fn assert_outside_grove_accepts_a_sibling_directory() {
        let dir = root();
        let sibling = tempfile::tempdir().unwrap();

        assert_outside_grove(dir.path(), sibling.path()).unwrap();
    }

    #[test]
    fn assert_outside_grove_rejects_the_root_itself_and_a_descendant() {
        let dir = root();

        assert_eq!(
            assert_outside_grove(dir.path(), dir.path())
                .unwrap_err()
                .class,
            crate::error::ExitClass::Failure
        );
        assert_eq!(
            assert_outside_grove(dir.path(), &dir.path().join(".bare"))
                .unwrap_err()
                .class,
            crate::error::ExitClass::Failure
        );
    }

    #[test]
    fn accepts_a_plain_branch_directory() {
        let dir = root();
        let path = contained_worktree_path(dir.path(), Path::new("main")).unwrap();
        assert_eq!(path, dir.path().join("main"));
    }

    #[test]
    fn accepts_nested_absolute_and_non_utf8_descendants() {
        let dir = root();
        let nested = contained_worktree_path(dir.path(), Path::new("release/1.0")).unwrap();
        assert_eq!(nested, dir.path().join("release/1.0"));

        let absolute = dir.path().join("feature/main");
        assert_eq!(
            contained_worktree_path(dir.path(), &absolute).unwrap(),
            absolute
        );

        let bytes = std::ffi::OsString::from_vec(vec![b'b', 0xff, b'r']);
        let requested = PathBuf::from(bytes);
        let accepted = contained_worktree_path(dir.path(), &requested).unwrap();
        assert_eq!(
            accepted.as_os_str().as_bytes(),
            dir.path().join(requested).as_os_str().as_bytes()
        );
    }

    #[test]
    fn writes_the_exact_grove_pointer() {
        let dir = tempfile::tempdir().unwrap();
        write_pointer(dir.path()).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join(".git")).unwrap(),
            POINTER_CONTENTS.as_bytes()
        );
    }

    #[test]
    fn pointer_creation_never_replaces_a_concurrent_entry() {
        let dir = tempfile::tempdir().unwrap();
        let pointer = dir.path().join(".git");
        std::fs::write(&pointer, b"foreign").unwrap();

        assert!(!write_pointer_if_absent(dir.path()).unwrap());
        assert_eq!(std::fs::read(pointer).unwrap(), b"foreign");
    }

    #[test]
    fn rejects_paths_that_are_not_strict_descendants() {
        let dir = root();
        let root = dir.path();
        let outside = root.parent().unwrap().join("outside");
        for bad in [
            PathBuf::from(""),
            PathBuf::from("."),
            PathBuf::from(".."),
            PathBuf::from("../outside"),
            PathBuf::from("main/../.."),
            root.to_path_buf(),
            outside,
        ] {
            let err = contained_worktree_path(root, &bad).unwrap_err();
            assert_eq!(
                err.class,
                crate::error::ExitClass::Usage,
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn rejects_layout_and_transaction_names_in_any_component() {
        let dir = root();
        for bad in [
            ".bare",
            ".git",
            "AGENTS.md",
            "CLAUDE.md",
            ".grove-adopt-main",
            "nested/.git/worktree",
            "nested/.grove-adopt-release/worktree",
        ] {
            let err = contained_worktree_path(dir.path(), Path::new(bad)).unwrap_err();
            assert_eq!(err.class, crate::error::ExitClass::Usage, "accepted {bad}");
        }
    }

    #[test]
    fn rejects_any_occupied_final_entry_including_a_broken_symlink() {
        for kind in ["directory", "file", "broken-symlink"] {
            let dir = root();
            let final_path = dir.path().join("main");
            match kind {
                "directory" => std::fs::create_dir(&final_path).unwrap(),
                "file" => std::fs::write(&final_path, b"occupied").unwrap(),
                "broken-symlink" => std::os::unix::fs::symlink("missing", &final_path).unwrap(),
                _ => unreachable!(),
            }

            let err = contained_worktree_path(dir.path(), Path::new("main")).unwrap_err();
            assert_eq!(
                err.class,
                crate::error::ExitClass::NeedsDecision,
                "accepted {kind}"
            );
            assert!(err.message.contains("main"));
        }
    }

    #[test]
    fn occupied_non_utf8_worktree_paths_are_escaped_reversibly() {
        let dir = root();
        let relative = PathBuf::from(std::ffi::OsString::from_vec(b"topic-\xff".to_vec()));
        std::fs::write(dir.path().join(&relative), b"foreign").unwrap();

        let error = validate_worktree_path(dir.path(), &relative).unwrap_err();

        assert!(error.message.contains(r"topic-\xFF"), "{error}");
    }

    #[test]
    fn rejects_a_symlinked_or_non_directory_ancestor() {
        let dir = root();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::os::unix::fs::symlink("real", dir.path().join("link")).unwrap();
        let err = contained_worktree_path(dir.path(), Path::new("link/wt")).unwrap_err();
        assert_eq!(err.class, crate::error::ExitClass::Usage);

        std::fs::write(dir.path().join("file"), b"not a directory").unwrap();
        let err = contained_worktree_path(dir.path(), Path::new("file/wt")).unwrap_err();
        assert_eq!(err.class, crate::error::ExitClass::Usage);
    }

    #[test]
    fn final_validation_rejects_a_replaced_grove_root() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("grove");
        std::fs::create_dir(&root).unwrap();
        let validated = validate_worktree_path(&root, Path::new("main")).unwrap();

        std::fs::rename(&root, base.path().join("moved")).unwrap();
        std::fs::create_dir(&root).unwrap();

        let err = validated.validate_vacant().unwrap_err();
        assert_eq!(err.class, crate::error::ExitClass::Usage);
    }

    #[test]
    fn held_root_validation_never_creates_parents_in_a_replacement_root() {
        let parent = tempfile::tempdir().unwrap();
        let root_path = parent.path().join("grove");
        let moved_path = parent.path().join("held-grove");
        std::fs::create_dir(&root_path).unwrap();
        let root_directory = openat(
            CWD,
            &root_path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map(File::from)
        .unwrap();
        let worktree =
            validate_worktree_path_at(&root_directory, &root_path, Path::new("topic/branch"))
                .unwrap();

        std::fs::rename(&root_path, &moved_path).unwrap();
        std::fs::create_dir(&root_path).unwrap();

        let error = worktree.create_parent_directories().unwrap_err();
        assert_eq!(error.class, crate::error::ExitClass::Usage);
        assert!(!root_path.join("topic").exists());
        assert!(!moved_path.join("topic").exists());
    }

    #[test]
    fn final_validation_rechecks_root_identity_after_descendant_walk() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("grove");
        let replacement = base.path().join("replacement");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        let validated = validate_worktree_path(&root, Path::new("main")).unwrap();

        let err = validated
            .validate_vacant_with_post_walk(|| {
                std::fs::rename(&root, base.path().join("moved")).unwrap();
                std::fs::rename(&replacement, &root).unwrap();
            })
            .unwrap_err();

        assert_eq!(err.class, crate::error::ExitClass::Usage);
    }

    #[test]
    fn creates_missing_parents_without_following_a_replacement_symlink() {
        let dir = root();
        let validated = validate_worktree_path(dir.path(), Path::new("release/1.0/main")).unwrap();
        validated.create_parent_directories().unwrap();
        assert!(dir.path().join("release/1.0").is_dir());
        assert!(!dir.path().join("release/1.0/main").exists());

        let attacked = root();
        let outside = tempfile::tempdir().unwrap();
        let validated = validate_worktree_path(attacked.path(), Path::new("release/main")).unwrap();
        std::os::unix::fs::symlink(outside.path(), attacked.path().join("release")).unwrap();
        let err = validated.create_parent_directories().unwrap_err();
        assert_eq!(err.class, crate::error::ExitClass::Usage);
        assert!(!outside.path().join("main").exists());
    }
}
