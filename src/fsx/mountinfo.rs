use crate::error::{GroveError, Result};
use crate::fsx::held::HeldDirectory;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MountEntry {
    pub id: u64,
    pub parent_id: u64,
    pub mountpoint: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MountTable {
    entries: Vec<MountEntry>,
}

impl MountTable {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let mut entries = Vec::new();
        for (number, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let fields = line.split(|byte| *byte == b' ').collect::<Vec<_>>();
            let separator = fields
                .iter()
                .position(|field| *field == b"-")
                .ok_or_else(|| malformed(number + 1))?;
            if separator < 6 || fields.len() < separator + 4 {
                return Err(malformed(number + 1));
            }
            let id = parse_u64(fields[0]).ok_or_else(|| malformed(number + 1))?;
            let parent_id = parse_u64(fields[1]).ok_or_else(|| malformed(number + 1))?;
            let mountpoint = PathBuf::from(OsString::from_vec(decode(fields[4])?));
            if !mountpoint.is_absolute() {
                return Err(malformed(number + 1));
            }
            entries.push(MountEntry {
                id,
                parent_id,
                mountpoint,
            });
        }
        if entries.is_empty() {
            return Err(GroveError::usage("/proc/self/mountinfo is empty"));
        }
        Ok(Self { entries })
    }

    pub fn read_live() -> Result<Self> {
        let bytes = std::fs::read("/proc/self/mountinfo").map_err(|error| {
            GroveError::usage(format!("cannot read /proc/self/mountinfo: {error}"))
        })?;
        Self::parse(&bytes)
    }

    pub fn longest_enclosing(&self, path: &Path) -> Option<&MountEntry> {
        self.entries
            .iter()
            .filter(|entry| path.starts_with(&entry.mountpoint))
            .max_by_key(|entry| entry.mountpoint.components().count())
    }

    pub fn at_or_below<'a>(&'a self, root: &'a Path) -> impl Iterator<Item = &'a MountEntry> {
        self.entries
            .iter()
            .filter(move |entry| entry.mountpoint.starts_with(root))
    }

    pub fn ensure_no_boundary_at_or_below(&self, root: &HeldDirectory) -> Result<()> {
        let kernel_path = root.kernel_path()?;
        if let Some(entry) = self.at_or_below(&kernel_path).next() {
            return Err(GroveError::needs_decision(format!(
                "mount boundary at {} prevents safe adoption",
                entry.mountpoint.display()
            )));
        }
        Ok(())
    }
}

fn parse_u64(bytes: &[u8]) -> Option<u64> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn malformed(line: usize) -> GroveError {
    GroveError::usage(format!("malformed /proc/self/mountinfo line {line}"))
}

fn decode(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            output.push(bytes[index]);
            index += 1;
            continue;
        }
        let escape = bytes
            .get(index + 1..index + 4)
            .ok_or_else(|| GroveError::usage("truncated mountinfo escape"))?;
        output.push(match escape {
            b"040" => b' ',
            b"011" => b'\t',
            b"012" => b'\n',
            b"134" => b'\\',
            _ => return Err(GroveError::usage("invalid mountinfo escape")),
        });
        index += 4;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn decodes_kernel_escapes_and_finds_the_longest_mount() {
        let table = MountTable::parse(
            b"1 0 8:1 / / rw - ext4 /dev/root rw\n2 1 8:1 /x /repo\\040space rw - ext4 /dev/root rw\n3 2 8:1 /x/y /repo\\040space/nested rw - ext4 /dev/root rw\n",
        )
        .unwrap();
        let path = Path::new("/repo space/nested/file");
        assert_eq!(table.longest_enclosing(path).unwrap().id, 3);
        assert_eq!(
            table.entries[1].mountpoint.as_os_str().as_bytes(),
            b"/repo space"
        );
    }

    #[test]
    fn component_boundaries_prevent_prefix_matches() {
        let table = MountTable::parse(
            b"1 0 8:1 / / rw - ext4 /dev/root rw\n2 1 8:1 / /repo-other rw - ext4 /dev/root rw\n",
        )
        .unwrap();
        assert_eq!(
            table.longest_enclosing(Path::new("/repo/file")).unwrap().id,
            1
        );
    }

    #[test]
    fn detects_nested_mounts_and_refuses_malformed_input() {
        let table = MountTable::parse(
            b"1 0 8:1 / / rw - ext4 /dev/root rw\n2 1 8:1 / /repo/nested rw - ext4 /dev/root rw\n",
        )
        .unwrap();
        assert_eq!(table.at_or_below(Path::new("/repo")).count(), 1);
        assert!(MountTable::parse(b"not mountinfo\n").is_err());
        assert!(MountTable::parse(b"1 0 8:1 / /bad\\999 rw - ext4 x rw\n").is_err());
    }

    #[test]
    fn live_mountinfo_parser_smoke() {
        let table = MountTable::read_live().unwrap();
        assert!(table.longest_enclosing(Path::new("/proc/self")).is_some());
    }
}
