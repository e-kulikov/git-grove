use crate::error::{GroveError, Result};
use crate::transaction::journal::{Journal, OperationState, Primitive, Progress};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reconciliation {
    Before,
    After,
    Ambiguous,
}

pub trait PrimitiveRuntime {
    fn reconcile(&mut self, primitive: &Primitive) -> Result<Reconciliation>;
    fn apply(&mut self, primitive: &Primitive) -> Result<()>;
    fn fsync_after(&mut self, primitive: &Primitive) -> Result<()>;
    fn reverse(&mut self, primitive: &Primitive) -> Result<()>;
    fn fsync_after_reverse(&mut self, primitive: &Primitive) -> Result<()>;
}

pub fn advance_forward<R, P>(journal: &mut Journal, runtime: &mut R, mut persist: P) -> Result<bool>
where
    R: PrimitiveRuntime,
    P: FnMut(&Journal) -> Result<()>,
{
    if journal.progress != Progress::Forward {
        return Err(GroveError::needs_decision(
            "cannot continue a journal that is not in the forward direction",
        ));
    }
    let Some(index) = journal
        .operations
        .iter()
        .position(|operation| operation.state == OperationState::Pending)
    else {
        return Ok(false);
    };
    let primitive = &journal.operations[index].primitive;
    match runtime.reconcile(primitive)? {
        Reconciliation::Before => runtime.apply(primitive)?,
        Reconciliation::After => {}
        Reconciliation::Ambiguous => {
            return Err(GroveError::needs_decision(format!(
                "operation {} is at neither its exact before nor after state",
                journal.operations[index].id
            )))
        }
    }
    runtime.fsync_after(primitive)?;
    let mut next = journal.clone();
    next.generation = next
        .generation
        .checked_add(1)
        .ok_or_else(|| GroveError::failure("journal generation overflow"))?;
    next.operations[index].state = OperationState::Done;
    journal.validate_next(&next)?;
    persist(&next)?;
    *journal = next;
    Ok(true)
}

pub fn begin_abort<P>(journal: &mut Journal, mut persist: P) -> Result<()>
where
    P: FnMut(&Journal) -> Result<()>,
{
    transition_progress(journal, Progress::Aborting, &mut persist)
}

pub fn advance_abort<R, P>(journal: &mut Journal, runtime: &mut R, mut persist: P) -> Result<bool>
where
    R: PrimitiveRuntime,
    P: FnMut(&Journal) -> Result<()>,
{
    if journal.progress != Progress::Aborting {
        return Err(GroveError::needs_decision(
            "cannot abort a journal before its abort direction is durable",
        ));
    }
    let Some(index) = journal
        .operations
        .iter()
        .rposition(|operation| operation.state == OperationState::Done)
    else {
        return Ok(false);
    };
    let primitive = &journal.operations[index].primitive;
    match runtime.reconcile(primitive)? {
        Reconciliation::After => runtime.reverse(primitive)?,
        Reconciliation::Before => {}
        Reconciliation::Ambiguous => {
            return Err(GroveError::needs_decision(format!(
                "operation {} is at neither its exact before nor after state",
                journal.operations[index].id
            )))
        }
    }
    runtime.fsync_after_reverse(primitive)?;
    let mut next = journal.clone();
    next.generation = next
        .generation
        .checked_add(1)
        .ok_or_else(|| GroveError::failure("journal generation overflow"))?;
    next.operations[index].state = OperationState::Pending;
    journal.validate_next(&next)?;
    persist(&next)?;
    *journal = next;
    Ok(true)
}

pub fn mark_committed<P>(journal: &mut Journal, mut persist: P) -> Result<()>
where
    P: FnMut(&Journal) -> Result<()>,
{
    transition_progress(journal, Progress::Committed, &mut persist)
}

pub fn mark_aborted<P>(journal: &mut Journal, mut persist: P) -> Result<()>
where
    P: FnMut(&Journal) -> Result<()>,
{
    transition_progress(journal, Progress::Aborted, &mut persist)
}

