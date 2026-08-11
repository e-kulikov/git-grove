use crate::error::{GroveError, Result};
use crate::git::runner::{GitOutput, GitRunner, Invocation};
use crate::grove::discover::Grove;
use crate::grove::layout;
use bstr::{BString, ByteSlice};
use rustix::fs::{open, Mode, OFlags};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Read;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

const MAX_GIT_POINTER_BYTES: u64 = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRecord {
    pub path: PathBuf,
    pub head: Option<BString>,
    pub branch: Option<BString>,
    pub bare: bool,
    pub detached: bool,
    pub locked: Option<BString>,
    pub prunable: Option<BString>,
}

#[derive(Default)]
struct WorktreeBuilder {
    path: Option<PathBuf>,
    head: Option<BString>,
    branch: Option<BString>,
    bare: bool,
    detached: bool,
    locked: Option<BString>,
    prunable: Option<BString>,
}

impl WorktreeBuilder {
    fn finish(self) -> Result<WorktreeRecord> {
        let path = self
            .path
            .ok_or_else(|| GroveError::failure("worktree record has no path"))?;
        if !path.is_absolute() {
            return Err(GroveError::failure(
                "git returned a non-absolute worktree path",
            ));
        }
        if self.bare {
            if self.head.is_some() || self.branch.is_some() || self.detached {
                return Err(GroveError::failure(
                    "bare worktree record contains checkout fields",
                ));
            }
        } else if self.head.is_none() || self.branch.is_some() == self.detached {
            return Err(GroveError::failure(
                "worktree record must contain HEAD and exactly one of branch or detached",
            ));
        }
        Ok(WorktreeRecord {
            path,
            head: self.head,
            branch: self.branch,
            bare: self.bare,
            detached: self.detached,
            locked: self.locked,
            prunable: self.prunable,
        })
    }
}

pub fn parse_worktrees(raw: &[u8]) -> Result<Vec<WorktreeRecord>> {
    if raw.is_empty() || !raw.ends_with(b"\0\0") {
        return Err(GroveError::failure(
            "git returned a truncated worktree-list protocol",
        ));
    }

    let mut records = Vec::new();
    let mut current = None::<WorktreeBuilder>;
    for field in raw[..raw.len() - 1].split(|byte| *byte == b'\0') {
        if field.is_empty() {
            if let Some(builder) = current.take() {
                records.push(builder.finish()?);
            } else {
                return Err(GroveError::failure(
                    "git returned an empty worktree-list record",
                ));
            }
            continue;
        }
        let (key, value) = field
            .split_once_str(" ")
            .map_or((field, None), |(key, value)| (key, Some(value)));
        if current.is_none() && key != b"worktree" {
            return Err(GroveError::failure(
                "worktree record does not start with its path",
            ));
        }
        let builder = current.get_or_insert_with(WorktreeBuilder::default);
        match key {
            b"worktree" => {
                let value = value
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| GroveError::failure("worktree record has an empty path"))?;
                if builder.path.is_some() {
                    return Err(GroveError::failure(
                        "worktree record contains duplicate path fields",
                    ));
                }
                builder.path = Some(PathBuf::from(OsString::from_vec(value.to_vec())));
            }
            b"HEAD" => {
                let value = value
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| GroveError::failure("worktree record has an empty HEAD"))?;
                if builder.head.replace(BString::from(value)).is_some() {
                    return Err(GroveError::failure(
                        "worktree record contains duplicate HEAD fields",
                    ));
                }
            }
            b"branch" => {
                let value = value
                    .and_then(|value| value.strip_prefix(b"refs/heads/"))
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| GroveError::failure("invalid worktree branch field"))?;
                if builder.branch.replace(BString::from(value)).is_some() {
                    return Err(GroveError::failure(
                        "worktree record contains duplicate branch fields",
                    ));
                }
            }
            b"bare" if value.is_none() => {
                if builder.bare {
                    return Err(GroveError::failure(
                        "worktree record contains duplicate bare fields",
                    ));
                }
                builder.bare = true;
            }
            b"detached" if value.is_none() => {
                if builder.detached {
                    return Err(GroveError::failure(
                        "worktree record contains duplicate detached fields",
                    ));
                }
                builder.detached = true;
            }
            b"locked" => {
                if builder
                    .locked
                    .replace(BString::from(value.unwrap_or_default()))
                    .is_some()
                {
                    return Err(GroveError::failure(
                        "worktree record contains duplicate locked fields",
                    ));
                }
            }
            b"prunable" => {
                if builder
                    .prunable
                    .replace(BString::from(value.unwrap_or_default()))
                    .is_some()
                {
                    return Err(GroveError::failure(
                        "worktree record contains duplicate prunable fields",
                    ));
                }
            }
            b"bare" | b"detached" => {
                return Err(GroveError::failure(
                    "worktree flag field unexpectedly has a value",
                ));
            }
            _ => {}
        }
    }
    if current.is_some() {
        return Err(GroveError::failure(
            "git returned a truncated worktree-list record",
        ));
    }
    Ok(records)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeLocation {
    Valid { admin_dir: PathBuf },
    Missing,
    Invalid,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub upstream: Option<BString>,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub dirty: bool,
    pub upstream_gone: bool,
    pub graph_unknown: bool,
}

