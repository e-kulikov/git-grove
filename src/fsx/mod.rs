use crate::error::{GroveError, Result};
use rustix::fs::{linkat, openat, AtFlags, Mode, OFlags};
use std::ffi::OsString;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

const TEMP_NAME_ATTEMPTS: usize = 16;

fn io(path: &Path, action: &str, error: std::io::Error) -> GroveError {
    GroveError::failure(format!("cannot {action} {}: {error}", path.display()))
}

pub fn fsync_dir(path: &Path) -> Result<()> {
    let directory = File::open(path).map_err(|error| io(path, "open directory", error))?;
    directory
        .sync_all()
        .map_err(|error| io(path, "fsync directory", error))
}

pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        GroveError::failure(format!("{} has no parent directory", path.display()))
    })?;
    if path.file_name().is_none() {
        return Err(GroveError::failure(format!(
            "{} has no file name",
            path.display()
        )));
    }
    let directory = File::open(parent).map_err(|error| io(parent, "open directory", error))?;
    let temporary = openat(
        &directory,
        ".",
        OFlags::WRONLY | OFlags::TMPFILE,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|error| io(parent, "create temporary file", error.into()))?;
    let mut file = File::from(temporary);

    file.write_all(contents)
        .map_err(|error| io(path, "write temporary file", error))?;
    file.sync_all()
        .map_err(|error| io(path, "fsync temporary file", error))?;

    let temporary_name = link_temporary_file(&file, &directory, parent)?;
    let temporary_path = parent.join(&temporary_name);
    if let Err(error) = std::fs::rename(&temporary_path, path) {
        return Err(with_cleanup_error(
            io(path, "rename into place", error),
            cleanup_temporary(&file, &temporary_path, parent),
        ));
    }

    fsync_dir(parent)
}

pub fn symlink_relative(link: &Path, target: &str) -> Result<()> {
    match std::fs::read_link(link) {
        Ok(existing) if existing == Path::new(target) => return Ok(()),
        Ok(_) => {
            return Err(GroveError::failure(format!(
                "cannot create symlink at {}: path already exists",
                link.display()
            )));
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(io(link, "inspect existing path", error));
        }
        Err(_) => {}
    }

    symlink(target, link).map_err(|error| io(link, "create symlink at", error))?;
    let parent = link.parent().ok_or_else(|| {
        GroveError::failure(format!("{} has no parent directory", link.display()))
    })?;
    fsync_dir(parent)
}

fn link_temporary_file(file: &File, directory: &File, parent: &Path) -> Result<OsString> {
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let name = temporary_name()?;
        match linkat(file, "", directory, &name, AtFlags::EMPTY_PATH) {
            Ok(()) => return Ok(name),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io(parent, "link temporary file", error.into())),
        }
    }

    Err(GroveError::failure(format!(
        "cannot create a unique temporary file in {}",
        parent.display()
    )))
}

fn temporary_name() -> Result<OsString> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|error| {
        GroveError::failure(format!("cannot generate temporary filename: {error}"))
    })?;

    let mut name = String::from(".git-grove-tmp-");
    for byte in random {
        use std::fmt::Write as _;
        write!(&mut name, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(name.into())
}

fn cleanup_temporary(file: &File, temporary: &Path, parent: &Path) -> Result<()> {
    let expected = file
        .metadata()
        .map_err(|error| io(temporary, "inspect temporary file", error))?;
    let actual = match std::fs::symlink_metadata(temporary) {
        Ok(actual) => actual,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io(temporary, "inspect temporary path", error)),
    };
    if expected.dev() != actual.dev() || expected.ino() != actual.ino() {
        return Ok(());
    }

    match std::fs::remove_file(temporary) {
        Ok(()) => fsync_dir(parent),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io(temporary, "remove temporary file", error)),
    }
}

fn with_cleanup_error(original: GroveError, cleanup: Result<()>) -> GroveError {
    match cleanup {
        Ok(()) => original,
        Err(cleanup) => GroveError::failure(format!("{original}; additionally {cleanup}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_file_atomically_and_leaves_no_temporaries() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("pointer");

        write_atomic(&target, b"gitdir: ./.bare\n").unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"gitdir: ./.bare\n");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != "pointer")
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn overwrites_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("f");

        write_atomic(&target, b"one").unwrap();
        write_atomic(&target, b"two").unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"two");
    }

    #[test]
    fn failed_replacement_preserves_destination_and_removes_its_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("destination");
        std::fs::create_dir(&target).unwrap();

        assert!(write_atomic(&target, b"replacement").is_err());

        assert!(target.is_dir());
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != "destination")
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn creates_a_relative_symlink() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), b"x").unwrap();

        symlink_relative(&dir.path().join("CLAUDE.md"), "AGENTS.md").unwrap();

        assert_eq!(
            std::fs::read_link(dir.path().join("CLAUDE.md"))
                .unwrap()
                .to_str()
                .unwrap(),
            "AGENTS.md"
        );
    }

    #[test]
    fn refuses_to_replace_an_existing_broken_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("CLAUDE.md");
        std::os::unix::fs::symlink("missing", &link).unwrap();

        assert!(symlink_relative(&link, "AGENTS.md").is_err());

        assert_eq!(
            std::fs::read_link(link).unwrap().to_str().unwrap(),
            "missing"
        );
    }
}
