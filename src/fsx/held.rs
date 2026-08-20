use crate::error::{GroveError, Result};
use crate::fsx::mountinfo::MountTable;
use rustix::fs::{
    fstat, openat, renameat_with, statat, statx, unlinkat, AtFlags, FileType, Mode, OFlags,
    RenameFlags, StatxFlags, CWD,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Timespec {
    pub seconds: i64,
    pub nanoseconds: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub nlink: u64,
    pub size: u64,
    pub mtime: Timespec,
    pub ctime: Timespec,
    pub mount_id: u64,
    pub sha256: Option<[u8; 32]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InventoryEntry {
    pub path: ValidatedRelativePath,
    pub identity: FileIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ValidatedRelativePath(PathBuf);

impl ValidatedRelativePath {
    pub fn new(path: &Path) -> Result<Self> {
        if path.is_absolute() {
            return Err(GroveError::failure("internal path is absolute"));
        }
        let mut validated = PathBuf::new();
        for component in path.components() {
            let Component::Normal(component) = component else {
                return Err(GroveError::failure(
                    "internal path contains an empty, dot, or parent component",
                ));
            };
            validate_component(component)?;
            validated.push(component);
        }
        if validated.as_os_str().is_empty() {
            return Err(GroveError::failure("internal path is empty"));
        }
        Ok(Self(validated))
    }

    pub fn component(component: &OsStr) -> Result<Self> {
        validate_component(component)?;
        Ok(Self(PathBuf::from(component)))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

fn validate_component(component: &OsStr) -> Result<()> {
    let bytes = component.as_bytes();
    if bytes.is_empty()
        || bytes == b"."
        || bytes == b".."
        || bytes.contains(&b'/')
        || bytes.contains(&0)
    {
        return Err(GroveError::failure(
            "internal path contains an invalid component",
        ));
    }
    Ok(())
}

#[derive(Debug)]
pub struct HeldDirectory {
    pub(crate) file: File,
    pub(crate) named_path: PathBuf,
    pub(crate) anchored_path: PathBuf,
    identity: FileIdentity,
}

impl HeldDirectory {
    pub fn open(path: &Path) -> Result<Self> {
        let file = openat(
            CWD,
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(|error| {
            GroveError::needs_decision(format!(
                "cannot open {} without following symlinks: {error}",
                escaped(path)
            ))
        })?;
        Self::new(file, path.to_path_buf())
    }

    pub(crate) fn new(file: File, named_path: PathBuf) -> Result<Self> {
        let anchored_path = PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            file.as_raw_fd()
        ));
        let identity = identity_for_fd(&file, None)?;
        let held = Self {
            file,
            named_path,
            anchored_path,
            identity,
        };
        held.validate()?;
        Ok(held)
    }

    pub fn path(&self) -> &Path {
        &self.named_path
    }

    pub fn kernel_path(&self) -> Result<PathBuf> {
        std::fs::read_link(&self.anchored_path).map_err(|error| {
            GroveError::failure(format!(
                "cannot resolve held directory {}: {error}",
                escaped(&self.named_path)
            ))
        })
    }

    pub fn identity(&self) -> Result<FileIdentity> {
        identity_for_fd(&self.file, None)
    }

    pub fn original_identity(&self) -> FileIdentity {
        self.identity
    }

    pub fn inventory(&self) -> Result<Vec<InventoryEntry>> {
        let mut entries = Vec::new();
        inventory_directory(self, &self.file, Path::new(""), &mut entries)?;
        Ok(entries)
    }

    pub fn validate(&self) -> Result<()> {
        let held = identity_for_fd(&self.file, None)?;
        let named = statat(CWD, &self.named_path, AtFlags::SYMLINK_NOFOLLOW).map_err(|error| {
            GroveError::needs_decision(format!(
                "{} changed while its directory was held: {error}",
                escaped(&self.named_path)
            ))
        })?;
        if held.dev != named.st_dev as u64 || held.ino != named.st_ino as u64 {
            return Err(GroveError::needs_decision(format!(
                "{} changed while its directory was held",
                escaped(&self.named_path)
            ))
            .with_detail("the replacement was preserved"));
        }
        Ok(())
    }

    pub(crate) fn ensure_empty(&self) -> Result<()> {
        self.validate()?;
        let mut entries = std::fs::read_dir(&self.anchored_path).map_err(|error| {
            GroveError::failure(format!(
                "cannot read {}: {error}",
                escaped(&self.named_path)
            ))
        })?;
        if entries.next().is_some() {
            return Err(GroveError::needs_decision(format!(
                "{} is not empty",
                escaped(&self.named_path)
            ))
            .with_detail("use `git grove adopt` to convert an existing repository"));
        }
        self.validate()
    }

    pub(crate) fn ensure_only_entry(&self, expected: &OsStr) -> Result<()> {
        self.validate()?;
        let entries = std::fs::read_dir(&self.anchored_path).map_err(|error| {
            GroveError::failure(format!(
                "cannot read {}: {error}",
                escaped(&self.named_path)
            ))
        })?;
        let mut names = entries
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|error| {
                GroveError::failure(format!(
                    "cannot read {}: {error}",
                    escaped(&self.named_path)
                ))
            })?;
        if names.len() != 1 || names.pop().as_deref() != Some(expected) {
            return Err(GroveError::needs_decision(format!(
                "{} changed while its directory was held",
                escaped(&self.named_path)
            ))
            .with_detail("the concurrently created entries were preserved"));
        }
        self.validate()
    }
}

pub(crate) fn open_directory_at(parent: &File, name: &OsStr) -> std::io::Result<File> {
    openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(Into::into)
}

pub struct Location<'a> {
    pub directory: &'a HeldDirectory,
    pub path: &'a ValidatedRelativePath,
}

pub trait FileSystem {
    fn identity_at(
        &self,
        dir: &HeldDirectory,
        name: &ValidatedRelativePath,
    ) -> Result<FileIdentity>;
    fn rename_at(&self, from: &Location<'_>, to: &Location<'_>) -> Result<()>;
    fn write_new_at(&self, at: &Location<'_>, bytes: &[u8], mode: u32) -> Result<()>;
    fn remove_at(&self, at: &Location<'_>) -> Result<()>;
    fn fsync_file_at(&self, at: &Location<'_>) -> Result<()>;
    fn fsync_dir(&self, dir: &HeldDirectory) -> Result<()>;
}

#[derive(Default)]
pub struct RealFileSystem;

impl FileSystem for RealFileSystem {
    fn identity_at(
        &self,
        dir: &HeldDirectory,
        name: &ValidatedRelativePath,
    ) -> Result<FileIdentity> {
        identity_at(dir, name)
    }

    fn rename_at(&self, from: &Location<'_>, to: &Location<'_>) -> Result<()> {
        let (from_parent, from_name) = open_parent(from.directory, from.path)?;
        let (to_parent, to_name) = open_parent(to.directory, to.path)?;
        renameat_with(
            &from_parent,
            from_name,
            &to_parent,
            to_name,
            RenameFlags::NOREPLACE,
        )
        .map_err(|error| GroveError::failure(format!("cannot rename transaction entry: {error}")))
    }

    fn write_new_at(&self, at: &Location<'_>, bytes: &[u8], mode: u32) -> Result<()> {
        let (parent, name) = open_parent(at.directory, at.path)?;
        let mut file = openat(
            &parent,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(mode),
        )
        .map(File::from)
        .map_err(|error| GroveError::failure(format!("cannot create transaction file: {error}")))?;
        file.write_all(bytes)
            .map_err(|error| GroveError::failure(format!("cannot write transaction file: {error}")))
    }

    fn remove_at(&self, at: &Location<'_>) -> Result<()> {
        let identity = self.identity_at(at.directory, at.path)?;
        let flags = if FileType::from_raw_mode(identity.mode).is_dir() {
            AtFlags::REMOVEDIR
        } else {
            AtFlags::empty()
        };
        let (parent, name) = open_parent(at.directory, at.path)?;
        unlinkat(&parent, name, flags).map_err(|error| {
            GroveError::failure(format!("cannot remove transaction entry: {error}"))
        })
    }

    fn fsync_file_at(&self, at: &Location<'_>) -> Result<()> {
        let (parent, name) = open_parent(at.directory, at.path)?;
        let file = openat(
            &parent,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(|error| GroveError::failure(format!("cannot open transaction file: {error}")))?;
        file.sync_all()
            .map_err(|error| GroveError::failure(format!("cannot fsync transaction file: {error}")))
    }

    fn fsync_dir(&self, dir: &HeldDirectory) -> Result<()> {
        dir.file
            .sync_all()
            .map_err(|error| GroveError::failure(format!("cannot fsync directory: {error}")))
    }
}

fn open_parent<'a>(
    directory: &'a HeldDirectory,
    path: &'a ValidatedRelativePath,
) -> Result<(File, &'a OsStr)> {
    let mut current = directory.file.try_clone().map_err(|error| {
        GroveError::failure(format!("cannot duplicate held directory: {error}"))
    })?;
    let mut components = path.as_path().iter().peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            return Ok((current, component));
        }
        current = open_directory_at(&current, component).map_err(|error| {
            GroveError::needs_decision(format!(
                "transaction path has a replaced or symlinked ancestor: {error}"
            ))
        })?;
    }
    unreachable!("validated paths are non-empty")
}

fn identity_at(dir: &HeldDirectory, path: &ValidatedRelativePath) -> Result<FileIdentity> {
    let (parent, name) = open_parent(dir, path)?;
    let stat = statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(|error| {
        GroveError::failure(format!("cannot inspect transaction path: {error}"))
    })?;
    let mount_id = mount_id_at(&parent, name)?;
    let sha256 = if FileType::from_raw_mode(stat.st_mode).is_file() {
        let mut file = openat(
            &parent,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(|error| GroveError::failure(format!("cannot open file for hashing: {error}")))?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|error| GroveError::failure(format!("cannot hash file: {error}")))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Some(hasher.finalize().into())
    } else {
        None
    };
    Ok(identity_from_stat(&stat, mount_id, sha256))
}

fn identity_for_fd(file: &File, sha256: Option<[u8; 32]>) -> Result<FileIdentity> {
    let stat = fstat(file)
        .map_err(|error| GroveError::failure(format!("cannot inspect held directory: {error}")))?;
    let mount_id = mount_id_at(file, OsStr::new(""))?;
    Ok(identity_from_stat(&stat, mount_id, sha256))
}

fn mount_id_at(dir: &File, name: &OsStr) -> Result<u64> {
    let flags = if name.is_empty() {
        AtFlags::EMPTY_PATH
    } else {
        AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT
    };
    match statx(
        dir,
        name,
        flags,
        StatxFlags::BASIC_STATS | StatxFlags::MNT_ID,
    ) {
        Ok(stat) if StatxFlags::from_bits_retain(stat.stx_mask).contains(StatxFlags::MNT_ID) => {
            Ok(stat.stx_mnt_id)
        }
        Ok(_) | Err(_) => {
            let directory = std::fs::read_link(proc_fd_path(dir)).map_err(|error| {
                GroveError::usage(format!(
                    "cannot resolve a held path for mount classification: {error}"
                ))
            })?;
            let path = if name.is_empty() {
                directory
            } else {
                directory.join(name)
            };
            MountTable::read_live()?
                .longest_enclosing(&path)
                .map(|entry| entry.id)
                .ok_or_else(|| {
                    GroveError::usage(format!(
                        "cannot classify the mount containing {}",
                        escaped(&path)
                    ))
                })
        }
    }
}

fn proc_fd_path(file: &File) -> PathBuf {
    PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        file.as_raw_fd()
    ))
}

fn inventory_directory(
    root: &HeldDirectory,
    directory: &File,
    prefix: &Path,
    output: &mut Vec<InventoryEntry>,
) -> Result<()> {
    let mut names = std::fs::read_dir(proc_fd_path(directory))
        .map_err(|error| GroveError::failure(format!("cannot inventory directory: {error}")))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| GroveError::failure(format!("cannot inventory directory: {error}")))?;
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    for name in names {
        validate_component(&name)?;
        let path = prefix.join(&name);
        let path = ValidatedRelativePath::new(&path)?;
        let identity = identity_at(root, &path)?;
        output.push(InventoryEntry {
            path: path.clone(),
            identity,
        });
        if FileType::from_raw_mode(identity.mode).is_dir() {
            let child = open_directory_at(directory, &name).map_err(|error| {
                GroveError::needs_decision(format!(
                    "directory changed during recursive inventory: {error}"
                ))
            })?;
            inventory_directory(root, &child, path.as_path(), output)?;
        }
    }
    Ok(())
}

