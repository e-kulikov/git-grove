use super::preflight::{self, AdoptPlan};
use super::AdoptArgs;
use crate::error::{GroveError, Result};
use crate::fsx::held::{FileSystem, HeldDirectory, RealFileSystem, ValidatedRelativePath};
use crate::git::runner::{GitOutput, GitRunner, Invocation};
use crate::grove::discover::Grove;
use crate::grove::layout;
use crate::grove::metadata::{self, Metadata, PublishState, FORMAT_VERSION};
use crate::transaction::failpoint::Checkpoints;
use crate::transaction::journal::*;
use bstr::{BString, ByteSlice};
use rustix::fs::{mkdirat, Mode};
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

const PHASES: usize = 8;

pub fn run(runner: &dyn GitRunner, args: &AdoptArgs, cwd: &Path) -> Result<()> {
    let plan = preflight::plan(runner, args, cwd)?;
    let root = plan.root.path().to_path_buf();
    let payload_relative = raw_path(&plan.decisions.payload_path)?;
    let default_relative = plan
        .decisions
        .default_path
        .as_ref()
        .map(raw_path)
        .transpose()?;
    validate_layout_destination(&payload_relative)?;
    if let Some(default) = &default_relative {
        validate_layout_destination(default)?;
    }

    let mut nonce = [0_u8; 16];
    getrandom::getrandom(&mut nonce)
        .map_err(|error| GroveError::failure(format!("cannot generate adoption nonce: {error}")))?;
    let nonce_hex = hex(&nonce);
    let transaction_name = OsString::from(format!(".grove-adopt-{nonce_hex}"));
    mkdirat(
        &plan.root.file,
        &transaction_name,
        Mode::from_raw_mode(0o700),
    )
    .map_err(|error| GroveError::failure(format!("cannot create adoption transaction: {error}")))?;
    plan.root.file.sync_all().map_err(|error| {
        GroveError::failure(format!(
            "cannot fsync repository root after transaction creation: {error}"
        ))
    })?;
    let transaction_path = root.join(&transaction_name);
    let transaction = HeldDirectory::open(&transaction_path)?;
    let payload_pointer = payload_pointer(&root, &payload_relative)?;
    if pointer_admin(&payload_pointer)?.exists() {
        return Err(GroveError::needs_decision(
            "the predicted payload administrative directory already exists",
        ));
    }
    let immutable = immutable_plan(
        &plan,
        &payload_relative,
        default_relative.as_deref(),
        &payload_pointer,
    )?;
    let mut journal = Journal {
        schema: JOURNAL_SCHEMA,
        generation: 1,
        nonce,
        root: plan.root_proof.clone(),
        plan: immutable,
        operations: (0..PHASES)
            .map(|index| OperationRecord {
                id: index as u64 + 1,
                state: OperationState::Pending,
                primitive: phase_primitive(index + 1),
            })
            .collect(),
        progress: Progress::Forward,
    };
    let mut checkpoints = Checkpoints::from_env()?;
    durable_replace(&transaction, &journal, &mut checkpoints).map_err(|error| {
        error.with_detail(format!(
            "the transaction is at {}; run `git grove adopt --continue {}` or `git grove adopt --abort {}`",
            transaction_path.as_os_str().as_bytes().escape_bytes(),
            root.as_os_str().as_bytes().escape_bytes(),
            root.as_os_str().as_bytes().escape_bytes()
        ))
    })?;

    let result = execute(
        runner,
        &plan.root,
        &transaction,
        &mut journal,
        &mut checkpoints,
        &payload_relative,
        default_relative.as_deref(),
        &payload_pointer,
    );
    if let Err(error) = result {
        if !transaction.path().exists() {
            return Err(error);
        }
        reconcile_after_signal(&error, &transaction, &journal)?;
        return Err(error.with_detail(format!(
            "adoption can be resumed with `git grove adopt --continue {}` or reversed with `git grove adopt --abort {}`",
            root.as_os_str().as_bytes().escape_bytes(),
            root.as_os_str().as_bytes().escape_bytes()
        )));
    }
    if checkpoints.is_counting() {
        eprintln!("git-grove: failpoint checkpoints: {}", checkpoints.total());
    }

    println!(
        "adopted: {} (default {}, payload {})",
        root.as_os_str().as_bytes().escape_bytes(),
        plan.decisions.default_branch.decode().escape_bytes(),
        plan.decisions.payload_path.decode().escape_bytes()
    );
    for path in &plan.retained_shared_fallthrough {
        eprintln!(
            "git-grove: retained shared Git entry .bare/{}",
            path.as_os_str().as_bytes().escape_bytes()
        );
    }
    Ok(())
}

pub(super) fn resume(
    runner: &dyn GitRunner,
    mut recovered: crate::transaction::recovery::Recovered,
) -> Result<()> {
    let mut checkpoints = Checkpoints::from_env()?;
    match recovered.journal.progress {
        Progress::Forward => {
            let payload = raw_path(&recovered.journal.plan.decisions.payload_path)?;
            let default = recovered
                .journal
                .plan
                .decisions
                .default_path
                .as_ref()
                .map(raw_path)
                .transpose()?;
            let pointer = payload_pointer(recovered.root.path(), &payload)?;
            let result = execute(
                runner,
                &recovered.root,
                &recovered.transaction,
                &mut recovered.journal,
                &mut checkpoints,
                &payload,
                default.as_deref(),
                &pointer,
            );
            if let Err(error) = &result {
                reconcile_after_signal(error, &recovered.transaction, &recovered.journal)?;
            }
            result
        }
        Progress::Committed => cleanup_and_sync(
            &recovered.transaction,
            recovered.root.path(),
            &mut checkpoints,
        ),
        Progress::Aborting | Progress::Aborted => Err(GroveError::needs_decision(
            "this adoption is aborting or aborted and cannot be continued",
        )),
    }
}

pub(super) fn abort(
    runner: &dyn GitRunner,
    mut recovered: crate::transaction::recovery::Recovered,
) -> Result<()> {
    let result = abort_inner(runner, &mut recovered);
    if let Err(error) = &result {
        if recovered.transaction.path().exists() {
            reconcile_after_signal(error, &recovered.transaction, &recovered.journal)?;
        }
    }
    result
}

