use crate::error::{GroveError, Result};
use crate::git::query::{self, WorktreeRecord};
use crate::git::runner::{GitRunner, Invocation};
use crate::grove::discover::Grove;
use bstr::{BString, ByteSlice};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchPlan {
    pub remotes: Vec<BString>,
}

impl FetchPlan {
    pub fn from_records(
        runner: &dyn GitRunner,
        grove: &Grove,
        records: &[WorktreeRecord],
    ) -> Result<FetchPlan> {
        let mut branches = BTreeSet::<&BString>::new();
        for record in records {
            let unborn = record
                .head
                .as_ref()
                .is_some_and(|head| head.iter().all(|byte| *byte == b'0'));
            if unborn {
                continue;
            }
            if let Some(branch) = &record.branch {
                branches.insert(branch);
            }
        }
        let mut remotes = BTreeSet::<BString>::new();
        for branch in branches {
            if let Some(upstream) = query::branch_upstream(runner, grove, branch.as_ref())? {
                if let Some(remote) = upstream.remote {
                    remotes.insert(remote);
                }
            }
        }
        Ok(FetchPlan {
            remotes: remotes.into_iter().collect(),
        })
    }

    pub fn execute(&self, runner: &dyn GitRunner, grove: &Grove) -> Result<()> {
        for remote in &self.remotes {
            let output = runner.run(Invocation::new().git_dir(grove.bare_dir()).args([
                OsStr::new("fetch"),
                OsStr::new("--atomic"),
                OsStr::new("--prune"),
                OsStr::new("--no-prune-tags"),
                OsStr::new("--no-recurse-submodules"),
                OsStr::new("--no-auto-maintenance"),
                OsStr::new("--no-write-commit-graph"),
                OsStr::new("--"),
                OsStr::from_bytes(remote.as_ref()),
            ]))?;
            if !output.ok() {
                return Err(GroveError::failure("git fetch for sync failed")
                    .with_detail(output.stderr.as_slice().escape_bytes().to_string()));
            }
        }
        Ok(())
    }
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

    fn record(branch: Option<&str>, detached: bool) -> WorktreeRecord {
        WorktreeRecord {
            path: "/g/wt".into(),
            head: Some(BString::from("abc")),
            branch: branch.map(BString::from),
            bare: false,
            detached,
            locked: None,
            prunable: None,
        }
    }

    /// An unborn checkout still reports `branch refs/heads/<name>`
    /// (HEAD is a symbolic ref to a branch that has no commit yet), unlike
    /// a detached worktree, which has no branch at all.
    fn unborn_record(branch: &str) -> WorktreeRecord {
        WorktreeRecord {
            path: "/g/wt".into(),
            head: Some(BString::from("0".repeat(40))),
            branch: Some(BString::from(branch)),
            bare: false,
            detached: false,
            locked: None,
            prunable: None,
        }
    }

    fn upstream_response(remote: &str, branch: &str) -> GitOutput {
        output(
            0,
            format!("refs/remotes/{remote}/{branch}\0{remote}/{branch}\0{remote}\0\n").as_bytes(),
            b"",
        )
    }

