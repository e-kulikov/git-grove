use crate::commands::list;
use crate::error::{ExitClass, GroveError, Result};
use crate::git::fetch::FetchPlan;
use crate::git::query::{self, WorktreeRecord};
use crate::git::runner::{GitRunner, Invocation};
use crate::grove::discover::Grove;
use crate::grove::state::{Snapshot, WorktreeState};
use crate::output::Row;
use bstr::ByteSlice;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    Updated(Snapshot),
    Changed(Snapshot),
    Blocked { snapshot: Snapshot, detail: String },
}

fn missing_snapshot(path: &Path) -> Snapshot {
    Snapshot {
        record: WorktreeRecord {
            path: path.to_path_buf(),
            head: None,
            branch: None,
            bare: false,
            detached: false,
            locked: None,
            prunable: None,
        },
        admin_dir: None,
        state: WorktreeState::Missing,
        dirty: false,
        tracking: None,
    }
}

/// Re-run the canonical worktree-list query and revalidate exactly the
/// worktree at `path`, matched by raw path bytes.
pub fn reinspect_path(runner: &dyn GitRunner, grove: &Grove, path: &Path) -> Result<Snapshot> {
    let records = query::worktrees(runner, grove)?;
    let requested = path.as_os_str().as_bytes();
    let mut matches = records
        .into_iter()
        .filter(|record| record.path.as_os_str().as_bytes() == requested);
    let Some(record) = matches.next() else {
        return Ok(missing_snapshot(path));
    };
    if matches.next().is_some() {
        return Err(GroveError::failure(
            "git returned duplicate worktree records for the same path",
        ));
    }
    list::snapshot_record(runner, grove, record)
}

/// The identity a `sync` candidate must retain, unchanged, from planning
/// time through immediately before the merge.
fn eligible(planned: &Snapshot, fresh: &Snapshot) -> bool {
    if fresh.state != WorktreeState::Behind {
        return false;
    }
    let (Some(planned_tracking), Some(fresh_tracking)) = (&planned.tracking, &fresh.tracking)
    else {
        return false;
    };
    planned.record.path.as_os_str().as_bytes() == fresh.record.path.as_os_str().as_bytes()
        && planned.record.branch == fresh.record.branch
        && planned.admin_dir == fresh.admin_dir
        && planned_tracking.upstream_ref == fresh_tracking.upstream_ref
        && planned_tracking.upstream_remote == fresh_tracking.upstream_remote
        && planned_tracking.head_oid == fresh_tracking.head_oid
        && planned_tracking.upstream_oid == fresh_tracking.upstream_oid
        && fresh_tracking.ahead == 0
        && fresh_tracking.behind > 0
}

