use super::preflight::{self, AdoptPlan};
use super::AdoptArgs;
use crate::error::{GroveError, Result};
use crate::fsx::held::HeldDirectory;
use crate::git::runner::{GitOutput, GitRunner, Invocation};
use crate::grove::agents_md::{self, Facts};
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

const PHASES: usize = 9;

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
    let payload_pointer = predicted_payload_pointer(&root, &payload_relative)?;
    let guide = guide(&plan);
    let immutable = immutable_plan(
        &plan,
        &payload_relative,
        default_relative.as_deref(),
        &payload_pointer,
        &guide,
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
            "the transaction is at {}; inspect it and run `git grove adopt --abort {}`",
            transaction_path.as_os_str().as_bytes().escape_bytes(),
            root.as_os_str().as_bytes().escape_bytes()
        ))
    })?;

    let result = execute(
        runner,
        &plan,
        &transaction,
        &mut journal,
        &mut checkpoints,
        &payload_relative,
        default_relative.as_deref(),
        &payload_pointer,
        &guide,
    );
    if let Err(error) = result {
        return Err(error.with_detail(format!(
            "adoption can be resumed with `git grove adopt --continue {}` or reversed with `git grove adopt --abort {}`",
            root.as_os_str().as_bytes().escape_bytes(),
            root.as_os_str().as_bytes().escape_bytes()
        )));
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

#[allow(clippy::too_many_arguments)]
fn execute(
    runner: &dyn GitRunner,
    plan: &AdoptPlan,
    transaction: &HeldDirectory,
    journal: &mut Journal,
    checkpoints: &mut Checkpoints,
    payload_relative: &Path,
    default_relative: Option<&Path>,
    predicted_pointer: &[u8],
    guide: &[u8],
) -> Result<()> {
    let root = plan.root.path();
    let bare = root.join(".bare");
    let staging = transaction.path().join("payload");

    std::fs::create_dir(&staging).map_err(|error| {
        GroveError::failure(format!("cannot create payload staging directory: {error}"))
    })?;
    sync_dir(transaction.path())?;
    finish_phase(journal, 0, transaction, checkpoints)?;

    std::fs::rename(root.join(".git"), &bare)
        .map_err(|error| GroveError::failure(format!("cannot rename .git to .bare: {error}")))?;
    sync_dir(root)?;
    crate::fsx::write_atomic_if_absent(&root.join(".git"), layout::POINTER_CONTENTS.as_bytes())?;
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
            .git_dir(&bare)
            .args(["rev-parse", "--is-bare-repository"]),
        "verify bare conversion",
    )?;
    if bare_check.stdout != b"true\n" || !bare_check.stderr.is_empty() {
        return Err(GroveError::needs_decision(
            "Git did not verify the converted repository as warning-free and bare",
        ));
    }
    finish_phase(journal, 1, transaction, checkpoints)?;

    for name in payload_top_level(plan)? {
        std::fs::rename(root.join(&name), staging.join(&name)).map_err(|error| {
            GroveError::failure(format!(
                "cannot stage payload entry {}: {error}",
                name.as_bytes().escape_bytes()
            ))
        })?;
        sync_dir(root)?;
        sync_dir(&staging)?;
    }
    finish_phase(journal, 2, transaction, checkpoints)?;

    create_parents(root, payload_relative)?;
    let payload_path = root.join(payload_relative);
    let reason = format!("git-grove adopt transaction {}", hex(&journal.nonce));
    let mut invocation = Invocation::new().git_dir(&bare);
    match &plan.decisions.payload_head {
        HeadProof::Attached { branch } => {
            invocation = invocation
                .args(["worktree", "add", "--no-checkout", "--lock", "--reason"])
                .arg(&reason)
                .arg("--")
                .arg(&payload_path)
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
                .arg(&reason)
                .arg("--")
                .arg(&payload_path)
                .arg(oid.as_os_string());
        }
    }
    run_ok(runner, invocation, "create payload worktree")?;
    let generated_pointer = std::fs::read(payload_path.join(".git")).map_err(|error| {
        GroveError::failure(format!("cannot read generated payload pointer: {error}"))
    })?;
    if generated_pointer != predicted_pointer {
        return Err(GroveError::needs_decision(
            "Git generated an unexpected payload administrative path",
        ));
    }
    finish_phase(journal, 3, transaction, checkpoints)?;

    let pointer_aside = transaction.path().join("payload.git");
    std::fs::rename(payload_path.join(".git"), &pointer_aside).map_err(|error| {
        GroveError::failure(format!(
            "cannot preserve generated payload pointer: {error}"
        ))
    })?;
    std::fs::remove_dir(&payload_path).map_err(|error| {
        GroveError::failure(format!(
            "generated payload directory was not empty: {error}"
        ))
    })?;
    std::fs::rename(&staging, &payload_path)
        .map_err(|error| GroveError::failure(format!("cannot install staged payload: {error}")))?;
    std::fs::rename(&pointer_aside, payload_path.join(".git"))
        .map_err(|error| GroveError::failure(format!("cannot install payload pointer: {error}")))?;
    sync_dir(root)?;
    sync_dir(&payload_path)?;
    finish_phase(journal, 4, transaction, checkpoints)?;

    let admin = pointer_admin(&generated_pointer)?;
    let worktree_config = config_bool(runner, &bare.join("config"), "extensions.worktreeConfig")?;
    migrate_private_state(runner, plan, &bare, &admin)?;
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
    write_bare_head(&bare, &plan.decisions.default_branch)?;
    verify_payload_snapshots(runner, plan, &payload_path)?;
    finish_phase(journal, 5, transaction, checkpoints)?;

    if let Some(default_relative) = default_relative {
        create_parents(root, default_relative)?;
        let default_path = root.join(default_relative);
        let default = plan.decisions.default_branch.as_os_string();
        let mut local_ref = OsString::from("refs/heads/");
        local_ref.push(&default);
        let local = runner.run(
            Invocation::new()
                .git_dir(&bare)
                .args(["show-ref", "--verify", "--quiet"])
                .arg(&local_ref),
        )?;
        let mut add = Invocation::new()
            .git_dir(&bare)
            .args(["worktree", "add", "--lock", "--reason"])
            .arg(&reason);
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
    run_ok(
        runner,
        Invocation::new()
            .git_dir(&bare)
            .args(["config", "worktree.guessRemote", "true"]),
        "enable worktree remote guessing",
    )?;
    finish_phase(journal, 6, transaction, checkpoints)?;

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
        },
    )?;
    crate::fsx::write_atomic_if_absent(&root.join("AGENTS.md"), guide)?;
    crate::fsx::symlink_relative(&root.join("CLAUDE.md"), "AGENTS.md")?;
    finish_phase(journal, 7, transaction, checkpoints)?;

    verify_payload_snapshots(runner, plan, &payload_path)?;
    unlock(runner, &bare, &payload_path)?;
    if let Some(default) = default_relative {
        unlock(runner, &bare, &root.join(default))?;
    }
    finish_phase(journal, 8, transaction, checkpoints)?;
    let mut committed = journal.clone();
    committed.generation += 1;
    committed.progress = Progress::Committed;
    journal.validate_next(&committed)?;
    durable_replace(transaction, &committed, checkpoints)?;
    *journal = committed;
    cleanup_transaction(transaction)?;
    sync_dir(root)
}

