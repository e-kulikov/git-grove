use crate::git::query::{Status, WorktreeLocation, WorktreeRecord};
use bstr::BString;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeState {
    Invalid,
    Missing,
    Locked,
    InProgress,
    Unborn,
    Detached,
    Local,
    UpstreamGone,
    UpToDate,
    Ahead,
    Behind,
    DirtyBehind,
    Diverged,
    Unknown,
    Blocked,
}

impl WorktreeState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "INVALID",
            Self::Missing => "MISSING",
            Self::Locked => "LOCKED",
            Self::InProgress => "IN-PROGRESS",
            Self::Unborn => "UNBORN",
            Self::Detached => "DETACHED",
            Self::Local => "LOCAL",
            Self::UpstreamGone => "UPSTREAM-GONE",
            Self::UpToDate => "UP-TO-DATE",
            Self::Ahead => "AHEAD",
            Self::Behind => "BEHIND",
            Self::DirtyBehind => "DIRTY-BEHIND",
            Self::Diverged => "DIVERGED",
            Self::Unknown => "UNKNOWN",
            Self::Blocked => "BLOCKED",
        }
    }

    pub fn sync_is_satisfied(self) -> bool {
        matches!(
            self,
            Self::Unborn | Self::Detached | Self::Local | Self::UpToDate | Self::Ahead
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackingSnapshot {
    pub upstream_short: BString,
    pub upstream_ref: BString,
    pub upstream_remote: Option<BString>,
    pub head_oid: BString,
    pub upstream_oid: BString,
    pub ahead: u32,
    pub behind: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub record: WorktreeRecord,
    pub admin_dir: Option<PathBuf>,
    pub state: WorktreeState,
    pub dirty: bool,
    pub upstream: Option<BString>,
    pub tracking: Option<TrackingSnapshot>,
}

pub fn classify(
    record: WorktreeRecord,
    location: WorktreeLocation,
    status: Option<Status>,
) -> Snapshot {
    let issue = match location {
        WorktreeLocation::Invalid => Some(WorktreeState::Invalid),
        WorktreeLocation::Missing => Some(WorktreeState::Missing),
        WorktreeLocation::Valid { .. } if record.prunable.is_some() => Some(WorktreeState::Invalid),
        WorktreeLocation::Valid { .. } => None,
    };
    if let Some(state) = issue {
        return Snapshot {
            record,
            admin_dir: None,
            state,
            dirty: false,
            upstream: None,
            tracking: None,
        };
    }

    let admin_dir = match location {
        WorktreeLocation::Valid { admin_dir } => Some(admin_dir),
        WorktreeLocation::Missing | WorktreeLocation::Invalid => unreachable!(),
    };
    let dirty = status.as_ref().is_some_and(|status| status.dirty);
    let upstream = status.as_ref().and_then(|status| status.upstream.clone());
    let tracking = status.as_ref().and_then(|status| {
        Some(TrackingSnapshot {
            upstream_short: status.upstream.clone()?,
            upstream_ref: status.upstream_ref.clone()?,
            upstream_remote: status.upstream_remote.clone(),
            head_oid: record.head.clone()?,
            upstream_oid: status.upstream_oid.clone()?,
            ahead: status.ahead?,
            behind: status.behind?,
        })
    });
    let state = if record.locked.is_some() {
        WorktreeState::Locked
    } else if status.as_ref().is_some_and(|status| status.in_progress) {
        WorktreeState::InProgress
    } else if record
        .head
        .as_ref()
        .is_some_and(|head| head.iter().all(|byte| *byte == b'0'))
    {
        WorktreeState::Unborn
    } else if record.detached {
        WorktreeState::Detached
    } else if let Some(status) = status.as_ref() {
        if status.upstream.is_none() {
            WorktreeState::Local
        } else if status.upstream_gone {
            WorktreeState::UpstreamGone
        } else if status.graph_unknown {
            WorktreeState::Unknown
        } else {
            match (status.ahead, status.behind) {
                (Some(0), Some(0)) => WorktreeState::UpToDate,
                (Some(ahead), Some(0)) if ahead > 0 => WorktreeState::Ahead,
                (Some(0), Some(behind)) if behind > 0 && status.dirty => WorktreeState::DirtyBehind,
                (Some(0), Some(behind)) if behind > 0 => WorktreeState::Behind,
                (Some(ahead), Some(behind)) if ahead > 0 && behind > 0 => WorktreeState::Diverged,
                _ => WorktreeState::Unknown,
            }
        }
    } else {
        WorktreeState::Unknown
    };
    Snapshot {
        record,
        admin_dir,
        state,
        dirty,
        upstream,
        tracking,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::query::{Status, WorktreeLocation};

    fn record() -> WorktreeRecord {
        WorktreeRecord {
            path: "/g/main".into(),
            head: Some(BString::from("abc")),
            branch: Some(BString::from("main")),
            bare: false,
            detached: false,
            locked: None,
            prunable: None,
        }
    }

    fn valid() -> WorktreeLocation {
        WorktreeLocation::Valid {
            admin_dir: "/g/.bare/worktrees/main".into(),
        }
    }

    #[test]
    fn labels_and_sync_policy_cover_every_state() {
        let cases = [
            (WorktreeState::Invalid, "INVALID", false),
            (WorktreeState::Missing, "MISSING", false),
            (WorktreeState::Locked, "LOCKED", false),
            (WorktreeState::InProgress, "IN-PROGRESS", false),
            (WorktreeState::Unborn, "UNBORN", true),
            (WorktreeState::Detached, "DETACHED", true),
            (WorktreeState::Local, "LOCAL", true),
            (WorktreeState::UpstreamGone, "UPSTREAM-GONE", false),
            (WorktreeState::UpToDate, "UP-TO-DATE", true),
            (WorktreeState::Ahead, "AHEAD", true),
            (WorktreeState::Behind, "BEHIND", false),
            (WorktreeState::DirtyBehind, "DIRTY-BEHIND", false),
            (WorktreeState::Diverged, "DIVERGED", false),
            (WorktreeState::Unknown, "UNKNOWN", false),
            (WorktreeState::Blocked, "BLOCKED", false),
        ];
        for (state, label, satisfied) in cases {
            assert_eq!(state.as_str(), label);
            assert_eq!(state.sync_is_satisfied(), satisfied, "{state:?}");
        }
    }

    #[test]
    fn classifier_preserves_precedence_and_complete_tracking_identity() {
        assert_eq!(
            classify(record(), WorktreeLocation::Invalid, None).state,
            WorktreeState::Invalid
        );
        assert_eq!(
            classify(record(), WorktreeLocation::Missing, None).state,
            WorktreeState::Missing
        );

        let mut locked = record();
        locked.locked = Some(BString::from("reason"));
        assert_eq!(
            classify(
                locked,
                valid(),
                Some(Status {
                    in_progress: true,
                    ..Status::default()
                })
            )
            .state,
            WorktreeState::Locked
        );

        assert_eq!(
            classify(
                record(),
                valid(),
                Some(Status {
                    in_progress: true,
                    ..Status::default()
                })
            )
            .state,
            WorktreeState::InProgress
        );

        let status = Status {
            upstream: Some(BString::from("up/stream/topic")),
            upstream_ref: Some(BString::from("refs/remotes/up/stream/topic")),
            upstream_remote: Some(BString::from("up/stream")),
            upstream_oid: Some(BString::from("def")),
            ahead: Some(0),
            behind: Some(3),
            dirty: true,
            ..Status::default()
        };
        let snapshot = classify(record(), valid(), Some(status));
        assert_eq!(snapshot.state, WorktreeState::DirtyBehind);
        assert!(snapshot.dirty);
        assert_eq!(snapshot.admin_dir, Some("/g/.bare/worktrees/main".into()));
        assert_eq!(
            snapshot.tracking,
            Some(TrackingSnapshot {
                upstream_short: BString::from("up/stream/topic"),
                upstream_ref: BString::from("refs/remotes/up/stream/topic"),
                upstream_remote: Some(BString::from("up/stream")),
                head_oid: BString::from("abc"),
                upstream_oid: BString::from("def"),
                ahead: 0,
                behind: 3,
            })
        );

        let upstream_gone = classify(
            record(),
            valid(),
            Some(Status {
                upstream: Some(BString::from("origin/main")),
                upstream_gone: true,
                ..Status::default()
            }),
        );
        assert_eq!(upstream_gone.state, WorktreeState::UpstreamGone);
        assert!(upstream_gone.tracking.is_none());
        assert_eq!(upstream_gone.upstream, Some(BString::from("origin/main")));

        let unknown = classify(
            record(),
            valid(),
            Some(Status {
                upstream: Some(BString::from("origin/main")),
                graph_unknown: true,
                ..Status::default()
            }),
        );
        assert_eq!(unknown.state, WorktreeState::Unknown);
        assert!(unknown.tracking.is_none());
        assert_eq!(unknown.upstream, Some(BString::from("origin/main")));
    }
}
