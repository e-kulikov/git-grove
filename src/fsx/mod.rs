use crate::error::{GroveError, Result};
use rustix::fs::{
    linkat, openat, readlinkat, renameat, renameat_with, symlinkat, AtFlags, Mode, OFlags,
    RenameFlags,
};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Write;
use std::path::Path;

const TEMP_NAME_ATTEMPTS: usize = 16;

#[allow(dead_code)]
#[derive(Clone, Copy)]
enum TempStrategy {
    Auto,
    NamedOnly,
}

struct Temporary {
    file: File,
    name: Option<OsString>,
}

fn io(path: &Path, action: &str, error: impl Into<std::io::Error>) -> GroveError {
    let error: std::io::Error = error.into();
    GroveError::failure(format!("cannot {action} {}: {error}", path.display()))
}

pub fn fsync_dir(path: &Path) -> Result<()> {
    let directory = File::open(path).map_err(|error| io(path, "open directory", error))?;
    fsync_directory(&directory, path)
}

pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    write_atomic_with_strategy(path, contents, TempStrategy::Auto)
}

/// Atomically create `path` when it has no directory entry.
///
/// Returns `Ok(false)` when an entry was already present before the creation
/// attempt. If an entry appears while creating the temporary file, the final
/// no-replace operation preserves it and returns an error.
pub fn write_atomic_if_absent(path: &Path, contents: &[u8]) -> Result<bool> {
    write_atomic_if_absent_with_strategy(path, contents, TempStrategy::Auto, || {})
}

fn write_atomic_if_absent_with_strategy<F>(
    path: &Path,
    contents: &[u8],
    strategy: TempStrategy,
    before_install: F,
) -> Result<bool>
where
    F: FnOnce(),
{
    match std::fs::symlink_metadata(path) {
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io(path, "inspect existing path", error)),
    }

    let (parent, destination) = split_path(path)?;
    let directory = File::open(parent).map_err(|error| io(parent, "open directory", error))?;
    let mut temporary = create_temporary(&directory, parent, strategy)?;

    if let Err(error) = temporary.file.write_all(contents) {
        return Err(retain_temporary(
            io(path, "write temporary file", error),
            &temporary,
            &directory,
            parent,
        ));
    }
    if let Err(error) = temporary.file.sync_all() {
        return Err(retain_temporary(
            io(path, "fsync temporary file", error),
            &temporary,
            &directory,
            parent,
        ));
    }

    before_install();
    match &temporary.name {
        None => {
            if let Err(error) = linkat(
                &temporary.file,
                "",
                &directory,
                destination,
                AtFlags::EMPTY_PATH,
            ) {
                return Err(io(path, "create without replacing an existing path", error));
            }
        }
        Some(name) => {
            if let Err(error) = renameat_with(
                &directory,
                name,
                &directory,
                destination,
                RenameFlags::NOREPLACE,
            ) {
                return Err(retain_temporary(
                    io(path, "rename into place without replacement", error),
                    &temporary,
                    &directory,
                    parent,
                ));
            }
        }
    }

    fsync_directory(&directory, parent)?;
    Ok(true)
}

fn write_atomic_with_strategy(path: &Path, contents: &[u8], strategy: TempStrategy) -> Result<()> {
    let (parent, destination) = split_path(path)?;
    let directory = File::open(parent).map_err(|error| io(parent, "open directory", error))?;
    let mut temporary = create_temporary(&directory, parent, strategy)?;

    if let Err(error) = temporary.file.write_all(contents) {
        return Err(retain_temporary(
            io(path, "write temporary file", error),
            &temporary,
            &directory,
            parent,
        ));
    }
    if let Err(error) = temporary.file.sync_all() {
        return Err(retain_temporary(
            io(path, "fsync temporary file", error),
            &temporary,
            &directory,
            parent,
        ));
    }

    let temporary_name = match &temporary.name {
        Some(name) => name.clone(),
        None => link_temporary_file(&temporary.file, &directory, parent)?,
    };
    if let Err(error) = renameat(&directory, &temporary_name, &directory, destination) {
        temporary.name = Some(temporary_name);
        return Err(retain_temporary(
            io(path, "rename into place", error),
            &temporary,
            &directory,
            parent,
        ));
    }

    fsync_directory(&directory, parent)
}

