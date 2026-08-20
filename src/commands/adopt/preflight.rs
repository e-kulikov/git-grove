use super::inventory::{self, Inventory};
use super::{AdoptAction, AdoptArgs};
use crate::error::{GroveError, Result};
use crate::fsx::held::HeldDirectory;
use crate::fsx::lock::{GroveLock, LockMode};
use crate::fsx::mountinfo::MountTable;
use crate::git::query;
use crate::git::runner::{GitOutput, GitRunner, Invocation};
use crate::transaction::journal::{
    AdoptArgumentsProof, AdoptDecisionsProof, ByteSnapshot, HeadProof, JournalEnvironment,
    JournalInvocation, NamedBlobProof, OriginalEvidence, RawBytes, RootProof,
};
use bstr::ByteSlice;
use rustix::fs::FileType;
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

const ACTIVE_MARKERS: &[&str] = &[
    "MERGE_HEAD",
    "rebase-merge",
    "rebase-apply",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "sequencer",
    "BISECT_LOG",
    "BISECT_START",
    "MERGE_AUTOSTASH",
];

const EXPLICIT_PRIVATE: &[&str] = &[
    "index",
    "HEAD",
    "logs/HEAD",
    "ORIG_HEAD",
    "COMMIT_EDITMSG",
    "FETCH_HEAD",
    "AUTO_MERGE",
    "config.worktree",
];

#[derive(Debug)]
pub struct AdoptPlan {
    pub root: HeldDirectory,
    pub repository_lock: GroveLock,
    pub root_proof: RootProof,
    pub arguments: AdoptArgumentsProof,
    pub decisions: AdoptDecisionsProof,
    pub original: OriginalEvidence,
    pub retained_shared_fallthrough: Vec<PathBuf>,
}