fn read_capped_regular(path: &Path, description: &str) -> Result<Option<Vec<u8>>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(GroveError::failure(format!(
                "cannot inspect {description}: {error}"
            )))
        }
    };
    if !metadata.file_type().is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_GIT_POINTER_BYTES
    {
        return Ok(None);
    }
    let file = open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| GroveError::failure(format!("cannot open {description}: {error}")))?;
    let mut contents = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_GIT_POINTER_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(|error| GroveError::failure(format!("cannot read {description}: {error}")))?;
    if contents.len() as u64 > MAX_GIT_POINTER_BYTES {
        return Ok(None);
    }
    Ok(Some(contents))
}

fn pointer_value(contents: &[u8], prefix: &[u8]) -> Option<PathBuf> {
    let line = contents.strip_suffix(b"\n")?;
    if line.is_empty()
        || line
            .iter()
            .any(|byte| matches!(byte, b'\0' | b'\n' | b'\r'))
    {
        return None;
    }
    let value = line.strip_prefix(prefix)?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(value.to_vec())))
}

pub fn inspect_worktree(grove: &Grove, record: &WorktreeRecord) -> Result<WorktreeLocation> {
    let Ok(relative) = record.path.strip_prefix(&grove.root) else {
        return Ok(WorktreeLocation::Invalid);
    };
    if layout::validate_relative_worktree_path(relative).is_err() {
        return Ok(WorktreeLocation::Invalid);
    }
    let metadata = match std::fs::symlink_metadata(&record.path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorktreeLocation::Missing)
        }
        Err(error) => {
            return Err(GroveError::failure(format!(
                "cannot inspect worktree {}: {error}",
                record.path.as_os_str().as_bytes().escape_bytes()
            )))
        }
    };
    if !metadata.file_type().is_dir() {
        return Ok(WorktreeLocation::Invalid);
    }
    match record.path.canonicalize() {
        Ok(canonical) if canonical == record.path => {}
        Ok(_) => return Ok(WorktreeLocation::Invalid),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorktreeLocation::Missing)
        }
        Err(error) => {
            return Err(GroveError::failure(format!(
                "cannot resolve worktree {}: {error}",
                record.path.as_os_str().as_bytes().escape_bytes()
            )))
        }
    }

    let Some(contents) = read_capped_regular(&record.path.join(".git"), "worktree .git pointer")?
    else {
        return Ok(WorktreeLocation::Invalid);
    };
    let Some(mut admin) = pointer_value(&contents, b"gitdir: ") else {
        return Ok(WorktreeLocation::Invalid);
    };
    if !admin.is_absolute() {
        admin = record.path.join(admin);
    }
    let admin = match admin.canonicalize() {
        Ok(admin) => admin,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorktreeLocation::Invalid)
        }
        Err(error) => {
            return Err(GroveError::failure(format!(
                "cannot resolve worktree admin directory: {error}"
            )))
        }
    };
    let worktree_admin_root = grove.bare_dir().join("worktrees");
    if !admin.starts_with(&worktree_admin_root) || admin == worktree_admin_root {
        return Ok(WorktreeLocation::Invalid);
    }
    if !std::fs::symlink_metadata(&admin).is_ok_and(|metadata| metadata.file_type().is_dir()) {
        return Ok(WorktreeLocation::Invalid);
    }
    let Some(back_pointer) = read_capped_regular(&admin.join("gitdir"), "admin gitdir pointer")?
    else {
        return Ok(WorktreeLocation::Invalid);
    };
    let Some(back_pointer) = pointer_value(&back_pointer, b"") else {
        return Ok(WorktreeLocation::Invalid);
    };
    if back_pointer != record.path.join(".git") {
        return Ok(WorktreeLocation::Invalid);
    }
    Ok(WorktreeLocation::Valid { admin_dir: admin })
}

