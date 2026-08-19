pub mod porcelain;

use crate::error::GroveError;
use crate::grove::state::Snapshot;
use bstr::{BString, ByteSlice};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub path: PathBuf,
    pub status: &'static str,
    pub branch: Option<BString>,
    pub upstream: Option<BString>,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub dirty: bool,
    pub locked: Option<BString>,
}

impl From<&Snapshot> for Row {
    fn from(snapshot: &Snapshot) -> Self {
        Row {
            path: snapshot.record.path.clone(),
            status: snapshot.state.as_str(),
            branch: snapshot.record.branch.clone(),
            upstream: snapshot
                .tracking
                .as_ref()
                .map(|tracking| tracking.upstream_short.clone()),
            ahead: snapshot.tracking.as_ref().map(|tracking| tracking.ahead),
            behind: snapshot.tracking.as_ref().map(|tracking| tracking.behind),
            dirty: snapshot.dirty,
            locked: snapshot.record.locked.clone(),
        }
    }
}

pub fn write_rows(
    writer: &mut dyn Write,
    rows: &[Row],
    porcelain: bool,
) -> crate::error::Result<()> {
    let bytes = if porcelain {
        self::porcelain::render(rows)
    } else {
        render_human(rows).into_bytes()
    };
    writer
        .write_all(&bytes)
        .and_then(|_| writer.flush())
        .map_err(|error| GroveError::failure(format!("cannot write stdout: {error}")))
}

fn escaped(bytes: &[u8]) -> String {
    bytes.escape_bytes().to_string()
}

pub fn render_human(rows: &[Row]) -> String {
    let mut text = String::from("STATUS\tAHEAD\tBEHIND\tBRANCH\tUPSTREAM\tPATH\tDIRTY\tLOCKED\n");
    for row in rows {
        let ahead = row
            .ahead
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".into());
        let behind = row
            .behind
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".into());
        let branch = row
            .branch
            .as_ref()
            .map(|value| escaped(value.as_ref()))
            .unwrap_or_else(|| "-".into());
        let upstream = row
            .upstream
            .as_ref()
            .map(|value| escaped(value.as_ref()))
            .unwrap_or_else(|| "-".into());
        let locked = row
            .locked
            .as_ref()
            .map(|value| escaped(value.as_ref()))
            .unwrap_or_else(|| "-".into());
        text.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            row.status,
            ahead,
            behind,
            branch,
            upstream,
            escaped(row.path.as_os_str().as_bytes()),
            if row.dirty { "1" } else { "0" },
            locked
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ExitClass;
    use bstr::BString;
    use std::ffi::OsString;
    use std::io;
    use std::os::unix::ffi::OsStringExt;

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

    #[test]
    fn human_output_reversibly_escapes_raw_path_and_ref_bytes() {
        let rows = [Row {
            path: OsString::from_vec(b"/g/work \xfe".to_vec()).into(),
            status: "LOCAL",
            branch: Some(BString::from(b"topic\t\xff".as_slice())),
            upstream: None,
            ahead: None,
            behind: None,
            dirty: true,
            locked: None,
        }];

        let rendered = render_human(&rows);

        assert!(rendered.contains(r"/g/work\x20\xFE"));
        assert!(rendered.contains(r"topic\t\xFF"));
        assert!(!rendered.contains('\u{fffd}'));
    }
}