pub fn plan(runner: &dyn GitRunner, args: &AdoptArgs, cwd: &Path) -> Result<AdoptPlan> {
    args.validate()?;
    if args.action != AdoptAction::Fresh {
        return Err(GroveError::usage(
            "recovery discovery is unavailable until the forward engine is installed",
        ));
    }
    let root_path = resolve_root(runner, args.path.as_deref(), cwd)?;
    let root = HeldDirectory::open(&root_path)?;
    refuse_reserved_root_entries(&root)?;
    let git_path = root_path.join(".git");
    let metadata = std::fs::symlink_metadata(&git_path).map_err(|error| {
        GroveError::needs_decision(format!("cannot inspect {}: {error}", git_path.display()))
    })?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(GroveError::needs_decision(
            "adopt requires a real .git directory, not a file or symlink",
        ));
    }
    let repository_lock =
        GroveLock::acquire_path(&git_path, LockMode::Exclusive, "git grove adopt")?;
    crate::transaction::signal::activate()?;
    let git = repository_lock.directory();
    MountTable::read_live()?.ensure_no_boundary_at_or_below(&root)?;

    let worktrees = snapshot(
        runner,
        &root_path,
        &["worktree", "list", "--porcelain", "-z"],
    )?;
    validate_single_worktree(&worktrees.bytes.decode(), &root_path)?;
    let initial_inventory = inventory::collect(&root, git)?;
    refuse_active_state(&initial_inventory)?;
    refuse_sparse_submodule_and_conflicts(runner, &root_path, git)?;

    let status = snapshot(
        runner,
        &root_path,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--untracked-files=all",
            "--ignored=matching",
        ],
    )?;
    let stages = snapshot(runner, &root_path, &["ls-files", "--stage", "-z"])?;
    let verbose = snapshot(runner, &root_path, &["ls-files", "-v", "-z"])?;
    let head = resolve_head(runner, &root_path)?;
    let remotes = configured_remotes(runner, &root_path)?;
    let selected_remote = select_remote(args.remote.as_deref(), &remotes)?;
    let default_branch = resolve_default_branch(
        runner,
        &root_path,
        args.default_branch.as_deref(),
        selected_remote.as_deref(),
        &remotes,
        &head,
    )?;
    validate_branch(runner, &root_path, &default_branch)?;
    require_default_exists(
        runner,
        &root_path,
        &default_branch,
        selected_remote.as_deref(),
        &head,
    )?;

    let selected_remote = if remotes.is_empty() {
        None
    } else {
        selected_remote
            .ok_or_else(|| {
                GroveError::needs_decision(
                    "multiple remotes require --remote to select grove.remote during adoption",
                )
            })?
            .into()
    };
    let payload_path = match &head {
        HeadProof::Attached { branch } => branch.clone(),
        HeadProof::Detached { oid } => {
            let bytes = oid.decode();
            let length = bytes.len().min(12);
            let mut path = b"detached-".to_vec();
            path.extend_from_slice(&bytes[..length]);
            RawBytes::from_bytes(&path)
        }
    };
    if payload_path.decode().starts_with(b"-") {
        return Err(GroveError::needs_decision(
            "the payload worktree name would begin with '-'",
        ));
    }
    let default_path = match &head {
        HeadProof::Attached { branch } if branch.decode() == default_branch.as_bytes() => None,
        _ => Some(RawBytes::from_bytes(default_branch.as_bytes())),
    };

    let (classified_private, retained_shared_fallthrough) =
        private_state(runner, &root_path, git, &initial_inventory)?;
    let classified_refs = shared_ref_files(git, &initial_inventory)?;
    let classified_shared_indexes = shared_indexes(runner, &root_path, git)?;
    let inventory = inventory::collect(&root, git)?;
    validate_query_side_effects(&initial_inventory, &inventory)?;
    let private_state = classified_private
        .iter()
        .map(|named| inventory::named_blob(git, &named.path.to_path_buf()))
        .collect::<Result<Vec<_>>>()?;
    let refs = classified_refs
        .iter()
        .map(|named| inventory::named_blob(git, &named.path.to_path_buf()))
        .collect::<Result<Vec<_>>>()?;
    let shared_indexes = classified_shared_indexes
        .iter()
        .map(|named| inventory::named_blob(git, &named.path.to_path_buf()))
        .collect::<Result<Vec<_>>>()?;
    let revalidated = inventory::collect(&root, git)?;
    if revalidated.payload != inventory.payload || revalidated.git_entries != inventory.git_entries
    {
        let detail = inventory
            .git_entries
            .iter()
            .zip(&revalidated.git_entries)
            .find(|(before, after)| before != after)
            .map(|(before, after)| format!("Git entry changed: {before:?} -> {after:?}"))
            .or_else(|| {
                inventory
                    .payload
                    .iter()
                    .zip(&revalidated.payload)
                    .find(|(before, after)| before != after)
                    .map(|(before, after)| {
                        format!("payload entry changed: {before:?} -> {after:?}")
                    })
            })
            .unwrap_or_else(|| "an entry was added or removed".to_string());
        return Err(GroveError::needs_decision(
            "repository contents changed during adopt preflight",
        )
        .with_detail(detail));
    }
    let original = OriginalEvidence {
        repository_identity: git.original_identity(),
        worktree_list_porcelain_z: worktrees,
        status_porcelain_v2_z: status,
        ls_files_stage_z: stages,
        ls_files_verbose_z: verbose,
        payload_manifest: inventory.payload,
        index: inventory::optional_blob(git, Path::new("index"))?,
        shared_indexes,
        config: inventory::blob(git, Path::new("config"))?,
        config_worktree: inventory::optional_blob(git, Path::new("config.worktree"))?,
        head: inventory::blob(git, Path::new("HEAD"))?,
        refs,
        private_state,
    };
    let root_proof = RootProof {
        canonical_path: RawBytes::from_bytes(root_path.as_os_str().as_bytes()),
        identity: root.identity()?,
    };
    let arguments = AdoptArgumentsProof {
        requested_root: RawBytes::from_bytes(
            args.path
                .as_deref()
                .unwrap_or(&root_path)
                .as_os_str()
                .as_bytes(),
        ),
        remote: args
            .remote
            .as_deref()
            .map(|value| RawBytes::from_bytes(value.as_bytes())),
        default_branch: args
            .default_branch
            .as_deref()
            .map(|value| RawBytes::from_bytes(value.as_bytes())),
    };
    let decisions = AdoptDecisionsProof {
        payload_head: head,
        default_branch: RawBytes::from_bytes(default_branch.as_bytes()),
        selected_remote: selected_remote
            .as_deref()
            .map(|value| RawBytes::from_bytes(value.as_bytes())),
        payload_path,
        default_path,
    };
    Ok(AdoptPlan {
        root,
        repository_lock,
        root_proof,
        arguments,
        decisions,
        original,
        retained_shared_fallthrough,
    })
}