fn abort_inner(
    runner: &dyn GitRunner,
    recovered: &mut crate::transaction::recovery::Recovered,
) -> Result<()> {
    let mut checkpoints = Checkpoints::from_env()?;
    match recovered.journal.progress {
        Progress::Committed => {
            return Err(GroveError::needs_decision(
                "a committed adoption cannot be aborted",
            ))
        }
        Progress::Aborted => {
            return cleanup_and_sync(
                &recovered.transaction,
                recovered.root.path(),
                &mut checkpoints,
            );
        }
        Progress::Forward => {
            normalize_pending_phase(
                &mut recovered.journal,
                &recovered.root,
                &recovered.transaction,
                &mut checkpoints,
            )?;
            set_progress(
                &mut recovered.journal,
                Progress::Aborting,
                &recovered.transaction,
                &mut checkpoints,
            )?;
        }
        Progress::Aborting => {}
    }

    while let Some(index) = recovered
        .journal
        .operations
        .iter()
        .rposition(|operation| operation.state == OperationState::Done)
    {
        reverse_phase(
            runner,
            &recovered.journal.plan,
            index,
            recovered.root.path(),
            recovered.transaction.path(),
        )?;
        mark_phase_reversed(
            &mut recovered.journal,
            index,
            &recovered.transaction,
            &mut checkpoints,
        )?;
    }
    set_progress(
        &mut recovered.journal,
        Progress::Aborted,
        &recovered.transaction,
        &mut checkpoints,
    )?;
    cleanup_and_sync(
        &recovered.transaction,
        recovered.root.path(),
        &mut checkpoints,
    )
}

fn normalize_pending_phase(
    journal: &mut Journal,
    root: &HeldDirectory,
    transaction: &HeldDirectory,
    checkpoints: &mut Checkpoints,
) -> Result<()> {
    let Some(index) = journal
        .operations
        .iter()
        .position(|operation| operation.state == OperationState::Pending)
    else {
        return Ok(());
    };
    if phase_is_after(&journal.plan, index, root.path(), transaction.path())? {
        finish_phase(journal, index, transaction, checkpoints)?;
    } else if !phase_is_before(&journal.plan, index, root.path(), transaction.path())? {
        return Err(GroveError::needs_decision(format!(
            "adoption phase {} is at neither its exact before nor after state",
            index + 1
        )));
    }
    Ok(())
}

