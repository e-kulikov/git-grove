use crate::error::{GroveError, Result};
use crate::git::config as git_config;
use crate::git::query;
use crate::git::runner::GitRunner;
use crate::grove::discover::Grove;
use crate::grove::layout;
use crate::grove::metadata::{self, Metadata};
use crate::hooks::config::{self, Format, HookGroup};
use clap::ValueEnum;
use rustix::fs::{openat, Mode, OFlags, CWD};
use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};

/// Which agent `setup --agent <x>` was asked to configure. `Claude` and
/// `Copilot` converge on the same target — see `Target`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
    Copilot,
}

/// The one native hook-config file an [`Agent`] writes, and the
/// serialization that file uses. The marker that decides which group inside
/// it this tool owns travels with the group itself — see
/// [`config::HookGroup`].
pub struct Target {
    pub relative_dir: &'static str,
    pub relative_file: &'static str,
    pub format: Format,
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
    /// Every agent, in the order `setup --agent` lists them — the order a
    /// grove's recorded policy is provisioned in.
    pub const ALL: [Agent; 3] = [Agent::Claude, Agent::Codex, Agent::Copilot];

    /// The exact `--agent` spelling, which is also the exact value stored in
    /// the grove's [`POLICY_KEY`] record.
    pub fn as_str(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Copilot => "copilot",
        }
    }

    /// Parse one recorded policy value. Returns `None` for anything this
    /// binary does not recognize — a newer git-grove's agent name, or a
    /// typo — which the reader reports rather than treating as fatal.
    pub fn parse(value: &[u8]) -> Option<Agent> {
        Agent::ALL
            .into_iter()
            .find(|agent| value == agent.as_str().as_bytes())
    }

    pub fn target(self) -> Target {
        match self {
            Agent::Claude | Agent::Copilot => Target {
                relative_dir: ".claude",
                relative_file: "settings.local.json",
                format: Format::Json,
            },
            // One file, in the worktree, with the group defined inline as
            // TOML: the shape measured working against installed Codex.
            // Codex needs no separate hooks file.
            Agent::Codex => Target {
                relative_dir: ".codex",
                relative_file: "config.toml",
                format: Format::Toml,
            },
        }
    }

    /// The canonical `PreToolUse` hook group this agent's target file
    /// carries. `executable` is the canonicalized absolute path to the
    /// current `git-grove` binary.
    pub fn group(self, executable: &str) -> HookGroup {
        match self {
            Agent::Claude | Agent::Copilot => HookGroup::claude_compatible(executable),
            Agent::Codex => HookGroup::codex(executable),
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
/// command's job. A merge failure (malformed JSON or TOML, a wrong-typed
/// `hooks`/`hooks.PreToolUse`) and a no-follow refusal both return before
/// `write_atomic_in` is reached, so neither writes any bytes.
pub fn write_hook_config(worktree_root: &Path, agent: Agent, executable: &str) -> Result<()> {
    let target = agent.target();
    let directory = open_target_directory(worktree_root, &target)?;
    let existing = read_existing(&directory, &target)?;
    let bytes = merged_bytes(&existing, &target, agent, executable)?;

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

fn merged_bytes(
    existing: &[u8],
    target: &Target,
    agent: Agent,
    executable: &str,
) -> Result<Vec<u8>> {
    config::merged_bytes(existing, target.format, &agent.group(executable)).map_err(|reason| {
        GroveError::needs_decision(format!(
            "{} {reason}; refusing to merge",
            target.relative_path().display()
        ))
    })
}

/// The multi-valued key in the grove's own `.bare/config` recording which
/// agents this grove's worktrees are provisioned for. `setup --agent <x>`
/// adds `<x>` to it; `git grove add` reads it and provisions a new worktree
/// for each recorded agent. Absent — the case for every grove whose owner
/// never ran `setup` — means `add` behaves exactly as it always did, so
/// this is opt-in and nothing about it is retroactive.
///
/// Remove the record by hand with
/// `git config --file <grove>/.bare/config --unset-all grove.hookAgent`.
pub const POLICY_KEY: &str = "grove.hookAgent";

/// The agents a grove's recorded policy names, plus every recorded value
/// this binary does not recognize. Unrecognized values are returned rather
/// than refused: a newer git-grove may record an agent this one has never
/// heard of, and that must not stop it from provisioning the ones it does
/// know. Duplicates are collapsed, and the result follows [`Agent::ALL`]
/// order rather than the record's, so provisioning is deterministic.
pub fn read_policy(runner: &dyn GitRunner, grove: &Grove) -> Result<(Vec<Agent>, Vec<String>)> {
    let config_path = metadata::config_path(grove);
    let values = git_config::config_values(runner, &config_path, OsStr::new(POLICY_KEY))?;

    let mut unknown = Vec::new();
    let mut recognized = Vec::new();
    for value in &values {
        match Agent::parse(value) {
            Some(agent) => {
                if !recognized.contains(&agent) {
                    recognized.push(agent);
                }
            }
            None => {
                let rendered = String::from_utf8_lossy(value).into_owned();
                if !unknown.contains(&rendered) {
                    unknown.push(rendered);
                }
            }
        }
    }
    let agents = Agent::ALL
        .into_iter()
        .filter(|agent| recognized.contains(agent))
        .collect();
    Ok((agents, unknown))
}

/// Idempotently record `agent` in the grove's policy, so future
/// `git grove add` runs provision it. Reads the current values first and
/// writes nothing when `agent` is already recorded, so repeated `setup`
/// runs converge instead of growing the record.
pub fn register_policy(runner: &dyn GitRunner, grove: &Grove, agent: Agent) -> Result<()> {
    let config_path = metadata::config_path(grove);
    let values = git_config::config_values(runner, &config_path, OsStr::new(POLICY_KEY))?;
    if values
        .iter()
        .any(|value| value == agent.as_str().as_bytes())
    {
        return Ok(());
    }
    git_config::set_config(
        runner,
        &config_path,
        OsStr::new(POLICY_KEY),
        OsStr::new(agent.as_str()),
        true,
    )
}

/// Compare two worktree paths through the filesystem where possible, so a
/// symlinked route to a worktree still matches the path Git registered.
/// Falls back to the path as given when it cannot be canonicalized, which
/// is the right answer for a record whose directory has been removed.
fn same_worktree(registered: &Path, candidate: &Path) -> bool {
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    registered == candidate || canonical(registered) == canonical(candidate)
}

/// Resolve which worktree `setup --agent` operates on.
///
/// `requested` is an explicit `--worktree <name>`: a path relative to the
/// grove root, exactly as `git grove add <branch>` derives one (so
/// `feature/auth` names the `feature/auth` directory), which must already
/// be a registered worktree of this grove. With no `--worktree`, a `cwd`
/// inside one of the grove's worktrees selects that worktree, and a `cwd`
/// that is in no working tree at all — the grove root, or any intermediate
/// directory under it, all of which walk up to the bare repository —
/// selects the worktree holding the grove's own recorded default branch.
///
/// The two failure kinds are deliberately different exit classes: a
/// `--worktree` value that names no worktree of this grove is a malformed
/// argument (usage, 64), while a grove that records no default branch, or
/// records one no worktree has checked out, is repository state the user
/// has to decide about (needs-decision, 2).
pub fn resolve_worktree(
    runner: &dyn GitRunner,
    grove: &Grove,
    metadata: &Metadata,
    requested: Option<&Path>,
    cwd: &Path,
) -> Result<PathBuf> {
    let checkouts: Vec<_> = query::worktrees(runner, grove)?
        .into_iter()
        .filter(|record| !record.bare)
        .collect();
    let list_hint = "`git grove list` shows every worktree of this grove";

    if let Some(requested) = requested {
        let relative = layout::validate_relative_worktree_path(requested)?;
        let path = grove.root.join(&relative);
        return checkouts
            .iter()
            .find(|record| same_worktree(&record.path, &path))
            .map(|record| record.path.clone())
            .ok_or_else(|| {
                GroveError::usage(format!(
                    "{} is not a worktree of this grove",
                    relative.display()
                ))
                .with_detail(list_hint)
            });
    }

    // A failure here is not an error to propagate: the grove root and every
    // intermediate directory under it walk up to the bare repository, where
    // `rev-parse --show-toplevel` fails outright, and that is precisely the
    // default-worktree case handled below.
    if let Ok(toplevel) = query::worktree_toplevel(runner, cwd) {
        if let Some(record) = checkouts
            .iter()
            .find(|record| same_worktree(&record.path, &toplevel))
        {
            return Ok(record.path.clone());
        }
        // A working tree that resolved, but is not one of this grove's
        // worktrees: an unrelated repository nested under the grove root.
        // Silently configuring some other worktree would be worse than
        // saying so.
        return Err(GroveError::usage(format!(
            "{} is a Git working tree, but not a worktree of this grove",
            toplevel.display()
        ))
        .with_detail(format!("{list_hint}; name one with --worktree <name>")));
    }

    let default_branch = metadata.default_branch.as_ref().ok_or_else(|| {
        GroveError::needs_decision("this grove records no default branch to fall back on")
            .with_detail(
                "run `git grove setup` from inside a worktree, or name one with \
                 --worktree <name>",
            )
    })?;
    checkouts
        .iter()
        .find(|record| record.branch.as_ref() == Some(default_branch))
        .map(|record| record.path.clone())
        .ok_or_else(|| {
            GroveError::needs_decision(format!(
                "the grove's default branch {default_branch} is not checked out in any worktree"
            ))
            .with_detail(format!(
                "run `git grove add {default_branch}`, or name another worktree with \
                 --worktree <name>"
            ))
        })
}

/// The measured Copilot CLI version this session found does not fire a
/// local shared-file hook under non-interactive `-p` — see
/// `.superpowers/specs/2026-08-21-git-grove-agent-integration.md`, Part 3,
/// "Copilot compatibility boundary".
const MEASURED_COPILOT_NONINTERACTIVE_LIMITATION_VERSION: &str = "1.0.80";

/// Refuse an exact tracked collision, then validate/merge/write the common
/// exclude entry and the agent's own hook config in that order. The exclude
/// write is safe to repeat, so a rerun after a crash between the two writes
/// only sees an already-present exclude line and proceeds straight to the
/// config write — see the failpoint test in `tests/setup.rs`.
///
/// `arm_failpoint` exists only for that recovery test: `setup --agent` arms
/// the checkpoint between the two writes, and `git grove add`'s
/// best-effort provisioning does not, so provisioning cannot shift the
/// failpoint step numbering `add`'s own failure matrix asserts against.
fn apply(
    runner: &dyn GitRunner,
    grove_root: &Path,
    worktree_root: &Path,
    agent: Agent,
    executable: &str,
    arm_failpoint: bool,
) -> Result<()> {
    let target = agent.target();

    if query::is_tracked(runner, worktree_root, &target.relative_path())? {
        return Err(GroveError::needs_decision(format!(
            "{} is already tracked in this worktree; untrack it or resolve the policy \
             conflict, then run setup again",
            target.relative_path().display()
        )));
    }

    let directory = open_target_directory(worktree_root, &target)?;
    let existing_config = read_existing(&directory, &target)?;
    let new_config_bytes = merged_bytes(&existing_config, &target, agent, executable)?;

    let exclude_path = grove_root.join(".bare/info/exclude");
    let existing_exclude = match std::fs::read(&exclude_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(GroveError::failure(format!(
                "cannot read {}: {error}",
                exclude_path.display()
            )))
        }
    };
    let new_exclude_bytes = config::add_exclude_entry(&existing_exclude, &target.exclude_entry());

    crate::fsx::write_atomic(&exclude_path, &new_exclude_bytes)?;

    if arm_failpoint {
        let mut checkpoints = crate::transaction::failpoint::Checkpoints::from_env()?;
        checkpoints.checkpoint()?;
    }

    let parent_path = worktree_root.join(target.relative_dir);
    let full_path = worktree_root.join(target.relative_path());
    crate::fsx::write_atomic_in(
        &directory,
        &parent_path,
        &full_path,
        OsStr::new(target.relative_file),
        &new_config_bytes,
    )
}

/// The full `setup --agent` orchestration: write the agent's hook config
/// into `worktree_root`, record the agent in the grove's policy so future
/// `git grove add` runs provision it too, and return the honest,
/// agent-specific next-steps message. Grove discovery, worktree
/// resolution, the mutation lock, and metadata support are the caller's
/// job; this function assumes all of that already holds.
pub fn run(
    runner: &dyn GitRunner,
    grove: &Grove,
    worktree_root: &Path,
    agent: Agent,
    executable: &str,
) -> Result<String> {
    apply(runner, &grove.root, worktree_root, agent, executable, true)?;
    register_policy(runner, grove, agent)?;
    Ok(next_steps(
        agent,
        &agent.target(),
        &grove.root,
        worktree_root,
    ))
}

/// What provisioning a new worktree from the grove's recorded policy
/// produced: one short line per agent configured, for stdout, and one loud
/// line per problem, for stderr.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Provisioning {
    pub configured: Vec<String>,
    pub warnings: Vec<String>,
}

/// Provision every agent the grove's policy records into a just-created
/// `worktree_root`, best effort — never an error: a worktree without hook
/// configs is still a perfectly usable worktree, just an unprotected one,
/// and failing worktree creation after the checkout already exists would be
/// worse than saying so. Each agent goes through the same checks and the
/// same two writes as `setup --agent` itself, so a tracked collision is
/// refused here too and every file provisioned is excluded from
/// `git status`.
pub fn provision(
    runner: &dyn GitRunner,
    grove: &Grove,
    worktree_root: &Path,
    executable: &str,
) -> Provisioning {
    let (agents, unknown) = match read_policy(runner, grove) {
        Ok(policy) => policy,
        Err(error) => {
            return Provisioning {
                configured: Vec::new(),
                warnings: vec![format!(
                    "cannot read {POLICY_KEY} for this grove ({error}); wrote no agent hook \
                     configs into {}",
                    worktree_root.display()
                )],
            }
        }
    };

    let mut result = Provisioning {
        configured: Vec::new(),
        warnings: unknown
            .into_iter()
            .map(|value| {
                format!(
                    "{POLICY_KEY} names an agent this git-grove does not know ({value}); \
                     skipped it"
                )
            })
            .collect(),
    };

    for agent in agents {
        let target = agent.target();
        match apply(runner, &grove.root, worktree_root, agent, executable, false) {
            Ok(()) => result.configured.push(format!(
                "configured the {} hook: {}",
                agent.as_str(),
                worktree_root.join(target.relative_path()).display()
            )),
            Err(error) => result.warnings.push(format!(
                "could not configure the {} hook in {} ({error}); the worktree exists but is \
                 not protected -- run `git grove setup --agent {}` from inside it",
                agent.as_str(),
                worktree_root.display(),
                agent.as_str()
            )),
        }
    }
    result
}

fn next_steps(agent: Agent, target: &Target, grove_root: &Path, worktree_root: &Path) -> String {
    let probe = grove_root.join(".bare/git-grove-hook-probe");
    let written = worktree_root.join(target.relative_path());
    let shared_intro = format!(
        "Wrote the shared hook config to {} -- this configures both Claude Code and Copilot \
         CLI.\n",
        written.display()
    );
    let policy = format!(
        "Recorded {} in {POLICY_KEY}, so `git grove add` configures it in new worktrees too.\n",
        agent.as_str()
    );
    match agent {
        Agent::Claude => format!(
            "{shared_intro}{policy}You do not need to run `git grove setup --agent copilot` \
             separately.\nVerify: ask Claude to create {} and confirm the tool call is \
             denied with the git-grove invariant message; then delete the probe file if it \
             was created (the guard failed to fire).\n",
            probe.display()
        ),
        Agent::Copilot => format!(
            "{shared_intro}{policy}You do not need to run `git grove setup --agent claude` \
             separately.\nMeasured: Copilot CLI {MEASURED_COPILOT_NONINTERACTIVE_LIMITATION_VERSION} \
             did not fire this local hook source under non-interactive `copilot -p` -- an \
             interactive session did. Re-verify against your installed version before relying \
             on this in non-interactive automation.\nVerify interactively: ask Copilot to \
             create {} and confirm the tool call is denied; then delete the probe file if it \
             was created (the guard failed to fire).\n",
            probe.display()
        ),
        Agent::Codex => format!(
            "Wrote {}.\n{policy}\
             \nWARNING: Codex hook enforcement does not work in a grove today. Do not rely on\n\
             this for protection.\n\
             Codex finds its project root by walking up for its own project_root_markers\n\
             (default `.git`), and a grove root carries git-grove's own `.git` pointer file, so\n\
             Codex resolves the grove root as the project and never reads the target worktree's\n\
             {}/ at all. What was written here is the exact shape measured working once that\n\
             collision is gone; until git-grove renames its grove-root signature -- a separate\n\
             change that has not landed -- this config is correct but inert.\n\
             \nCodex also requires an interactive trust review before it enforces any hook: open\n\
             /hooks in an interactive `codex` session, review the exact command and its hash,\n\
             and trust it. `codex exec` is not protected until that review is complete.\n\
             \nVerify, once both hold: ask Codex to \
             create {} and confirm the tool call is denied; then delete the probe file if it was \
             created (the guard failed to fire).\nsetup never runs or approves any of these \
             steps itself.\n",
            written.display(),
            target.relative_dir,
            probe.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::runner::{GitOutput, RecordingFake};
    use bstr::BString;
    use serde_json::Value;

    fn output(status: i32, stdout: &[u8]) -> GitOutput {
        GitOutput {
            status,
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        }
    }

    fn grove_at(root: &Path) -> Grove {
        std::fs::create_dir_all(root.join(".bare")).unwrap();
        Grove {
            root: root.canonicalize().unwrap(),
        }
    }

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
            Path::new(".codex/config.toml")
        );
        assert_eq!(Agent::Claude.target().format, Format::Json);
        assert_eq!(Agent::Codex.target().format, Format::Toml);
    }

    #[test]
    fn exclude_entries_are_anchored_to_the_worktree_root() {
        assert_eq!(
            Agent::Claude.target().exclude_entry(),
            "/.claude/settings.local.json"
        );
        assert_eq!(Agent::Codex.target().exclude_entry(), "/.codex/config.toml");
    }

    #[test]
    fn policy_values_round_trip_through_every_agent_spelling() {
        for agent in Agent::ALL {
            assert_eq!(Agent::parse(agent.as_str().as_bytes()), Some(agent));
        }
        assert_eq!(Agent::parse(b"gemini"), None);
        assert_eq!(Agent::parse(b"Claude"), None);
        assert_eq!(Agent::parse(b""), None);
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
    fn write_hook_config_writes_codex_toml_into_the_worktree() {
        let root = tempfile::tempdir().unwrap();
        write_hook_config(root.path(), Agent::Codex, "/abs/git-grove").unwrap();
        assert!(
            !root.path().join(".codex/hooks.json").exists(),
            "the separate hooks.json file is gone"
        );
        let text = std::fs::read_to_string(root.path().join(".codex/config.toml")).unwrap();
        let document: toml_edit::DocumentMut = text.parse().unwrap();
        assert_eq!(document["features"]["hooks"].as_bool(), Some(true));
        let entry = &document["hooks"]["PreToolUse"][0];
        assert_eq!(
            entry[config::CODEX_MARKER_KEY].as_str(),
            Some(config::CODEX_MARKER_VALUE)
        );
        assert_eq!(
            entry["hooks"][0]["command"].as_str(),
            Some("/abs/git-grove hook-guard --protocol codex PreToolUse")
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
    fn write_hook_config_is_idempotent_for_codex_too() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(".codex/config.toml");
        write_hook_config(root.path(), Agent::Codex, "/abs/git-grove").unwrap();
        let once = std::fs::read(&path).unwrap();
        write_hook_config(root.path(), Agent::Codex, "/abs/git-grove").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), once);
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
    fn write_hook_config_preserves_unrelated_codex_settings_and_comments() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".codex")).unwrap();
        let path = root.path().join(".codex/config.toml");
        std::fs::write(&path, b"# keep me\nmodel = \"gpt-5\"\n").unwrap();
        write_hook_config(root.path(), Agent::Codex, "/abs/git-grove").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep me"), "got {text}");
        assert!(text.contains("model = \"gpt-5\""), "got {text}");
        assert!(text.contains("[[hooks.PreToolUse]]"), "got {text}");
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
    fn write_hook_config_leaves_a_malformed_existing_codex_config_untouched() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".codex")).unwrap();
        let path = root.path().join(".codex/config.toml");
        std::fs::write(&path, b"this is not = = toml\n").unwrap();
        assert!(write_hook_config(root.path(), Agent::Codex, "/abs/git-grove").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"this is not = = toml\n");
    }

    #[test]
    fn write_hook_config_leaves_a_symlinked_leaf_untouched() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".codex")).unwrap();
        std::fs::write(root.path().join("elsewhere.toml"), b"").unwrap();
        std::os::unix::fs::symlink(
            root.path().join("elsewhere.toml"),
            root.path().join(".codex/config.toml"),
        )
        .unwrap();
        assert!(write_hook_config(root.path(), Agent::Codex, "/abs/git-grove").is_err());
        assert_eq!(
            std::fs::read_link(root.path().join(".codex/config.toml")).unwrap(),
            root.path().join("elsewhere.toml")
        );
    }

    fn worktree_list(grove: &Grove, checkouts: &[(&str, &str)]) -> Vec<u8> {
        let mut raw = Vec::new();
        raw.extend_from_slice(b"worktree ");
        raw.extend_from_slice(grove.bare_dir().to_str().unwrap().as_bytes());
        raw.extend_from_slice(b"\0bare\0\0");
        for (relative, branch) in checkouts {
            raw.extend_from_slice(b"worktree ");
            raw.extend_from_slice(grove.root.join(relative).to_str().unwrap().as_bytes());
            raw.extend_from_slice(
                b"\0HEAD 0123456789012345678901234567890123456789\0branch refs/heads/",
            );
            raw.extend_from_slice(branch.as_bytes());
            raw.extend_from_slice(b"\0\0");
        }
        raw
    }

    fn metadata_with_default(branch: Option<&str>) -> Metadata {
        Metadata {
            version: Some(1),
            default_branch: branch.map(BString::from),
            remote: None,
            publish_state: crate::grove::metadata::PublishState::Unpublished,
            publish_remote: None,
            publish_url: None,
            publish_provider: None,
            publish_owner: None,
            publish_name: None,
        }
    }

    #[test]
    fn a_cwd_inside_a_worktree_selects_that_worktree() {
        let root = tempfile::tempdir().unwrap();
        let grove = grove_at(root.path());
        let topic = grove.root.join("topic");
        std::fs::create_dir_all(topic.join("nested")).unwrap();

        let fake = RecordingFake::new();
        fake.push_response(output(
            0,
            &worktree_list(&grove, &[("main", "main"), ("topic", "topic")]),
        ));
        fake.push_response(output(0, format!("{}\n", topic.display()).as_bytes()));

        let resolved = resolve_worktree(
            &fake,
            &grove,
            &metadata_with_default(Some("main")),
            None,
            &topic.join("nested"),
        )
        .unwrap();
        assert_eq!(resolved, topic);
    }

    /// Measured: the grove root and every intermediate directory under it
    /// walk up to the bare repository, where `rev-parse --show-toplevel`
    /// exits 128 rather than naming a working tree. Both must fall through
    /// to the recorded default branch, so both are exercised here.
    #[test]
    fn a_cwd_in_no_working_tree_falls_back_to_the_recorded_default_branch() {
        for from_root in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let grove = grove_at(root.path());
            std::fs::create_dir_all(grove.root.join("trunk")).unwrap();
            std::fs::create_dir_all(grove.root.join("intermediate")).unwrap();

            let fake = RecordingFake::new();
            fake.push_response(output(0, &worktree_list(&grove, &[("trunk", "trunk")])));
            fake.push_response(output(128, b""));

            let cwd = if from_root {
                grove.root.clone()
            } else {
                grove.root.join("intermediate")
            };
            let resolved = resolve_worktree(
                &fake,
                &grove,
                &metadata_with_default(Some("trunk")),
                None,
                &cwd,
            )
            .unwrap();
            assert_eq!(resolved, grove.root.join("trunk"), "from {}", cwd.display());
        }
    }

    #[test]
    fn a_working_tree_that_is_not_a_worktree_of_this_grove_is_a_usage_error() {
        let root = tempfile::tempdir().unwrap();
        let grove = grove_at(root.path());
        std::fs::create_dir_all(grove.root.join("main")).unwrap();
        let unrelated = grove.root.join("vendor/other-repo");
        std::fs::create_dir_all(&unrelated).unwrap();

        let fake = RecordingFake::new();
        fake.push_response(output(0, &worktree_list(&grove, &[("main", "main")])));
        fake.push_response(output(0, format!("{}\n", unrelated.display()).as_bytes()));

        let error = resolve_worktree(
            &fake,
            &grove,
            &metadata_with_default(Some("main")),
            None,
            &unrelated,
        )
        .unwrap_err();
        assert_eq!(error.class, crate::error::ExitClass::Usage);
        assert!(error.message.contains("not a worktree of this grove"));
    }

    #[test]
    fn an_explicit_worktree_wins_over_the_cwd_and_never_runs_rev_parse() {
        let root = tempfile::tempdir().unwrap();
        let grove = grove_at(root.path());
        std::fs::create_dir_all(grove.root.join("feature/auth")).unwrap();
        std::fs::create_dir_all(grove.root.join("main")).unwrap();

        let fake = RecordingFake::new();
        fake.push_response(output(
            0,
            &worktree_list(
                &grove,
                &[("main", "main"), ("feature/auth", "feature/auth")],
            ),
        ));

        let resolved = resolve_worktree(
            &fake,
            &grove,
            &metadata_with_default(Some("main")),
            Some(Path::new("feature/auth")),
            &grove.root.join("main"),
        )
        .unwrap();
        assert_eq!(resolved, grove.root.join("feature/auth"));
        assert_eq!(fake.calls().len(), 1, "no rev-parse is needed");
    }

    #[test]
    fn an_explicit_worktree_that_is_not_a_worktree_is_a_usage_error() {
        let root = tempfile::tempdir().unwrap();
        let grove = grove_at(root.path());
        std::fs::create_dir_all(grove.root.join("main")).unwrap();
        std::fs::create_dir_all(grove.root.join("not-a-worktree")).unwrap();

        let fake = RecordingFake::new();
        fake.push_response(output(0, &worktree_list(&grove, &[("main", "main")])));
        let error = resolve_worktree(
            &fake,
            &grove,
            &metadata_with_default(Some("main")),
            Some(Path::new("not-a-worktree")),
            &grove.root,
        )
        .unwrap_err();
        assert_eq!(error.class, crate::error::ExitClass::Usage);
        assert!(error.message.contains("not a worktree of this grove"));
    }

    #[test]
    fn a_missing_default_worktree_names_the_fix_instead_of_failing_obscurely() {
        let root = tempfile::tempdir().unwrap();
        let grove = grove_at(root.path());
        std::fs::create_dir_all(grove.root.join("topic")).unwrap();

        let fake = RecordingFake::new();
        fake.push_response(output(0, &worktree_list(&grove, &[("topic", "topic")])));
        fake.push_response(output(128, b""));
        let error = resolve_worktree(
            &fake,
            &grove,
            &metadata_with_default(Some("main")),
            None,
            &grove.root,
        )
        .unwrap_err();
        assert_eq!(error.class, crate::error::ExitClass::NeedsDecision);
        assert!(error.message.contains("not checked out in any worktree"));
        assert!(error
            .detail
            .as_deref()
            .unwrap()
            .contains("git grove add main"));
    }

    #[test]
    fn a_grove_with_no_recorded_default_branch_says_so() {
        let root = tempfile::tempdir().unwrap();
        let grove = grove_at(root.path());

        let fake = RecordingFake::new();
        fake.push_response(output(0, &worktree_list(&grove, &[])));
        fake.push_response(output(128, b""));
        let error = resolve_worktree(
            &fake,
            &grove,
            &metadata_with_default(None),
            None,
            &grove.root,
        )
        .unwrap_err();
        assert_eq!(error.class, crate::error::ExitClass::NeedsDecision);
        assert!(error.message.contains("records no default branch"));
    }

    #[test]
    fn reading_an_empty_policy_yields_no_agents_and_no_complaints() {
        let root = tempfile::tempdir().unwrap();
        let grove = grove_at(root.path());
        let fake = RecordingFake::new();
        fake.push_response(output(1, b""));
        assert_eq!(
            read_policy(&fake, &grove).unwrap(),
            (Vec::new(), Vec::new())
        );
    }

    #[test]
    fn reading_a_policy_collapses_duplicates_and_reports_unknown_values() {
        let root = tempfile::tempdir().unwrap();
        let grove = grove_at(root.path());
        let fake = RecordingFake::new();
        fake.push_response(output(0, b"codex\0claude\0codex\0gemini\0"));
        let (agents, unknown) = read_policy(&fake, &grove).unwrap();
        assert_eq!(agents, vec![Agent::Claude, Agent::Codex]);
        assert_eq!(unknown, vec!["gemini".to_string()]);
    }

    #[test]
    fn registering_an_already_recorded_agent_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let grove = grove_at(root.path());
        let fake = RecordingFake::new();
        fake.push_response(output(0, b"codex\0"));
        register_policy(&fake, &grove, Agent::Codex).unwrap();
        assert_eq!(fake.calls().len(), 1, "the read is the only call");
    }

    #[test]
    fn registering_a_new_agent_adds_it_without_replacing_the_others() {
        let root = tempfile::tempdir().unwrap();
        let grove = grove_at(root.path());
        let fake = RecordingFake::new();
        fake.push_response(output(0, b"codex\0"));
        fake.push_response(output(0, b""));
        register_policy(&fake, &grove, Agent::Claude).unwrap();
        let argv = fake.calls().last().unwrap().argv_os();
        assert_eq!(
            argv,
            [
                std::ffi::OsString::from("config"),
                std::ffi::OsString::from("--file"),
                grove.bare_dir().join("config").into_os_string(),
                std::ffi::OsString::from("--add"),
                std::ffi::OsString::from(POLICY_KEY),
                std::ffi::OsString::from("claude"),
            ]
        );
    }

    #[test]
    fn provisioning_an_empty_policy_writes_nothing_and_warns_about_nothing() {
        let root = tempfile::tempdir().unwrap();
        let grove = grove_at(root.path());
        let worktree = grove.root.join("topic");
        std::fs::create_dir(&worktree).unwrap();

        let fake = RecordingFake::new();
        fake.push_response(output(1, b""));
        assert_eq!(
            provision(&fake, &grove, &worktree, "/abs/git-grove"),
            Provisioning::default()
        );
        assert!(!worktree.join(".claude").exists());
        assert!(!worktree.join(".codex").exists());
    }

    #[test]
    fn provisioning_writes_every_recorded_agent_and_the_shared_exclude() {
        let root = tempfile::tempdir().unwrap();
        let grove = grove_at(root.path());
        std::fs::create_dir_all(grove.bare_dir().join("info")).unwrap();
        let worktree = grove.root.join("topic");
        std::fs::create_dir(&worktree).unwrap();

        let fake = RecordingFake::new();
        fake.push_response(output(0, b"claude\0codex\0"));
        fake.push_response(output(1, b"")); // ls-files for claude: untracked
        fake.push_response(output(1, b"")); // ls-files for codex: untracked
        let provisioned = provision(&fake, &grove, &worktree, "/abs/git-grove");
        assert!(provisioned.warnings.is_empty(), "got {provisioned:?}");
        assert_eq!(provisioned.configured.len(), 2, "got {provisioned:?}");
        assert!(provisioned.configured[0].contains("configured the claude hook"));
        assert!(provisioned.configured[1].contains(".codex/config.toml"));
        assert!(worktree.join(".claude/settings.local.json").is_file());
        assert!(worktree.join(".codex/config.toml").is_file());

        let exclude = std::fs::read_to_string(grove.bare_dir().join("info/exclude")).unwrap();
        assert!(exclude.contains("/.claude/settings.local.json"));
        assert!(exclude.contains("/.codex/config.toml"));
    }

    #[test]
    fn a_failed_provision_warns_and_still_provisions_the_other_agents() {
        let root = tempfile::tempdir().unwrap();
        let grove = grove_at(root.path());
        std::fs::create_dir_all(grove.bare_dir().join("info")).unwrap();
        let worktree = grove.root.join("topic");
        std::fs::create_dir(&worktree).unwrap();
        // A file where Claude's directory must go: a refusal, not a panic.
        std::fs::write(worktree.join(".claude"), b"not a directory").unwrap();

        let fake = RecordingFake::new();
        fake.push_response(output(0, b"claude\0codex\0"));
        fake.push_response(output(1, b""));
        fake.push_response(output(1, b""));
        let provisioned = provision(&fake, &grove, &worktree, "/abs/git-grove");
        assert_eq!(provisioned.warnings.len(), 1, "got {provisioned:?}");
        assert!(provisioned.warnings[0].contains("could not configure the claude hook"));
        assert!(provisioned.warnings[0].contains("git grove setup --agent claude"));
        assert_eq!(provisioned.configured.len(), 1, "codex still succeeded");
        assert!(
            worktree.join(".codex/config.toml").is_file(),
            "codex is provisioned regardless"
        );
    }

    #[test]
    fn an_unknown_recorded_agent_is_reported_and_skipped() {
        let root = tempfile::tempdir().unwrap();
        let grove = grove_at(root.path());
        let worktree = grove.root.join("topic");
        std::fs::create_dir(&worktree).unwrap();

        let fake = RecordingFake::new();
        fake.push_response(output(0, b"gemini\0"));
        let provisioned = provision(&fake, &grove, &worktree, "/abs/git-grove");
        assert_eq!(provisioned.warnings.len(), 1);
        assert!(provisioned.warnings[0].contains("gemini"));
        assert!(provisioned.configured.is_empty());
        assert!(!worktree.join(".claude").exists());
    }

    #[test]
    fn codex_next_steps_state_the_non_enforcement_without_hedging() {
        let message = next_steps(
            Agent::Codex,
            &Agent::Codex.target(),
            Path::new("/g"),
            Path::new("/g/topic"),
        );
        assert!(message.contains("/g/topic/.codex/config.toml"));
        assert!(message.contains("does not work in a grove today"));
        assert!(message.contains("project_root_markers"));
        assert!(message.contains("Do not rely on\nthis for protection"));
        assert!(message.contains("correct but inert"));
    }
}