fn validate_query_side_effects(before: &Inventory, after: &Inventory) -> Result<()> {
    if before.payload != after.payload
        || before.git_entries.len() != after.git_entries.len()
        || before.git_entries.iter().zip(&after.git_entries).any(
            |((before_path, before), (after_path, after))| {
                before_path != after_path
                    || (before != after
                        && !(before_path
                            .file_name()
                            .is_some_and(|name| name.as_bytes().starts_with(b"sharedindex."))
                            && same_stable_file(before, after)))
            },
        )
    {
        return Err(GroveError::needs_decision(
            "repository contents changed during adopt preflight",
        ));
    }
    Ok(())
}

fn same_stable_file(
    left: &crate::fsx::held::FileIdentity,
    right: &crate::fsx::held::FileIdentity,
) -> bool {
    left.dev == right.dev
        && left.ino == right.ino
        && left.mode == right.mode
        && left.nlink == right.nlink
        && left.size == right.size
        && left.mount_id == right.mount_id
        && left.sha256 == right.sha256
}

fn resolve_root(runner: &dyn GitRunner, requested: Option<&Path>, cwd: &Path) -> Result<PathBuf> {
    let base = requested.unwrap_or(cwd);
    let output = runner.run(
        Invocation::new()
            .cwd(base)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(["rev-parse", "--show-toplevel"]),
    )?;
    if !output.ok() {
        return Err(git_failure("resolve repository top level", &output));
    }
    let path = one_line(output.stdout, "repository top level")?;
    let path = PathBuf::from(OsString::from_vec(path));
    let canonical = path.canonicalize().map_err(|error| {
        GroveError::needs_decision(format!("cannot resolve {}: {error}", path.display()))
    })?;
    if requested.is_some()
        && !base
            .canonicalize()
            .is_ok_and(|base| base.starts_with(&canonical))
    {
        return Err(GroveError::needs_decision(
            "the requested path is not inside the resolved repository",
        ));
    }
    Ok(canonical)
}

fn refuse_reserved_root_entries(root: &HeldDirectory) -> Result<()> {
    for entry in std::fs::read_dir(&root.anchored_path)
        .map_err(|error| GroveError::failure(format!("cannot read repository root: {error}")))?
    {
        let name = entry
            .map_err(|error| GroveError::failure(format!("cannot read repository root: {error}")))?
            .file_name();
        let bytes = name.as_bytes();
        if bytes == b".bare" || bytes.starts_with(b".grove-adopt-") {
            return Err(GroveError::needs_decision(format!(
                "reserved adoption path {} already exists",
                bytes.escape_bytes()
            )));
        }
    }
    Ok(())
}

fn validate_single_worktree(bytes: &[u8], root: &Path) -> Result<()> {
    let records = bytes
        .split(|byte| *byte == 0)
        .filter(|field| field.starts_with(b"worktree "))
        .collect::<Vec<_>>();
    if records.len() != 1
        || records[0].strip_prefix(b"worktree ") != Some(root.as_os_str().as_bytes())
    {
        return Err(GroveError::needs_decision(
            "adopt requires exactly one main worktree at the repository root",
        ));
    }
    Ok(())
}

