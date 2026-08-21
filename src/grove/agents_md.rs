use std::io::ErrorKind;
use std::path::Path;

use bstr::{BString, ByteSlice};

use crate::error::{GroveError, Result};
use crate::fsx;
use crate::grove::discover::Grove;

#[derive(Debug, Clone)]
pub struct Facts {
    pub remote: Option<BString>,
    pub default_branch: BString,
    pub published: bool,
    pub narrowed: bool,
}

fn display(bytes: &BString) -> String {
    bytes.as_slice().escape_bytes().to_string()
}

fn is_scp_style_url(bytes: &[u8]) -> bool {
    let Some(at) = bytes.iter().position(|byte| *byte == b'@') else {
        return false;
    };
    let (user, host_and_path) = bytes.split_at(at);
    let host_and_path = &host_and_path[1..];
    let Some(colon) = host_and_path.iter().position(|byte| *byte == b':') else {
        return false;
    };
    let (host, path) = host_and_path.split_at(colon);
    let path = &path[1..];

    !user.is_empty()
        && !user.contains(&b':')
        && !user.contains(&b'/')
        && !host.is_empty()
        && !host.contains(&b'/')
        && !path.is_empty()
}

fn display_remote(remote: &BString) -> String {
    let bytes = remote.as_slice();
    if bytes.windows(3).any(|part| part == b"://")
        || bytes
            .iter()
            .position(|byte| *byte == b'@')
            .is_some_and(|at| bytes[..at].contains(&b':'))
        || is_scp_style_url(bytes)
    {
        "[redacted remote]".to_string()
    } else {
        display(remote)
    }
}

pub fn render(facts: &Facts) -> String {
    let branch = display(&facts.default_branch);
    let remote = facts.remote.as_ref().map(display_remote);
    let mut text = String::from(
        "# Grove checkout layout\n\n\
         This directory is the **grove root**, not a working copy. It contains the\n\
         shared bare repository and one directory for each worktree. Do project work\n\
         inside a worktree directory.\n\n\
         ```text\n\
         .\n\
         ├── .bare/       shared bare repository\n\
         ├── .git         file pointing at .bare\n\
         ├── AGENTS.md    this layout guide\n\
         ├── CLAUDE.md    symlink to AGENTS.md\n\
         └── <name>/      worktree: one checkout of one branch\n\
         ```\n\n\
         ## Rules\n\n\
         - Work inside a worktree, never at the grove root.\n\
         - A branch can be checked out in only one worktree at a time.\n\
         - `.bare/`, `.git`, `AGENTS.md`, and `CLAUDE.md` are outside every\n\
           worktree and outside repository history.\n\
         - Never edit or delete anything inside `.bare/`.\n\n\
         ## Available commands\n\n\
         ```bash\n\
         git grove list                 # inspect worktrees and their branches\n\
         git grove adopt <repository>   # convert an ordinary repository into a grove\n\
         git grove add <branch>         # create a worktree for a branch\n\
         git grove sync                 # fetch and fast-forward eligible worktrees\n\
         git grove publish <url>        # give an unpublished grove a remote and push it\n\
         ```\n\n",
    );

    if facts.published {
        text.push_str(&format!(
            "The default branch is `{branch}`. The configured remote is `{}`, so remote\n\
             branches are named like `{}/<branch>`.\n\n",
            remote.as_deref().unwrap_or("origin"),
            remote.as_deref().unwrap_or("origin"),
        ));
    } else {
        text.push_str(&format!(
            "This grove is **not published**: it has no upstream branch. Its default\n\
             branch is `{branch}`.\n\n"
        ));
    }

    if facts.narrowed {
        text.push_str(
            "This clone is **narrowed**: only part of the branch namespace or history\n\
             may be available locally.\n\n",
        );
    }

    text.push_str(
        "## Reconstruction escape hatch\n\n\
         The grove is ordinary Git underneath. Use these underlying commands only\n\
         when reconstructing the layout by hand:\n\n\
         ```bash\n\
         git worktree list\n\
         git worktree add <path> <branch>\n\
         git worktree remove <path>\n\
         ```\n",
    );

    text
}

fn path_is_absent(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(true),
        Err(error) => Err(GroveError::failure(format!(
            "cannot inspect {}: {error}",
            path.display()
        ))),
    }
}

