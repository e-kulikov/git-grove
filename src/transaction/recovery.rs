use crate::error::{GroveError, Result};
use crate::fsx::held::{FileIdentity, HeldDirectory};
use crate::fsx::lock::{GroveLock, LockMode};
use crate::git::runner::{GitRunner, Invocation};
use crate::transaction::journal::{Journal, Progress, JOURNAL_CURRENT, JOURNAL_NEW};
use bstr::ByteSlice;
use rustix::fs::{openat, renameat, unlinkat, AtFlags, Mode, OFlags};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const PREFIX: &[u8] = b".grove-adopt-";
const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;

pub struct Recovered {
    pub root: HeldDirectory,
    pub transaction: HeldDirectory,
    pub journal: Journal,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RecoveryRegion {
    Forward,
    Committed,
    None,
}

pub fn resolve_root(requested: Option<&Path>, cwd: &Path) -> Result<PathBuf> {
    if let Some(requested) = requested {
        return requested.canonicalize().map_err(|error| {
            GroveError::needs_decision(format!(
                "cannot resolve adoption recovery root {}: {error}",
                requested.as_os_str().as_bytes().escape_bytes()
            ))
        });
    }
    let mut current = cwd.canonicalize().map_err(|error| {
        GroveError::needs_decision(format!("cannot resolve current directory: {error}"))
    })?;
    loop {
        if candidate_names(&current).is_ok_and(|names| !names.is_empty()) {
            return Ok(current);
        }
        if !current.pop() {
            return Err(GroveError::failure("no interrupted adoption was found")
                .with_detail("pass the original repository root to --continue or --abort"));
        }
    }
}

pub fn inspect_region(root_path: &Path) -> Result<RecoveryRegion> {
    let names = candidate_names(root_path)?;
    if names.is_empty() {
        return Ok(RecoveryRegion::None);
    }
    if names.len() != 1 {
        return Err(GroveError::needs_decision(
            "multiple adoption transactions require inspection",
        ));
    }
    let name = &names[0];
    validate_candidate_metadata(root_path, name)?;
    let root = HeldDirectory::open(root_path)?;
    let transaction = HeldDirectory::open(&root_path.join(name))?;
    let current = read_optional(&transaction, JOURNAL_CURRENT)?
        .map(|bytes| Journal::parse_strict(&bytes))
        .transpose()?;
    let new = read_optional(&transaction, JOURNAL_NEW)?
        .map(|bytes| Journal::parse_strict(&bytes))
        .transpose()
        .ok()
        .flatten();
    let selected = match (current, new) {
        (Some(current), Some(next)) if current.validate_next(&next).is_ok() => next,
        (Some(current), _) => current,
        (None, Some(initial)) if initial.generation == 1 => initial,
        _ => {
            return Err(GroveError::needs_decision(
                "no valid adoption journal generation can be selected",
            ))
        }
    };
    validate_binding(&root, name, &selected)?;
    match selected.progress {
        Progress::Forward => Ok(RecoveryRegion::Forward),
        Progress::Committed => Ok(RecoveryRegion::Committed),
        Progress::Aborting | Progress::Aborted => Err(GroveError::needs_decision(
            "adoption journal is in the abort direction",
        )),
    }
}

pub fn ensure_none(root: &Path) -> Result<()> {
    let names = candidate_names(root)?;
    if names.is_empty() {
        return Ok(());
    }
    let root = root.as_os_str().as_bytes().escape_bytes();
    Err(
        GroveError::needs_decision("an adoption transaction blocks this grove command")
            .with_detail(format!(
                "run `git grove adopt --continue {root}` or `git grove adopt --abort {root}`"
            )),
    )
}

pub fn discover(root_path: &Path) -> Result<Recovered> {
    let root = HeldDirectory::open(root_path)?;
    let names = candidate_names(root_path)?;
    if names.is_empty() {
        return Err(GroveError::failure("no interrupted adoption was found"));
    }
    if names.len() != 1 {
        let listed = names
            .iter()
            .map(|name| name.as_bytes().escape_bytes().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(GroveError::needs_decision(format!(
            "multiple adoption transactions require inspection: {listed}"
        )));
    }
    let name = &names[0];
    validate_candidate_metadata(root_path, name)?;
    let transaction = HeldDirectory::open(&root_path.join(name))?;
    let journal = select_journal(&transaction)?;
    validate_binding(&root, name, &journal)?;
    Ok(Recovered {
        root,
        transaction,
        journal,
    })
}

pub fn abort_torn_bootstrap(runner: &dyn GitRunner, root_path: &Path) -> Result<bool> {
    let names = candidate_names(root_path)?;
    if names.len() != 1 {
        return Ok(false);
    }
    let name = &names[0];
    validate_candidate_metadata(root_path, name)?;
    if real_directory(&root_path.join(".bare"))? || !real_directory(&root_path.join(".git"))? {
        return Ok(false);
    }
    let lock = GroveLock::acquire_path(
        &root_path.join(".git"),
        LockMode::Exclusive,
        "git grove adopt --abort",
    )?;
    let transaction = HeldDirectory::open(&root_path.join(name))?;
    if read_optional(&transaction, JOURNAL_CURRENT)?.is_some() {
        return Ok(false);
    }
    let Some(new) = read_optional(&transaction, JOURNAL_NEW)? else {
        return Ok(false);
    };
    if Journal::parse_strict(&new).is_ok() {
        return Ok(false);
    }
    let entries = std::fs::read_dir(transaction.path())
        .map_err(|error| {
            GroveError::needs_decision(format!("cannot inspect torn transaction: {error}"))
        })?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| {
            GroveError::needs_decision(format!("cannot inspect torn transaction: {error}"))
        })?;
    if entries.len() != 1 || entries[0].file_name() != JOURNAL_NEW {
        return Ok(false);
    }
    let canonical = root_path.canonicalize().map_err(|error| {
        GroveError::needs_decision(format!("cannot resolve bootstrap root: {error}"))
    })?;
    let worktrees = runner.run(
        Invocation::new()
            .cwd(&canonical)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(["worktree", "list", "--porcelain", "-z"]),
    )?;
    if !worktrees.ok() || !exact_single_worktree(&worktrees.stdout, &canonical) {
        return Ok(false);
    }
    lock.directory().validate()?;
    unlinkat(&transaction.file, JOURNAL_NEW, AtFlags::empty()).map_err(|error| {
        GroveError::failure(format!("cannot remove torn bootstrap journal: {error}"))
    })?;
    transaction.file.sync_all().map_err(|error| {
        GroveError::failure(format!("cannot fsync torn bootstrap transaction: {error}"))
    })?;
    let root = HeldDirectory::open(root_path)?;
    unlinkat(&root.file, name, AtFlags::REMOVEDIR).map_err(|error| {
        GroveError::failure(format!("cannot remove torn bootstrap transaction: {error}"))
    })?;
    root.file.sync_all().map_err(|error| {
        GroveError::failure(format!("cannot fsync bootstrap root cleanup: {error}"))
    })?;
    Ok(true)
}

fn candidate_names(root: &Path) -> Result<Vec<OsString>> {
    let mut names = std::fs::read_dir(root)
        .map_err(|error| {
            GroveError::needs_decision(format!("cannot inspect recovery root: {error}"))
        })?
        .filter_map(|entry| match entry {
            Ok(entry) if entry.file_name().as_bytes().starts_with(PREFIX) => {
                Some(Ok(entry.file_name()))
            }
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| {
            GroveError::needs_decision(format!("cannot inspect recovery root: {error}"))
        })?;
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    Ok(names)
}

fn validate_candidate_metadata(root: &Path, name: &OsStr) -> Result<()> {
    let path = root.join(name);
    let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
        GroveError::needs_decision(format!("cannot inspect adoption transaction: {error}"))
    })?;
    if !metadata.file_type().is_dir()
        || metadata.file_type().is_symlink()
        || metadata.mode() & 0o7777 != 0o700
        || metadata.uid() != rustix::process::geteuid().as_raw()
    {
        return Err(GroveError::needs_decision(format!(
            "adoption transaction {} has unsafe type, mode, or owner",
            name.as_bytes().escape_bytes()
        )));
    }
    Ok(())
}

