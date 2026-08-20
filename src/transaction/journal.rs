use crate::error::{GroveError, Result};
use crate::fsx::held::{FileIdentity, HeldDirectory};
use crate::transaction::failpoint::Checkpoints;
use rustix::fs::{openat, renameat, Mode, OFlags};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

pub const JOURNAL_SCHEMA: u32 = 1;
pub const JOURNAL_CURRENT: &str = "journal.json";
pub const JOURNAL_NEW: &str = "journal.json.new";

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "encoding", content = "value")]
pub enum RawBytes {
    Hex(String),
}

impl RawBytes {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut encoded = String::with_capacity(bytes.len() * 2);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in bytes {
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0x0f) as usize] as char);
        }
        Self::Hex(encoded)
    }

    pub fn decode(&self) -> Vec<u8> {
        let Self::Hex(encoded) = self;
        encoded
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| (nibble(pair[0]).unwrap() << 4) | nibble(pair[1]).unwrap())
            .collect()
    }

    pub fn as_os_string(&self) -> OsString {
        OsString::from_vec(self.decode())
    }
}

impl<'de> Deserialize<'de> for RawBytes {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "encoding", content = "value", deny_unknown_fields)]
        enum Wire {
            Hex(String),
        }
        let Wire::Hex(encoded) = Wire::deserialize(deserializer)?;
        if encoded.len() % 2 != 0
            || !encoded
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        {
            return Err(D::Error::custom(
                "hex raw bytes must contain an even number of lowercase hexadecimal digits",
            ));
        }
        Ok(Self::Hex(encoded))
    }
}

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ValidatedBytePath {
    components: Vec<RawBytes>,
}

impl ValidatedBytePath {
    pub fn new(path: &Path) -> Result<Self> {
        if path.is_absolute() {
            return Err(GroveError::failure("journal path is absolute"));
        }
        let mut components = Vec::new();
        for component in path.components() {
            let Component::Normal(component) = component else {
                return Err(GroveError::failure(
                    "journal path contains a dot or parent component",
                ));
            };
            validate_path_component(component.as_bytes())?;
            components.push(RawBytes::from_bytes(component.as_bytes()));
        }
        Self::from_components(components)
    }

    pub fn component(bytes: &[u8]) -> Result<Self> {
        Self::from_components(vec![RawBytes::from_bytes(bytes)])
    }

    pub fn from_components(components: Vec<RawBytes>) -> Result<Self> {
        if components.is_empty() {
            return Err(GroveError::failure("journal path is empty"));
        }
        for component in &components {
            validate_path_component(&component.decode())?;
        }
        Ok(Self { components })
    }

    pub fn to_path_buf(&self) -> PathBuf {
        let mut path = PathBuf::new();
        for component in &self.components {
            path.push(component.as_os_string());
        }
        path
    }
}

impl<'de> Deserialize<'de> for ValidatedBytePath {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            components: Vec<RawBytes>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::from_components(wire.components).map_err(D::Error::custom)
    }
}