    #[test]
    fn detached_and_unborn_worktrees_contribute_no_remote() {
        let fake = RecordingFake::new();
        let records = [
            record(None, true),
            record(None, false),
            unborn_record("main"),
        ];

        let plan = FetchPlan::from_records(&fake, &grove(), &records).unwrap();

        assert_eq!(plan.remotes, Vec::<BString>::new());
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn a_local_only_branch_contributes_no_remote() {
        let fake = RecordingFake::new();
        fake.push_response(output(0, b"refs/heads/main\0main\0.\0\n", b""));
        let records = [record(Some("topic"), false)];

        let plan = FetchPlan::from_records(&fake, &grove(), &records).unwrap();

        assert_eq!(plan.remotes, Vec::<BString>::new());
    }

    #[test]
    fn duplicate_branches_query_the_upstream_once() {
        let fake = RecordingFake::new();
        fake.push_response(upstream_response("origin", "topic"));
        let records = [record(Some("topic"), false), record(Some("topic"), false)];

        let plan = FetchPlan::from_records(&fake, &grove(), &records).unwrap();

        assert_eq!(plan.remotes, vec![BString::from("origin")]);
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn two_remotes_are_deduplicated_and_sorted_by_raw_bytes() {
        let fake = RecordingFake::new();
        fake.push_response(upstream_response("z-remote", "topic"));
        fake.push_response(upstream_response("a-remote", "other"));
        let records = [record(Some("topic"), false), record(Some("other"), false)];

        let plan = FetchPlan::from_records(&fake, &grove(), &records).unwrap();

        assert_eq!(
            plan.remotes,
            vec![BString::from("a-remote"), BString::from("z-remote")]
        );
    }

    #[test]
    fn skip_fetch_all_is_not_consulted() {
        let fake = RecordingFake::new();
        fake.push_response(upstream_response("origin", "topic"));
        let records = [record(Some("topic"), false)];

        let plan = FetchPlan::from_records(&fake, &grove(), &records).unwrap();

        assert_eq!(plan.remotes, vec![BString::from("origin")]);
        assert_eq!(
            fake.calls()[0].argv_for_test(),
            [
                "--git-dir=/g/.bare",
                "for-each-ref",
                "--format=%(upstream)%00%(upstream:short)%00%(upstream:remotename)%00",
                "--",
                "refs/heads/topic",
            ]
        );
    }

    #[test]
    fn an_unexpected_upstream_query_status_is_a_failure() {
        let fake = RecordingFake::new();
        fake.push_response(output(7, b"", b"broken query"));
        let records = [record(Some("topic"), false)];

        let error = FetchPlan::from_records(&fake, &grove(), &records).unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
    }

    #[test]
    fn an_empty_plan_performs_no_fetch() {
        let fake = RecordingFake::new();
        let plan = FetchPlan { remotes: vec![] };

        plan.execute(&fake, &grove()).unwrap();

        assert!(fake.calls().is_empty());
    }

    #[test]
    fn execute_issues_one_exact_atomic_fetch_per_remote() {
        let fake = RecordingFake::new();
        let plan = FetchPlan {
            remotes: vec![BString::from("origin")],
        };

        plan.execute(&fake, &grove()).unwrap();

        assert_eq!(
            fake.calls()[0].argv_for_test(),
            [
                "--git-dir=/g/.bare",
                "fetch",
                "--atomic",
                "--prune",
                "--no-prune-tags",
                "--no-recurse-submodules",
                "--no-auto-maintenance",
                "--no-write-commit-graph",
                "--",
                "origin",
            ]
        );
    }

    #[test]
    fn execute_fetches_two_remotes_in_raw_byte_order_without_all_or_multiple() {
        let fake = RecordingFake::new();
        let plan = FetchPlan {
            remotes: vec![BString::from("a-remote"), BString::from("z-remote")],
        };

        plan.execute(&fake, &grove()).unwrap();

        let calls = fake.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].argv_for_test().last().unwrap(), "a-remote");
        assert_eq!(calls[1].argv_for_test().last().unwrap(), "z-remote");
        for call in &calls {
            let argv = call.argv_for_test();
            assert!(!argv.contains(&"--all".to_string()));
            assert!(!argv.contains(&"--multiple".to_string()));
        }
    }

    #[test]
    fn execute_stops_at_the_first_fetch_failure() {
        let fake = RecordingFake::new();
        fake.push_response(output(1, b"", b"fetch failed"));
        let plan = FetchPlan {
            remotes: vec![BString::from("a-remote"), BString::from("z-remote")],
        };

        let error = plan.execute(&fake, &grove()).unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
        assert_eq!(error.detail.as_deref(), Some(r"fetch\x20failed"));
        assert_eq!(fake.calls().len(), 1);
    }
}
