use crate::error::{GroveError, Result};
use crate::fsx::held::{
    FileIdentity, FileSystem, HeldDirectory, RealFileSystem, ValidatedRelativePath,
};
use crate::transaction::journal::{
    sha256, BlobProof, ManifestContent, ManifestEntry, NamedBlobProof, RawBytes, ValidatedBytePath,
};
use rustix::fs::FileType;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub struct Inventory {
    pub payload: Vec<ManifestEntry>,
    pub git_entries: Vec<(PathBuf, FileIdentity)>,
}

pub fn collect(root: &HeldDirectory, git: &HeldDirectory) -> Result<Inventory> {
    let mut payload = Vec::new();
    for entry in root.inventory()? {
        let Some(first) = entry.path.as_path().components().next() else {
            continue;
        };
        let first = first.as_os_str().as_bytes();
        if first == b".git" || first == b".bare" || first.starts_with(b".grove-adopt-") {
            continue;
        }
        if entry.identity.mount_id != root.original_identity().mount_id {
            return Err(GroveError::needs_decision(format!(
                "{} crosses a mount boundary",
                entry.path.as_path().display()
            )));
        }
        let kind = FileType::from_raw_mode(entry.identity.mode);
        if kind.is_file() && entry.identity.nlink > 1 {
            return Err(GroveError::needs_decision(format!(
                "{} is hard-linked and cannot be adopted safely",
                entry.path.as_path().display()
            )));
        }
        let absolute = root.anchored_path.join(entry.path.as_path());
        let content = if kind.is_file() {
            let bytes = std::fs::read(&absolute).map_err(|error| {
                GroveError::failure(format!("cannot read {}: {error}", absolute.display()))
            })?;
            ManifestContent::Blob {
                sha256: sha256(&bytes),
                bytes: RawBytes::from_bytes(&bytes),
            }
        } else if kind.is_symlink() {
            let target = std::fs::read_link(&absolute).map_err(|error| {
                GroveError::failure(format!("cannot read {}: {error}", absolute.display()))
            })?;
            let bytes = target.as_os_str().as_bytes();
            ManifestContent::Symlink {
                target: RawBytes::from_bytes(bytes),
                sha256: sha256(bytes),
            }
        } else {
            ManifestContent::None
        };
        payload.push(ManifestEntry {
            path: ValidatedBytePath::new(entry.path.as_path())?,
            identity: entry.identity,
            content,
        });
    }
    let git_entries = git
        .inventory()?
        .into_iter()
        .map(|entry| (entry.path.as_path().to_path_buf(), entry.identity))
        .collect();
    Ok(Inventory {
        payload,
        git_entries,
    })
}

pub fn optional_blob(git: &HeldDirectory, relative: &Path) -> Result<Option<BlobProof>> {
    match std::fs::symlink_metadata(git.anchored_path.join(relative)) {
        Ok(metadata) if metadata.file_type().is_file() => blob(git, relative).map(Some),
        Ok(_) => Err(GroveError::needs_decision(format!(
            "{} is not a regular file",
            relative.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(GroveError::failure(format!(
            "cannot inspect {}: {error}",
            relative.display()
        ))),
    }
}

pub fn blob(git: &HeldDirectory, relative: &Path) -> Result<BlobProof> {
    let relative = ValidatedRelativePath::new(relative)?;
    let identity = RealFileSystem.identity_at(git, &relative)?;
    if !FileType::from_raw_mode(identity.mode).is_file() {
        return Err(GroveError::needs_decision(format!(
            "{} is not a regular file",
            relative.as_path().display()
        )));
    }
    let bytes = std::fs::read(git.anchored_path.join(relative.as_path())).map_err(|error| {
        GroveError::failure(format!(
            "cannot read {}: {error}",
            relative.as_path().display()
        ))
    })?;
    Ok(BlobProof {
        bytes: RawBytes::from_bytes(&bytes),
        sha256: sha256(&bytes),
        mode: identity.mode,
        identity,
    })
}

pub fn named_blob(git: &HeldDirectory, relative: &Path) -> Result<NamedBlobProof> {
    Ok(NamedBlobProof {
        path: ValidatedBytePath::new(relative)?,
        blob: blob(git, relative)?,
    })
}

pub fn first_component(path: &Path) -> Option<&OsStr> {
    path.components()
        .next()
        .map(|component| component.as_os_str())
}
