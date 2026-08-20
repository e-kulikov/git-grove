use crate::error::{GroveError, Result};
use crate::fsx;
use crate::fsx::held::{open_directory_at, FileIdentity as DirectoryIdentity, HeldDirectory};
use crate::git::runner::{GitOutput, GitRunner, Invocation};
use crate::grove::agents_md::{self, Facts};
use crate::grove::discover::Grove;
use crate::grove::layout;
use crate::grove::metadata::{self, Metadata, PublishState, FORMAT_VERSION};
use bstr::{BString, ByteSlice};
use rustix::fs::{mkdirat, openat, statat, AtFlags, Mode, OFlags, CWD};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::ErrorKind;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

const REPOSITORY_NEUTRAL_CWD: &str = "/proc";

pub(crate) fn escaped_path(path: &Path) -> String {
    path.as_os_str().as_bytes().escape_bytes().to_string()
}

pub(crate) fn state_conflict(message: impl Into<String>, detail: impl Into<String>) -> GroveError {
    GroveError::needs_decision(message).with_detail(detail)
}

pub(crate) fn normalize_absolute(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(GroveError::failure("internal root path is not absolute"));
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(GroveError::usage(
                        "the grove root escapes the filesystem root",
                    ));
                }
            }
            Component::Prefix(_) => {
                return Err(GroveError::usage("the grove root is not a Linux path"))
            }
        }
    }
    Ok(normalized)
}

#[derive(Default)]
pub(crate) struct RecoveryState {
    pub(crate) root: Option<DirectoryIdentity>,
    pub(crate) bare: Option<DirectoryIdentity>,
}

pub(crate) fn open_or_create_root(root: &Path, mutated: &mut bool) -> Result<HeldDirectory> {
    let mut missing = Vec::new();
    let mut current = root;
    let ancestor = loop {
        match std::fs::symlink_metadata(current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                break current.to_path_buf();
            }
            Ok(_) => {
                return Err(state_conflict(
                    format!("{} is not an empty directory", escaped_path(root)),
                    "use `git grove adopt` to convert existing contents",
                ))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let name = current.file_name().ok_or_else(|| {
                    GroveError::failure(format!(
                        "cannot find an existing parent for {}",
                        escaped_path(root)
                    ))
                })?;
                missing.push(name.to_os_string());
                current = current.parent().ok_or_else(|| {
                    GroveError::failure(format!(
                        "cannot find an existing parent for {}",
                        escaped_path(root)
                    ))
                })?;
            }
            Err(error) => {
                return Err(GroveError::failure(format!(
                    "cannot inspect {}: {error}",
                    escaped_path(current)
                )))
            }
        }
    };

    let mut directory = openat(
        CWD,
        &ancestor,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| {
        state_conflict(
            format!("cannot open {} safely", escaped_path(&ancestor)),
            error.to_string(),
        )
    })?;
    for name in missing.iter().rev() {
        match mkdirat(&directory, name, Mode::from_raw_mode(0o755)) {
            Ok(()) => *mutated = true,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(GroveError::failure(format!(
                    "cannot create {}: {error}",
                    escaped_path(root)
                )))
            }
        }
        directory = open_directory_at(&directory, name).map_err(|error| {
            state_conflict(
                format!("{} changed while it was being created", escaped_path(root)),
                format!("a non-directory or symlink was preserved: {error}"),
            )
        })?;
    }

    let held = HeldDirectory::new(directory, root.to_path_buf())?;
    held.ensure_empty()?;
    Ok(held)
}

pub(crate) fn create_bare(root: &HeldDirectory, mutated: &mut bool) -> Result<HeldDirectory> {
    create_bare_with(root, mutated, || {})
}