fn transition_progress<P>(journal: &mut Journal, progress: Progress, persist: &mut P) -> Result<()>
where
    P: FnMut(&Journal) -> Result<()>,
{
    let mut next = journal.clone();
    next.generation = next
        .generation
        .checked_add(1)
        .ok_or_else(|| GroveError::failure("journal generation overflow"))?;
    next.progress = progress;
    journal.validate_next(&next)?;
    persist(&next)?;
    *journal = next;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsx::held::{FileIdentity, Timespec};
    use crate::transaction::journal::*;

    #[derive(Default)]
    struct Runtime {
        reconciliation: Option<Reconciliation>,
        applies: usize,
        reversals: usize,
        fsyncs: usize,
    }

    impl PrimitiveRuntime for Runtime {
        fn reconcile(&mut self, _primitive: &Primitive) -> Result<Reconciliation> {
            Ok(self.reconciliation.unwrap_or(Reconciliation::Before))
        }
        fn apply(&mut self, _primitive: &Primitive) -> Result<()> {
            self.applies += 1;
            Ok(())
        }
        fn fsync_after(&mut self, _primitive: &Primitive) -> Result<()> {
            self.fsyncs += 1;
            Ok(())
        }
        fn reverse(&mut self, _primitive: &Primitive) -> Result<()> {
            self.reversals += 1;
            Ok(())
        }
        fn fsync_after_reverse(&mut self, _primitive: &Primitive) -> Result<()> {
            self.fsyncs += 1;
            Ok(())
        }
    }

    fn identity() -> FileIdentity {
        FileIdentity {
            dev: 1,
            ino: 1,
            mode: 0o100644,
            nlink: 1,
            size: 0,
            mtime: Timespec {
                seconds: 1,
                nanoseconds: 0,
            },
            ctime: Timespec {
                seconds: 1,
                nanoseconds: 0,
            },
            mount_id: 1,
            sha256: None,
        }
    }

    fn placeholder_blob() -> BlobProof {
        BlobProof {
            bytes: RawBytes::from_bytes(b""),
            sha256: sha256(b""),
            mode: 0o100644,
            identity: identity(),
        }
    }

    fn invocation() -> JournalInvocation {
        JournalInvocation {
            git_dir: None,
            work_tree: None,
            cwd: None,
            args: vec![RawBytes::from_bytes(b"status")],
            environment: vec![],
        }
    }

    fn snapshot() -> ByteSnapshot {
        ByteSnapshot::new(invocation(), b"")
    }

    fn known() -> IdentityProof {
        IdentityProof::Known {
            identity: identity(),
        }
    }

    fn generated_file() -> PathProof {
        PathProof {
            at: Location::Root {
                path: ValidatedBytePath::component(b"generated").unwrap(),
            },
            identity: IdentityProof::Created {
                object_type: ObjectType::RegularFile,
                mode: 0o644,
                mount_id: 1,
                sha256: Some(sha256(b"")),
                symlink_target: None,
            },
        }
    }

    fn journal() -> Journal {
        let blob = placeholder_blob();
        Journal {
            schema: JOURNAL_SCHEMA,
            generation: 1,
            nonce: [1; 16],
            root: RootProof {
                canonical_path: RawBytes::from_bytes(b"/repo"),
                identity: identity(),
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
                original: OriginalEvidence {
                    repository_identity: identity(),
                    worktree_list_porcelain_z: snapshot(),
                    status_porcelain_v2_z: snapshot(),
                    ls_files_stage_z: snapshot(),
                    ls_files_verbose_z: snapshot(),
                    payload_manifest: vec![],
                    index: None,
                    shared_indexes: vec![],
                    config: blob.clone(),
                    config_worktree: None,
                    head: blob.clone(),
                    refs: vec![],
                    private_state: vec![],
                },
                generated: GeneratedEvidence {
                    payload_pointer: generated_file(),
                    default_pointer: None,
                },
                expected_final: FinalEvidence {
                    worktrees: vec![],
                    payload_status_porcelain_v2_z: snapshot(),
                    payload_ls_files_stage_z: snapshot(),
                    payload_ls_files_verbose_z: snapshot(),
                    config_values: vec![],
                    refs: vec![],
                    pointer_files: vec![],
                    metadata: vec![],
                },
            },
            operations: vec![OperationRecord {
                id: 1,
                state: OperationState::Pending,
                primitive: Primitive::Remove {
                    at: Location::Root {
                        path: ValidatedBytePath::component(b"generated").unwrap(),
                    },
                    expected: known(),
                },
            }],
            progress: Progress::Forward,
        }
    }

    #[test]
    fn before_applies_fsyncs_and_persists_done() {
        let mut journal = journal();
        let mut runtime = Runtime::default();
        let mut persisted = Vec::new();
        assert!(advance_forward(&mut journal, &mut runtime, |next| {
            persisted.push(next.clone());
            Ok(())
        })
        .unwrap());
        assert_eq!((runtime.applies, runtime.fsyncs), (1, 1));
        assert_eq!(journal.operations[0].state, OperationState::Done);
        assert_eq!(persisted.len(), 1);
    }

    #[test]
    fn after_advances_without_reapplying_and_ambiguous_refuses() {
        let mut after_journal = journal();
        let mut runtime = Runtime {
            reconciliation: Some(Reconciliation::After),
            ..Runtime::default()
        };
        advance_forward(&mut after_journal, &mut runtime, |_| Ok(())).unwrap();
        assert_eq!(runtime.applies, 0);

        let mut ambiguous_journal = journal();
        runtime.reconciliation = Some(Reconciliation::Ambiguous);
        assert!(advance_forward(&mut ambiguous_journal, &mut runtime, |_| Ok(())).is_err());
        assert_eq!(ambiguous_journal.generation, 1);
    }

    #[test]
    fn abort_direction_is_persisted_before_reverse_progress() {
        let mut journal = journal();
        journal.operations[0].state = OperationState::Done;
        let mut generations = Vec::new();
        begin_abort(&mut journal, |next| {
            generations.push(next.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(journal.progress, Progress::Aborting);
        let mut runtime = Runtime {
            reconciliation: Some(Reconciliation::After),
            ..Runtime::default()
        };
        advance_abort(&mut journal, &mut runtime, |next| {
            generations.push(next.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(runtime.reversals, 1);
        assert_eq!(journal.operations[0].state, OperationState::Pending);
        assert_eq!(generations.len(), 2);
    }
}
