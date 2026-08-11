use crate::error::{GroveError, Result};
use crate::git::runner::{GitOutput, GitRunner, Invocation};
use crate::grove::discover::Grove;
use bstr::{BString, ByteSlice};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;

fn failure(operation: &str, output: &GitOutput) -> GroveError {
    GroveError::failure(format!(
        "git {operation} failed with exit status {}",
        output.status
    ))
    .with_detail(output.stderr.as_slice().escape_bytes().to_string())
}

fn bare(grove: &Grove) -> Invocation {
    Invocation::new().git_dir(grove.bare_dir())
}

fn branch_ref(branch: &OsStr) -> OsString {
    let mut name = OsString::from("refs/heads/");
    name.push(branch);
    name
}

pub fn validate_branch_name(runner: &dyn GitRunner, branch: &OsStr) -> Result<()> {
    if branch.as_bytes().starts_with(b"-") {
        return Err(GroveError::usage(format!(
            "branch name {} must not start with a dash",
            branch.as_bytes().escape_bytes()
        )));
    }
    let output = runner.run(Invocation::new().args([
        OsStr::new("check-ref-format"),
        OsStr::new("--branch"),
        branch,
    ]))?;
    if output.ok() {
        Ok(())
    } else {
        Err(GroveError::usage(format!(
            "{} is not a valid short branch name",
            branch.as_bytes().escape_bytes()
        )))
    }
}

pub fn local_branch_exists(runner: &dyn GitRunner, grove: &Grove, branch: &OsStr) -> Result<bool> {
    let reference = branch_ref(branch);
    let output = runner.run(bare(grove).args([
        OsStr::new("show-ref"),
        OsStr::new("--verify"),
        OsStr::new("--quiet"),
        reference.as_os_str(),
    ]))?;
    match output.status {
        0 => Ok(true),
        1 => Ok(false),
        _ => Err(failure("show-ref --verify", &output)),
    }
}

pub fn has_any_commit(runner: &dyn GitRunner, grove: &Grove) -> Result<bool> {
    let output = runner.run(bare(grove).args([
        "for-each-ref",
        "--count=1",
        "--format=%(refname)",
        "refs/heads",
    ]))?;
    if !output.ok() {
        return Err(failure("for-each-ref", &output));
    }
    Ok(!output.stdout.is_empty())
}

pub fn branch_checked_out_at(
    runner: &dyn GitRunner,
    grove: &Grove,
    branch: &OsStr,
) -> Result<Option<BString>> {
    let output = runner.run(bare(grove).args(["worktree", "list", "--porcelain", "-z"]))?;
    if !output.ok() {
        return Err(failure("worktree list --porcelain -z", &output));
    }
    let expected = branch_ref(branch);
    let mut path = None::<BString>;
    for field in output.stdout.split(|byte| *byte == b'\0') {
        if field.is_empty() {
            path = None;
        } else if let Some(value) = field.strip_prefix(b"worktree ") {
            path = Some(BString::from(value));
        } else if field
            .strip_prefix(b"branch ")
            .is_some_and(|value| value == expected.as_bytes())
        {
            return path.map(Some).ok_or_else(|| {
                GroveError::failure("git returned a branch before its worktree path")
            });
        }
    }
    Ok(None)
}

fn pattern_match<'a>(pattern: &[u8], value: &'a [u8]) -> Option<&'a [u8]> {
    let star = pattern.iter().position(|byte| *byte == b'*');
    match star {
        None => (pattern == value).then_some(&value[value.len()..]),
        Some(star) if !pattern[star + 1..].contains(&b'*') => {
            let prefix = &pattern[..star];
            let suffix = &pattern[star + 1..];
            value
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix(suffix))
        }
        Some(_) => None,
    }
}

fn map_refspec(spec: &[u8], source_ref: &[u8]) -> Option<Vec<u8>> {
    let spec = spec.strip_prefix(b"+").unwrap_or(spec);
    if spec.starts_with(b"^") {
        return None;
    }
    let (source, destination) = spec.split_once_str(":")?;
    let matched = pattern_match(source, source_ref)?;
    let stars = destination.iter().filter(|byte| **byte == b'*').count();
    match stars {
        0 if matched.is_empty() => Some(destination.to_vec()),
        1 => {
            let star = destination.iter().position(|byte| *byte == b'*')?;
            let mut mapped = Vec::with_capacity(destination.len() + matched.len());
            mapped.extend_from_slice(&destination[..star]);
            mapped.extend_from_slice(matched);
            mapped.extend_from_slice(&destination[star + 1..]);
            Some(mapped)
        }
        _ => None,
    }
}

fn excludes_source(spec: &[u8], source_ref: &[u8]) -> bool {
    spec.strip_prefix(b"^")
        .is_some_and(|pattern| pattern_match(pattern, source_ref).is_some())
}

