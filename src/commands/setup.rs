use crate::error::{GroveError, Result};
use crate::hooks::config;
use rustix::fs::{openat, Mode, OFlags, CWD};
use serde_json::Value;
use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};

/// Which agent `setup --agent <x>` was asked to configure. `Claude` and
/// `Copilot` converge on the same target — see `Target`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
    Copilot,
}

/// The one native hook-config file an [`Agent`] writes, and the exact
/// marker `hooks::config::merge` uses to recognize a group this tool
/// already owns there.
pub struct Target {
    pub relative_dir: &'static str,
    pub relative_file: &'static str,
    pub marker_key: &'static str,
    pub marker_value: &'static str,
}

impl Target {
    pub fn relative_path(&self) -> PathBuf {
        Path::new(self.relative_dir).join(self.relative_file)
    }

    /// The anchored line `setup` idempotently adds to the grove's common
    /// `.bare/info/exclude` (plan's binding correction: anchored to the
    /// worktree root, since a linked worktree has no effective private
    /// `info/exclude` of its own — every worktree shares one).
    pub fn exclude_entry(&self) -> String {
        format!("/{}", self.relative_path().display())
    }
}

impl Agent {
    pub fn target(self) -> Target {
        match self {
            Agent::Claude | Agent::Copilot => Target {
                relative_dir: ".claude",
                relative_file: "settings.local.json",
                marker_key: config::CLAUDE_COMPATIBLE_MARKER_KEY,
                marker_value: config::CLAUDE_COMPATIBLE_MARKER_VALUE,
            },
            Agent::Codex => Target {
                relative_dir: ".codex",
                relative_file: "hooks.json",
                marker_key: config::CODEX_MARKER_KEY,
                marker_value: config::CODEX_MARKER_VALUE,
            },
        }
    }

    /// The canonical `PreToolUse` hook group this agent's target file
    /// carries. `executable` is the canonicalized absolute path to the
    /// current `git-grove` binary.
    pub fn group(self, executable: &str) -> Value {
        match self {
            Agent::Claude | Agent::Copilot => config::claude_compatible_group(executable),
            Agent::Codex => config::codex_group(executable),
        }
    }
}

/// Safely open a [`Target`]'s parent directory inside `worktree_root`,
/// never following a symlink: not the parent directory itself, and not the
/// leaf file, if either already exists. Creates the parent directory when
/// it is absent. Refuses — before any bytes are written — a symlinked
/// parent, a symlinked leaf, a non-directory occupying the parent's name,
/// or a non-regular existing leaf.
///
/// The returned directory handle is what makes the parent check race-safe,
/// not merely advisory: every subsequent write goes through
/// `fsx::write_atomic_in` against *this* open handle, so nothing re-opens
/// the parent by name after this check — an attacker who replaces the
/// directory with a symlink after this call returns gains nothing, because
/// the write path never looks the name up again. The leaf check has no
/// equivalent race to close: `write_atomic_in`'s final `renameat` replaces
/// whatever directory entry currently holds that name without ever
/// following it, symlink or not, so this check is a user-data refusal
/// (don't silently blow away a deliberately symlinked file), not a second
/// security boundary.
pub fn open_target_directory(worktree_root: &Path, target: &Target) -> Result<File> {
    let dir_path = worktree_root.join(target.relative_dir);
    match std::fs::symlink_metadata(&dir_path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(GroveError::needs_decision(format!(
                "{} exists and is not a plain directory; refusing to write the hook config there",
                dir_path.display()
            )))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(&dir_path).map_err(|error| {
                GroveError::failure(format!("cannot create {}: {error}", dir_path.display()))
            })?;
        }
        Err(error) => {
            return Err(GroveError::failure(format!(
                "cannot inspect {}: {error}",
                dir_path.display()
            )))
        }
    }

    let directory = openat(
        CWD,
        &dir_path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| {
        GroveError::needs_decision(format!(
            "{} is not a plain directory: {error}",
            dir_path.display()
        ))
    })?;

    let leaf_path = dir_path.join(target.relative_file);
    match std::fs::symlink_metadata(&leaf_path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(GroveError::needs_decision(format!(
                "{} exists and is not a plain file; refusing to overwrite it",
                leaf_path.display()
            )))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(GroveError::failure(format!(
                "cannot inspect {}: {error}",
                leaf_path.display()
            )))
        }
    }

    Ok(directory)
}

