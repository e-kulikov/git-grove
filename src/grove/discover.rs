use crate::error::{GroveError, Result};
use crate::grove::layout;
use rustix::fs::{openat, Mode, OFlags, CWD};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Grove {
    pub root: PathBuf,
}

impl Grove {
    pub fn bare_dir(&self) -> PathBuf {
        self.root.join(".bare")
    }

    pub fn at(path: &Path) -> Result<Grove> {
        let root = path.canonicalize().map_err(|error| {
            GroveError::usage(format!("cannot resolve {}: {error}", path.display()))
        })?;
        validate_signature(&root)?;
        Ok(Grove { root })
    }

    pub fn discover(start: &Path) -> Result<Grove> {
        let mut current = start.canonicalize().map_err(|error| {
            GroveError::usage(format!("cannot resolve {}: {error}", start.display()))
        })?;
        loop {
            if validate_signature(&current).is_ok() {
                return Ok(Grove { root: current });
            }
            if !current.pop() {
                return Err(GroveError::usage("not inside a grove")
                    .with_detail("run `git grove clone <url>` or `git grove init` first"));
            }
        }
    }
}

fn validate_signature(root: &Path) -> Result<()> {
    let not_a_grove = || GroveError::usage(format!("{} is not a grove", root.display()));
    let directory = openat(
        CWD,
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| not_a_grove())?;
    openat(
        &directory,
        ".bare",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| not_a_grove())?;

    let mut pointer = openat(
        &directory,
        ".git",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| not_a_grove())?;
    if !pointer.metadata().map_err(|_| not_a_grove())?.is_file() {
        return Err(not_a_grove());
    }
    let mut contents = Vec::new();
    pointer.read_to_end(&mut contents).map_err(|error| {
        GroveError::failure(format!("cannot read {}/.git: {error}", root.display()))
    })?;
    if contents != layout::POINTER_CONTENTS.as_bytes() {
        return Err(GroveError::usage(format!(
            "{}/.git does not contain the exact grove pointer",
            root.display()
        ))
        .with_detail("resolve the conflicting pointer file by hand"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_grove(dir: &Path) {
        std::fs::create_dir_all(dir.join(".bare")).unwrap();
        std::fs::write(dir.join(".git"), crate::grove::layout::POINTER_CONTENTS).unwrap();
    }

    #[test]
    fn recognises_a_grove_root() {
        let dir = tempfile::tempdir().unwrap();
        make_grove(dir.path());
        let grove = Grove::at(dir.path()).unwrap();
        assert_eq!(grove.root, dir.path().canonicalize().unwrap());
        assert_eq!(grove.bare_dir(), dir.path().join(".bare"));
    }

    #[test]
    fn walks_ancestors_and_skips_false_grove_candidates() {
        let dir = tempfile::tempdir().unwrap();
        make_grove(dir.path());
        let nested = dir.path().join("main/src");
        std::fs::create_dir_all(nested.join(".bare")).unwrap();
        std::fs::write(nested.join(".git"), b"gitdir: elsewhere\n").unwrap();

        let grove = Grove::discover(&nested).unwrap();

        assert_eq!(grove.root, dir.path().canonicalize().unwrap());
    }

    #[test]
    fn refuses_a_plain_directory() {
        let dir = tempfile::tempdir().unwrap();
        let err = Grove::discover(dir.path()).unwrap_err();
        assert_eq!(err.class, crate::error::ExitClass::Usage);
        assert!(err.message.contains("grove"));
    }

    #[test]
    fn requires_real_entries_and_byte_exact_pointer_contents() {
        for contents in [
            b"gitdir: ./.bare".as_slice(),
            b"gitdir: ./.bare\r\n".as_slice(),
            b"gitdir: ./.bare\n\n".as_slice(),
            b" gitdir: ./.bare\n".as_slice(),
            b"gitdir: /somewhere/else\n".as_slice(),
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir(dir.path().join(".bare")).unwrap();
            std::fs::write(dir.path().join(".git"), contents).unwrap();
            assert!(Grove::at(dir.path()).is_err(), "accepted {contents:?}");
        }

        let bare_link = tempfile::tempdir().unwrap();
        std::fs::create_dir(bare_link.path().join("actual-bare")).unwrap();
        std::os::unix::fs::symlink("actual-bare", bare_link.path().join(".bare")).unwrap();
        std::fs::write(
            bare_link.path().join(".git"),
            crate::grove::layout::POINTER_CONTENTS,
        )
        .unwrap();
        assert!(Grove::at(bare_link.path()).is_err());

        let pointer_link = tempfile::tempdir().unwrap();
        std::fs::create_dir(pointer_link.path().join(".bare")).unwrap();
        std::fs::write(
            pointer_link.path().join("actual-pointer"),
            crate::grove::layout::POINTER_CONTENTS,
        )
        .unwrap();
        std::os::unix::fs::symlink("actual-pointer", pointer_link.path().join(".git")).unwrap();
        assert!(Grove::at(pointer_link.path()).is_err());
    }
}