/// Return existing remote-tracking refs mapped from `refs/heads/<branch>` by
/// each remote's configured fetch refspec.
pub fn remote_candidates(
    runner: &dyn GitRunner,
    grove: &Grove,
    branch: &OsStr,
) -> Result<Vec<BString>> {
    let source_ref = branch_ref(branch);
    let remotes = runner.run(bare(grove).args(["remote"]))?;
    if !remotes.ok() {
        return Err(failure("remote", &remotes));
    }

    let mut found = Vec::<BString>::new();
    for remote in remotes.stdout.as_slice().lines() {
        let remote = remote.strip_suffix(b"\r").unwrap_or(remote);
        if remote.is_empty() {
            continue;
        }
        let mut key = Vec::with_capacity(remote.len() + 13);
        key.extend_from_slice(b"remote.");
        key.extend_from_slice(remote);
        key.extend_from_slice(b".fetch");
        let refspecs = runner.run(bare(grove).args([
            OsStr::new("config"),
            OsStr::new("--get-all"),
            OsStr::from_bytes(&key),
        ]))?;
        let specs = match refspecs.status {
            0 => refspecs
                .stdout
                .as_slice()
                .lines()
                .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>(),
            1 => continue,
            _ => return Err(failure("config --get-all remote.*.fetch", &refspecs)),
        };
        if specs
            .iter()
            .any(|spec| excludes_source(spec, source_ref.as_bytes()))
        {
            continue;
        }
        for spec in specs {
            let Some(mapped) = map_refspec(spec, source_ref.as_bytes()) else {
                continue;
            };
            if mapped.ends_with(b"/HEAD") {
                continue;
            }
            let mapped_os = OsStr::from_bytes(&mapped);
            let exists = runner.run(bare(grove).args([
                OsStr::new("show-ref"),
                OsStr::new("--verify"),
                OsStr::new("--quiet"),
                mapped_os,
            ]))?;
            match exists.status {
                0 => {
                    let short = mapped.strip_prefix(b"refs/remotes/").unwrap_or(&mapped);
                    found.push(BString::from(short));
                }
                1 => {}
                _ => return Err(failure("show-ref --verify", &exists)),
            }
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ExitClass;
    use crate::git::runner::{GitOutput, RecordingFake};

    fn output(status: i32, stdout: &[u8], stderr: &[u8]) -> GitOutput {
        GitOutput {
            status,
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    fn grove() -> Grove {
        Grove { root: "/g".into() }
    }

    #[test]
    fn maps_wildcard_and_narrowed_fetch_refspecs() {
        assert_eq!(
            map_refspec(
                b"+refs/heads/*:refs/remotes/origin/*",
                b"refs/heads/release/1"
            ),
            Some(b"refs/remotes/origin/release/1".to_vec())
        );
        assert_eq!(
            map_refspec(
                b"+refs/heads/main:refs/remotes/upstream/main",
                b"refs/heads/main"
            ),
            Some(b"refs/remotes/upstream/main".to_vec())
        );
        assert_eq!(
            map_refspec(
                b"+refs/heads/main:refs/remotes/upstream/main",
                b"refs/heads/other"
            ),
            None
        );
    }

    #[test]
    fn resolves_candidates_through_each_fetch_refspec_and_sorts_them() {
        let fake = RecordingFake::new();
        fake.push_response(output(0, b"origin\nup/stream\n", b""));
        fake.push_response(output(0, b"+refs/heads/*:refs/remotes/origin/*\n", b""));
        fake.push_response(output(0, b"", b""));
        fake.push_response(output(
            0,
            b"+refs/heads/topic:refs/remotes/up/stream/topic\n",
            b"",
        ));
        fake.push_response(output(0, b"", b""));

        let candidates = remote_candidates(&fake, &grove(), OsStr::new("topic")).unwrap();

        assert_eq!(
            candidates,
            [
                BString::from("origin/topic"),
                BString::from("up/stream/topic")
            ]
        );
        let calls = fake.calls();
        assert_eq!(
            calls[3].argv_for_test(),
            [
                "--git-dir=/g/.bare",
                "config",
                "--get-all",
                "remote.up/stream.fetch"
            ]
        );
    }

    #[test]
    fn does_not_treat_unexpected_query_status_as_absence() {
        let fake = RecordingFake::new();
        fake.push_response(output(7, b"", b"broken query"));

        let error = local_branch_exists(&fake, &grove(), OsStr::new("main")).unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
        assert!(error.message.contains("exit status 7"));
        assert_eq!(error.detail.as_deref(), Some(r"broken\x20query"));
    }

    #[test]
    fn parses_checked_out_branch_and_path_as_raw_bytes() {
        let fake = RecordingFake::new();
        fake.push_response(output(
            0,
            b"worktree /g/work-\xfe\0HEAD deadbeef\0branch refs/heads/topic-\xff\0\0",
            b"",
        ));

        let branch = OsStr::from_bytes(b"topic-\xff");
        let path = branch_checked_out_at(&fake, &grove(), branch).unwrap();

        assert_eq!(path, Some(BString::from(b"/g/work-\xfe".as_slice())));
    }
}