/// Read a [`Target`]'s current bytes from an already-open, already-validated
/// directory handle, or `Vec::new()` if the leaf is absent. Uses the same
/// handle `open_target_directory` returned, so this does not re-open the
/// parent by name either.
pub fn read_existing(directory: &File, target: &Target) -> Result<Vec<u8>> {
    use std::io::Read;
    match openat(
        directory,
        target.relative_file,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(file) => {
            let mut file = File::from(file);
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).map_err(|error| {
                GroveError::failure(format!(
                    "cannot read {}: {error}",
                    target.relative_path().display()
                ))
            })?;
            Ok(bytes)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(GroveError::failure(format!(
            "cannot open {}: {error}",
            target.relative_path().display()
        ))),
    }
}

/// Validate, merge, and atomically write one agent's hook config inside
/// `worktree_root`. Pure orchestration of the pieces above: no grove
/// discovery, locking, or tracked-path check — those are the calling
/// command's job (`setup --agent`, wired in a later task). A merge failure
/// (malformed JSON, wrong-typed `hooks`/`hooks.PreToolUse`) and a
/// no-follow refusal both return before `write_atomic_in` is reached, so
/// neither writes any bytes.
pub fn write_hook_config(worktree_root: &Path, agent: Agent, executable: &str) -> Result<()> {
    let target = agent.target();
    let directory = open_target_directory(worktree_root, &target)?;
    let existing = read_existing(&directory, &target)?;
    let merged = config::merge(
        &existing,
        target.marker_key,
        target.marker_value,
        agent.group(executable),
    )
    .map_err(|reason| {
        GroveError::needs_decision(format!(
            "{} {reason}; refusing to merge",
            target.relative_path().display()
        ))
    })?;
    let bytes = config::render(&merged);

    let parent_path = worktree_root.join(target.relative_dir);
    let full_path = worktree_root.join(target.relative_path());
    crate::fsx::write_atomic_in(
        &directory,
        &parent_path,
        &full_path,
        OsStr::new(target.relative_file),
        &bytes,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_and_copilot_converge_on_the_same_target() {
        assert_eq!(
            Agent::Claude.target().relative_path(),
            Agent::Copilot.target().relative_path()
        );
        assert_eq!(
            Agent::Claude.target().relative_path(),
            Path::new(".claude/settings.local.json")
        );
        assert_eq!(
            Agent::Codex.target().relative_path(),
            Path::new(".codex/hooks.json")
        );
    }

    #[test]
    fn exclude_entries_are_anchored_to_the_worktree_root() {
        assert_eq!(
            Agent::Claude.target().exclude_entry(),
            "/.claude/settings.local.json"
        );
        assert_eq!(Agent::Codex.target().exclude_entry(), "/.codex/hooks.json");
    }

    #[test]
    fn opens_and_creates_a_missing_target_directory() {
        let root = tempfile::tempdir().unwrap();
        let target = Agent::Claude.target();
        open_target_directory(root.path(), &target).unwrap();
        assert!(root.path().join(".claude").is_dir());
    }

    #[test]
    fn refuses_a_symlinked_parent_directory() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("elsewhere");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, root.path().join(".claude")).unwrap();
        let target = Agent::Claude.target();
        assert!(open_target_directory(root.path(), &target).is_err());
    }

    #[test]
    fn refuses_a_non_directory_occupying_the_parent_name() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".codex"), b"not a directory").unwrap();
        let target = Agent::Codex.target();
        assert!(open_target_directory(root.path(), &target).is_err());
    }

    #[test]
    fn refuses_a_symlinked_leaf() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".claude")).unwrap();
        std::fs::write(root.path().join("elsewhere.json"), b"{}").unwrap();
        std::os::unix::fs::symlink(
            root.path().join("elsewhere.json"),
            root.path().join(".claude/settings.local.json"),
        )
        .unwrap();
        let target = Agent::Claude.target();
        assert!(open_target_directory(root.path(), &target).is_err());
    }

    #[test]
    fn refuses_a_non_regular_existing_leaf() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".claude/settings.local.json")).unwrap();
        let target = Agent::Claude.target();
        assert!(open_target_directory(root.path(), &target).is_err());
    }

    #[test]
    fn reads_absent_target_as_empty_and_existing_target_verbatim() {
        let root = tempfile::tempdir().unwrap();
        let target = Agent::Claude.target();
        let directory = open_target_directory(root.path(), &target).unwrap();
        assert_eq!(
            read_existing(&directory, &target).unwrap(),
            Vec::<u8>::new()
        );

        std::fs::write(
            root.path().join(".claude/settings.local.json"),
            b"{\"x\":1}",
        )
        .unwrap();
        assert_eq!(
            read_existing(&directory, &target).unwrap(),
            b"{\"x\":1}".to_vec()
        );
    }

    #[test]
    fn write_hook_config_creates_the_directory_and_file_from_nothing() {
        let root = tempfile::tempdir().unwrap();
        write_hook_config(root.path(), Agent::Claude, "/abs/git-grove").unwrap();
        let bytes = std::fs::read(root.path().join(".claude/settings.local.json")).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value["hooks"]["PreToolUse"][0]["description"],
            config::CLAUDE_COMPATIBLE_MARKER_VALUE
        );
        assert_eq!(
            value["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "/abs/git-grove hook-guard --protocol claude-compatible PreToolUse"
        );
    }

    #[test]
    fn write_hook_config_is_idempotent_and_claude_then_copilot_converge() {
        let root = tempfile::tempdir().unwrap();
        write_hook_config(root.path(), Agent::Claude, "/abs/git-grove").unwrap();
        let after_claude = std::fs::read(root.path().join(".claude/settings.local.json")).unwrap();
        write_hook_config(root.path(), Agent::Copilot, "/abs/git-grove").unwrap();
        let after_copilot = std::fs::read(root.path().join(".claude/settings.local.json")).unwrap();
        assert_eq!(after_claude, after_copilot);
        write_hook_config(root.path(), Agent::Claude, "/abs/git-grove").unwrap();
        let rerun = std::fs::read(root.path().join(".claude/settings.local.json")).unwrap();
        assert_eq!(rerun, after_copilot);
    }

    #[test]
    fn write_hook_config_preserves_unrelated_settings() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".claude")).unwrap();
        std::fs::write(
            root.path().join(".claude/settings.local.json"),
            br#"{"someOtherSetting": true, "hooks": {"SessionStart": [{"unrelated": true}]}}"#,
        )
        .unwrap();
        write_hook_config(root.path(), Agent::Claude, "/abs/git-grove").unwrap();
        let bytes = std::fs::read(root.path().join(".claude/settings.local.json")).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["someOtherSetting"], true);
        assert_eq!(value["hooks"]["SessionStart"][0]["unrelated"], true);
    }

    #[test]
    fn write_hook_config_leaves_a_malformed_existing_file_untouched() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".claude")).unwrap();
        let path = root.path().join(".claude/settings.local.json");
        std::fs::write(&path, b"not json").unwrap();
        assert!(write_hook_config(root.path(), Agent::Claude, "/abs/git-grove").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"not json");
    }

    #[test]
    fn write_hook_config_leaves_a_symlinked_leaf_untouched() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".codex")).unwrap();
        std::fs::write(root.path().join("elsewhere.json"), b"{}").unwrap();
        std::os::unix::fs::symlink(
            root.path().join("elsewhere.json"),
            root.path().join(".codex/hooks.json"),
        )
        .unwrap();
        assert!(write_hook_config(root.path(), Agent::Codex, "/abs/git-grove").is_err());
        assert_eq!(
            std::fs::read_link(root.path().join(".codex/hooks.json")).unwrap(),
            root.path().join("elsewhere.json")
        );
    }
}