fn create_bare_with(
    root: &HeldDirectory,
    mutated: &mut bool,
    before_mkdir: impl FnOnce(),
) -> Result<HeldDirectory> {
    root.ensure_empty()?;
    before_mkdir();
    match mkdirat(&root.file, ".bare", Mode::from_raw_mode(0o755)) {
        Ok(()) => *mutated = true,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            return Err(state_conflict(
                format!(
                    "{} already exists",
                    escaped_path(&root.named_path.join(".bare"))
                ),
                "the existing bare repository or link was preserved",
            ))
        }
        Err(error) => {
            return Err(GroveError::failure(format!(
                "cannot create {}: {error}",
                escaped_path(&root.named_path.join(".bare"))
            )))
        }
    }
    let file = open_directory_at(&root.file, OsStr::new(".bare")).map_err(|error| {
        state_conflict(
            format!(
                "{} changed while it was being created",
                escaped_path(&root.named_path.join(".bare"))
            ),
            format!("the replacement was preserved: {error}"),
        )
    })?;
    root.validate()?;
    let bare = HeldDirectory::new(file, root.named_path.join(".bare"))?;
    root.ensure_only_entry(OsStr::new(".bare"))?;
    bare.validate()?;
    Ok(bare)
}

pub(crate) struct GuardedRunner<'a> {
    pub(crate) runner: &'a dyn GitRunner,
    pub(crate) root: &'a HeldDirectory,
    pub(crate) bare: &'a HeldDirectory,
}

impl GitRunner for GuardedRunner<'_> {
    fn run(&self, invocation: Invocation) -> Result<GitOutput> {
        self.root.validate()?;
        self.bare.validate()?;
        let output = self.runner.run(invocation)?;
        self.root.validate()?;
        self.bare.validate()?;
        Ok(output)
    }
}

fn trimmed_line(mut output: Vec<u8>, description: &str) -> Result<BString> {
    while matches!(output.last(), Some(b'\n' | b'\r')) {
        output.pop();
    }
    if output.is_empty() || output.contains(&b'\n') || output.contains(&b'\r') {
        return Err(GroveError::failure(format!(
            "git returned an invalid {description}"
        )));
    }
    Ok(BString::from(output))
}

struct BranchPlan {
    initial_branch: Option<OsString>,
    expected: BString,
    relative_worktree: PathBuf,
}

fn preflight_branch(runner: &dyn GitRunner, branch: Option<OsString>) -> Result<BranchPlan> {
    let expected = match &branch {
        Some(branch) => BString::from(branch.as_bytes()),
        None => trimmed_line(
            runner
                .run_ok(
                    Invocation::new()
                        .cwd(REPOSITORY_NEUTRAL_CWD)
                        .args(["var", "GIT_DEFAULT_BRANCH"]),
                )?
                .stdout,
            "default branch",
        )?,
    };
    let branch_os = OsStr::from_bytes(expected.as_ref());
    let checked = runner.run(Invocation::new().args([
        OsStr::new("check-ref-format"),
        OsStr::new("--branch"),
        branch_os,
    ]))?;
    if !checked.ok() {
        return Err(GroveError::usage(format!(
            "{} is not a valid short branch name",
            expected.as_slice().escape_bytes()
        )));
    }
    let relative_worktree = layout::validate_relative_worktree_path(Path::new(branch_os))?;
    Ok(BranchPlan {
        initial_branch: branch,
        expected,
        relative_worktree,
    })
}

pub(crate) fn post_mutation_layout_error(error: GroveError) -> GroveError {
    if error.class == crate::error::ExitClass::Usage {
        state_conflict(
            error.message,
            error
                .detail
                .unwrap_or_else(|| "the conflicting state was preserved".into()),
        )
    } else {
        error
    }
}

fn identity_matches(path: &Path, expected: DirectoryIdentity) -> bool {
    statat(CWD, path, AtFlags::SYMLINK_NOFOLLOW)
        .is_ok_and(|stat| stat.st_dev == expected.dev && stat.st_ino == expected.ino)
}

pub(crate) fn retain_partial(
    error: GroveError,
    root: &Path,
    recovery_state: &RecoveryState,
) -> GroveError {
    retain_partial_for(error, root, recovery_state, "initialization")
}