fn identity_from_stat(
    stat: &rustix::fs::Stat,
    mount_id: u64,
    sha256: Option<[u8; 32]>,
) -> FileIdentity {
    FileIdentity {
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
        mode: stat.st_mode as u32,
        nlink: stat.st_nlink as u64,
        size: stat.st_size.max(0) as u64,
        mtime: Timespec {
            seconds: stat.st_mtime as i64,
            nanoseconds: stat.st_mtime_nsec as u32,
        },
        ctime: Timespec {
            seconds: stat.st_ctime as i64,
            nanoseconds: stat.st_ctime_nsec as u32,
        },
        mount_id,
        sha256,
    }
}

fn escaped(path: &Path) -> String {
    use bstr::ByteSlice;
    path.as_os_str().as_bytes().escape_bytes().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn held_directory_keeps_its_identity_across_a_rename() {
        let parent = tempfile::tempdir().unwrap();
        let original = parent.path().join("original");
        let renamed = parent.path().join("renamed");
        std::fs::create_dir(&original).unwrap();
        let held = HeldDirectory::open(&original).unwrap();
        let before = held.identity().unwrap();
        std::fs::rename(&original, &renamed).unwrap();
        let after = held.identity().unwrap();
        assert_eq!(
            (before.dev, before.ino, before.mode),
            (after.dev, after.ino, after.mode)
        );
        assert_eq!(after.ino, std::fs::metadata(renamed).unwrap().ino());
    }

    #[test]
    fn validated_paths_reject_dot_parent_slash_nul_and_empty_components() {
        for path in [
            Path::new(""),
            Path::new("."),
            Path::new(".."),
            Path::new("a/../b"),
            Path::new("/a"),
        ] {
            assert!(
                ValidatedRelativePath::new(path).is_err(),
                "accepted {path:?}"
            );
        }
        assert!(ValidatedRelativePath::component(OsStr::from_bytes(b"a/b")).is_err());
        assert!(ValidatedRelativePath::component(OsStr::from_bytes(b"a\0b")).is_err());
    }

    #[test]
    fn identity_records_metadata_and_regular_file_hash() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), b"contents").unwrap();
        let held = HeldDirectory::open(root.path()).unwrap();
        let path = ValidatedRelativePath::component(OsStr::new("file")).unwrap();
        let identity = RealFileSystem.identity_at(&held, &path).unwrap();
        assert_eq!(identity.size, 8);
        assert_eq!(identity.nlink, 1);
        assert!(identity.mtime.seconds > 0);
        assert!(identity.ctime.seconds > 0);
        assert!(identity.sha256.is_some());
    }

    #[test]
    fn recursive_inventory_records_nested_types_metadata_and_hashes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("nested")).unwrap();
        std::fs::write(root.path().join("nested/file"), b"contents").unwrap();
        std::os::unix::fs::symlink("file", root.path().join("nested/link")).unwrap();
        let held = HeldDirectory::open(root.path()).unwrap();
        let inventory = held.inventory().unwrap();
        assert_eq!(
            inventory
                .iter()
                .map(|entry| entry.path.as_path().to_path_buf())
                .collect::<Vec<_>>(),
            [
                PathBuf::from("nested"),
                PathBuf::from("nested/file"),
                PathBuf::from("nested/link")
            ]
        );
        let file = &inventory[1].identity;
        assert_eq!(file.size, 8);
        assert_eq!(file.nlink, 1);
        assert!(file.sha256.is_some());
        assert!(inventory[2].identity.sha256.is_none());
    }
}