fn immutable_plan(
    plan: &AdoptPlan,
    payload: &Path,
    default: Option<&Path>,
    pointer: &[u8],
    guide: &[u8],
) -> Result<ImmutablePlan> {
    let root_mount = plan.root.original_identity().mount_id;
    let pointer_path = payload.join(".git");
    let pointer_proof = PathProof {
        at: Location::Root {
            path: ValidatedBytePath::new(&pointer_path)?,
        },
        identity: created_file(pointer, 0o644, root_mount),
    };
    let guide = ContentProof {
        bytes: RawBytes::from_bytes(guide),
        sha256: sha256(guide),
        mode: 0o644,
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
            guide: guide.clone(),
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
            guide,
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

fn payload_top_level(plan: &AdoptPlan) -> Result<Vec<OsString>> {
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
    plan: &AdoptPlan,
    bare: &Path,
    admin: &Path,
) -> Result<()> {
    for named in &plan.original.private_state {
        let relative = named.path.to_path_buf();
        let source = bare.join(&relative);
        if !source.exists() {
            continue;
        }
        let destination = admin.join(&relative);
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
    plan: &AdoptPlan,
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

fn predicted_payload_pointer(root: &Path, payload: &Path) -> Result<Vec<u8>> {
    let id = payload
        .file_name()
        .ok_or_else(|| GroveError::usage("payload path has no final component"))?;
    let admin = root.join(".bare/worktrees").join(id);
    if admin.exists() {
        return Err(GroveError::needs_decision(
            "the predicted payload administrative directory already exists",
        ));
    }
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

fn guide(plan: &AdoptPlan) -> Vec<u8> {
    agents_md::render(&Facts {
        remote: plan
            .decisions
            .selected_remote
            .as_ref()
            .map(|value| BString::from(value.decode())),
        default_branch: BString::from(plan.decisions.default_branch.decode()),
        published: plan.decisions.selected_remote.is_some(),
        narrowed: false,
    })
    .into_bytes()
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