pub fn symlink_relative(link: &Path, target: &str) -> Result<()> {
    let (parent, name) = split_path(link)?;
    let directory = File::open(parent).map_err(|error| io(parent, "open directory", error))?;

    match readlinkat(&directory, name, Vec::new()) {
        Ok(existing) if existing.as_bytes() == target.as_bytes() => return Ok(()),
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

    symlinkat(target, &directory, name).map_err(|error| io(link, "create symlink at", error))?;
    fsync_directory(&directory, parent)
}

fn split_path(path: &Path) -> Result<(&Path, &OsStr)> {
    let name = path
        .file_name()
        .ok_or_else(|| GroveError::failure(format!("{} has no file name", path.display())))?;
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    Ok((parent, name))
}

fn fsync_directory(directory: &File, path: &Path) -> Result<()> {
    directory
        .sync_all()
        .map_err(|error| io(path, "fsync directory", error))
}

fn create_temporary(directory: &File, parent: &Path, strategy: TempStrategy) -> Result<Temporary> {
    if matches!(strategy, TempStrategy::Auto) {
        match openat(
            directory,
            ".",
            OFlags::WRONLY | OFlags::TMPFILE,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(file) => {
                return Ok(Temporary {
                    file: File::from(file),
                    name: None,
                });
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Unsupported | std::io::ErrorKind::InvalidInput
                ) => {}
            Err(error) => return Err(io(parent, "create temporary file", error)),
        }
    }

    create_named_temporary(directory, parent)
}

fn create_named_temporary(directory: &File, parent: &Path) -> Result<Temporary> {
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let name = temporary_name()?;
        match openat(
            directory,
            &name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(file) => {
                return Ok(Temporary {
                    file: File::from(file),
                    name: Some(name),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io(parent, "create temporary file", error)),
        }
    }

    Err(GroveError::failure(format!(
        "cannot create a unique temporary file in {}",
        parent.display()
    )))
}

fn link_temporary_file(file: &File, directory: &File, parent: &Path) -> Result<OsString> {
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let name = temporary_name()?;
        match linkat(file, "", directory, &name, AtFlags::EMPTY_PATH) {
            Ok(()) => return Ok(name),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io(parent, "link temporary file", error)),
        }
    }

    Err(GroveError::failure(format!(
        "cannot create a unique temporary file in {}",
        parent.display()
    )))
}

fn temporary_name() -> Result<OsString> {
    let mut random = [0_u8; 16];
    getrandom::getrandom(&mut random).map_err(|error| {
        GroveError::failure(format!("cannot generate temporary filename: {error}"))
    })?;

    let mut name = String::from(".git-grove-tmp-");
    for byte in random {
        use std::fmt::Write as _;
        write!(&mut name, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(name.into())
}

fn retain_temporary(
    original: GroveError,
    temporary: &Temporary,
    directory: &File,
    parent: &Path,
) -> GroveError {
    let Some(name) = &temporary.name else {
        return original;
    };
    let retained = parent.join(name);
    match fsync_directory(directory, parent) {
        Ok(()) => GroveError::failure(format!(
            "{original}; retained temporary file {}",
            retained.display()
        )),
        Err(sync_error) => GroveError::failure(format!(
            "{original}; retained temporary file {}; additionally {sync_error}",
            retained.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static CURRENT_DIRECTORY: Mutex<()> = Mutex::new(());

    struct CurrentDirectory {
        previous: PathBuf,
    }

    impl CurrentDirectory {
        fn enter(path: &Path) -> Self {
            let previous = std::env::current_dir().unwrap();
            std::env::set_current_dir(path).unwrap();
            Self { previous }
        }
    }

    impl Drop for CurrentDirectory {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.previous).unwrap();
        }
    }

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
    fn creates_a_file_only_when_the_destination_remains_absent() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("AGENTS.md");

        assert!(write_atomic_if_absent(&target, b"ours").unwrap());
        assert!(!write_atomic_if_absent(&target, b"replacement").unwrap());

        assert_eq!(std::fs::read(&target).unwrap(), b"ours");
    }

    #[test]
    fn no_replace_creation_preserves_an_entry_created_after_its_probe() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("AGENTS.md");

        let error =
            write_atomic_if_absent_with_strategy(&target, b"ours", TempStrategy::NamedOnly, || {
                std::fs::write(&target, b"foreign").unwrap()
            })
            .unwrap_err();

        assert_eq!(std::fs::read(&target).unwrap(), b"foreign");
        assert!(error
            .message
            .contains("rename into place without replacement"));
    }

    #[test]
    fn no_replace_creation_preserves_a_broken_link_created_after_its_probe() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("AGENTS.md");

        write_atomic_if_absent_with_strategy(&target, b"ours", TempStrategy::NamedOnly, || {
            std::os::unix::fs::symlink("foreign", &target).unwrap()
        })
        .unwrap_err();

        assert_eq!(std::fs::read_link(&target).unwrap(), Path::new("foreign"));
    }

    #[test]
    fn writes_with_the_named_fallback_when_unnamed_temps_are_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("pointer");

        write_atomic_with_strategy(&target, b"gitdir: ./.bare\n", TempStrategy::NamedOnly).unwrap();

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
    fn writes_to_a_bare_relative_destination() {
        let dir = tempfile::tempdir().unwrap();
        let _lock = CURRENT_DIRECTORY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _cwd = CurrentDirectory::enter(dir.path());

        write_atomic(Path::new("pointer"), b"gitdir: ./.bare\n").unwrap();

        assert_eq!(std::fs::read("pointer").unwrap(), b"gitdir: ./.bare\n");
    }

    #[test]
    fn failed_replacement_preserves_destination_and_retains_identifiable_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("destination");
        std::fs::create_dir(&target).unwrap();

        let error = write_atomic(&target, b"replacement").unwrap_err();

        assert!(target.is_dir());
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != "destination")
            .collect();
        assert_eq!(leftovers.len(), 1, "unexpected entries: {leftovers:?}");
        assert!(leftovers[0].starts_with(".git-grove-tmp-"));
        assert!(error.message.contains(&leftovers[0]));
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
    fn creates_a_bare_relative_symlink() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), b"x").unwrap();
        let _lock = CURRENT_DIRECTORY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _cwd = CurrentDirectory::enter(dir.path());

        symlink_relative(Path::new("CLAUDE.md"), "AGENTS.md").unwrap();

        assert_eq!(
            std::fs::read_link("CLAUDE.md").unwrap(),
            Path::new("AGENTS.md")
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