pub fn worktrees(runner: &dyn GitRunner, grove: &Grove) -> Result<Vec<WorktreeRecord>> {
    let output = runner.run(Invocation::new().git_dir(grove.bare_dir()).args([
        "worktree",
        "list",
        "--porcelain",
        "-z",
    ]))?;
    if !output.ok() {
        return Err(failure("worktree list --porcelain -z", &output));
    }
    parse_worktrees(&output.stdout)
}

fn worktree_invocation(record: &WorktreeRecord, admin_dir: &Path) -> Invocation {
    Invocation::new().git_dir(admin_dir).work_tree(&record.path)
}

fn parse_upstream(raw: &[u8]) -> Result<Option<(BString, BString)>> {
    let raw = raw
        .strip_suffix(b"\n")
        .ok_or_else(|| GroveError::failure("git returned a truncated upstream record"))?;
    let fields = raw.split(|byte| *byte == b'\0').collect::<Vec<_>>();
    match fields.as_slice() {
        [b"", b"", b""] => Ok(None),
        [full, short, b""] if !full.is_empty() && !short.is_empty() => {
            Ok(Some((BString::from(*full), BString::from(*short))))
        }
        _ => Err(GroveError::failure(
            "git returned an invalid upstream record",
        )),
    }
}

fn parse_counts(raw: &[u8]) -> Result<(u32, u32)> {
    let line = raw.strip_suffix(b"\n").unwrap_or(raw);
    if line.is_empty() || line.contains(&b'\n') || line.contains(&b'\r') {
        return Err(GroveError::failure(
            "git returned invalid ahead/behind counts",
        ));
    }
    let fields = line
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    let [ahead, behind] = fields.as_slice() else {
        return Err(GroveError::failure(
            "git returned invalid ahead/behind counts",
        ));
    };
    let parse = |value: &[u8]| -> Result<u32> {
        if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
            return Err(GroveError::failure(
                "git returned invalid ahead/behind counts",
            ));
        }
        std::str::from_utf8(value)
            .expect("ASCII digits are UTF-8")
            .parse()
            .map_err(|_| GroveError::failure("git returned invalid ahead/behind counts"))
    };
    Ok((parse(ahead)?, parse(behind)?))
}

