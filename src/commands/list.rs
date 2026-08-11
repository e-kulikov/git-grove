use crate::error::{ExitClass, GroveError, Result};
use crate::git::query::{self, WorktreeLocation};
use crate::git::runner::GitRunner;
use crate::grove::discover::Grove;
use crate::output::{self, Row};
use std::io::Write;

fn is_unborn(record: &query::WorktreeRecord) -> bool {
    record
        .head
        .as_ref()
        .is_some_and(|head| head.iter().all(|byte| *byte == b'0'))
}

fn classify(record: &query::WorktreeRecord, status: &query::Status) -> &'static str {
    if record.locked.is_some() {
        "LOCKED"
    } else if is_unborn(record) {
        "UNBORN"
    } else if record.detached {
        "DETACHED"
    } else if status.upstream.is_none() {
        "LOCAL"
    } else if status.upstream_gone {
        "UPSTREAM-GONE"
    } else if status.graph_unknown {
        "UNKNOWN"
    } else {
        match (status.ahead, status.behind) {
            (Some(0), Some(0)) => "UP-TO-DATE",
            (Some(ahead), Some(0)) if ahead > 0 => "AHEAD",
            (Some(0), Some(behind)) if behind > 0 && status.dirty => "DIRTY-BEHIND",
            (Some(0), Some(behind)) if behind > 0 => "BEHIND",
            (Some(ahead), Some(behind)) if ahead > 0 && behind > 0 => "DIVERGED",
            _ => "UNKNOWN",
        }
    }
}

fn row_for_valid(
    runner: &dyn GitRunner,
    record: query::WorktreeRecord,
    admin_dir: &std::path::Path,
) -> Result<Row> {
    let status = query::status_at(runner, &record, admin_dir)?;
    let label = classify(&record, &status);
    Ok(Row {
        path: record.path,
        status: label,
        branch: record.branch,
        upstream: status.upstream,
        ahead: status.ahead,
        behind: status.behind,
        dirty: status.dirty,
        locked: record.locked,
    })
}

fn issue_row(record: query::WorktreeRecord, label: &'static str) -> Row {
    Row {
        path: record.path,
        status: label,
        branch: record.branch,
        upstream: None,
        ahead: None,
        behind: None,
        dirty: false,
        locked: record.locked,
    }
}

fn collect_rows(runner: &dyn GitRunner, grove: &Grove) -> Result<(Vec<Row>, ExitClass)> {
    let mut rows = Vec::new();
    let mut class = ExitClass::Ok;
    for record in query::worktrees(runner, grove)? {
        if record.bare {
            continue;
        }
        let location = query::inspect_worktree(grove, &record)?;
        let row = match location {
            WorktreeLocation::Missing => {
                class = ExitClass::NeedsDecision;
                issue_row(record, "MISSING")
            }
            WorktreeLocation::Invalid => {
                class = ExitClass::NeedsDecision;
                issue_row(record, "INVALID")
            }
            WorktreeLocation::Valid { admin_dir: _ } if record.prunable.is_some() => {
                class = ExitClass::NeedsDecision;
                issue_row(record, "INVALID")
            }
            WorktreeLocation::Valid { admin_dir } => row_for_valid(runner, record, &admin_dir)?,
        };
        rows.push(row);
    }
    Ok((rows, class))
}

fn write_rows(writer: &mut dyn Write, rows: &[Row], porcelain: bool) -> Result<()> {
    let bytes = if porcelain {
        output::porcelain::render(rows)
    } else {
        output::render_human(rows).into_bytes()
    };
    writer
        .write_all(&bytes)
        .and_then(|_| writer.flush())
        .map_err(|error| GroveError::failure(format!("cannot write stdout: {error}")))
}

pub fn run(runner: &dyn GitRunner, grove: &Grove, porcelain: bool) -> Result<ExitClass> {
    let (rows, class) = collect_rows(runner, grove)?;
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    write_rows(&mut writer, &rows, porcelain)?;
    Ok(class)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ExitClass;
    use crate::git::query::{Status, WorktreeRecord};
    use bstr::BString;
    use std::io::{self, Write};

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }
    }

    #[test]
    fn propagates_stdout_write_failures_as_failure() {
        let error = write_rows(&mut FailingWriter, &[], true).unwrap_err();
        assert_eq!(error.class, ExitClass::Failure);
        assert!(error.message.contains("stdout"));
    }

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

    #[test]
    fn applies_the_documented_classification_precedence() {
        let cases = [
            (Status::default(), "LOCAL"),
            (
                Status {
                    upstream: Some(BString::from("origin/main")),
                    upstream_gone: true,
                    ..Status::default()
                },
                "UPSTREAM-GONE",
            ),
            (
                Status {
                    upstream: Some(BString::from("origin/main")),
                    graph_unknown: true,
                    ..Status::default()
                },
                "UNKNOWN",
            ),
            (
                Status {
                    upstream: Some(BString::from("origin/main")),
                    ahead: Some(0),
                    behind: Some(0),
                    ..Status::default()
                },
                "UP-TO-DATE",
            ),
            (
                Status {
                    upstream: Some(BString::from("origin/main")),
                    ahead: Some(2),
                    behind: Some(0),
                    dirty: true,
                    ..Status::default()
                },
                "AHEAD",
            ),
            (
                Status {
                    upstream: Some(BString::from("origin/main")),
                    ahead: Some(0),
                    behind: Some(3),
                    ..Status::default()
                },
                "BEHIND",
            ),
            (
                Status {
                    upstream: Some(BString::from("origin/main")),
                    ahead: Some(0),
                    behind: Some(3),
                    dirty: true,
                    ..Status::default()
                },
                "DIRTY-BEHIND",
            ),
            (
                Status {
                    upstream: Some(BString::from("origin/main")),
                    ahead: Some(2),
                    behind: Some(3),
                    ..Status::default()
                },
                "DIVERGED",
            ),
        ];
        for (status, expected) in cases {
            assert_eq!(classify(&record(), &status), expected);
        }

        let mut unborn = record();
        unborn.head = Some(BString::from("0000000000000000000000000000000000000000"));
        unborn.locked = Some(BString::from("reason"));
        assert_eq!(classify(&unborn, &Status::default()), "LOCKED");
        unborn.locked = None;
        assert_eq!(classify(&unborn, &Status::default()), "UNBORN");

        let mut detached = record();
        detached.detached = true;
        detached.branch = None;
        assert_eq!(classify(&detached, &Status::default()), "DETACHED");
    }
}