fn refuse_active_state(inventory: &Inventory) -> Result<()> {
    for marker in ACTIVE_MARKERS {
        if inventory
            .git_entries
            .iter()
            .any(|(path, _)| path == Path::new(marker) || path.starts_with(marker))
        {
            return Err(GroveError::needs_decision(format!(
                "Git operation marker {marker} is active"
            )));
        }
    }
    if let Some((path, _)) = inventory.git_entries.iter().find(|(path, _)| {
        path.file_name()
            .is_some_and(|name| name.as_bytes().ends_with(b".lock"))
    }) {
        return Err(GroveError::needs_decision(format!(
            "Git lock {} exists",
            path.display()
        )));
    }
    Ok(())
}

fn refuse_sparse_submodule_and_conflicts(
    runner: &dyn GitRunner,
    root: &Path,
    git: &HeldDirectory,
) -> Result<()> {
    let conflicts = run_optional_locks(runner, root, &["ls-files", "--unmerged", "-z"])?;
    if !conflicts.stdout.is_empty() {
        return Err(GroveError::needs_decision(
            "the index contains unresolved conflict stages",
        ));
    }
    for key in [
        "core.sparseCheckout",
        "core.sparseCheckoutCone",
        "index.sparse",
    ] {
        let output = run_optional_locks(runner, root, &["config", "--bool", "--get", key])?;
        if output.ok() {
            if one_line(output.stdout, key)? == b"true" {
                return Err(GroveError::needs_decision(format!(
                    "sparse checkout setting {key} is enabled"
                )));
            }
        } else if output.status != 1 {
            return Err(git_failure(&format!("read {key}"), &output));
        }
    }
    if git.anchored_path.join("modules").exists() {
        return Err(GroveError::needs_decision(
            "initialized submodules are not supported by adopt",
        ));
    }
    Ok(())
}

fn resolve_head(runner: &dyn GitRunner, root: &Path) -> Result<HeadProof> {
    let symbolic = run_optional_locks(
        runner,
        root,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    )?;
    if symbolic.ok() {
        return Ok(HeadProof::Attached {
            branch: RawBytes::from_bytes(&one_line(symbolic.stdout, "current branch")?),
        });
    }
    if symbolic.status != 1 {
        return Err(git_failure("read HEAD", &symbolic));
    }
    let oid = run_optional_locks(runner, root, &["rev-parse", "--verify", "HEAD"])?;
    if !oid.ok() {
        return Err(GroveError::needs_decision(
            "an unborn detached HEAD cannot be adopted",
        ));
    }
    Ok(HeadProof::Detached {
        oid: RawBytes::from_bytes(&one_line(oid.stdout, "HEAD object ID")?),
    })
}

fn configured_remotes(runner: &dyn GitRunner, root: &Path) -> Result<Vec<OsString>> {
    let output = run_optional_locks(runner, root, &["remote"])?;
    if !output.ok() {
        return Err(git_failure("list remotes", &output));
    }
    let mut remotes = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|name| !name.is_empty())
        .map(|name| OsString::from_vec(name.to_vec()))
        .collect::<Vec<_>>();
    remotes.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    Ok(remotes)
}

fn select_remote(explicit: Option<&OsStr>, remotes: &[OsString]) -> Result<Option<OsString>> {
    if let Some(explicit) = explicit {
        if !remotes.iter().any(|remote| remote == explicit) {
            return Err(GroveError::needs_decision(format!(
                "remote {} is not configured",
                explicit.as_bytes().escape_bytes()
            )));
        }
        return Ok(Some(explicit.to_os_string()));
    }
    Ok((remotes.len() == 1).then(|| remotes[0].clone()))
}