pub(crate) fn status_at(
    runner: &dyn GitRunner,
    record: &WorktreeRecord,
    admin_dir: &Path,
) -> Result<Status> {
    let base = || worktree_invocation(record, admin_dir);
    let dirty = runner.run(base().args([
        "--no-optional-locks",
        "status",
        "--porcelain=v2",
        "-z",
        "--untracked-files=all",
        "--ignore-submodules=none",
    ]))?;
    if !dirty.ok() {
        return Err(failure("status --porcelain=v2 -z", &dirty));
    }
    let mut status = Status {
        dirty: !dirty.stdout.is_empty(),
        ..Status::default()
    };
    let unborn = record
        .head
        .as_ref()
        .is_some_and(|head| head.iter().all(|byte| *byte == b'0'));
    if unborn || record.detached || record.branch.is_none() {
        return Ok(status);
    }

    let branch = record.branch.as_ref().expect("branch checked above");
    let mut branch_ref = OsString::from("refs/heads/");
    branch_ref.push(OsStr::from_bytes(branch.as_ref()));
    let upstream = runner.run(base().args([
        OsStr::new("for-each-ref"),
        OsStr::new("--format=%(upstream)%00%(upstream:short)%00"),
        OsStr::new("--"),
        branch_ref.as_os_str(),
    ]))?;
    if !upstream.ok() {
        return Err(failure("for-each-ref upstream", &upstream));
    }
    let Some((full_upstream, short_upstream)) = parse_upstream(&upstream.stdout)? else {
        return Ok(status);
    };
    status.upstream = Some(short_upstream);

    let exists = runner.run(base().args([
        OsStr::new("show-ref"),
        OsStr::new("--verify"),
        OsStr::new("--quiet"),
        OsStr::from_bytes(full_upstream.as_ref()),
    ]))?;
    match exists.status {
        0 => {}
        1 => {
            status.upstream_gone = true;
            return Ok(status);
        }
        _ => return Err(failure("show-ref --verify upstream", &exists)),
    }

    let mut range = OsString::from("HEAD...");
    range.push(OsStr::from_bytes(full_upstream.as_ref()));
    let counts = runner.run(base().args([
        OsStr::new("rev-list"),
        OsStr::new("--left-right"),
        OsStr::new("--count"),
        range.as_os_str(),
    ]))?;
    if !counts.ok() {
        status.graph_unknown = true;
        return Ok(status);
    }
    let (ahead, behind) = parse_counts(&counts.stdout)?;
    status.ahead = Some(ahead);
    status.behind = Some(behind);
    Ok(status)
}

