use crate::error::{GroveError, Result};
use crate::fsx;
use crate::git::runner::{GitRunner, Invocation};
use crate::grove::agents_md::{self, Facts};
use crate::grove::discover::Grove;
use crate::grove::layout;
use crate::grove::metadata::{self, Metadata, PublishState, FORMAT_VERSION};
use bstr::BString;
use std::ffi::{OsStr, OsString};
use std::io::ErrorKind;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

fn create_root(root: &Path, mutated: &mut bool) -> Result<()> {
    match std::fs::symlink_metadata(root) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(GroveError::usage(format!(
                    "{} is not an empty directory",
                    root.display()
                ))
                .with_detail("use `git grove adopt` to convert existing contents"));
            }
            let mut entries = std::fs::read_dir(root).map_err(|error| {
                GroveError::failure(format!("cannot read {}: {error}", root.display()))
            })?;
            if entries.next().is_some() {
                return Err(
                    GroveError::usage(format!("{} is not empty", root.display()))
                        .with_detail("use `git grove adopt` to convert an existing repository"),
                );
            }
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let mut missing = Vec::new();
            let mut current = root;
            loop {
                match std::fs::symlink_metadata(current) {
                    Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                        break;
                    }
                    Ok(_) => {
                        return Err(GroveError::usage(format!(
                            "{} has a non-directory or symlinked ancestor",
                            root.display()
                        )))
                    }
                    Err(error) if error.kind() == ErrorKind::NotFound => {
                        missing.push(current.to_path_buf());
                        current = current.parent().ok_or_else(|| {
                            GroveError::failure(format!(
                                "cannot find an existing parent for {}",
                                root.display()
                            ))
                        })?;
                    }
                    Err(error) => {
                        return Err(GroveError::failure(format!(
                            "cannot inspect {}: {error}",
                            current.display()
                        )))
                    }
                }
            }

            for directory in missing.into_iter().rev() {
                match std::fs::create_dir(&directory) {
                    Ok(()) => *mutated = true,
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                        let metadata = std::fs::symlink_metadata(&directory).map_err(|error| {
                            GroveError::failure(format!(
                                "cannot inspect {} after it appeared: {error}",
                                directory.display()
                            ))
                        })?;
                        if !metadata.is_dir() || metadata.file_type().is_symlink() {
                            return Err(GroveError::usage(format!(
                                "{} has a non-directory or symlinked ancestor",
                                root.display()
                            )));
                        }
                    }
                    Err(error) => {
                        return Err(GroveError::failure(format!(
                            "cannot create {}: {error}",
                            directory.display()
                        )))
                    }
                }
            }
            Ok(())
        }
        Err(error) => Err(GroveError::failure(format!(
            "cannot inspect {}: {error}",
            root.display()
        ))),
    }
}

fn branch_from_head(output: Vec<u8>) -> Result<BString> {
    let mut branch = output;
    while matches!(branch.last(), Some(b'\n' | b'\r')) {
        branch.pop();
    }
    if branch.is_empty() {
        return Err(GroveError::failure(
            "the bare repository has no symbolic HEAD",
        ));
    }
    Ok(BString::from(branch))
}

fn retain_partial(mut error: GroveError, root: &Path) -> GroveError {
    let recovery = format!(
        "partial initialization retained at {}; inspect it and remove it by hand before retrying",
        root.display()
    );
    error.detail = Some(match error.detail.take() {
        Some(detail) if !detail.is_empty() => format!("{detail}\n  {recovery}"),
        _ => recovery,
    });
    error
}

fn run_inner(
    runner: &dyn GitRunner,
    root: &Path,
    branch: Option<OsString>,
    mutated: &mut bool,
) -> Result<Grove> {
    create_root(root, mutated)?;

    let bare = root.join(".bare");
    let mut init_args = vec![
        OsString::from("init"),
        OsString::from("--quiet"),
        OsString::from("--bare"),
    ];
    if let Some(branch) = &branch {
        init_args.push(OsString::from("--initial-branch"));
        init_args.push(branch.clone());
    }
    init_args.push(bare.as_os_str().to_os_string());
    *mutated = true;
    runner.run_ok(Invocation::new().cwd(root).args(init_args))?;

    if !layout::write_pointer_if_absent(root)? {
        return Err(GroveError::usage(format!(
            "{} appeared while initializing the grove",
            root.join(".git").display()
        ))
        .with_detail("the existing entry was preserved"));
    }
    let grove = Grove::at(root)?;

    let default_branch = match branch {
        Some(branch) => BString::from(branch.into_vec()),
        None => branch_from_head(
            runner
                .run_ok(Invocation::new().git_dir(grove.bare_dir()).args([
                    "symbolic-ref",
                    "--short",
                    "HEAD",
                ]))?
                .stdout,
        )?,
    };

    metadata::write(
        runner,
        &grove,
        &Metadata {
            version: Some(FORMAT_VERSION),
            default_branch: Some(default_branch.clone()),
            remote: None,
            publish_state: PublishState::Unpublished,
        },
    )?;

    let facts = Facts {
        remote: None,
        default_branch: default_branch.clone(),
        published: false,
        narrowed: false,
    };
    let agents = root.join("AGENTS.md");
    if !fsx::write_atomic_if_absent(&agents, agents_md::render(&facts).as_bytes())? {
        return Err(GroveError::usage(format!(
            "{} appeared while initializing the grove",
            agents.display()
        ))
        .with_detail("the existing entry was preserved"));
    }

    let claude = root.join("CLAUDE.md");
    if !fsx::symlink_relative_if_absent(&claude, "AGENTS.md")? {
        return Err(GroveError::usage(format!(
            "{} appeared while initializing the grove",
            claude.display()
        ))
        .with_detail("the existing entry was preserved"));
    }

    let branch_path = Path::new(OsStr::from_bytes(default_branch.as_ref()));
    let worktree = layout::validate_worktree_path(&grove.root, branch_path)?;
    worktree.create_parent_directories()?;
    let worktree_path = worktree.path();
    let worktree_args = [
        OsString::from("worktree"),
        OsString::from("add"),
        OsString::from("--orphan"),
        OsString::from("-b"),
        OsString::from_vec(default_branch.to_vec()),
        worktree_path.as_os_str().to_os_string(),
    ];
    worktree.validate_vacant()?;
    runner.run_ok(
        Invocation::new()
            .git_dir(grove.bare_dir())
            .args(worktree_args),
    )?;

    println!("ready: {}", grove.root.display());
    println!("next: cd {}", worktree_path.display());
    Ok(grove)
}

pub fn run(
    runner: &dyn GitRunner,
    dir: Option<PathBuf>,
    branch: Option<OsString>,
    cwd: &Path,
) -> Result<Grove> {
    let root = match dir {
        Some(dir) if dir.is_absolute() => dir,
        Some(dir) => cwd.join(dir),
        None => cwd.to_path_buf(),
    };
    let mut mutated = false;
    match run_inner(runner, &root, branch, &mut mutated) {
        Err(error) if mutated => Err(retain_partial(error, &root)),
        result => result,
    }
}