pub(crate) fn retain_partial_for(
    mut error: GroveError,
    root: &Path,
    recovery_state: &RecoveryState,
    operation: &str,
) -> GroveError {
    let root_matches = recovery_state
        .root
        .is_some_and(|identity| identity_matches(root, identity));
    let bare_matches = recovery_state
        .bare
        .map(|identity| identity_matches(&root.join(".bare"), identity))
        .unwrap_or(true);
    let recovery = if root_matches && bare_matches {
        format!(
            "partial {operation} retained at {}; inspect it and remove only confirmed invocation-created entries by hand before retrying",
            escaped_path(root)
        )
    } else {
        format!(
            "the requested path {} changed and is not a safe cleanup target; foreign replacement state was preserved, so locate the invocation-created partial state separately",
            escaped_path(root)
        )
    };
    error.detail = Some(match error.detail.take() {
        Some(detail) if !detail.is_empty() => format!("{detail}\n  {recovery}"),
        _ => recovery,
    });
    error
}

fn run_transaction(
    runner: &dyn GitRunner,
    root_path: &Path,
    plan: BranchPlan,
    mutated: &mut bool,
    recovery_state: &mut RecoveryState,
) -> Result<Grove> {
    let root = open_or_create_root(root_path, mutated)?;
    recovery_state.root = Some(root.identity()?);
    let bare = create_bare(&root, mutated)?;
    recovery_state.bare = Some(bare.identity()?);
    let guarded = GuardedRunner {
        runner,
        root: &root,
        bare: &bare,
    };

    let mut init_args = vec![
        OsString::from("init"),
        OsString::from("--quiet"),
        OsString::from("--bare"),
    ];
    if let Some(branch) = &plan.initial_branch {
        init_args.push(OsString::from("--initial-branch"));
        init_args.push(branch.clone());
    }
    init_args.push(bare.anchored_path.as_os_str().to_os_string());
    guarded.run_ok(
        Invocation::new()
            .cwd(REPOSITORY_NEUTRAL_CWD)
            .args(init_args),
    )?;

    root.validate()?;
    bare.validate()?;
    let created = layout::write_pointer_if_absent(&root.anchored_path)?;
    if !created {
        return Err(state_conflict(
            format!(
                "{} already exists",
                escaped_path(&root.named_path.join(".git"))
            ),
            "the existing entry was preserved",
        ));
    }
    root.validate()?;
    bare.validate()?;

    let actual_branch = trimmed_line(
        guarded
            .run_ok(Invocation::new().git_dir(&bare.anchored_path).args([
                "symbolic-ref",
                "--short",
                "HEAD",
            ]))?
            .stdout,
        "bare HEAD",
    )?;
    if actual_branch != plan.expected {
        return Err(state_conflict(
            "Git's default branch changed during initialization",
            format!(
                "expected {}, found {}",
                plan.expected.as_slice().escape_bytes(),
                actual_branch.as_slice().escape_bytes()
            ),
        ));
    }

    metadata::write_to_config(
        &guarded,
        &bare.anchored_path.join("config"),
        &Metadata {
            version: Some(FORMAT_VERSION),
            default_branch: Some(actual_branch.clone()),
            remote: None,
            publish_state: PublishState::Unpublished,
            publish_remote: None,
            publish_url: None,
        },
    )?;

    let facts = Facts {
        remote: None,
        default_branch: actual_branch.clone(),
        published: false,
        narrowed: false,
    };
    let agents = root.anchored_path.join("AGENTS.md");
    root.validate()?;
    bare.validate()?;
    let created = fsx::write_atomic_if_absent(&agents, agents_md::render(&facts).as_bytes())?;
    if !created {
        return Err(state_conflict(
            format!(
                "{} already exists",
                escaped_path(&root.named_path.join("AGENTS.md"))
            ),
            "the existing entry was preserved",
        ));
    }

    let claude = root.anchored_path.join("CLAUDE.md");
    let created = fsx::symlink_relative_if_absent(&claude, "AGENTS.md")?;
    if !created {
        return Err(state_conflict(
            format!(
                "{} already exists",
                escaped_path(&root.named_path.join("CLAUDE.md"))
            ),
            "the existing entry was preserved",
        ));
    }
    root.validate()?;
    bare.validate()?;

    let named_grove = Grove {
        root: root.named_path.clone(),
    };
    let worktree =
        layout::validate_worktree_path_at(&root.file, &named_grove.root, &plan.relative_worktree)
            .map_err(post_mutation_layout_error)?;
    worktree
        .create_parent_directories()
        .map_err(post_mutation_layout_error)?;
    root.validate()?;
    bare.validate()?;
    let worktree_path = worktree.path();
    let anchored_worktree = root.anchored_path.join(worktree.relative());
    let worktree_args = [
        OsString::from("worktree"),
        OsString::from("add"),
        OsString::from("--orphan"),
        OsString::from("-b"),
        OsString::from_vec(actual_branch.to_vec()),
        anchored_worktree.as_os_str().to_os_string(),
    ];
    worktree
        .validate_vacant()
        .map_err(post_mutation_layout_error)?;
    guarded.run_ok(
        Invocation::new()
            .git_dir(&bare.anchored_path)
            .args(worktree_args),
    )?;

    root.validate()?;
    bare.validate()?;
    let grove = Grove::at(&root.named_path).map_err(|error| {
        state_conflict(
            format!(
                "{} changed during initialization",
                escaped_path(&root.named_path)
            ),
            error.to_string(),
        )
    })?;
    println!("ready: {}", escaped_path(&grove.root));
    println!("next: cd {}", escaped_path(&worktree_path));
    Ok(grove)
}