pub fn write(grove: &Grove, facts: &Facts) -> Result<()> {
    let agents = grove.root.join("AGENTS.md");
    fsx::write_atomic_if_absent(&agents, render(facts).as_bytes())?;

    let claude = grove.root.join("CLAUDE.md");
    if path_is_absent(&claude)? {
        fsx::symlink_relative(&claude, "AGENTS.md")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bstr::BString;

    fn facts() -> Facts {
        Facts {
            remote: Some(BString::from("origin")),
            default_branch: BString::from("main"),
            published: true,
            narrowed: false,
        }
    }

    #[test]
    fn leads_with_the_available_grove_commands() {
        let text = render(&facts());
        let list = text
            .find("git grove list")
            .expect("list must be documented");
        let add = text.find("git grove add").expect("add must be documented");
        let adopt = text
            .find("git grove adopt")
            .expect("adopt must be documented");
        let sync = text
            .find("git grove sync")
            .expect("sync must be documented");
        let raw = text
            .find("git worktree add")
            .expect("escape-hatch command must be documented");

        let publish = text
            .find("git grove publish")
            .expect("publish must be documented");

        assert!(list < adopt);
        assert!(adopt < add);
        assert!(add < sync);
        assert!(sync < publish);
        assert!(publish < raw);
        assert!(text.contains(
            "git grove sync                 # fetch and fast-forward eligible worktrees"
        ));
        assert!(text.contains(
            "git grove publish <url>        # give an unpublished grove a remote and push it"
        ));
        for unavailable in ["git grove ls", "git grove new"] {
            assert!(
                !text.contains(unavailable),
                "must not recommend {unavailable}"
            );
        }
    }

    #[test]
    fn renders_the_remote_and_default_branch() {
        let mut facts = facts();
        facts.remote = Some(BString::from("upstream"));
        facts.default_branch = BString::from("trunk");

        let text = render(&facts);

        assert!(text.contains("upstream/<branch>"));
        assert!(text.contains("trunk"));
        assert!(!text.contains("origin/"));
    }

    #[test]
    fn escapes_non_utf8_branch_and_remote_names_reversibly() {
        let mut facts = facts();
        facts.remote = Some(BString::from(vec![b'u', b'p', 0xff]));
        facts.default_branch = BString::from(vec![b'm', 0xfe]);

        let text = render(&facts);

        assert!(text.contains("up\\xFF"));
        assert!(text.contains("m\\xFE"));
    }

    #[test]
    fn preserves_a_valid_remote_name_containing_an_at_sign() {
        let mut facts = facts();
        facts.remote = Some(BString::from("origin@mirror"));

        assert!(render(&facts).contains("origin@mirror/<branch>"));
    }

    #[test]
    fn states_when_a_grove_is_unpublished_without_a_tracking_expression() {
        let mut facts = facts();
        facts.published = false;
        facts.remote = None;

        let text = render(&facts);

        assert!(text.contains("not published"));
        assert!(!text.contains("@{upstream}"));
    }

    #[test]
    fn warns_about_a_narrowed_clone() {
        let mut facts = facts();
        facts.narrowed = true;

        assert!(render(&facts).contains("narrowed"));
    }

    #[test]
    fn never_renders_a_url_or_credentials() {
        let mut facts = facts();
        facts.remote = Some(BString::from("https://alice:secret@example.test/repo"));
        let text = render(&facts);

        assert!(!text.contains("://"));
        assert!(!text.contains("alice:secret"));
    }

    #[test]
    fn redacts_a_credential_shaped_remote_without_redacting_an_at_sign() {
        let mut facts = facts();
        facts.remote = Some(BString::from("alice:secret@mirror"));

        let text = render(&facts);

        assert!(!text.contains("alice:secret"));
        assert!(text.contains("[redacted remote]"));
    }

    #[test]
    fn redacts_an_scp_style_url_without_redacting_an_at_sign() {
        let mut facts = facts();
        facts.remote = Some(BString::from("git@github.example:owner/repository"));

        let text = render(&facts);

        assert!(!text.contains("github.example"));
        assert!(text.contains("[redacted remote]"));
    }

    #[test]
    fn writes_agents_and_a_relative_claude_link() {
        let dir = tempfile::tempdir().unwrap();
        let grove = crate::grove::discover::Grove {
            root: dir.path().to_path_buf(),
        };

        write(&grove, &facts()).unwrap();

        assert!(dir.path().join("AGENTS.md").is_file());
        assert_eq!(
            std::fs::read_link(dir.path().join("CLAUDE.md")).unwrap(),
            std::path::Path::new("AGENTS.md")
        );
    }

    #[test]
    fn preserves_every_existing_agents_or_claude_path() {
        let dir = tempfile::tempdir().unwrap();
        let grove = crate::grove::discover::Grove {
            root: dir.path().to_path_buf(),
        };
        let agents = dir.path().join("AGENTS.md");
        let claude = dir.path().join("CLAUDE.md");
        std::fs::write(&agents, b"mine").unwrap();
        std::os::unix::fs::symlink("missing", &claude).unwrap();

        write(&grove, &facts()).unwrap();

        assert_eq!(std::fs::read(&agents).unwrap(), b"mine");
        assert_eq!(
            std::fs::read_link(&claude).unwrap(),
            std::path::Path::new("missing")
        );
    }

    #[test]
    fn preserves_a_broken_agents_link() {
        let dir = tempfile::tempdir().unwrap();
        let grove = crate::grove::discover::Grove {
            root: dir.path().to_path_buf(),
        };
        let agents = dir.path().join("AGENTS.md");
        std::os::unix::fs::symlink("missing", &agents).unwrap();

        write(&grove, &facts()).unwrap();

        assert_eq!(
            std::fs::read_link(&agents).unwrap(),
            std::path::Path::new("missing")
        );
        assert!(std::fs::symlink_metadata(dir.path().join("CLAUDE.md")).is_ok());
    }

    #[test]
    fn preserves_a_foreign_claude_file() {
        let dir = tempfile::tempdir().unwrap();
        let grove = crate::grove::discover::Grove {
            root: dir.path().to_path_buf(),
        };
        let claude = dir.path().join("CLAUDE.md");
        std::fs::write(&claude, b"foreign instructions").unwrap();

        write(&grove, &facts()).unwrap();

        assert_eq!(std::fs::read(&claude).unwrap(), b"foreign instructions");
    }
}