fn phase_is_before(
    plan: &ImmutablePlan,
    index: usize,
    root: &Path,
    transaction: &Path,
) -> Result<bool> {
    let payload_relative = raw_path(&plan.decisions.payload_path)?;
    let payload = root.join(&payload_relative);
    let staging = transaction.join("payload");
    let pointer = payload_pointer(root, &payload_relative)?;
    let admin = pointer_admin(&pointer)?;
    match index {
        0 => Ok(!path_exists(&staging)?),
        1 => Ok(real_directory(&root.join(".git"))? && !path_exists(&root.join(".bare"))?),
        2 => Ok(payload_top_level(plan)?
            .iter()
            .all(|name| root.join(name).exists())),
        3 => Ok(!path_exists(&payload)? && !path_exists(&admin)?),
        4 => Ok(path_exists(&staging)?
            && std::fs::read(payload.join(".git")).ok().as_deref() == Some(pointer.as_slice())),
        5 => {
            for named in &plan.original.private_state {
                if !blob_matches(
                    &root.join(".bare").join(named.path.to_path_buf()),
                    &named.blob,
                )? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        6 => match &plan.decisions.default_path {
            Some(path) => Ok(!path_exists(&root.join(raw_path(path)?))?),
            None => Ok(true),
        },
        7 => {
            if !path_exists(&admin.join("locked"))? {
                return Ok(false);
            }
            if let Some(default) = &plan.decisions.default_path {
                let default_pointer = std::fs::read(root.join(raw_path(default)?).join(".git"))
                    .map_err(|error| {
                        GroveError::needs_decision(format!(
                            "cannot inspect default worktree lock: {error}"
                        ))
                    })?;
                if !path_exists(&pointer_admin(&default_pointer)?.join("locked"))? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        _ => Err(GroveError::failure("unknown adoption phase")),
    }
}

fn phase_is_after(
    plan: &ImmutablePlan,
    index: usize,
    root: &Path,
    transaction: &Path,
) -> Result<bool> {
    let payload_relative = raw_path(&plan.decisions.payload_path)?;
    let payload = root.join(&payload_relative);
    let staging = transaction.join("payload");
    let pointer = payload_pointer(root, &payload_relative)?;
    let admin = pointer_admin(&pointer)?;
    match index {
        0 => real_directory(&staging),
        1 => Ok(real_directory(&root.join(".bare"))?
            && std::fs::read(root.join(".git")).ok().as_deref()
                == Some(layout::POINTER_CONTENTS.as_bytes())),
        2 => Ok(payload_top_level(plan)?
            .iter()
            .all(|name| staging.join(name).exists())),
        3 => Ok(std::fs::read(payload.join(".git")).ok().as_deref() == Some(pointer.as_slice())),
        4 => Ok(!path_exists(&staging)?
            && !path_exists(&transaction.join("payload.git"))?
            && std::fs::read(payload.join(".git")).ok().as_deref() == Some(pointer.as_slice())),
        5 => {
            for named in &plan.original.private_state {
                if !blob_matches(&admin.join(named.path.to_path_buf()), &named.blob)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        6 => match &plan.decisions.default_path {
            Some(path) => Ok(path_exists(&root.join(raw_path(path)?).join(".git"))?),
            None => Ok(true),
        },
        7 => {
            if path_exists(&admin.join("locked"))? {
                return Ok(false);
            }
            if let Some(default) = &plan.decisions.default_path {
                let default_pointer = std::fs::read(root.join(raw_path(default)?).join(".git"))
                    .map_err(|error| {
                        GroveError::needs_decision(format!(
                            "cannot inspect default worktree lock: {error}"
                        ))
                    })?;
                if path_exists(&pointer_admin(&default_pointer)?.join("locked"))? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        _ => Err(GroveError::failure("unknown adoption phase")),
    }
}

fn reverse_phase(
    runner: &dyn GitRunner,
    plan: &ImmutablePlan,
    index: usize,
    root: &Path,
    transaction: &Path,
) -> Result<()> {
    let bare = root.join(".bare");
    let payload_relative = raw_path(&plan.decisions.payload_path)?;
    let payload = root.join(&payload_relative);
    let staging = transaction.join("payload");
    let pointer = payload_pointer(root, &payload_relative)?;
    let admin = pointer_admin(&pointer)?;
    match index {
        7 => relock_generated_worktrees(runner, plan, root, &bare),
        6 => remove_default_worktree(runner, plan, root, &bare),
        5 => restore_private_state(runner, plan, &bare, &admin),
        4 => stage_installed_payload(plan, root, transaction, &payload, &staging),
        3 => remove_payload_registration(runner, &bare, &payload),
        2 => restore_payload_entries(plan, root, &staging),
        1 => restore_git_directory(plan, root, &bare),
        0 => {
            std::fs::remove_dir(&staging).map_err(|error| {
                GroveError::needs_decision(format!(
                    "cannot remove payload staging directory: {error}"
                ))
            })?;
            sync_dir(transaction)
        }
        _ => Err(GroveError::failure("unknown adoption phase")),
    }
}

fn relock_generated_worktrees(
    runner: &dyn GitRunner,
    plan: &ImmutablePlan,
    root: &Path,
    bare: &Path,
) -> Result<()> {
    let reason = format!("git-grove adopt transaction {}", "rollback");
    let payload = root.join(raw_path(&plan.decisions.payload_path)?);
    lock_if_unlocked(runner, bare, &payload, &reason)?;
    if let Some(default) = &plan.decisions.default_path {
        lock_if_unlocked(runner, bare, &root.join(raw_path(default)?), &reason)?;
    }
    Ok(())
}

fn remove_default_worktree(
    runner: &dyn GitRunner,
    plan: &ImmutablePlan,
    root: &Path,
    bare: &Path,
) -> Result<()> {
    let Some(default_relative) = &plan.decisions.default_path else {
        return Ok(());
    };
    let default = root.join(raw_path(default_relative)?);
    if path_exists(&default)? {
        let status = run_ok(
            runner,
            Invocation::new()
                .cwd(&default)
                .args(["status", "--porcelain", "-z"]),
            "verify generated default worktree",
        )?;
        if !status.stdout.is_empty() {
            return Err(GroveError::needs_decision(
                "generated default worktree was modified; refusing rollback",
            ));
        }
        unlock_if_locked(runner, bare, &default)?;
        run_ok(
            runner,
            Invocation::new()
                .git_dir(bare)
                .args(["worktree", "remove", "--force", "--"])
                .arg(&default),
            "remove generated default worktree",
        )?;
    }
    let mut local = b"refs/heads/".to_vec();
    local.extend_from_slice(&plan.decisions.default_branch.decode());
    let existed = plan
        .original
        .refs
        .iter()
        .any(|named| named.path.to_path_buf().as_os_str().as_bytes() == local);
    if !existed {
        run_ok(
            runner,
            Invocation::new()
                .git_dir(bare)
                .args(["update-ref", "-d", "--"])
                .arg(OsString::from_vec(local)),
            "remove generated local default branch",
        )?;
    }
    Ok(())
}

fn restore_private_state(
    runner: &dyn GitRunner,
    plan: &ImmutablePlan,
    bare: &Path,
    admin: &Path,
) -> Result<()> {
    for named in plan.original.private_state.iter().rev() {
        let relative = named.path.to_path_buf();
        let source = admin.join(&relative);
        let destination = bare.join(&relative);
        if blob_matches(&destination, &named.blob)? {
            continue;
        }
        if !blob_matches(&source, &named.blob)? {
            return Err(GroveError::needs_decision(format!(
                "private state {} was modified; refusing rollback",
                relative.as_os_str().as_bytes().escape_bytes()
            )));
        }
        if path_exists(&destination)? {
            let removable = relative == Path::new("HEAD")
                || (relative == Path::new("config.worktree")
                    && config_bool(runner, &destination, "core.bare")?);
            if !removable {
                return Err(GroveError::needs_decision(format!(
                    "private-state destination {} is unexpectedly occupied",
                    relative.as_os_str().as_bytes().escape_bytes()
                )));
            }
            std::fs::remove_file(&destination).map_err(|error| {
                GroveError::failure(format!("cannot remove generated private state: {error}"))
            })?;
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                GroveError::failure(format!("cannot restore private-state parent: {error}"))
            })?;
        }
        std::fs::rename(&source, &destination).map_err(|error| {
            GroveError::failure(format!("cannot restore private state: {error}"))
        })?;
    }
    restore_blob(&bare.join("config"), &plan.original.config)?;
    sync_dir(bare)
}

fn stage_installed_payload(
    plan: &ImmutablePlan,
    root: &Path,
    transaction: &Path,
    payload: &Path,
    staging: &Path,
) -> Result<()> {
    verify_manifest_roots(plan, payload)?;
    let payload_relative = raw_path(&plan.decisions.payload_path)?;
    let expected_pointer = payload_pointer(root, &payload_relative)?;
    if std::fs::read(payload.join(".git")).ok().as_deref() != Some(expected_pointer.as_slice()) {
        return Err(GroveError::needs_decision(
            "generated payload pointer was modified; refusing rollback",
        ));
    }
    let aside = transaction.join("payload.git");
    std::fs::rename(payload.join(".git"), &aside)
        .map_err(|error| GroveError::failure(format!("cannot stage payload pointer: {error}")))?;
    std::fs::rename(payload, staging)
        .map_err(|error| GroveError::failure(format!("cannot restage payload: {error}")))?;
    std::fs::create_dir(payload).map_err(|error| {
        GroveError::failure(format!(
            "cannot recreate generated payload directory: {error}"
        ))
    })?;
    std::fs::rename(&aside, payload.join(".git")).map_err(|error| {
        GroveError::failure(format!("cannot restore generated payload pointer: {error}"))
    })?;
    sync_dir(root)
}

fn remove_payload_registration(runner: &dyn GitRunner, bare: &Path, payload: &Path) -> Result<()> {
    unlock_if_locked(runner, bare, payload)?;
    run_ok(
        runner,
        Invocation::new()
            .git_dir(bare)
            .args(["worktree", "remove", "--force", "--"])
            .arg(payload),
        "remove generated payload registration",
    )?;
    Ok(())
}

fn restore_payload_entries(plan: &ImmutablePlan, root: &Path, staging: &Path) -> Result<()> {
    for name in payload_top_level(plan)? {
        let source = staging.join(&name);
        let destination = root.join(&name);
        if path_exists(&destination)? || !path_exists(&source)? {
            return Err(GroveError::needs_decision(format!(
                "cannot safely restore payload entry {}",
                name.as_bytes().escape_bytes()
            )));
        }
        std::fs::rename(&source, &destination).map_err(|error| {
            GroveError::failure(format!("cannot restore payload entry: {error}"))
        })?;
    }
    sync_dir(root)
}

fn restore_git_directory(plan: &ImmutablePlan, root: &Path, bare: &Path) -> Result<()> {
    restore_blob(&bare.join("config"), &plan.original.config)?;
    let pointer = root.join(".git");
    if std::fs::read(&pointer).ok().as_deref() != Some(layout::POINTER_CONTENTS.as_bytes()) {
        return Err(GroveError::needs_decision(
            "root .git pointer was modified; refusing rollback",
        ));
    }
    std::fs::remove_file(&pointer).map_err(|error| {
        GroveError::failure(format!("cannot remove root .git pointer: {error}"))
    })?;
    std::fs::rename(bare, &pointer)
        .map_err(|error| GroveError::failure(format!("cannot restore .git directory: {error}")))?;
    sync_dir(root)
}

fn verify_manifest_roots(plan: &ImmutablePlan, payload: &Path) -> Result<()> {
    verify_manifest(plan, payload, false)
}

fn verify_manifest(plan: &ImmutablePlan, base: &Path, exact_times: bool) -> Result<()> {
    let held = HeldDirectory::open(base)?;
    let actual_count = held
        .inventory()?
        .into_iter()
        .filter(|entry| entry.path.as_path() != Path::new(".git"))
        .count();
    if actual_count != plan.original.payload_manifest.len() {
        return Err(GroveError::needs_decision(
            "payload entry set changed during adoption",
        ));
    }
    for entry in &plan.original.payload_manifest {
        verify_manifest_entry(&held, entry, exact_times)?;
    }
    Ok(())
}

fn verify_manifest_component(
    plan: &ImmutablePlan,
    base: &Path,
    component: &OsStr,
    exact_times: bool,
) -> Result<()> {
    let held = HeldDirectory::open(base)?;
    for entry in &plan.original.payload_manifest {
        if entry
            .path
            .to_path_buf()
            .components()
            .next()
            .is_some_and(|first| first.as_os_str() == component)
        {
            verify_manifest_entry(&held, entry, exact_times)?;
        }
    }
    Ok(())
}

fn verify_manifest_entry(
    base: &HeldDirectory,
    entry: &ManifestEntry,
    exact_times: bool,
) -> Result<()> {
    let path = entry.path.to_path_buf();
    let relative = ValidatedRelativePath::new(&path)?;
    let actual = RealFileSystem.identity_at(base, &relative)?;
    let expected = entry.identity;
    let stable = actual.dev == expected.dev
        && actual.ino == expected.ino
        && actual.mode == expected.mode
        && actual.nlink == expected.nlink
        && actual.size == expected.size
        && actual.mtime == expected.mtime
        && actual.mount_id == expected.mount_id
        && actual.sha256 == expected.sha256
        && (!exact_times || actual.ctime == expected.ctime);
    if !stable {
        return Err(GroveError::needs_decision(format!(
            "payload entry {} changed during adoption",
            path.as_os_str().as_bytes().escape_bytes()
        )));
    }
    if let ManifestContent::Symlink {
        target,
        sha256: expected_hash,
    } = &entry.content
    {
        let actual_target =
            std::fs::read_link(base.anchored_path.join(&path)).map_err(|error| {
                GroveError::needs_decision(format!("cannot verify payload symlink: {error}"))
            })?;
        let bytes = actual_target.as_os_str().as_bytes();
        if sha256(bytes) != *expected_hash || bytes != target.decode() {
            return Err(GroveError::needs_decision(format!(
                "payload symlink {} changed during adoption",
                path.as_os_str().as_bytes().escape_bytes()
            )));
        }
    }
    Ok(())
}

fn restore_blob(path: &Path, proof: &BlobProof) -> Result<()> {
    if blob_matches(path, proof)? {
        return Ok(());
    }
    crate::fsx::write_atomic(path, &proof.bytes.decode())?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(proof.mode & 0o7777))
        .map_err(|error| GroveError::failure(format!("cannot restore file mode: {error}")))
}

fn mark_phase_reversed(
    journal: &mut Journal,
    index: usize,
    transaction: &HeldDirectory,
    checkpoints: &mut Checkpoints,
) -> Result<()> {
    checkpoints.checkpoint()?;
    let mut next = journal.clone();
    next.generation += 1;
    next.operations[index].state = OperationState::Pending;
    journal.validate_next(&next)?;
    durable_replace(transaction, &next, checkpoints)?;
    *journal = next;
    Ok(())
}

fn set_progress(
    journal: &mut Journal,
    progress: Progress,
    transaction: &HeldDirectory,
    checkpoints: &mut Checkpoints,
) -> Result<()> {
    let mut next = journal.clone();
    next.generation += 1;
    next.progress = progress;
    journal.validate_next(&next)?;
    durable_replace(transaction, &next, checkpoints)?;
    *journal = next;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn execute(
    runner: &dyn GitRunner,
    root_directory: &HeldDirectory,
    transaction: &HeldDirectory,
    journal: &mut Journal,
    checkpoints: &mut Checkpoints,
    payload_relative: &Path,
    default_relative: Option<&Path>,
    predicted_pointer: &[u8],
) -> Result<()> {
    let immutable = journal.plan.clone();
    let plan = &immutable;
    let root = root_directory.path();
    let bare = root.join(".bare");
    let staging = transaction.path().join("payload");

    if pending(journal, 0) {
        match std::fs::symlink_metadata(&staging) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(GroveError::needs_decision(
                    "payload staging path is no longer a real directory",
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&staging).map_err(|error| {
                    GroveError::failure(format!("cannot create payload staging directory: {error}"))
                })?;
            }
            Err(error) => {
                return Err(GroveError::failure(format!(
                    "cannot inspect payload staging directory: {error}"
                )))
            }
        }
        sync_dir(transaction.path())?;
        finish_phase(journal, 0, transaction, checkpoints)?;
    }

    if pending(journal, 1) {
        install_bare_repository(runner, journal, root, &bare)?;
        finish_phase(journal, 1, transaction, checkpoints)?;
    }

    if pending(journal, 2) {
        stage_payload(plan, root, &staging)?;
        finish_phase(journal, 2, transaction, checkpoints)?;
    }

    let payload_path = root.join(payload_relative);
    let reason = format!("git-grove adopt transaction {}", hex(&journal.nonce));
    if pending(journal, 3) {
        create_payload_worktree(
            runner,
            plan,
            root,
            &bare,
            &payload_path,
            &reason,
            predicted_pointer,
        )?;
        finish_phase(journal, 3, transaction, checkpoints)?;
    }
    let generated_pointer = predicted_pointer.to_vec();

    if pending(journal, 4) {
        install_payload(
            transaction.path(),
            root,
            &staging,
            &payload_path,
            predicted_pointer,
        )?;
        finish_phase(journal, 4, transaction, checkpoints)?;
    }

    let admin = pointer_admin(&generated_pointer)?;
    if pending(journal, 5) {
        install_private_state(runner, plan, &bare, &admin, &payload_path)?;
        finish_phase(journal, 5, transaction, checkpoints)?;
    }

    if pending(journal, 6) {
        install_default_worktree(runner, plan, root, &bare, default_relative, &reason)?;
        finish_phase(journal, 6, transaction, checkpoints)?;
    }

    if pending(journal, 7) {
        install_metadata(runner, plan, root)?;
        verify_payload_snapshots(runner, plan, &payload_path)?;
        unlock_if_locked(runner, &bare, &payload_path)?;
        if let Some(default) = default_relative {
            unlock_if_locked(runner, &bare, &root.join(default))?;
        }
        finish_phase(journal, 7, transaction, checkpoints)?;
    }
    let mut committed = journal.clone();
    committed.generation += 1;
    committed.progress = Progress::Committed;
    journal.validate_next(&committed)?;
    durable_replace(transaction, &committed, checkpoints)?;
    *journal = committed;
    cleanup_and_sync(transaction, root, checkpoints)
}

fn immutable_plan(
    plan: &AdoptPlan,
    payload: &Path,
    default: Option<&Path>,
    pointer: &[u8],
) -> Result<ImmutablePlan> {
    let root_mount = plan.root.original_identity().mount_id;
    let pointer_path = payload.join(".git");
    let pointer_proof = PathProof {
        at: Location::Root {
            path: ValidatedBytePath::new(&pointer_path)?,
        },
        identity: created_file(pointer, 0o644, root_mount),
    };
    let mut worktrees = vec![WorktreeProof {
        path: RawBytes::from_bytes(plan.root.path().join(payload).as_os_str().as_bytes()),
        head: plan.decisions.payload_head.clone(),
        locked_reason: None,
        bare: false,
    }];
    if let Some(default) = default {
        worktrees.push(WorktreeProof {
            path: RawBytes::from_bytes(plan.root.path().join(default).as_os_str().as_bytes()),
            head: HeadProof::Attached {
                branch: plan.decisions.default_branch.clone(),
            },
            locked_reason: None,
            bare: false,
        });
    }
    Ok(ImmutablePlan {
        arguments: plan.arguments.clone(),
        decisions: plan.decisions.clone(),
        original: plan.original.clone(),
        generated: GeneratedEvidence {
            payload_pointer: pointer_proof.clone(),
            default_pointer: None,
        },
        expected_final: FinalEvidence {
            worktrees,
            payload_status_porcelain_v2_z: plan.original.status_porcelain_v2_z.clone(),
            payload_ls_files_stage_z: plan.original.ls_files_stage_z.clone(),
            payload_ls_files_verbose_z: plan.original.ls_files_verbose_z.clone(),
            config_values: Vec::new(),
            refs: plan
                .original
                .refs
                .iter()
                .map(|named| PathProof {
                    at: Location::Bare {
                        path: named.path.clone(),
                    },
                    identity: IdentityProof::Known {
                        identity: named.blob.identity,
                    },
                })
                .collect(),
            pointer_files: vec![pointer_proof],
            metadata: Vec::new(),
        },
    })
}

fn phase_primitive(phase: usize) -> Primitive {
    Primitive::Git {
        invocation: JournalInvocation {
            git_dir: None,
            work_tree: None,
            cwd: None,
            args: vec![RawBytes::from_bytes(
                format!("adopt-phase-{phase}").as_bytes(),
            )],
            environment: Vec::new(),
        },
        postcondition: GitPostcondition::All { proofs: Vec::new() },
    }
}

fn finish_phase(
    journal: &mut Journal,
    index: usize,
    transaction: &HeldDirectory,
    checkpoints: &mut Checkpoints,
) -> Result<()> {
    checkpoints.checkpoint()?;
    let mut next = journal.clone();
    next.generation += 1;
    next.operations[index].state = OperationState::Done;
    journal.validate_next(&next)?;
    durable_replace(transaction, &next, checkpoints)?;
    *journal = next;
    Ok(())
}

fn pending(journal: &Journal, index: usize) -> bool {
    journal.operations[index].state == OperationState::Pending
}

fn install_bare_repository(
    runner: &dyn GitRunner,
    journal: &Journal,
    root: &Path,
    bare: &Path,
) -> Result<()> {
    let git = root.join(".git");
    let bare_exists = real_directory(bare)?;
    let git_is_directory = real_directory(&git)?;
    if !bare_exists && git_is_directory {
        let held = HeldDirectory::open(&git)?;
        if !crate::transaction::recovery::same_held_identity(
            held.identity()?,
            journal.plan.original.repository_identity,
        ) {
            return Err(GroveError::needs_decision(
                "the original .git directory no longer matches the journal",
            ));
        }
        std::fs::rename(&git, bare).map_err(|error| {
            GroveError::failure(format!("cannot rename .git to .bare: {error}"))
        })?;
        sync_dir(root)?;
    } else if !bare_exists || git_is_directory {
        return Err(GroveError::needs_decision(
            "repository is at neither the exact pre-conversion nor converted layout",
        ));
    }
    let held_bare = HeldDirectory::open(bare)?;
    if !crate::transaction::recovery::same_held_identity(
        held_bare.identity()?,
        journal.plan.original.repository_identity,
    ) {
        return Err(GroveError::needs_decision(
            "the converted .bare directory no longer matches the original .git inode",
        ));
    }
    match std::fs::read(&git) {
        Ok(bytes) if bytes == layout::POINTER_CONTENTS.as_bytes() => {}
        Ok(_) => {
            return Err(GroveError::needs_decision(
                "the root .git pointer was replaced",
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            crate::fsx::write_atomic_if_absent(&git, layout::POINTER_CONTENTS.as_bytes())?;
        }
        Err(error) => {
            return Err(GroveError::failure(format!(
                "cannot inspect the root .git pointer: {error}"
            )))
        }
    }
    unset_layout(runner, &bare.join("config"))?;
    run_ok(
        runner,
        Invocation::new().args([
            OsStr::new("config"),
            OsStr::new("--file"),
            bare.join("config").as_os_str(),
            OsStr::new("core.bare"),
            OsStr::new("true"),
        ]),
        "mark converted repository bare",
    )?;
    let bare_check = run_ok(
        runner,
        Invocation::new()
            .git_dir(bare)
            .args(["rev-parse", "--is-bare-repository"]),
        "verify bare conversion",
    )?;
    if bare_check.stdout != b"true\n" || !bare_check.stderr.is_empty() {
        return Err(GroveError::needs_decision(
            "Git did not verify the converted repository as warning-free and bare",
        ));
    }
    Ok(())
}

fn stage_payload(plan: &ImmutablePlan, root: &Path, staging: &Path) -> Result<()> {
    for name in payload_top_level(plan)? {
        let source = root.join(&name);
        let destination = staging.join(&name);
        match (path_exists(&source)?, path_exists(&destination)?) {
            (true, false) => {
                verify_manifest_component(plan, root, &name, true)?;
                std::fs::rename(&source, &destination).map_err(|error| {
                    GroveError::failure(format!(
                        "cannot stage payload entry {}: {error}",
                        name.as_bytes().escape_bytes()
                    ))
                })?;
                sync_dir(root)?;
                sync_dir(staging)?;
            }
            (false, true) => verify_manifest_component(plan, staging, &name, false)?,
            _ => {
                return Err(GroveError::needs_decision(format!(
                    "payload entry {} is at neither its exact before nor after location",
                    name.as_bytes().escape_bytes()
                )))
            }
        }
    }
    Ok(())
}

fn create_payload_worktree(
    runner: &dyn GitRunner,
    plan: &ImmutablePlan,
    root: &Path,
    bare: &Path,
    payload_path: &Path,
    reason: &str,
    expected_pointer: &[u8],
) -> Result<()> {
    if !path_exists(payload_path)? {
        create_parents(
            root,
            payload_path.strip_prefix(root).unwrap_or(payload_path),
        )?;
        let mut invocation = Invocation::new().git_dir(bare);
        match &plan.decisions.payload_head {
            HeadProof::Attached { branch } => {
                invocation = invocation
                    .args(["worktree", "add", "--no-checkout", "--lock", "--reason"])
                    .arg(reason)
                    .arg("--")
                    .arg(payload_path)
                    .arg(branch.as_os_string());
            }
            HeadProof::Detached { oid } => {
                invocation = invocation
                    .args([
                        "worktree",
                        "add",
                        "--detach",
                        "--no-checkout",
                        "--lock",
                        "--reason",
                    ])
                    .arg(reason)
                    .arg("--")
                    .arg(payload_path)
                    .arg(oid.as_os_string());
            }
        }
        run_ok(runner, invocation, "create payload worktree")?;
    }
    let generated = std::fs::read(payload_path.join(".git")).map_err(|error| {
        GroveError::needs_decision(format!("cannot verify generated payload pointer: {error}"))
    })?;
    if generated != expected_pointer {
        return Err(GroveError::needs_decision(
            "Git generated an unexpected payload administrative path",
        ));
    }
    Ok(())
}

fn install_payload(
    transaction: &Path,
    root: &Path,
    staging: &Path,
    payload: &Path,
    expected_pointer: &[u8],
) -> Result<()> {
    let aside = transaction.join("payload.git");
    if path_exists(staging)? && path_exists(&payload.join(".git"))? && !path_exists(&aside)? {
        std::fs::rename(payload.join(".git"), &aside).map_err(|error| {
            GroveError::failure(format!(
                "cannot preserve generated payload pointer: {error}"
            ))
        })?;
    }
    if path_exists(&aside)? && path_exists(payload)? {
        std::fs::remove_dir(payload).map_err(|error| {
            GroveError::needs_decision(format!(
                "generated payload directory was not empty: {error}"
            ))
        })?;
    }
    if path_exists(staging)? && !path_exists(payload)? {
        std::fs::rename(staging, payload).map_err(|error| {
            GroveError::failure(format!("cannot install staged payload: {error}"))
        })?;
    }
    if path_exists(&aside)? && !path_exists(&payload.join(".git"))? {
        std::fs::rename(&aside, payload.join(".git")).map_err(|error| {
            GroveError::failure(format!("cannot install payload pointer: {error}"))
        })?;
    }
    if path_exists(staging)?
        || path_exists(&aside)?
        || std::fs::read(payload.join(".git")).ok().as_deref() != Some(expected_pointer)
    {
        return Err(GroveError::needs_decision(
            "payload replacement is at neither its exact before nor after state",
        ));
    }
    sync_dir(root)?;
    sync_dir(payload)
}

fn install_private_state(
    runner: &dyn GitRunner,
    plan: &ImmutablePlan,
    bare: &Path,
    admin: &Path,
    payload: &Path,
) -> Result<()> {
    let worktree_config = config_bool(runner, &bare.join("config"), "extensions.worktreeConfig")?;
    migrate_private_state(runner, plan, bare, admin)?;
    if worktree_config {
        unset_layout(runner, &bare.join("config"))?;
        run_ok(
            runner,
            Invocation::new()
                .args(["config", "--file"])
                .arg(bare.join("config.worktree"))
                .args(["core.bare", "true"]),
            "mark the bare main worktree configuration",
        )?;
    }
    write_bare_head(bare, &plan.decisions.default_branch)?;
    verify_manifest(plan, payload, false)?;
    verify_payload_snapshots(runner, plan, payload)
}

fn install_default_worktree(
    runner: &dyn GitRunner,
    plan: &ImmutablePlan,
    root: &Path,
    bare: &Path,
    default_relative: Option<&Path>,
    reason: &str,
) -> Result<()> {
    if let Some(default_relative) = default_relative {
        create_parents(root, default_relative)?;
        let default_path = root.join(default_relative);
        if !path_exists(&default_path.join(".git"))? {
            if path_exists(&default_path)? {
                return Err(GroveError::needs_decision(
                    "default worktree path is occupied by partial or foreign state",
                ));
            }
            let default = plan.decisions.default_branch.as_os_string();
            let mut local_ref = OsString::from("refs/heads/");
            local_ref.push(&default);
            let local = runner.run(
                Invocation::new()
                    .git_dir(bare)
                    .args(["show-ref", "--verify", "--quiet"])
                    .arg(&local_ref),
            )?;
            let mut add = Invocation::new()
                .git_dir(bare)
                .args(["worktree", "add", "--lock", "--reason"])
                .arg(reason);
            if local.status == 0 {
                add = add.arg("--").arg(&default_path).arg(&default);
            } else if local.status == 1 {
                let remote = plan.decisions.selected_remote.as_ref().ok_or_else(|| {
                    GroveError::needs_decision("remote-only default branch has no selected remote")
                })?;
                let mut tracking = remote.as_os_string();
                tracking.push("/");
                tracking.push(&default);
                add = add
                    .args(["--track", "-b"])
                    .arg(&default)
                    .arg("--")
                    .arg(&default_path)
                    .arg(tracking);
            } else {
                return Err(GroveError::failure(
                    "cannot determine whether the default branch is local",
                ));
            }
            run_ok(runner, add, "create default worktree")?;
        }
    }
    run_ok(
        runner,
        Invocation::new()
            .git_dir(bare)
            .args(["config", "worktree.guessRemote", "true"]),
        "enable worktree remote guessing",
    )?;
    Ok(())
}

fn install_metadata(runner: &dyn GitRunner, plan: &ImmutablePlan, root: &Path) -> Result<()> {
    let grove = Grove {
        root: root.to_path_buf(),
    };
    let remote = plan
        .decisions
        .selected_remote
        .as_ref()
        .map(|value| BString::from(value.decode()));
    metadata::write(
        runner,
        &grove,
        &Metadata {
            version: Some(FORMAT_VERSION),
            default_branch: Some(BString::from(plan.decisions.default_branch.decode())),
            remote: remote.clone(),
            publish_state: if remote.is_some() {
                PublishState::Published
            } else {
                PublishState::Unpublished
            },
            publish_remote: None,
            publish_url: None,
            publish_provider: None,
            publish_owner: None,
            publish_name: None,
        },
    )
}

fn payload_top_level(plan: &ImmutablePlan) -> Result<Vec<OsString>> {
    let mut names = BTreeSet::new();
    for entry in &plan.original.payload_manifest {
        let path = entry.path.to_path_buf();
        let name = path
            .components()
            .next()
            .ok_or_else(|| GroveError::failure("empty payload path"))?;
        names.insert(name.as_os_str().as_bytes().to_vec());
    }
    Ok(names.into_iter().map(OsString::from_vec).collect())
}

fn migrate_private_state(
    runner: &dyn GitRunner,
    plan: &ImmutablePlan,
    bare: &Path,
    admin: &Path,
) -> Result<()> {
    for named in &plan.original.private_state {
        let relative = named.path.to_path_buf();
        let source = bare.join(&relative);
        let destination = admin.join(&relative);
        if blob_matches(&destination, &named.blob)? {
            continue;
        }
        if !blob_matches(&source, &named.blob)? {
            return Err(GroveError::needs_decision(format!(
                "private state {} is at neither its exact source nor destination",
                relative.as_os_str().as_bytes().escape_bytes()
            )));
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                GroveError::failure(format!("cannot create private-state parent: {error}"))
            })?;
        }
        match std::fs::symlink_metadata(&destination) {
            Ok(metadata) if metadata.is_dir() => std::fs::remove_dir(&destination),
            Ok(_) => std::fs::remove_file(&destination),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
        .map_err(|error| {
            GroveError::failure(format!("cannot remove generated private state: {error}"))
        })?;
        std::fs::rename(&source, &destination).map_err(|error| {
            GroveError::failure(format!(
                "cannot migrate {}: {error}",
                relative.as_os_str().as_bytes().escape_bytes()
            ))
        })?;
        if relative == Path::new("config.worktree") {
            unset_layout(runner, &destination)?;
        }
    }
    sync_dir(bare)?;
    sync_dir(admin)
}

fn unset_layout(runner: &dyn GitRunner, config: &Path) -> Result<()> {
    for key in ["core.bare", "core.worktree"] {
        let output = runner.run(
            Invocation::new()
                .args(["config", "--file"])
                .arg(config)
                .args(["--unset-all", key]),
        )?;
        if output.status != 0 && output.status != 5 {
            return Err(git_error(&format!("unset {key}"), &output));
        }
    }
    Ok(())
}

fn config_bool(runner: &dyn GitRunner, config: &Path, key: &str) -> Result<bool> {
    let output = runner.run(
        Invocation::new()
            .args(["config", "--file"])
            .arg(config)
            .args(["--bool", "--get", key]),
    )?;
    match output.status {
        0 if output.stdout == b"true\n" => Ok(true),
        0 if output.stdout == b"false\n" => Ok(false),
        1 => Ok(false),
        _ => Err(git_error(&format!("read {key}"), &output)),
    }
}

fn write_bare_head(bare: &Path, branch: &RawBytes) -> Result<()> {
    let mut bytes = b"ref: refs/heads/".to_vec();
    bytes.extend_from_slice(&branch.decode());
    bytes.push(b'\n');
    crate::fsx::write_atomic(&bare.join("HEAD"), &bytes)
}

fn verify_payload_snapshots(
    runner: &dyn GitRunner,
    plan: &ImmutablePlan,
    payload: &Path,
) -> Result<()> {
    for (args, expected, description) in [
        (
            vec![
                "status",
                "--porcelain=v2",
                "-z",
                "--branch",
                "--untracked-files=all",
                "--ignored=matching",
            ],
            &plan.original.status_porcelain_v2_z,
            "status",
        ),
        (
            vec!["ls-files", "--stage", "-z"],
            &plan.original.ls_files_stage_z,
            "index stages",
        ),
        (
            vec!["ls-files", "-v", "-z"],
            &plan.original.ls_files_verbose_z,
            "index flags",
        ),
    ] {
        let output = runner.run(
            Invocation::new()
                .cwd(payload)
                .env("GIT_OPTIONAL_LOCKS", "0")
                .args(args),
        )?;
        if !output.ok() || output.stdout != expected.bytes.decode() {
            return Err(GroveError::needs_decision(format!(
                "adopted payload {description} differs from the exact preflight snapshot"
            )));
        }
    }
    Ok(())
}

fn payload_pointer(root: &Path, payload: &Path) -> Result<Vec<u8>> {
    let id = payload
        .file_name()
        .ok_or_else(|| GroveError::usage("payload path has no final component"))?;
    let admin = root.join(".bare/worktrees").join(id);
    let mut bytes = b"gitdir: ".to_vec();
    bytes.extend_from_slice(admin.as_os_str().as_bytes());
    bytes.push(b'\n');
    Ok(bytes)
}

fn pointer_admin(pointer: &[u8]) -> Result<PathBuf> {
    let bytes = pointer
        .strip_prefix(b"gitdir: ")
        .and_then(|bytes| bytes.strip_suffix(b"\n"))
        .ok_or_else(|| GroveError::needs_decision("Git generated a malformed payload pointer"))?;
    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

fn raw_path(raw: &RawBytes) -> Result<PathBuf> {
    ValidatedBytePath::new(Path::new(&raw.as_os_string())).map(|path| path.to_path_buf())
}

fn validate_layout_destination(path: &Path) -> Result<()> {
    crate::grove::layout::validate_relative_worktree_path(path).map(|_| ())
}

fn create_parents(root: &Path, relative: &Path) -> Result<()> {
    if let Some(parent) = relative
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(root.join(parent)).map_err(|error| {
            GroveError::failure(format!(
                "cannot create worktree parent directories: {error}"
            ))
        })?;
    }
    Ok(())
}

fn created_file(bytes: &[u8], mode: u32, mount_id: u64) -> IdentityProof {
    IdentityProof::Created {
        object_type: ObjectType::RegularFile,
        mode,
        mount_id,
        sha256: Some(sha256(bytes)),
        symlink_target: None,
    }
}

fn unlock(runner: &dyn GitRunner, bare: &Path, path: &Path) -> Result<()> {
    run_ok(
        runner,
        Invocation::new()
            .git_dir(bare)
            .args(["worktree", "unlock", "--"])
            .arg(path),
        "unlock adopted worktree",
    )?;
    Ok(())
}

fn unlock_if_locked(runner: &dyn GitRunner, bare: &Path, path: &Path) -> Result<()> {
    let pointer = std::fs::read(path.join(".git")).map_err(|error| {
        GroveError::needs_decision(format!(
            "cannot read worktree pointer before unlock: {error}"
        ))
    })?;
    let admin = pointer_admin(&pointer)?;
    if path_exists(&admin.join("locked"))? {
        unlock(runner, bare, path)?;
    }
    Ok(())
}

fn lock_if_unlocked(runner: &dyn GitRunner, bare: &Path, path: &Path, reason: &str) -> Result<()> {
    let pointer = std::fs::read(path.join(".git")).map_err(|error| {
        GroveError::needs_decision(format!(
            "cannot read worktree pointer before relock: {error}"
        ))
    })?;
    let admin = pointer_admin(&pointer)?;
    if !path_exists(&admin.join("locked"))? {
        run_ok(
            runner,
            Invocation::new()
                .git_dir(bare)
                .args(["worktree", "lock", "--reason"])
                .arg(reason)
                .arg("--")
                .arg(path),
            "relock adopted worktree for rollback",
        )?;
    }
    Ok(())
}

fn run_ok(runner: &dyn GitRunner, invocation: Invocation, action: &str) -> Result<GitOutput> {
    let output = runner.run(invocation)?;
    if output.ok() {
        Ok(output)
    } else {
        Err(git_error(action, &output))
    }
}

fn git_error(action: &str, output: &GitOutput) -> GroveError {
    GroveError::failure(format!("git failed to {action} (exit {})", output.status))
        .with_detail(output.stderr.as_slice().escape_bytes().to_string())
}

fn sync_dir(path: &Path) -> Result<()> {
    std::fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| GroveError::failure(format!("cannot fsync {}: {error}", path.display())))
}

fn path_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(GroveError::failure(format!(
            "cannot inspect {}: {error}",
            path.display()
        ))),
    }
}

fn real_directory(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_dir() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(GroveError::failure(format!(
            "cannot inspect {}: {error}",
            path.display()
        ))),
    }
}

fn blob_matches(path: &Path, proof: &BlobProof) -> Result<bool> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(sha256(&bytes) == proof.sha256),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(GroveError::failure(format!(
            "cannot verify {}: {error}",
            path.display()
        ))),
    }
}