fn real_directory(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_dir() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(GroveError::needs_decision(format!(
            "cannot inspect {}: {error}",
            path.as_os_str().as_bytes().escape_bytes()
        ))),
    }
}

fn exact_single_worktree(bytes: &[u8], root: &Path) -> bool {
    let records = bytes
        .split(|byte| *byte == 0)
        .filter_map(|field| field.strip_prefix(b"worktree "))
        .collect::<Vec<_>>();
    records == [root.as_os_str().as_bytes()]
}

fn select_journal(transaction: &HeldDirectory) -> Result<Journal> {
    let current = read_optional(transaction, JOURNAL_CURRENT)?;
    let new = read_optional(transaction, JOURNAL_NEW)?;
    match current {
        Some(current_bytes) => {
            let current = Journal::parse_strict(&current_bytes).map_err(|error| {
                GroveError::needs_decision("the current adoption journal is corrupt")
                    .with_detail(error.to_string())
            })?;
            match new {
                None => Ok(current),
                Some(new_bytes) => match Journal::parse_strict(&new_bytes) {
                    Ok(next) => {
                        current.validate_next(&next).map_err(|error| {
                            GroveError::needs_decision(
                                "journal.json.new is not the unique legal next generation",
                            )
                            .with_detail(error.to_string())
                        })?;
                        promote_new(transaction)?;
                        Ok(next)
                    }
                    Err(_) => {
                        unlink_new(transaction)?;
                        Ok(current)
                    }
                },
            }
        }
        None => {
            let new_bytes = new.ok_or_else(|| {
                GroveError::needs_decision("adoption transaction contains no journal generation")
            })?;
            let journal = Journal::parse_strict(&new_bytes).map_err(|error| {
                GroveError::needs_decision("the initial adoption journal is torn or corrupt")
                    .with_detail(error.to_string())
            })?;
            if journal.generation != 1 {
                return Err(GroveError::needs_decision(
                    "an uninstalled adoption journal is not generation 1",
                ));
            }
            promote_new(transaction)?;
            Ok(journal)
        }
    }
}

