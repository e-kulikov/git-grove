use crate::cli::AddMode;
use crate::error::{GroveError, Result};
use crate::git::query;
use crate::git::runner::{GitOutput, GitRunner, Invocation};
use crate::grove::discover::Grove;
use crate::grove::layout;
use bstr::{BString, ByteSlice};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

fn escaped(bytes: &[u8]) -> String {
    bytes.escape_bytes().to_string()
}

fn one_line(mut bytes: Vec<u8>, what: &str) -> Result<BString> {
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    if bytes.is_empty() || bytes.contains(&b'\n') || bytes.contains(&b'\r') {
        return Err(GroveError::failure(format!(
            "git returned an invalid {what}"
        )));
    }
    Ok(BString::from(bytes))
}

fn run_required(
    runner: &dyn GitRunner,
    invocation: Invocation,
    operation: &str,
) -> Result<GitOutput> {
    let output = runner.run(invocation)?;
    if !output.ok() {
        return Err(GroveError::failure(format!(
            "git {operation} failed with exit status {}",
            output.status
        ))
        .with_detail(escaped(&output.stderr)));
    }
    Ok(output)
}

fn add_worktree(
    runner: &dyn GitRunner,
    grove: &Grove,
    requested: &Path,
    args: Vec<OsString>,
) -> Result<PathBuf> {
    let validated = layout::validate_worktree_path(&grove.root, requested)?;
    let path = validated.path();
    let invocation = Invocation::new().git_dir(grove.bare_dir()).args(args);
    validated.create_parent_directories()?;
    validated.validate_vacant()?;
    run_required(runner, invocation, "worktree add")?;
    println!("next: cd {}", escaped(path.as_os_str().as_bytes()));
    Ok(path)
}

pub fn run(runner: &dyn GitRunner, grove: &Grove, mode: AddMode) -> Result<PathBuf> {
    match mode {
        AddMode::Detached { revision, dir } => detached(runner, grove, revision, dir),
        AddMode::Branch { branch, dir, start } => add_branch(runner, grove, branch, dir, start),
    }
}

fn detached(
    runner: &dyn GitRunner,
    grove: &Grove,
    revision: OsString,
    dir: Option<PathBuf>,
) -> Result<PathBuf> {
    let mut commit = revision.clone();
    commit.push("^{commit}");
    let resolved = run_required(
        runner,
        Invocation::new().git_dir(grove.bare_dir()).args([
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new("--short"),
            OsStr::new("--end-of-options"),
            commit.as_os_str(),
        ]),
        "rev-parse --verify --short",
    )?;
    let short = one_line(resolved.stdout, "abbreviated revision")?;
    let requested = dir.unwrap_or_else(|| {
        let mut name = OsString::from("detached-");
        name.push(OsStr::from_bytes(short.as_ref()));
        PathBuf::from(name)
    });
    let path = if requested.is_absolute() {
        requested.clone()
    } else {
        grove.root.join(&requested)
    };
    add_worktree(
        runner,
        grove,
        &requested,
        vec![
            OsString::from("worktree"),
            OsString::from("add"),
            OsString::from("--detach"),
            path.into_os_string(),
            revision,
        ],
    )
}