fn validate_path_component(bytes: &[u8]) -> Result<()> {
    if bytes.is_empty()
        || bytes == b"."
        || bytes == b".."
        || bytes.contains(&b'/')
        || bytes.contains(&0)
    {
        return Err(GroveError::failure(
            "journal path contains an invalid component",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "root", rename_all = "snake_case", deny_unknown_fields)]
pub enum Location {
    Root {
        path: ValidatedBytePath,
    },
    Bare {
        path: ValidatedBytePath,
    },
    Transaction {
        path: ValidatedBytePath,
    },
    WorktreeAdmin {
        id: ValidatedBytePath,
        path: ValidatedBytePath,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootProof {
    pub canonical_path: RawBytes,
    pub identity: FileIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HeadProof {
    Attached { branch: RawBytes },
    Detached { oid: RawBytes },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalEnvironment {
    pub key: RawBytes,
    pub value: RawBytes,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalInvocation {
    pub git_dir: Option<Location>,
    pub work_tree: Option<Location>,
    pub cwd: Option<Location>,
    pub args: Vec<RawBytes>,
    pub environment: Vec<JournalEnvironment>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ByteSnapshot {
    pub invocation: JournalInvocation,
    pub bytes: RawBytes,
    pub sha256: [u8; 32],
}

impl ByteSnapshot {
    pub fn new(invocation: JournalInvocation, bytes: &[u8]) -> Self {
        Self {
            invocation,
            bytes: RawBytes::from_bytes(bytes),
            sha256: sha256(bytes),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobProof {
    pub bytes: RawBytes,
    pub sha256: [u8; 32],
    pub mode: u32,
    pub identity: FileIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedBlobProof {
    pub path: ValidatedBytePath,
    pub blob: BlobProof,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManifestContent {
    None,
    Blob { bytes: RawBytes, sha256: [u8; 32] },
    Symlink { target: RawBytes, sha256: [u8; 32] },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestEntry {
    pub path: ValidatedBytePath,
    pub identity: FileIdentity,
    pub content: ManifestContent,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigScope {
    Shared,
    MainWorktree,
    LinkedWorktree,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigValueProof {
    pub file: Location,
    pub key: RawBytes,
    pub values: Option<Vec<RawBytes>>,
    pub scope: ConfigScope,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdoptArgumentsProof {
    pub requested_root: RawBytes,
    pub remote: Option<RawBytes>,
    pub default_branch: Option<RawBytes>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdoptDecisionsProof {
    pub payload_head: HeadProof,
    pub default_branch: RawBytes,
    pub selected_remote: Option<RawBytes>,
    pub payload_path: RawBytes,
    pub default_path: Option<RawBytes>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginalEvidence {
    pub worktree_list_porcelain_z: ByteSnapshot,
    pub status_porcelain_v2_z: ByteSnapshot,
    pub ls_files_stage_z: ByteSnapshot,
    pub ls_files_verbose_z: ByteSnapshot,
    pub payload_manifest: Vec<ManifestEntry>,
    pub index: Option<BlobProof>,
    pub shared_indexes: Vec<NamedBlobProof>,
    pub config: BlobProof,
    pub config_worktree: Option<BlobProof>,
    pub head: BlobProof,
    pub refs: Vec<NamedBlobProof>,
    pub private_state: Vec<NamedBlobProof>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalEvidence {
    pub worktree_list_porcelain_z: ByteSnapshot,
    pub payload_status_porcelain_v2_z: ByteSnapshot,
    pub payload_ls_files_stage_z: ByteSnapshot,
    pub payload_ls_files_verbose_z: ByteSnapshot,
    pub config_values: Vec<ConfigValueProof>,
    pub refs: Vec<NamedBlobProof>,
    pub pointer_files: Vec<NamedBlobProof>,
    pub metadata: Vec<ConfigValueProof>,
    pub guide: BlobProof,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratedEvidence {
    pub transformed_config: BlobProof,
    pub transformed_config_worktree: Option<BlobProof>,
    pub payload_pointer: BlobProof,
    pub default_pointer: Option<BlobProof>,
    pub guide: BlobProof,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImmutablePlan {
    pub arguments: AdoptArgumentsProof,
    pub decisions: AdoptDecisionsProof,
    pub original: OriginalEvidence,
    pub generated: GeneratedEvidence,
    pub expected_final: FinalEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Presence {
    Absent,
    Present {
        identity: FileIdentity,
        bytes: RawBytes,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GitPostcondition {
    Path {
        at: Location,
        identity: FileIdentity,
    },
    Config {
        proof: ConfigValueProof,
    },
    Bytes {
        at: Location,
        bytes: RawBytes,
        sha256: [u8; 32],
    },
    Snapshot {
        snapshot: ByteSnapshot,
    },
    All {
        proofs: Vec<GitPostcondition>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Primitive {
    Rename {
        from: Location,
        to: Location,
        before: FileIdentity,
        after: FileIdentity,
    },
    Write {
        at: Location,
        previous: Presence,
        content: RawBytes,
        sha256: [u8; 32],
        mode: u32,
    },
    Remove {
        at: Location,
        expected: FileIdentity,
    },
    Git {
        invocation: JournalInvocation,
        postcondition: GitPostcondition,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Pending,
    Done,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationRecord {
    pub id: u64,
    pub state: OperationState,
    pub primitive: Primitive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Progress {
    Forward,
    Aborting,
    Committed,
    Aborted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    pub schema: u32,
    pub generation: u64,
    pub nonce: [u8; 16],
    pub root: RootProof,
    pub plan: ImmutablePlan,
    pub operations: Vec<OperationRecord>,
    pub progress: Progress,
}

impl Journal {
    pub fn validate(&self) -> Result<()> {
        if self.schema != JOURNAL_SCHEMA {
            return Err(GroveError::needs_decision(format!(
                "unsupported adoption journal schema {}",
                self.schema
            )));
        }
        if self.generation == 0 {
            return Err(invalid("journal generation must be positive"));
        }
        let root = self.root.canonical_path.decode();
        if root.contains(&0) || !Path::new(OsStr::from_bytes(&root)).is_absolute() {
            return Err(invalid("journal root proof is not an absolute Linux path"));
        }
        validate_plan(&self.plan)?;
        let mut ids = HashSet::new();
        for operation in &self.operations {
            if operation.id == 0 || !ids.insert(operation.id) {
                return Err(invalid("journal operation IDs must be unique and nonzero"));
            }
            validate_primitive(&operation.primitive)?;
        }
        match self.progress {
            Progress::Committed
                if self
                    .operations
                    .iter()
                    .any(|operation| operation.state != OperationState::Done) =>
            {
                Err(invalid("committed journal contains a pending operation"))
            }
            Progress::Aborted
                if self
                    .operations
                    .iter()
                    .any(|operation| operation.state != OperationState::Pending) =>
            {
                Err(invalid("aborted journal contains a completed operation"))
            }
            _ => Ok(()),
        }
    }

    pub fn parse_strict(bytes: &[u8]) -> Result<Self> {
        let mut deserializer = serde_json::Deserializer::from_slice(bytes);
        let journal = Self::deserialize(&mut deserializer)
            .map_err(|error| invalid(format!("invalid adoption journal JSON: {error}")))?;
        deserializer
            .end()
            .map_err(|error| invalid(format!("trailing adoption journal bytes: {error}")))?;
        journal.validate()?;
        Ok(journal)
    }

    pub fn validate_next(&self, next: &Self) -> Result<()> {
        self.validate()?;
        next.validate()?;
        if next.generation
            != self
                .generation
                .checked_add(1)
                .ok_or_else(|| invalid("journal generation overflow"))?
        {
            return Err(invalid("journal generation is not exactly current + 1"));
        }
        if self.schema != next.schema
            || self.nonce != next.nonce
            || self.root != next.root
            || self.plan != next.plan
            || self.operations.len() != next.operations.len()
        {
            return Err(invalid("journal immutable fields changed"));
        }

        let mut state_change = None;
        for (index, (current, candidate)) in
            self.operations.iter().zip(&next.operations).enumerate()
        {
            if current.id != candidate.id || current.primitive != candidate.primitive {
                return Err(invalid("journal operation identity or primitive changed"));
            }
            if current.state != candidate.state
                && state_change
                    .replace((index, current.state, candidate.state))
                    .is_some()
            {
                return Err(invalid("more than one journal operation state changed"));
            }
        }
        let progress_changed = self.progress != next.progress;
        if progress_changed == state_change.is_some() {
            return Err(invalid(
                "a generation must change exactly one mutable field",
            ));
        }

        if let Some((_index, from, to)) = state_change {
            let legal = match self.progress {
                Progress::Forward => from == OperationState::Pending && to == OperationState::Done,
                Progress::Aborting => from == OperationState::Done && to == OperationState::Pending,
                Progress::Committed | Progress::Aborted => false,
            };
            if !legal || self.progress != next.progress {
                return Err(invalid("illegal journal operation transition"));
            }
            return Ok(());
        }

        let legal = matches!(
            (self.progress, next.progress),
            (Progress::Forward, Progress::Aborting)
                | (Progress::Forward, Progress::Committed)
                | (Progress::Aborting, Progress::Aborted)
        );
        if !legal {
            return Err(invalid("illegal journal progress transition"));
        }
        Ok(())
    }
}

fn validate_primitive(primitive: &Primitive) -> Result<()> {
    match primitive {
        Primitive::Write {
            previous,
            content,
            sha256: expected,
            mode,
            ..
        } => {
            if sha256(&content.decode()) != *expected {
                return Err(invalid("journal write content hash does not match"));
            }
            if mode & !0o7777 != 0 {
                return Err(invalid("journal write mode contains non-permission bits"));
            }
            if let Presence::Present {
                identity, bytes, ..
            } = previous
            {
                validate_identity_bytes(*identity, bytes)?;
            }
        }
        Primitive::Git {
            invocation,
            postcondition,
        } => {
            if invocation.args.is_empty() {
                return Err(invalid("journal Git invocation has no arguments"));
            }
            validate_postcondition(postcondition)?;
        }
        Primitive::Rename { .. } | Primitive::Remove { .. } => {}
    }
    Ok(())
}

fn validate_plan(plan: &ImmutablePlan) -> Result<()> {
    for snapshot in [
        &plan.original.worktree_list_porcelain_z,
        &plan.original.status_porcelain_v2_z,
        &plan.original.ls_files_stage_z,
        &plan.original.ls_files_verbose_z,
        &plan.expected_final.worktree_list_porcelain_z,
        &plan.expected_final.payload_status_porcelain_v2_z,
        &plan.expected_final.payload_ls_files_stage_z,
        &plan.expected_final.payload_ls_files_verbose_z,
    ] {
        validate_snapshot(snapshot)?;
    }
    for blob in [
        Some(&plan.original.config),
        plan.original.config_worktree.as_ref(),
        Some(&plan.original.head),
        Some(&plan.generated.transformed_config),
        plan.generated.transformed_config_worktree.as_ref(),
        Some(&plan.generated.payload_pointer),
        plan.generated.default_pointer.as_ref(),
        Some(&plan.generated.guide),
        Some(&plan.expected_final.guide),
        plan.original.index.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        validate_blob(blob)?;
    }
    for named in plan
        .original
        .shared_indexes
        .iter()
        .chain(&plan.original.refs)
        .chain(&plan.original.private_state)
        .chain(&plan.expected_final.refs)
        .chain(&plan.expected_final.pointer_files)
    {
        validate_blob(&named.blob)?;
    }
    for entry in &plan.original.payload_manifest {
        match &entry.content {
            ManifestContent::None => {}
            ManifestContent::Blob {
                bytes,
                sha256: hash,
            }
            | ManifestContent::Symlink {
                target: bytes,
                sha256: hash,
            } => {
                if sha256(&bytes.decode()) != *hash {
                    return Err(invalid("manifest content hash does not match"));
                }
            }
        }
    }
    Ok(())
}

fn validate_snapshot(snapshot: &ByteSnapshot) -> Result<()> {
    if sha256(&snapshot.bytes.decode()) != snapshot.sha256 {
        return Err(invalid("Git byte snapshot hash does not match"));
    }
    if snapshot.invocation.args.is_empty() {
        return Err(invalid("Git byte snapshot has no invocation arguments"));
    }
    Ok(())
}

fn validate_blob(blob: &BlobProof) -> Result<()> {
    let bytes = blob.bytes.decode();
    if sha256(&bytes) != blob.sha256 {
        return Err(invalid("blob proof hash does not match"));
    }
    if let Some(identity_hash) = blob.identity.sha256 {
        if identity_hash != blob.sha256 {
            return Err(invalid("blob proof identity hash does not match"));
        }
    }
    Ok(())
}

fn validate_identity_bytes(identity: FileIdentity, bytes: &RawBytes) -> Result<()> {
    if let Some(expected) = identity.sha256 {
        if sha256(&bytes.decode()) != expected {
            return Err(invalid("presence proof content hash does not match"));
        }
    }
    Ok(())
}

fn validate_postcondition(postcondition: &GitPostcondition) -> Result<()> {
    match postcondition {
        GitPostcondition::Path { .. } | GitPostcondition::Config { .. } => Ok(()),
        GitPostcondition::Bytes {
            bytes,
            sha256: hash,
            ..
        } => {
            if sha256(&bytes.decode()) != *hash {
                Err(invalid("Git postcondition content hash does not match"))
            } else {
                Ok(())
            }
        }
        GitPostcondition::Snapshot { snapshot } => validate_snapshot(snapshot),
        GitPostcondition::All { proofs } => {
            for proof in proofs {
                validate_postcondition(proof)?;
            }
            Ok(())
        }
    }
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub fn durable_replace(
    transaction: &HeldDirectory,
    journal: &Journal,
    checkpoints: &mut Checkpoints,
) -> Result<()> {
    journal.validate()?;
    let bytes = serde_json::to_vec(journal).map_err(|error| {
        GroveError::failure(format!("cannot serialize adoption journal: {error}"))
    })?;
    let mut new = openat(
        &transaction.file,
        JOURNAL_NEW,
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map(File::from)
    .map_err(|error| GroveError::failure(format!("cannot open {JOURNAL_NEW}: {error}")))?;
    new.write_all(&bytes)
        .map_err(|error| GroveError::failure(format!("cannot write {JOURNAL_NEW}: {error}")))?;
    checkpoints.checkpoint()?;
    new.sync_all()
        .map_err(|error| GroveError::failure(format!("cannot fsync {JOURNAL_NEW}: {error}")))?;
    checkpoints.checkpoint()?;
    renameat(
        &transaction.file,
        JOURNAL_NEW,
        &transaction.file,
        JOURNAL_CURRENT,
    )
    .map_err(|error| GroveError::failure(format!("cannot install {JOURNAL_CURRENT}: {error}")))?;
    checkpoints.checkpoint()?;
    transaction.file.sync_all().map_err(|error| {
        GroveError::failure(format!("cannot fsync transaction directory: {error}"))
    })?;
    checkpoints.checkpoint()?;
    Ok(())
}

fn invalid(message: impl Into<String>) -> GroveError {
    GroveError::needs_decision(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsx::held::Timespec;

    fn identity(seed: u64) -> FileIdentity {
        FileIdentity {
            dev: 1,
            ino: seed,
            mode: 0o100644,
            nlink: 1,
            size: 0,
            mtime: Timespec {
                seconds: 1,
                nanoseconds: 2,
            },
            ctime: Timespec {
                seconds: 1,
                nanoseconds: 3,
            },
            mount_id: 7,
            sha256: None,
        }
    }

    fn invocation() -> JournalInvocation {
        JournalInvocation {
            git_dir: None,
            work_tree: None,
            cwd: None,
            args: vec![RawBytes::from_bytes(b"status")],
            environment: Vec::new(),
        }
    }

    fn blob(seed: u64) -> BlobProof {
        BlobProof {
            bytes: RawBytes::from_bytes(b""),
            sha256: sha256(b""),
            mode: 0o100644,
            identity: identity(seed),
        }
    }

    fn snapshot() -> ByteSnapshot {
        ByteSnapshot::new(invocation(), b"")
    }

    fn journal() -> Journal {
        let path = || ValidatedBytePath::component(b"file").unwrap();
        let original = OriginalEvidence {
            worktree_list_porcelain_z: snapshot(),
            status_porcelain_v2_z: snapshot(),
            ls_files_stage_z: snapshot(),
            ls_files_verbose_z: snapshot(),
            payload_manifest: Vec::new(),
            index: None,
            shared_indexes: Vec::new(),
            config: blob(10),
            config_worktree: None,
            head: blob(11),
            refs: Vec::new(),
            private_state: Vec::new(),
        };
        let final_evidence = FinalEvidence {
            worktree_list_porcelain_z: snapshot(),
            payload_status_porcelain_v2_z: snapshot(),
            payload_ls_files_stage_z: snapshot(),
            payload_ls_files_verbose_z: snapshot(),
            config_values: Vec::new(),
            refs: Vec::new(),
            pointer_files: Vec::new(),
            metadata: Vec::new(),
            guide: blob(12),
        };
        Journal {
            schema: JOURNAL_SCHEMA,
            generation: 1,
            nonce: [1; 16],
            root: RootProof {
                canonical_path: RawBytes::from_bytes(b"/repo"),
                identity: identity(1),
            },
            plan: ImmutablePlan {
                arguments: AdoptArgumentsProof {
                    requested_root: RawBytes::from_bytes(b"/repo"),
                    remote: None,
                    default_branch: None,
                },
                decisions: AdoptDecisionsProof {
                    payload_head: HeadProof::Attached {
                        branch: RawBytes::from_bytes(b"main"),
                    },
                    default_branch: RawBytes::from_bytes(b"main"),
                    selected_remote: None,
                    payload_path: RawBytes::from_bytes(b"main"),
                    default_path: None,
                },
                original,
                generated: GeneratedEvidence {
                    transformed_config: blob(13),
                    transformed_config_worktree: None,
                    payload_pointer: blob(14),
                    default_pointer: None,
                    guide: blob(15),
                },
                expected_final: final_evidence,
            },
            operations: vec![OperationRecord {
                id: 1,
                state: OperationState::Pending,
                primitive: Primitive::Rename {
                    from: Location::Root { path: path() },
                    to: Location::Transaction { path: path() },
                    before: identity(2),
                    after: identity(2),
                },
            }],
            progress: Progress::Forward,
        }
    }

    #[test]
    fn raw_bytes_are_tagged_lowercase_hex_and_round_trip_non_utf8() {
        let raw = RawBytes::from_bytes(b"a\0\xff");
        assert_eq!(
            serde_json::to_string(&raw).unwrap(),
            r#"{"encoding":"Hex","value":"6100ff"}"#
        );
        assert_eq!(raw.decode(), b"a\0\xff");
        for invalid in [
            r#"{"encoding":"Hex","value":"A0"}"#,
            r#"{"encoding":"Hex","value":"a"}"#,
            r#"{"encoding":"Hex","value":"xz"}"#,
        ] {
            assert!(serde_json::from_str::<RawBytes>(invalid).is_err());
        }
    }

    #[test]
    fn paths_reject_absolute_dot_parent_slash_and_nul_components() {
        for bad in [
            Path::new("/x"),
            Path::new("."),
            Path::new(".."),
            Path::new("a/../b"),
        ] {
            assert!(ValidatedBytePath::new(bad).is_err());
        }
        for bad in [b"".as_slice(), b".", b"..", b"a/b", b"a\0b"] {
            assert!(ValidatedBytePath::component(bad).is_err());
        }
    }

    #[test]
    fn strict_parser_rejects_unknown_schema_duplicates_hash_mismatch_and_trailing_bytes() {
        let valid = journal();
        let bytes = serde_json::to_vec(&valid).unwrap();
        assert_eq!(Journal::parse_strict(&bytes).unwrap(), valid);
        let mut trailing = bytes.clone();
        trailing.extend_from_slice(b" garbage");
        assert!(Journal::parse_strict(&trailing).is_err());

        let mut bad = journal();
        bad.schema = 99;
        assert!(bad.validate().is_err());
        bad = journal();
        bad.operations.push(bad.operations[0].clone());
        assert!(bad.validate().is_err());

        bad = journal();
        bad.operations[0].primitive = Primitive::Write {
            at: Location::Root {
                path: ValidatedBytePath::component(b"file").unwrap(),
            },
            previous: Presence::Absent,
            content: RawBytes::from_bytes(b"data"),
            sha256: [0; 32],
            mode: 0o644,
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn next_generation_changes_exactly_one_legal_field() {
        let current = journal();
        let mut done = current.clone();
        done.generation += 1;
        done.operations[0].state = OperationState::Done;
        current.validate_next(&done).unwrap();

        let mut skipped = done.clone();
        skipped.generation += 2;
        assert!(done.validate_next(&skipped).is_err());

        let mut plan_changed = done.clone();
        plan_changed.generation += 1;
        plan_changed.plan.arguments.remote = Some(RawBytes::from_bytes(b"origin"));
        assert!(done.validate_next(&plan_changed).is_err());

        let mut committed = done.clone();
        committed.generation += 1;
        committed.progress = Progress::Committed;
        done.validate_next(&committed).unwrap();
    }

    #[test]
    fn abort_direction_is_durable_and_only_then_allows_done_to_pending() {
        let mut done = journal();
        done.operations[0].state = OperationState::Done;
        let mut aborting = done.clone();
        aborting.generation += 1;
        aborting.progress = Progress::Aborting;
        done.validate_next(&aborting).unwrap();

        let mut reversed = aborting.clone();
        reversed.generation += 1;
        reversed.operations[0].state = OperationState::Pending;
        aborting.validate_next(&reversed).unwrap();
        assert!(done.validate_next(&reversed).is_err());

        let mut aborted = reversed.clone();
        aborted.generation += 1;
        aborted.progress = Progress::Aborted;
        reversed.validate_next(&aborted).unwrap();
    }

    #[test]
    fn durable_replacement_uses_the_fixed_new_name_and_leaves_valid_current() {
        let transaction = tempfile::tempdir().unwrap();
        let held = HeldDirectory::open(transaction.path()).unwrap();
        let mut checkpoints = Checkpoints::disabled();
        durable_replace(&held, &journal(), &mut checkpoints).unwrap();
        assert!(!transaction.path().join(JOURNAL_NEW).exists());
        let bytes = std::fs::read(transaction.path().join(JOURNAL_CURRENT)).unwrap();
        assert_eq!(Journal::parse_strict(&bytes).unwrap(), journal());
        assert_eq!(checkpoints.total(), 4);
    }
}