pub fn run(
    runner: &dyn GitRunner,
    dir: Option<PathBuf>,
    branch: Option<OsString>,
    cwd: &Path,
) -> Result<Grove> {
    let requested = match dir {
        Some(dir) if dir.is_absolute() => dir,
        Some(dir) => cwd.join(dir),
        None => cwd.to_path_buf(),
    };
    let root = normalize_absolute(&requested)?;
    let plan = preflight_branch(runner, branch)?;
    let mut mutated = false;
    let mut recovery_state = RecoveryState::default();
    match run_transaction(runner, &root, plan, &mut mutated, &mut recovery_state) {
        Err(error) if mutated => Err(retain_partial(error, &root, &recovery_state)),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ExitClass;

    #[test]
    fn exclusive_bare_creation_preserves_a_concurrent_directory_or_symlink() {
        for kind in ["directory", "symlink"] {
            let parent = tempfile::tempdir().unwrap();
            let root_path = parent.path().join("grove");
            std::fs::create_dir(&root_path).unwrap();
            let mut mutated = false;
            let root = open_or_create_root(&root_path, &mut mutated).unwrap();

            let error = create_bare_with(&root, &mut mutated, || match kind {
                "directory" => std::fs::create_dir(root_path.join(".bare")).unwrap(),
                "symlink" => {
                    std::os::unix::fs::symlink("foreign", root_path.join(".bare")).unwrap()
                }
                _ => unreachable!(),
            })
            .unwrap_err();

            assert_eq!(error.class, ExitClass::NeedsDecision, "kind {kind}");
            assert!(std::fs::symlink_metadata(root_path.join(".bare")).is_ok());
        }
    }

    #[test]
    fn bare_creation_stops_before_git_when_another_root_entry_appears() {
        let parent = tempfile::tempdir().unwrap();
        let root_path = parent.path().join("grove");
        std::fs::create_dir(&root_path).unwrap();
        let mut mutated = false;
        let root = open_or_create_root(&root_path, &mut mutated).unwrap();

        let error = create_bare_with(&root, &mut mutated, || {
            std::fs::write(root_path.join("foreign"), b"preserve me").unwrap();
        })
        .unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        assert_eq!(
            std::fs::read(root_path.join("foreign")).unwrap(),
            b"preserve me"
        );
        assert!(root_path.join(".bare").is_dir());
    }

    #[test]
    fn post_mutation_path_policy_errors_are_state_conflicts() {
        let error = post_mutation_layout_error(
            GroveError::usage("worktree path changed").with_detail("foreign entry preserved"),
        );

        assert_eq!(error.class, ExitClass::NeedsDecision);
        assert_eq!(error.message, "worktree path changed");
        assert_eq!(error.detail.as_deref(), Some("foreign entry preserved"));
    }
}