pub fn status(runner: &dyn GitRunner, grove: &Grove, record: &WorktreeRecord) -> Result<Status> {
    match inspect_worktree(grove, record)? {
        WorktreeLocation::Valid { admin_dir } => status_at(runner, record, &admin_dir),
        WorktreeLocation::Missing => Err(GroveError::needs_decision("worktree is missing")),
        WorktreeLocation::Invalid => Err(GroveError::needs_decision("worktree is invalid")),
    }
}

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
    use std::os::unix::ffi::OsStrExt;

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

    #[test]
    fn parses_nul_delimited_worktree_records_without_losing_bytes() {
        let raw = b"worktree /g/.bare\0bare\0\0\
                    worktree /g/topic-\xfe\0HEAD abc\0branch refs/heads/topic-\xff\0\0\
                    worktree /g/rev\0HEAD def\0detached\0locked review-\xfd\0\0\
                    worktree /g/missing\0HEAD 012\0branch refs/heads/missing\0prunable gitdir file points to non-existent location\0\0";

        let records = parse_worktrees(raw).unwrap();

        assert_eq!(records.len(), 4);
        assert!(records[0].bare);
        assert_eq!(records[1].path.as_os_str().as_bytes(), b"/g/topic-\xfe");
        assert_eq!(
            records[1].branch.as_ref().map(BString::as_ref),
            Some(b"topic-\xff".as_slice())
        );
        assert!(records[2].detached);
        assert_eq!(
            records[2].locked.as_ref().map(BString::as_ref),
            Some(b"review-\xfd".as_slice())
        );
        assert_eq!(
            records[3].prunable.as_ref().map(BString::as_ref),
            Some(b"gitdir file points to non-existent location".as_slice())
        );
    }

    #[test]
    fn rejects_malformed_truncated_and_duplicate_worktree_records() {
        for raw in [
            b"worktree /g/main\0HEAD abc\0branch refs/heads/main\0".as_slice(),
            b"HEAD abc\0branch refs/heads/main\0\0",
            b"worktree /g/main\0worktree /g/other\0HEAD abc\0branch refs/heads/main\0\0",
            b"worktree /g/main\0HEAD abc\0HEAD def\0branch refs/heads/main\0\0",
            b"worktree relative\0HEAD abc\0branch refs/heads/main\0\0",
            b"worktree /g/main\0HEAD abc\0branch refs/heads/main\0detached\0\0",
            b"worktree /g/main\0HEAD abc\0branch refs/heads/main\0\0\0",
            b"HEAD abc\0worktree /g/main\0branch refs/heads/main\0\0",
        ] {
            let error = parse_worktrees(raw).unwrap_err();
            assert_eq!(error.class, ExitClass::Failure, "accepted {raw:?}");
        }
    }

    fn registered_worktree() -> (tempfile::TempDir, Grove, WorktreeRecord, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let bare = root_path.join(".bare");
        let admin = bare.join("worktrees/main");
        let path = root_path.join("main");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        let mut back_pointer = path.join(".git").as_os_str().as_bytes().to_vec();
        back_pointer.push(b'\n');
        std::fs::write(admin.join("gitdir"), back_pointer).unwrap();
        let record = WorktreeRecord {
            path,
            head: Some(BString::from("abc")),
            branch: Some(BString::from("main")),
            bare: false,
            detached: false,
            locked: None,
            prunable: None,
        };
        (root, Grove { root: root_path }, record, admin)
    }

    #[test]
    fn pins_every_worktree_query_to_the_pointer_admin_directory() {
        let (_root, grove, record, admin) = registered_worktree();
        let fake = RecordingFake::new();
        fake.push_response(output(0, b"", b""));
        fake.push_response(output(0, b"refs/remotes/origin/main\0origin/main\0\n", b""));
        fake.push_response(output(0, b"", b""));
        fake.push_response(output(0, b"2\t3\n", b""));

        let status = status(&fake, &grove, &record).unwrap();

        assert!(!status.dirty);
        assert_eq!(
            status.upstream.as_ref().map(BString::as_ref),
            Some(b"origin/main".as_slice())
        );
        assert_eq!((status.ahead, status.behind), (Some(2), Some(3)));
        for call in fake.calls() {
            let argv = call.argv_os();
            assert_eq!(argv[0].as_bytes(), {
                let mut expected = b"--git-dir=".to_vec();
                expected.extend_from_slice(admin.as_os_str().as_bytes());
                expected
            });
            assert_eq!(argv[1].as_bytes(), {
                let mut expected = b"--work-tree=".to_vec();
                expected.extend_from_slice(record.path.as_os_str().as_bytes());
                expected
            });
        }
        assert_eq!(
            fake.calls()[0].argv_for_test()[2..],
            [
                "--no-optional-locks",
                "status",
                "--porcelain=v2",
                "-z",
                "--untracked-files=all",
                "--ignore-submodules=none",
            ]
        );
    }

    #[test]
    fn distinguishes_absent_upstream_from_unexpected_git_errors() {
        let (_root, grove, record, _admin) = registered_worktree();
        let no_upstream = RecordingFake::new();
        no_upstream.push_response(output(0, b"", b""));
        no_upstream.push_response(output(0, b"\0\0\n", b""));

        let local_status = status(&no_upstream, &grove, &record).unwrap();
        assert!(local_status.upstream.is_none());
        assert_eq!((local_status.ahead, local_status.behind), (None, None));

        for responses in [
            vec![output(7, b"", b"status broke")],
            vec![output(0, b"", b""), output(7, b"", b"upstream broke")],
        ] {
            let fake = RecordingFake::new();
            for response in responses {
                fake.push_response(response);
            }
            let error = status(&fake, &grove, &record).unwrap_err();
            assert_eq!(error.class, ExitClass::Failure);
        }
    }

    #[test]
    fn reads_the_worktree_list_from_the_canonical_bare_directory() {
        let fake = RecordingFake::new();
        fake.push_response(output(0, b"worktree /g/.bare\0bare\0\0", b""));

        let rows = worktrees(&fake, &grove()).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(
            fake.calls()[0].argv_for_test(),
            [
                "--git-dir=/g/.bare",
                "worktree",
                "list",
                "--porcelain",
                "-z"
            ]
        );
    }
}
