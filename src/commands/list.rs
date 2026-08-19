use crate::error::{ExitClass, Result};
use crate::git::query::{self, WorktreeLocation};
use crate::git::runner::GitRunner;
use crate::grove::discover::Grove;
use crate::grove::state::{self, Snapshot, WorktreeState};
use crate::output::{self, Row};

pub fn snapshot_record(
    runner: &dyn GitRunner,
    grove: &Grove,
    record: query::WorktreeRecord,
) -> Result<Snapshot> {
    let location = query::inspect_worktree(grove, &record)?;
    let status = match &location {
        WorktreeLocation::Valid { admin_dir } if record.prunable.is_none() => {
            Some(query::status_at(runner, &record, admin_dir)?)
        }
        WorktreeLocation::Valid { .. } | WorktreeLocation::Missing | WorktreeLocation::Invalid => {
            None
        }
    };
    Ok(state::classify(record, location, status))
}

pub fn collect_snapshots(runner: &dyn GitRunner, grove: &Grove) -> Result<Vec<Snapshot>> {
    let mut snapshots = Vec::new();
    for record in query::worktrees(runner, grove)? {
        if record.bare {
            continue;
        }
        snapshots.push(snapshot_record(runner, grove, record)?);
    }
    Ok(snapshots)
}

fn exit_class(snapshots: &[Snapshot]) -> ExitClass {
    let needs_decision = snapshots.iter().any(|snapshot| {
        matches!(
            snapshot.state,
            WorktreeState::Invalid | WorktreeState::Missing
        )
    });
    if needs_decision {
        ExitClass::NeedsDecision
    } else {
        ExitClass::Ok
    }
}

pub fn run(runner: &dyn GitRunner, grove: &Grove, porcelain: bool) -> Result<ExitClass> {
    let snapshots = collect_snapshots(runner, grove)?;
    let class = exit_class(&snapshots);
    let rows: Vec<Row> = snapshots.iter().map(Row::from).collect();
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    output::write_rows(&mut writer, &rows, porcelain)?;
    Ok(class)
}