/// Revalidate `planned` immediately before mutating it, merge only if its
/// safety-relevant identity is unchanged, and reinspect after the attempt.
pub fn update_one(
    runner: &dyn GitRunner,
    grove: &Grove,
    planned: &Snapshot,
) -> Result<UpdateOutcome> {
    let fresh = reinspect_path(runner, grove, &planned.record.path)?;
    if !eligible(planned, &fresh) {
        return Ok(UpdateOutcome::Changed(fresh));
    }
    let admin_dir = fresh
        .admin_dir
        .as_ref()
        .expect("a clean BEHIND snapshot always has a validated admin directory");
    let output = runner.run(
        Invocation::new()
            .git_dir(admin_dir)
            .work_tree(&fresh.record.path)
            .args([
                "merge",
                "--ff-only",
                "--no-edit",
                "--no-autostash",
                "--no-overwrite-ignore",
                "@{upstream}",
            ]),
    )?;
    let reinspected = reinspect_path(runner, grove, &planned.record.path)?;
    if output.ok() {
        Ok(UpdateOutcome::Updated(reinspected))
    } else {
        Ok(UpdateOutcome::Blocked {
            snapshot: reinspected,
            detail: output.stderr.as_slice().escape_bytes().to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    pub class: ExitClass,
    pub rows: Vec<Row>,
    pub diagnostics: Vec<String>,
}

/// Fetch every required remote, then fast-forward each eligible worktree in
/// stable raw-path order. Renders no pre-fetch or intermediate rows.
pub fn run(runner: &dyn GitRunner, grove: &Grove) -> Result<SyncReport> {
    let records = query::worktrees(runner, grove)?;
    let plan = FetchPlan::from_records(runner, grove, &records)?;
    plan.execute(runner, grove)?;

    let mut snapshots: Vec<Snapshot> = records
        .into_iter()
        .filter(|record| !record.bare)
        .map(|record| list::snapshot_record(runner, grove, record))
        .collect::<Result<_>>()?;
    snapshots.sort_by(|a, b| {
        a.record
            .path
            .as_os_str()
            .as_bytes()
            .cmp(b.record.path.as_os_str().as_bytes())
    });

    let mut diagnostics = Vec::new();
    let mut finals = Vec::with_capacity(snapshots.len());
    for snapshot in snapshots {
        if snapshot.state != WorktreeState::Behind {
            finals.push(snapshot);
            continue;
        }
        match update_one(runner, grove, &snapshot)? {
            UpdateOutcome::Updated(fresh) => finals.push(fresh),
            UpdateOutcome::Changed(fresh) => finals.push(fresh),
            UpdateOutcome::Blocked { snapshot, detail } => {
                diagnostics.push(detail);
                finals.push(Snapshot {
                    state: WorktreeState::Blocked,
                    ..snapshot
                });
            }
        }
    }

    let class = if finals
        .iter()
        .all(|snapshot| snapshot.state.sync_is_satisfied())
    {
        ExitClass::Ok
    } else {
        ExitClass::NeedsDecision
    };
    let rows = finals.iter().map(Row::from).collect();
    Ok(SyncReport {
        class,
        rows,
        diagnostics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::runner::{GitOutput, RecordingFake};
    use bstr::BString;

    fn output(status: i32, stdout: &[u8], stderr: &[u8]) -> GitOutput {
        GitOutput {
            status,
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    struct Fixture {
        _root: tempfile::TempDir,
        grove: Grove,
        path: std::path::PathBuf,
        admin: std::path::PathBuf,
    }

    fn fixture() -> Fixture {
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
        Fixture {
            _root: root,
            grove: Grove { root: root_path },
            path,
            admin,
        }
    }

    fn worktree_list_record(fixture: &Fixture, branch: &str, head: &str) -> Vec<u8> {
        let mut raw =
            format!("worktree {}\0bare\0\0", fixture.grove.bare_dir().display()).into_bytes();
        raw.extend_from_slice(
            format!(
                "worktree {}\0HEAD {head}\0branch refs/heads/{branch}\0\0",
                fixture.path.display()
            )
            .as_bytes(),
        );
        raw
    }

    fn planned_snapshot(fixture: &Fixture) -> Snapshot {
        let fake = RecordingFake::new();
        fake.push_response(output(
            0,
            &worktree_list_record(fixture, "main", "abc"),
            b"",
        ));
        fake.push_response(output(0, b"", b""));
        fake.push_response(output(
            0,
            b"refs/remotes/origin/main\0origin/main\0origin\0\n",
            b"",
        ));
        fake.push_response(output(0, b"", b""));
        fake.push_response(output(0, b"def\n", b""));
        fake.push_response(output(0, b"0\t3\n", b""));
        let snapshot = reinspect_path(&fake, &fixture.grove, &fixture.path).unwrap();
        assert_eq!(snapshot.state, WorktreeState::Behind);
        snapshot
    }

    /// Enqueue the exact git-call sequence `reinspect_path` issues:
    /// worktree list, dirty check, upstream, show-ref, rev-parse, rev-list.
    #[allow(clippy::too_many_arguments)]
    fn enqueue_reinspection(
        fake: &RecordingFake,
        fixture: &Fixture,
        branch: &str,
        head: &str,
        upstream_full: &str,
        upstream_short: &str,
        upstream_remote: &str,
        upstream_oid: &str,
        ahead: u32,
        behind: u32,
    ) {
        fake.push_response(output(0, &worktree_list_record(fixture, branch, head), b""));
        fake.push_response(output(0, b"", b""));
        fake.push_response(output(
            0,
            format!("{upstream_full}\0{upstream_short}\0{upstream_remote}\0\n").as_bytes(),
            b"",
        ));
        fake.push_response(output(0, b"", b""));
        fake.push_response(output(0, format!("{upstream_oid}\n").as_bytes(), b""));
        fake.push_response(output(0, format!("{ahead}\t{behind}\n").as_bytes(), b""));
    }

    #[test]
    fn a_changed_registered_path_yields_changed_with_no_merge() {
        let fixture = fixture();
        let planned = planned_snapshot(&fixture);

        let fake = RecordingFake::new();
        // No worktree at all with a different registered path: report MISSING.
        fake.push_response(output(
            0,
            format!("worktree {}\0bare\0\0", fixture.grove.bare_dir().display()).as_bytes(),
            b"",
        ));

        let outcome = update_one(&fake, &fixture.grove, &planned).unwrap();

        assert!(
            matches!(outcome, UpdateOutcome::Changed(ref snapshot) if snapshot.state == WorktreeState::Missing)
        );
        assert!(fake
            .calls()
            .iter()
            .all(|call| !call.argv_for_test().iter().any(|arg| arg == "merge")));
    }

    #[test]
    fn a_changed_branch_yields_changed_with_no_merge() {
        let fixture = fixture();
        let planned = planned_snapshot(&fixture);

        let fake = RecordingFake::new();
        enqueue_reinspection(
            &fake,
            &fixture,
            "topic",
            "abc",
            "refs/remotes/origin/main",
            "origin/main",
            "origin",
            "def",
            0,
            3,
        );

        let outcome = update_one(&fake, &fixture.grove, &planned).unwrap();

        assert!(matches!(outcome, UpdateOutcome::Changed(_)));
        assert!(fake
            .calls()
            .iter()
            .all(|call| !call.argv_for_test().iter().any(|arg| arg == "merge")));
    }

    #[test]
    fn a_changed_head_oid_yields_changed_with_no_merge() {
        let fixture = fixture();
        let planned = planned_snapshot(&fixture);

        let fake = RecordingFake::new();
        enqueue_reinspection(
            &fake,
            &fixture,
            "main",
            "changed",
            "refs/remotes/origin/main",
            "origin/main",
            "origin",
            "def",
            0,
            3,
        );

        let outcome = update_one(&fake, &fixture.grove, &planned).unwrap();

        assert!(matches!(outcome, UpdateOutcome::Changed(_)));
        assert!(fake
            .calls()
            .iter()
            .all(|call| !call.argv_for_test().iter().any(|arg| arg == "merge")));
    }

    #[test]
    fn a_changed_upstream_full_ref_yields_changed_with_no_merge() {
        let fixture = fixture();
        let planned = planned_snapshot(&fixture);

        let fake = RecordingFake::new();
        enqueue_reinspection(
            &fake,
            &fixture,
            "main",
            "abc",
            "refs/remotes/origin/renamed",
            "origin/renamed",
            "origin",
            "def",
            0,
            3,
        );

        let outcome = update_one(&fake, &fixture.grove, &planned).unwrap();

        assert!(matches!(outcome, UpdateOutcome::Changed(_)));
        assert!(fake
            .calls()
            .iter()
            .all(|call| !call.argv_for_test().iter().any(|arg| arg == "merge")));
    }

    #[test]
    fn a_changed_upstream_remote_yields_changed_with_no_merge() {
        let fixture = fixture();
        let planned = planned_snapshot(&fixture);

        let fake = RecordingFake::new();
        enqueue_reinspection(
            &fake,
            &fixture,
            "main",
            "abc",
            "refs/remotes/origin/main",
            "origin/main",
            "up/stream",
            "def",
            0,
            3,
        );

        let outcome = update_one(&fake, &fixture.grove, &planned).unwrap();

        assert!(matches!(outcome, UpdateOutcome::Changed(_)));
        assert!(fake
            .calls()
            .iter()
            .all(|call| !call.argv_for_test().iter().any(|arg| arg == "merge")));
    }

    #[test]
    fn a_changed_upstream_oid_yields_changed_with_no_merge() {
        let fixture = fixture();
        let planned = planned_snapshot(&fixture);

        let fake = RecordingFake::new();
        enqueue_reinspection(
            &fake,
            &fixture,
            "main",
            "abc",
            "refs/remotes/origin/main",
            "origin/main",
            "origin",
            "newoid",
            0,
            3,
        );

        let outcome = update_one(&fake, &fixture.grove, &planned).unwrap();

        assert!(matches!(outcome, UpdateOutcome::Changed(_)));
        assert!(fake
            .calls()
            .iter()
            .all(|call| !call.argv_for_test().iter().any(|arg| arg == "merge")));
    }

    #[test]
    fn a_worktree_already_brought_up_to_date_yields_changed_with_no_merge() {
        let fixture = fixture();
        let planned = planned_snapshot(&fixture);

        // Same identities as planned, but no longer behind (a concurrent
        // fast-forward already landed): ahead=0, behind=0 is UP-TO-DATE,
        // not BEHIND, so it must not be merged again.
        let fake = RecordingFake::new();
        enqueue_reinspection(
            &fake,
            &fixture,
            "main",
            "abc",
            "refs/remotes/origin/main",
            "origin/main",
            "origin",
            "def",
            0,
            0,
        );

        let outcome = update_one(&fake, &fixture.grove, &planned).unwrap();

        assert!(matches!(outcome, UpdateOutcome::Changed(_)));
        assert!(fake
            .calls()
            .iter()
            .all(|call| !call.argv_for_test().iter().any(|arg| arg == "merge")));
    }

    #[test]
    fn a_now_ahead_worktree_yields_changed_with_no_merge() {
        let fixture = fixture();
        let planned = planned_snapshot(&fixture);

        let fake = RecordingFake::new();
        enqueue_reinspection(
            &fake,
            &fixture,
            "main",
            "abc",
            "refs/remotes/origin/main",
            "origin/main",
            "origin",
            "def",
            2,
            3,
        );

        let outcome = update_one(&fake, &fixture.grove, &planned).unwrap();

        assert!(matches!(outcome, UpdateOutcome::Changed(_)));
        assert!(fake
            .calls()
            .iter()
            .all(|call| !call.argv_for_test().iter().any(|arg| arg == "merge")));
    }

    #[test]
    fn a_now_dirty_worktree_yields_changed_with_no_merge() {
        let fixture = fixture();
        let planned = planned_snapshot(&fixture);

        let fake = RecordingFake::new();
        fake.push_response(output(
            0,
            &worktree_list_record(&fixture, "main", "abc"),
            b"",
        ));
        fake.push_response(output(0, b"?? dirty\0", b""));
        fake.push_response(output(
            0,
            b"refs/remotes/origin/main\0origin/main\0origin\0\n",
            b"",
        ));
        fake.push_response(output(0, b"", b""));
        fake.push_response(output(0, b"def\n", b""));
        fake.push_response(output(0, b"0\t3\n", b""));

        let outcome = update_one(&fake, &fixture.grove, &planned).unwrap();

        assert!(matches!(outcome, UpdateOutcome::Changed(_)));
        assert!(fake
            .calls()
            .iter()
            .all(|call| !call.argv_for_test().iter().any(|arg| arg == "merge")));
    }

    #[test]
    fn a_now_locked_worktree_yields_changed_with_no_merge() {
        let fixture = fixture();
        let planned = planned_snapshot(&fixture);

        let fake = RecordingFake::new();
        let mut raw =
            format!("worktree {}\0bare\0\0", fixture.grove.bare_dir().display()).into_bytes();
        raw.extend_from_slice(
            format!(
                "worktree {}\0HEAD abc\0branch refs/heads/main\0locked reason\0\0",
                fixture.path.display()
            )
            .as_bytes(),
        );
        fake.push_response(output(0, &raw, b""));
        // snapshot_record still queries status for a locked worktree;
        // classify() gives LOCKED precedence over it regardless of content.
        fake.push_response(output(0, b"", b""));
        fake.push_response(output(
            0,
            b"refs/remotes/origin/main\0origin/main\0origin\0\n",
            b"",
        ));
        fake.push_response(output(0, b"", b""));
        fake.push_response(output(0, b"def\n", b""));
        fake.push_response(output(0, b"0\t3\n", b""));

        let outcome = update_one(&fake, &fixture.grove, &planned).unwrap();

        assert!(
            matches!(outcome, UpdateOutcome::Changed(ref snapshot) if snapshot.state == WorktreeState::Locked)
        );
        assert!(fake
            .calls()
            .iter()
            .all(|call| !call.argv_for_test().iter().any(|arg| arg == "merge")));
    }

    #[test]
    fn a_now_in_progress_worktree_yields_changed_with_no_merge() {
        let fixture = fixture();
        let planned = planned_snapshot(&fixture);
        std::fs::write(fixture.admin.join("index.lock"), b"").unwrap();

        let fake = RecordingFake::new();
        fake.push_response(output(
            0,
            &worktree_list_record(&fixture, "main", "abc"),
            b"",
        ));

        let outcome = update_one(&fake, &fixture.grove, &planned).unwrap();
        std::fs::remove_file(fixture.admin.join("index.lock")).unwrap();

        assert!(
            matches!(outcome, UpdateOutcome::Changed(ref snapshot) if snapshot.state == WorktreeState::InProgress)
        );
        assert!(fake
            .calls()
            .iter()
            .all(|call| !call.argv_for_test().iter().any(|arg| arg == "merge")));
    }

    #[test]
    fn unchanged_candidate_merges_with_the_exact_pinned_arguments() {
        let fixture = fixture();
        let planned = planned_snapshot(&fixture);

        let fake = RecordingFake::new();
        enqueue_reinspection(
            &fake,
            &fixture,
            "main",
            "abc",
            "refs/remotes/origin/main",
            "origin/main",
            "origin",
            "def",
            0,
            3,
        );
        fake.push_response(output(0, b"", b""));
        enqueue_reinspection(
            &fake,
            &fixture,
            "main",
            "def",
            "refs/remotes/origin/main",
            "origin/main",
            "origin",
            "def",
            0,
            0,
        );

        let outcome = update_one(&fake, &fixture.grove, &planned).unwrap();

        assert!(
            matches!(outcome, UpdateOutcome::Updated(ref snapshot) if snapshot.state == WorktreeState::UpToDate)
        );
        let calls = fake.calls();
        let merge_call = calls
            .iter()
            .find(|call| call.argv_for_test().contains(&"merge".to_string()))
            .unwrap();
        assert_eq!(
            merge_call.argv_for_test()[2..],
            [
                "merge",
                "--ff-only",
                "--no-edit",
                "--no-autostash",
                "--no-overwrite-ignore",
                "@{upstream}",
            ]
        );
    }

    #[test]
    fn a_refused_merge_becomes_blocked_with_escaped_stderr() {
        let fixture = fixture();
        let planned = planned_snapshot(&fixture);

        let fake = RecordingFake::new();
        enqueue_reinspection(
            &fake,
            &fixture,
            "main",
            "abc",
            "refs/remotes/origin/main",
            "origin/main",
            "origin",
            "def",
            0,
            3,
        );
        fake.push_response(output(1, b"", b"local changes would be overwritten\xff"));
        enqueue_reinspection(
            &fake,
            &fixture,
            "main",
            "abc",
            "refs/remotes/origin/main",
            "origin/main",
            "origin",
            "def",
            0,
            3,
        );

        let outcome = update_one(&fake, &fixture.grove, &planned).unwrap();

        match outcome {
            UpdateOutcome::Blocked { snapshot, detail } => {
                assert_eq!(snapshot.state, WorktreeState::Behind);
                assert_eq!(
                    detail,
                    r"local\x20changes\x20would\x20be\x20overwritten\xFF"
                );
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_worktree_at_reinspection_is_reported_as_missing() {
        let fixture = fixture();
        let path = fixture.grove.root.join("gone");

        let fake = RecordingFake::new();
        fake.push_response(output(
            0,
            format!("worktree {}\0bare\0\0", fixture.grove.bare_dir().display()).as_bytes(),
            b"",
        ));

        let snapshot = reinspect_path(&fake, &fixture.grove, &path).unwrap();

        assert_eq!(snapshot.state, WorktreeState::Missing);
        assert_eq!(
            BString::from(snapshot.record.path.as_os_str().as_bytes()),
            BString::from(path.as_os_str().as_bytes())
        );
    }

    // --- `run()` orchestration: ordering, residual state, byte safety ---

    struct MultiFixture {
        _root: tempfile::TempDir,
        grove: Grove,
    }

    impl MultiFixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let root_path = root.path().canonicalize().unwrap();
            std::fs::create_dir_all(root_path.join(".bare/worktrees")).unwrap();
            MultiFixture {
                _root: root,
                grove: Grove { root: root_path },
            }
        }

        fn add_worktree(&self, name: &std::ffi::OsStr) -> (std::path::PathBuf, std::path::PathBuf) {
            let admin = self.grove.bare_dir().join("worktrees").join(name);
            let path = self.grove.root.join(name);
            std::fs::create_dir_all(&admin).unwrap();
            std::fs::create_dir(&path).unwrap();
            let mut pointer = b"gitdir: ".to_vec();
            pointer.extend_from_slice(admin.as_os_str().as_bytes());
            pointer.push(b'\n');
            std::fs::write(path.join(".git"), pointer).unwrap();
            let mut back_pointer = path.join(".git").as_os_str().as_bytes().to_vec();
            back_pointer.push(b'\n');
            std::fs::write(admin.join("gitdir"), back_pointer).unwrap();
            (path, admin)
        }
    }

    fn push_worktree_list(
        fake: &RecordingFake,
        grove: &Grove,
        entries: &[(&std::path::Path, &[u8], &[u8])],
    ) {
        let mut raw = b"worktree ".to_vec();
        raw.extend_from_slice(grove.bare_dir().as_os_str().as_bytes());
        raw.extend_from_slice(b"\0bare\0\0");
        for (path, branch, head) in entries {
            raw.extend_from_slice(b"worktree ");
            raw.extend_from_slice(path.as_os_str().as_bytes());
            raw.extend_from_slice(b"\0HEAD ");
            raw.extend_from_slice(head);
            raw.extend_from_slice(b"\0branch refs/heads/");
            raw.extend_from_slice(branch);
            raw.extend_from_slice(b"\0\0");
        }
        fake.push_response(output(0, &raw, b""));
    }

    #[allow(clippy::too_many_arguments)]
    fn push_status_ok_behind(
        fake: &RecordingFake,
        upstream_full: &str,
        upstream_short: &str,
        upstream_remote: &str,
        oid: &str,
        ahead: u32,
        behind: u32,
    ) {
        fake.push_response(output(0, b"", b""));
        fake.push_response(output(
            0,
            format!("{upstream_full}\0{upstream_short}\0{upstream_remote}\0\n").as_bytes(),
            b"",
        ));
        fake.push_response(output(0, b"", b""));
        fake.push_response(output(0, format!("{oid}\n").as_bytes(), b""));
        fake.push_response(output(0, format!("{ahead}\t{behind}\n").as_bytes(), b""));
    }

    #[test]
    fn run_merges_two_behind_candidates_sequentially_in_raw_path_order() {
        let fixture = MultiFixture::new();
        let (zzz_path, _zzz_admin) = fixture.add_worktree(std::ffi::OsStr::new("zzz"));
        let (aaa_path, _aaa_admin) = fixture.add_worktree(std::ffi::OsStr::new("aaa"));

        let fake = RecordingFake::new();
        // 1. Canonical records, deliberately listed in reverse path order.
        push_worktree_list(
            &fake,
            &fixture.grove,
            &[(&zzz_path, b"zzz", b"abc"), (&aaa_path, b"aaa", b"abc")],
        );
        // 2. FetchPlan queries each unique branch once, in branch-byte order.
        fake.push_response(output(
            0,
            b"refs/remotes/origin/aaa\0origin/aaa\0origin\0\n",
            b"",
        ));
        fake.push_response(output(
            0,
            b"refs/remotes/origin/zzz\0origin/zzz\0origin\0\n",
            b"",
        ));
        // 3. One fetch for the single deduplicated remote.
        fake.push_response(output(0, b"", b""));
        // 4. Fresh post-fetch snapshots, in the original (unsorted) record order.
        push_status_ok_behind(
            &fake,
            "refs/remotes/origin/zzz",
            "origin/zzz",
            "origin",
            "zzz-oid",
            0,
            3,
        );
        push_status_ok_behind(
            &fake,
            "refs/remotes/origin/aaa",
            "origin/aaa",
            "origin",
            "aaa-oid",
            0,
            3,
        );
        // 5/6. update_one("aaa") first (raw-path order): reinspect, merge, reinspect.
        push_worktree_list(
            &fake,
            &fixture.grove,
            &[(&zzz_path, b"zzz", b"abc"), (&aaa_path, b"aaa", b"abc")],
        );
        push_status_ok_behind(
            &fake,
            "refs/remotes/origin/aaa",
            "origin/aaa",
            "origin",
            "aaa-oid",
            0,
            3,
        );
        fake.push_response(output(0, b"", b""));
        push_worktree_list(
            &fake,
            &fixture.grove,
            &[(&zzz_path, b"zzz", b"abc"), (&aaa_path, b"aaa", b"abc")],
        );
        push_status_ok_behind(
            &fake,
            "refs/remotes/origin/aaa",
            "origin/aaa",
            "origin",
            "aaa-oid",
            0,
            0,
        );
        // update_one("zzz") second: reinspect, merge, reinspect.
        push_worktree_list(
            &fake,
            &fixture.grove,
            &[(&zzz_path, b"zzz", b"abc"), (&aaa_path, b"aaa", b"abc")],
        );
        push_status_ok_behind(
            &fake,
            "refs/remotes/origin/zzz",
            "origin/zzz",
            "origin",
            "zzz-oid",
            0,
            3,
        );
        fake.push_response(output(0, b"", b""));
        push_worktree_list(
            &fake,
            &fixture.grove,
            &[(&zzz_path, b"zzz", b"abc"), (&aaa_path, b"aaa", b"abc")],
        );
        push_status_ok_behind(
            &fake,
            "refs/remotes/origin/zzz",
            "origin/zzz",
            "origin",
            "zzz-oid",
            0,
            0,
        );

        let report = run(&fake, &fixture.grove).unwrap();

        assert_eq!(report.class, ExitClass::Ok);
        assert_eq!(
            report.rows.iter().map(|row| row.status).collect::<Vec<_>>(),
            ["UP-TO-DATE", "UP-TO-DATE"]
        );
        let merge_indices: Vec<usize> = fake
            .calls()
            .iter()
            .enumerate()
            .filter(|(_, call)| call.argv_for_test().contains(&"merge".to_string()))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(merge_indices.len(), 2, "expected exactly two merge calls");
        let first_merge_path = fake.calls()[merge_indices[0]]
            .argv_for_test()
            .iter()
            .find(|arg| arg.contains("aaa"))
            .is_some();
        assert!(
            first_merge_path,
            "the first merge call must target the raw-path-first candidate (aaa), got {:?}",
            fake.calls()[merge_indices[0]].argv_for_test()
        );
        assert!(merge_indices[0] < merge_indices[1]);
    }

    #[test]
    fn run_reports_a_residual_behind_row_and_exit_two_when_a_candidate_only_partially_changes() {
        let fixture = MultiFixture::new();
        let (path, _admin) = fixture.add_worktree(std::ffi::OsStr::new("main"));

        let fake = RecordingFake::new();
        push_worktree_list(&fake, &fixture.grove, &[(&path, b"main", b"abc")]);
        fake.push_response(output(
            0,
            b"refs/remotes/origin/main\0origin/main\0origin\0\n",
            b"",
        ));
        fake.push_response(output(0, b"", b""));
        push_status_ok_behind(
            &fake,
            "refs/remotes/origin/main",
            "origin/main",
            "origin",
            "def",
            0,
            3,
        );
        // update_one's reinspection reports a *different* upstream remote
        // than planned (a benign identity change mid-run), which is
        // ineligible for merge; the graph is still clean BEHIND, so the
        // final row legitimately remains BEHIND and the run still needs a
        // human decision.
        push_worktree_list(&fake, &fixture.grove, &[(&path, b"main", b"abc")]);
        push_status_ok_behind(
            &fake,
            "refs/remotes/origin/main",
            "origin/main",
            "up/stream",
            "def",
            0,
            3,
        );

        let report = run(&fake, &fixture.grove).unwrap();

        assert_eq!(report.class, ExitClass::NeedsDecision);
        assert_eq!(report.rows.len(), 1);
        assert_eq!(report.rows[0].status, "BEHIND");
        assert!(fake
            .calls()
            .iter()
            .all(|call| !call.argv_for_test().contains(&"merge".to_string())));
    }

    #[cfg(unix)]
    #[test]
    fn run_preserves_non_utf8_path_bytes_through_sorting_and_a_blocked_diagnostic() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let fixture = MultiFixture::new();
        let odd_name = OsString::from_vec(b"odd-\xff".to_vec());
        let (path, _admin) = fixture.add_worktree(&odd_name);

        let fake = RecordingFake::new();
        push_worktree_list(&fake, &fixture.grove, &[(&path, b"main", b"abc")]);
        fake.push_response(output(
            0,
            b"refs/remotes/origin/main\0origin/main\0origin\0\n",
            b"",
        ));
        fake.push_response(output(0, b"", b""));
        push_status_ok_behind(
            &fake,
            "refs/remotes/origin/main",
            "origin/main",
            "origin",
            "def",
            0,
            3,
        );
        push_worktree_list(&fake, &fixture.grove, &[(&path, b"main", b"abc")]);
        push_status_ok_behind(
            &fake,
            "refs/remotes/origin/main",
            "origin/main",
            "origin",
            "def",
            0,
            3,
        );
        fake.push_response(output(1, b"", b"local changes would be overwritten\xff"));
        push_worktree_list(&fake, &fixture.grove, &[(&path, b"main", b"abc")]);
        push_status_ok_behind(
            &fake,
            "refs/remotes/origin/main",
            "origin/main",
            "origin",
            "def",
            0,
            3,
        );

        let report = run(&fake, &fixture.grove).unwrap();

        assert_eq!(report.class, ExitClass::NeedsDecision);
        assert_eq!(report.rows[0].status, "BLOCKED");
        assert_eq!(
            report.rows[0].path.as_os_str().as_bytes(),
            path.as_os_str().as_bytes()
        );
        assert_eq!(
            report.diagnostics,
            vec![r"local\x20changes\x20would\x20be\x20overwritten\xFF".to_string()]
        );
        // The path's raw bytes must round-trip, byte-for-byte, through
        // every git argv that carries it — never replaced or corrupted.
        let carries_path = fake.calls().iter().any(|call| {
            call.argv_os().iter().any(|arg| {
                arg.as_bytes()
                    .windows(odd_name.len())
                    .any(|window| window == odd_name.as_bytes())
            })
        });
        assert!(
            carries_path,
            "expected at least one argv to carry the raw path bytes"
        );
    }
}