fn cleanup_transaction(transaction: &HeldDirectory) -> Result<()> {
    let entries = std::fs::read_dir(transaction.path())
        .map_err(|error| {
            GroveError::failure(format!("cannot inspect committed transaction: {error}"))
        })?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| {
            GroveError::failure(format!("cannot inspect committed transaction: {error}"))
        })?;
    for entry in entries {
        let name = entry.file_name();
        if name != JOURNAL_CURRENT && name != JOURNAL_NEW {
            return Err(GroveError::needs_decision(
                "committed transaction contains an unexpected entry",
            ));
        }
        std::fs::remove_file(entry.path()).map_err(|error| {
            GroveError::failure(format!("cannot remove committed journal: {error}"))
        })?;
    }
    std::fs::remove_dir(transaction.path()).map_err(|error| {
        GroveError::failure(format!("cannot remove committed transaction: {error}"))
    })
}

fn cleanup_and_sync(
    transaction: &HeldDirectory,
    root: &Path,
    checkpoints: &mut Checkpoints,
) -> Result<()> {
    cleanup_transaction(transaction)?;
    post_cleanup_checkpoint(checkpoints)?;
    sync_dir(root)?;
    post_cleanup_checkpoint(checkpoints)
}

fn post_cleanup_checkpoint(checkpoints: &mut Checkpoints) -> Result<()> {
    checkpoints.checkpoint().map_err(|error| {
        if error.message.starts_with("injected failure") {
            GroveError::failure(error.message)
                .with_detail("adoption is already committed and its transaction is cleaned up")
        } else {
            error
        }
    })
}

fn reconcile_after_signal(
    error: &GroveError,
    transaction: &HeldDirectory,
    journal: &Journal,
) -> Result<()> {
    if error
        .exit_code
        .is_some_and(|code| matches!(code, 129 | 130 | 143))
    {
        durable_replace(transaction, journal, &mut Checkpoints::disabled())?;
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
