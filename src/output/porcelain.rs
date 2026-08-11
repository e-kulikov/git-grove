use crate::output::Row;
use std::os::unix::ffi::OsStrExt;

fn field(out: &mut Vec<u8>, key: &[u8], value: &[u8]) {
    out.extend_from_slice(key);
    out.push(0);
    out.extend_from_slice(value);
    out.push(0);
}

pub fn render(rows: &[Row]) -> Vec<u8> {
    let mut out = b"git-grove-list-v1\0".to_vec();
    for row in rows {
        field(&mut out, b"worktree", row.path.as_os_str().as_bytes());
        field(&mut out, b"status", row.status.as_bytes());
        field(
            &mut out,
            b"branch",
            row.branch.as_ref().map_or(b"", |value| value.as_ref()),
        );
        field(
            &mut out,
            b"upstream",
            row.upstream.as_ref().map_or(b"", |value| value.as_ref()),
        );
        let ahead = row.ahead.map(|value| value.to_string());
        field(
            &mut out,
            b"ahead",
            ahead.as_ref().map_or(b"", String::as_bytes),
        );
        let behind = row.behind.map(|value| value.to_string());
        field(
            &mut out,
            b"behind",
            behind.as_ref().map_or(b"", String::as_bytes),
        );
        field(&mut out, b"dirty", if row.dirty { b"1" } else { b"0" });
        field(
            &mut out,
            b"locked",
            row.locked.as_ref().map_or(b"", |value| value.as_ref()),
        );
        out.push(0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bstr::BString;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn emits_exact_versioned_raw_nul_protocol() {
        let rows = [Row {
            path: OsString::from_vec(b"/g/work-\xfe".to_vec()).into(),
            status: "DIVERGED",
            branch: Some(BString::from(b"topic-\xff".as_slice())),
            upstream: Some(BString::from(b"origin/topic-\xfd".as_slice())),
            ahead: Some(2),
            behind: Some(3),
            dirty: true,
            locked: Some(BString::from(b"review-\xfc".as_slice())),
        }];

        let bytes = render(&rows);

        assert_eq!(
            bytes,
            b"git-grove-list-v1\x00\
              worktree\x00/g/work-\xfe\x00status\x00DIVERGED\x00branch\x00topic-\xff\x00\
              upstream\x00origin/topic-\xfd\x00ahead\x002\x00behind\x003\x00dirty\x001\x00\
              locked\x00review-\xfc\x00\x00"
        );
    }
}