fn resolve_default_branch(
    runner: &dyn GitRunner,
    root: &Path,
    explicit: Option<&OsStr>,
    selected_remote: Option<&OsStr>,
    remotes: &[OsString],
    head: &HeadProof,
) -> Result<OsString> {
    if let Some(explicit) = explicit {
        return Ok(explicit.to_os_string());
    }
    let remote = selected_remote.or_else(|| (remotes.len() == 1).then(|| remotes[0].as_os_str()));
    if let Some(remote) = remote {
        let mut reference = OsString::from("refs/remotes/");
        reference.push(remote);
        reference.push("/HEAD");
        let output = run_optional_locks_os(
            runner,
            root,
            &[
                OsString::from("symbolic-ref"),
                OsString::from("--quiet"),
                OsString::from("--short"),
                reference,
            ],
        )?;
        if output.ok() {
            let target = one_line(output.stdout, "remote HEAD")?;
            let prefix = [remote.as_bytes(), b"/"].concat();
            let branch = target.strip_prefix(prefix.as_slice()).ok_or_else(|| {
                GroveError::needs_decision("the selected remote HEAD has an unexpected target")
            })?;
            return Ok(OsString::from_vec(branch.to_vec()));
        }
        if output.status != 1 {
            return Err(git_failure("resolve remote HEAD", &output));
        }
    }
    if remotes.is_empty() {
        if let HeadProof::Attached { branch } = head {
            return Ok(branch.as_os_string());
        }
    }
    Err(GroveError::needs_decision(
        "the default branch is ambiguous; pass --default-branch",
    ))
}

fn validate_branch(runner: &dyn GitRunner, root: &Path, branch: &OsStr) -> Result<()> {
    if branch.as_bytes().starts_with(b"-") {
        return Err(GroveError::usage(
            "the default branch must not begin with '-'",
        ));
    }
    let output = run_optional_locks_os(
        runner,
        root,
        &[
            OsString::from("check-ref-format"),
            OsString::from("--branch"),
            branch.to_os_string(),
        ],
    )?;
    if !output.ok() {
        return Err(GroveError::usage(format!(
            "{} is not a valid branch name",
            branch.as_bytes().escape_bytes()
        )));
    }
    Ok(())
}

fn require_default_exists(
    runner: &dyn GitRunner,
    root: &Path,
    branch: &OsStr,
    remote: Option<&OsStr>,
    head: &HeadProof,
) -> Result<()> {
    let mut local = OsString::from("refs/heads/");
    local.push(branch);
    if ref_exists(runner, root, &local)? {
        return Ok(());
    }
    if matches!(head, HeadProof::Attached { branch: current } if current.decode() == branch.as_bytes())
    {
        return Ok(());
    }
    if let Some(remote) = remote {
        let mut tracking = OsString::from("refs/remotes/");
        tracking.push(remote);
        tracking.push("/");
        tracking.push(branch);
        if ref_exists(runner, root, &tracking)? && remote_maps_branch(runner, root, remote, branch)?
        {
            return Ok(());
        }
    }
    Err(GroveError::needs_decision(format!(
        "default branch {} is neither local nor present under the selected remote",
        branch.as_bytes().escape_bytes()
    )))
}

fn remote_maps_branch(
    runner: &dyn GitRunner,
    root: &Path,
    remote: &OsStr,
    branch: &OsStr,
) -> Result<bool> {
    let mut key = OsString::from("remote.");
    key.push(remote);
    key.push(".fetch");
    let output = run_optional_locks_os(
        runner,
        root,
        &[OsString::from("config"), OsString::from("--get-all"), key],
    )?;
    if output.status == 1 {
        return Ok(false);
    }
    if !output.ok() {
        return Err(git_failure("read selected remote fetch refspec", &output));
    }
    let mut source = b"refs/heads/".to_vec();
    source.extend_from_slice(branch.as_bytes());
    let mut expected = b"refs/remotes/".to_vec();
    expected.extend_from_slice(remote.as_bytes());
    expected.push(b'/');
    expected.extend_from_slice(branch.as_bytes());
    let specs = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|spec| !spec.is_empty())
        .collect::<Vec<_>>();
    if specs
        .iter()
        .any(|spec| query::excludes_source(spec, &source))
    {
        return Ok(false);
    }
    Ok(specs
        .iter()
        .any(|spec| query::map_refspec(spec, &source).as_deref() == Some(expected.as_slice())))
}