fn read_optional(transaction: &HeldDirectory, name: &str) -> Result<Option<Vec<u8>>> {
    let opened = openat(
        &transaction.file,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    );
    let mut file = match opened {
        Ok(file) => File::from(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(GroveError::needs_decision(format!(
                "cannot open adoption {name}: {error}"
            )))
        }
    };
    let metadata = file.metadata().map_err(|error| {
        GroveError::needs_decision(format!("cannot inspect adoption {name}: {error}"))
    })?;
    if !metadata.is_file() || metadata.len() > MAX_JOURNAL_BYTES {
        return Err(GroveError::needs_decision(format!(
            "adoption {name} is not a bounded regular file"
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes).map_err(|error| {
        GroveError::needs_decision(format!("cannot read adoption {name}: {error}"))
    })?;
    Ok(Some(bytes))
}

fn promote_new(transaction: &HeldDirectory) -> Result<()> {
    renameat(
        &transaction.file,
        JOURNAL_NEW,
        &transaction.file,
        JOURNAL_CURRENT,
    )
    .map_err(|error| GroveError::failure(format!("cannot promote recovery journal: {error}")))?;
    sync_transaction(transaction)
}

fn unlink_new(transaction: &HeldDirectory) -> Result<()> {
    unlinkat(&transaction.file, JOURNAL_NEW, AtFlags::empty()).map_err(|error| {
        GroveError::failure(format!("cannot remove torn recovery journal: {error}"))
    })?;
    sync_transaction(transaction)
}

fn sync_transaction(transaction: &HeldDirectory) -> Result<()> {
    transaction.file.sync_all().map_err(|error| {
        GroveError::failure(format!(
            "cannot fsync adoption transaction directory: {error}"
        ))
    })
}

fn validate_binding(root: &HeldDirectory, name: &OsStr, journal: &Journal) -> Result<()> {
    let canonical = root.path().canonicalize().map_err(|error| {
        GroveError::needs_decision(format!("cannot resolve recovery root: {error}"))
    })?;
    if journal.root.canonical_path.decode() != canonical.as_os_str().as_bytes() {
        return Err(GroveError::needs_decision(
            "adoption journal belongs to a different repository root",
        ));
    }
    if !same_held_identity(root.identity()?, journal.root.identity) {
        return Err(GroveError::needs_decision(
            "adoption repository root identity no longer matches the journal",
        ));
    }
    let expected = format!(".grove-adopt-{}", hex(&journal.nonce));
    if name.as_bytes() != expected.as_bytes() {
        return Err(GroveError::needs_decision(
            "adoption transaction name does not match its journal nonce",
        ));
    }
    Ok(())
}

pub fn same_held_identity(left: FileIdentity, right: FileIdentity) -> bool {
    left.dev == right.dev
        && left.ino == right.ino
        && left.mode == right.mode
        && left.mount_id == right.mount_id
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