fn add_branch(
    runner: &dyn GitRunner,
    grove: &Grove,
    branch: OsString,
    dir: Option<PathBuf>,
    start: Option<OsString>,
) -> Result<PathBuf> {
    query::validate_branch_name(runner, &branch)?;
    let requested = dir.unwrap_or_else(|| PathBuf::from(&branch));
    let path = if requested.is_absolute() {
        requested.clone()
    } else {
        grove.root.join(&requested)
    };
    let local = query::local_branch_exists(runner, grove, &branch)?;
    let args = if local {
        if start.is_some() {
            return Err(GroveError::usage(format!(
                "branch {} already exists locally",
                escaped(branch.as_bytes())
            ))
            .with_detail("--start only applies when the branch has to be created"));
        }
        if let Some(existing_path) = query::branch_checked_out_at(runner, grove, &branch)? {
            return Err(GroveError::needs_decision(format!(
                "branch {} is already checked out at {}",
                escaped(branch.as_bytes()),
                escaped(existing_path.as_ref())
            ))
            .with_detail("choose another branch or remove the existing worktree by hand"));
        }
        vec![
            OsString::from("worktree"),
            OsString::from("add"),
            path.into_os_string(),
            branch,
        ]
    } else {
        let candidates = query::remote_candidates(runner, grove, &branch)?;
        if !candidates.is_empty() && start.is_some() {
            return Err(GroveError::usage(format!(
                "branch {} already exists on a remote",
                escaped(branch.as_bytes())
            ))
            .with_detail("--start only applies when the branch has to be created"));
        }
        match (candidates.as_slice(), start) {
            ([candidate], None) => vec![
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("--track"),
                OsString::from("-b"),
                branch,
                path.into_os_string(),
                OsString::from_vec(candidate.to_vec()),
            ],
            ([_, _, ..], None) => {
                let names = candidates
                    .iter()
                    .map(|candidate| escaped(candidate.as_ref()))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(GroveError::needs_decision(format!(
                    "branch {} matches several remotes: {names}",
                    escaped(branch.as_bytes())
                ))
                .with_detail("choose a remote branch explicitly before retrying"));
            }
            ([], Some(start)) => vec![
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("-b"),
                branch,
                path.into_os_string(),
                start,
            ],
            ([], None) if !query::has_any_commit(runner, grove)? => vec![
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("--orphan"),
                OsString::from("-b"),
                branch,
                path.into_os_string(),
            ],
            ([], None) => {
                return Err(GroveError::usage(format!(
                    "branch {} exists neither locally nor on a remote",
                    escaped(branch.as_bytes())
                ))
                .with_detail("pass --start <revision> to create it"));
            }
            _ => unreachable!("remote candidates with --start were rejected above"),
        }
    };
    add_worktree(runner, grove, &requested, args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::runner::{GitOutput, RecordingFake};

    fn output(status: i32, stdout: &[u8]) -> GitOutput {
        GitOutput {
            status,
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        }
    }

    fn grove() -> (tempfile::TempDir, Grove) {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".bare")).unwrap();
        let grove = Grove {
            root: root.path().canonicalize().unwrap(),
        };
        (root, grove)
    }

    #[test]
    fn generates_exact_existing_branch_worktree_argv() {
        let (_root, grove) = grove();
        let fake = RecordingFake::new();
        fake.push_response(output(0, b""));
        fake.push_response(output(0, b""));
        fake.push_response(output(0, b""));
        fake.push_response(output(0, b""));

        run(
            &fake,
            &grove,
            AddMode::Branch {
                branch: OsString::from("release/1"),
                dir: Some(PathBuf::from("nested/worktree")),
                start: None,
            },
        )
        .unwrap();

        let calls = fake.calls();
        assert_eq!(
            calls.last().unwrap().argv_os(),
            [
                {
                    let mut flag = OsString::from("--git-dir=");
                    flag.push(grove.bare_dir());
                    flag
                },
                OsString::from("worktree"),
                OsString::from("add"),
                grove.root.join("nested/worktree").into_os_string(),
                OsString::from("release/1"),
            ]
        );
    }

    #[test]
    fn generates_exact_remote_tracking_worktree_argv() {
        let (_root, grove) = grove();
        let fake = RecordingFake::new();
        fake.push_response(output(0, b""));
        fake.push_response(output(1, b""));
        fake.push_response(output(0, b"origin\n"));
        fake.push_response(output(0, b"+refs/heads/*:refs/remotes/origin/*\n"));
        fake.push_response(output(0, b""));
        fake.push_response(output(0, b""));

        run(
            &fake,
            &grove,
            AddMode::Branch {
                branch: OsString::from("topic"),
                dir: None,
                start: None,
            },
        )
        .unwrap();

        let argv = fake.calls().last().unwrap().argv_os();
        assert_eq!(
            argv,
            [
                {
                    let mut flag = OsString::from("--git-dir=");
                    flag.push(grove.bare_dir());
                    flag
                },
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("--track"),
                OsString::from("-b"),
                OsString::from("topic"),
                grove.root.join("topic").into_os_string(),
                OsString::from("origin/topic"),
            ]
        );
    }

    #[test]
    fn generates_exact_detached_worktree_argv() {
        let (_root, grove) = grove();
        let fake = RecordingFake::new();
        fake.push_response(output(0, b"deadbee\n"));
        fake.push_response(output(0, b""));

        run(
            &fake,
            &grove,
            AddMode::Detached {
                revision: OsString::from("main~2"),
                dir: None,
            },
        )
        .unwrap();

        assert_eq!(
            fake.calls().last().unwrap().argv_os(),
            [
                {
                    let mut flag = OsString::from("--git-dir=");
                    flag.push(grove.bare_dir());
                    flag
                },
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("--detach"),
                grove.root.join("detached-deadbee").into_os_string(),
                OsString::from("main~2"),
            ]
        );
    }

    #[test]
    fn generates_exact_new_branch_and_orphan_argv() {
        for (has_commit, tail) in [
            (
                true,
                vec![
                    OsString::from("-b"),
                    OsString::from("topic"),
                    OsString::from("PATH"),
                    OsString::from("main~1"),
                ],
            ),
            (
                false,
                vec![
                    OsString::from("--orphan"),
                    OsString::from("-b"),
                    OsString::from("topic"),
                    OsString::from("PATH"),
                ],
            ),
        ] {
            let (_root, grove) = grove();
            let fake = RecordingFake::new();
            fake.push_response(output(0, b""));
            fake.push_response(output(1, b""));
            fake.push_response(output(0, b""));
            fake.push_response(output(
                0,
                if has_commit {
                    b"refs/heads/main\n"
                } else {
                    b""
                },
            ));
            fake.push_response(output(0, b""));

            run(
                &fake,
                &grove,
                AddMode::Branch {
                    branch: OsString::from("topic"),
                    dir: None,
                    start: has_commit.then(|| OsString::from("main~1")),
                },
            )
            .unwrap();

            let mut expected = vec![
                {
                    let mut flag = OsString::from("--git-dir=");
                    flag.push(grove.bare_dir());
                    flag
                },
                OsString::from("worktree"),
                OsString::from("add"),
            ];
            expected.extend(tail.into_iter().map(|arg| {
                if arg == OsStr::new("PATH") {
                    grove.root.join("topic").into_os_string()
                } else {
                    arg
                }
            }));
            assert_eq!(fake.calls().last().unwrap().argv_os(), expected);
        }
    }

    #[test]
    fn git_failure_diagnostics_escape_raw_stderr_reversibly() {
        let (_root, grove) = grove();
        let fake = RecordingFake::new();
        fake.push_response(output(0, b""));
        fake.push_response(output(0, b""));
        fake.push_response(output(0, b""));
        fake.push_response(GitOutput {
            status: 7,
            stdout: Vec::new(),
            stderr: b"bad-\xff".to_vec(),
        });

        let error = run(
            &fake,
            &grove,
            AddMode::Branch {
                branch: OsString::from("main"),
                dir: None,
                start: None,
            },
        )
        .unwrap_err();

        assert_eq!(error.detail.as_deref(), Some(r"bad-\xFF"));
    }
}