fn ref_exists(runner: &dyn GitRunner, root: &Path, reference: &OsStr) -> Result<bool> {
    let output = run_optional_locks_os(
        runner,
        root,
        &[
            OsString::from("show-ref"),
            OsString::from("--verify"),
            OsString::from("--quiet"),
            reference.to_os_string(),
        ],
    )?;
    match output.status {
        0 => Ok(true),
        1 => Ok(false),
        _ => Err(git_failure("verify branch", &output)),
    }
}

fn private_state(
    runner: &dyn GitRunner,
    root: &Path,
    git: &HeldDirectory,
    inventory: &Inventory,
) -> Result<(Vec<NamedBlobProof>, Vec<PathBuf>)> {
    let shared_index_names = shared_index_names(runner, root)?;
    let explicit = EXPLICIT_PRIVATE.iter().copied().collect::<HashSet<_>>();
    let mut private = Vec::new();
    let mut fallthrough = Vec::new();
    for (path, identity) in &inventory.git_entries {
        if !FileType::from_raw_mode(identity.mode).is_file() {
            continue;
        }
        let bytes = path.as_os_str().as_bytes();
        let path_string = path.to_string_lossy();
        let migrate = explicit.contains(path_string.as_ref())
            || path.starts_with("refs/worktree")
            || path.starts_with("refs/bisect")
            || path.starts_with("refs/rewritten")
            || shared_index_names
                .iter()
                .any(|name| name.as_os_str() == path.as_os_str())
            || is_object_pseudoref(runner, root, path)?;
        if migrate {
            private.push(inventory::named_blob(git, path)?);
            continue;
        }
        if path.components().count() == 1 && !is_known_shared_top_level(bytes) {
            fallthrough.push(path.clone());
        }
    }
    private.sort_by(|left, right| {
        left.path
            .to_path_buf()
            .as_os_str()
            .as_bytes()
            .cmp(right.path.to_path_buf().as_os_str().as_bytes())
    });
    fallthrough.sort_by(|left, right| {
        left.as_os_str()
            .as_bytes()
            .cmp(right.as_os_str().as_bytes())
    });
    Ok((private, fallthrough))
}

fn is_object_pseudoref(runner: &dyn GitRunner, root: &Path, path: &Path) -> Result<bool> {
    if path.components().count() != 1 {
        return Ok(false);
    }
    let bytes = path.as_os_str().as_bytes();
    if !bytes
        .iter()
        .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_')
    {
        return Ok(false);
    }
    let mut revision = path.as_os_str().to_os_string();
    revision.push("^{object}");
    let output = run_optional_locks_os(
        runner,
        root,
        &[
            OsString::from("rev-parse"),
            OsString::from("--verify"),
            OsString::from("--quiet"),
            OsString::from("--end-of-options"),
            revision,
        ],
    )?;
    Ok(output.ok())
}

fn is_known_shared_top_level(name: &[u8]) -> bool {
    matches!(
        name,
        b"config" | b"description" | b"packed-refs" | b"shallow" | b"gc.log" | b"index.lock"
    )
}

fn shared_ref_files(git: &HeldDirectory, inventory: &Inventory) -> Result<Vec<NamedBlobProof>> {
    let mut refs = Vec::new();
    for (path, identity) in &inventory.git_entries {
        if FileType::from_raw_mode(identity.mode).is_file()
            && (path.starts_with("refs") || path == Path::new("packed-refs"))
            && !path.starts_with("refs/worktree")
            && !path.starts_with("refs/bisect")
            && !path.starts_with("refs/rewritten")
        {
            refs.push(inventory::named_blob(git, path)?);
        }
    }
    Ok(refs)
}

fn shared_indexes(
    runner: &dyn GitRunner,
    root: &Path,
    git: &HeldDirectory,
) -> Result<Vec<NamedBlobProof>> {
    shared_index_names(runner, root)?
        .into_iter()
        .map(|path| inventory::named_blob(git, &path))
        .collect()
}

fn shared_index_names(runner: &dyn GitRunner, root: &Path) -> Result<Vec<PathBuf>> {
    let output = run_optional_locks(runner, root, &["rev-parse", "--shared-index-path"])?;
    if !output.ok() {
        return Err(git_failure("read shared index path", &output));
    }
    let bytes = one_line_allow_empty(output.stdout, "shared index path")?;
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let path = PathBuf::from(OsString::from_vec(bytes));
    let name = path
        .file_name()
        .ok_or_else(|| GroveError::needs_decision("Git returned an invalid shared index path"))?;
    if !name.as_bytes().starts_with(b"sharedindex.") {
        return Err(GroveError::needs_decision(
            "Git returned an unexpected shared index name",
        ));
    }
    Ok(vec![PathBuf::from(name)])
}

fn snapshot(runner: &dyn GitRunner, root: &Path, args: &[&str]) -> Result<ByteSnapshot> {
    let output = run_optional_locks(runner, root, args)?;
    if !output.ok() {
        return Err(git_failure(&args.join(" "), &output));
    }
    let invocation = JournalInvocation {
        git_dir: None,
        work_tree: None,
        cwd: None,
        args: args
            .iter()
            .map(|argument| RawBytes::from_bytes(argument.as_bytes()))
            .collect(),
        environment: vec![JournalEnvironment {
            key: RawBytes::from_bytes(b"GIT_OPTIONAL_LOCKS"),
            value: RawBytes::from_bytes(b"0"),
        }],
    };
    Ok(ByteSnapshot::new(invocation, &output.stdout))
}

fn run_optional_locks(runner: &dyn GitRunner, root: &Path, args: &[&str]) -> Result<GitOutput> {
    runner.run(
        Invocation::new()
            .cwd(root)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(args),
    )
}

fn run_optional_locks_os(
    runner: &dyn GitRunner,
    root: &Path,
    args: &[OsString],
) -> Result<GitOutput> {
    runner.run(
        Invocation::new()
            .cwd(root)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(args),
    )
}

fn one_line(bytes: Vec<u8>, description: &str) -> Result<Vec<u8>> {
    let bytes = one_line_allow_empty(bytes, description)?;
    if bytes.is_empty() {
        return Err(GroveError::failure(format!(
            "Git returned an empty {description}"
        )));
    }
    Ok(bytes)
}

fn one_line_allow_empty(mut bytes: Vec<u8>, description: &str) -> Result<Vec<u8>> {
    while bytes
        .last()
        .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
    {
        bytes.pop();
    }
    if bytes.contains(&b'\n') || bytes.contains(&b'\r') || bytes.contains(&0) {
        return Err(GroveError::failure(format!(
            "Git returned an invalid {description}"
        )));
    }
    Ok(bytes)
}

fn git_failure(action: &str, output: &GitOutput) -> GroveError {
    GroveError::failure(format!("git failed to {action} (exit {})", output.status))
        .with_detail(output.stderr.as_slice().escape_bytes().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::runner::{GitOutput, RecordingFake};

    #[test]
    fn worktree_parser_requires_the_exact_single_root() {
        assert!(
            validate_single_worktree(b"worktree /repo\0HEAD a\0\0", Path::new("/repo")).is_ok()
        );
        assert!(validate_single_worktree(
            b"worktree /repo\0\0worktree /other\0\0",
            Path::new("/repo")
        )
        .is_err());
    }

    #[test]
    fn snapshots_pin_optional_locks_in_the_child_only() {
        let fake = RecordingFake::new();
        fake.push_response(GitOutput {
            status: 0,
            stdout: b"snapshot".to_vec(),
            stderr: vec![],
        });
        let proof = snapshot(&fake, Path::new("/repo"), &["status", "-z"]).unwrap();
        assert_eq!(proof.bytes.decode(), b"snapshot");
        assert_eq!(
            fake.calls()[0].environment_for_test(),
            vec![(OsString::from("GIT_OPTIONAL_LOCKS"), OsString::from("0"))]
        );
    }
}
