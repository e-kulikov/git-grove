use crate::error::{GroveError, Result};
use crate::grove::discover::Grove;
use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

pub const KNOWN: &[&str] = &[
    "clone",
    "plant",
    "init",
    "seed",
    "add",
    "sprout",
    "list",
    "survey",
    "completion",
    "help",
];

#[derive(Parser, Debug)]
#[command(
    name = "git-grove",
    version,
    about = "Manage repositories as a bare clone surrounded by git worktrees",
    after_help = "Aliases: plant=clone  seed=init  sprout=add  survey=list"
)]
pub struct Cli {
    /// Consent to sanitizing unsafe Git environment variables
    #[arg(long, global = true)]
    pub ignore_unsupported: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum CompletionShell {
    Zsh,
    Bash,
    Fish,
}

impl From<CompletionShell> for Shell {
    fn from(shell: CompletionShell) -> Self {
        match shell {
            CompletionShell::Zsh => Shell::Zsh,
            CompletionShell::Bash => Shell::Bash,
            CompletionShell::Fish => Shell::Fish,
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Clone a repository into a new grove
    #[command(visible_alias = "plant")]
    Clone {
        url: OsString,
        dir: Option<PathBuf>,
        /// Branch and directory of the first worktree
        #[arg(short = 'b', long = "branch")]
        branch: Option<OsString>,
        /// Options forwarded to `git clone`
        #[arg(last = true)]
        git_options: Vec<OsString>,
    },
    /// Create a new repository directly as a grove
    #[command(visible_alias = "seed")]
    Init {
        dir: Option<PathBuf>,
        #[arg(short = 'b', long = "branch")]
        branch: Option<OsString>,
    },
    /// Add a worktree for a branch
    #[command(
        visible_alias = "sprout",
        after_help = "Forms:\n  git-grove add <branch> [dir]\n  git-grove add --detach <revision> [dir]\n\nThe branch form accepts at most two positional arguments; the detached form accepts at most one."
    )]
    Add(AddArgs),
    /// Show the grove and the state of every worktree
    #[command(visible_alias = "survey")]
    List {
        /// Machine-readable NUL-delimited output
        #[arg(long)]
        porcelain: bool,
    },
    /// Generate shell completion code
    Completion { shell: CompletionShell },
}

#[derive(Args, Debug)]
pub struct AddArgs {
    #[arg(value_name = "BRANCH_OR_DIR", num_args = 0..=2)]
    positionals: Vec<OsString>,
    /// Start point for a branch that does not exist yet
    #[arg(long = "start", conflicts_with = "detach")]
    start: Option<OsString>,
    /// Check out a revision without a branch
    #[arg(long = "detach", value_name = "REVISION")]
    detach: Option<OsString>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AddMode {
    Branch {
        branch: OsString,
        dir: Option<PathBuf>,
        start: Option<OsString>,
    },
    Detached {
        revision: OsString,
        dir: Option<PathBuf>,
    },
}

impl AddArgs {
    pub fn resolve(self) -> Result<AddMode> {
        if let Some(revision) = self.detach {
            let dir = match self.positionals.as_slice() {
                [] => None,
                [dir] => Some(PathBuf::from(dir)),
                _ => {
                    return Err(GroveError::usage(
                        "`add --detach <revision>` accepts at most one directory",
                    ))
                }
            };
            return Ok(AddMode::Detached { revision, dir });
        }

        let (branch, dir) = match self.positionals.as_slice() {
            [] => return Err(GroveError::usage("`add` requires a branch")),
            [branch] => (branch.clone(), None),
            [branch, dir] => (branch.clone(), Some(PathBuf::from(dir))),
            _ => unreachable!("clap limits add to two positional arguments"),
        };
        Ok(AddMode::Branch {
            branch,
            dir,
            start: self.start,
        })
    }
}

fn is_known(arg: &OsStr) -> bool {
    KNOWN.iter().any(|known| arg == OsStr::new(known))
}

#[cfg(unix)]
fn locator_bytes(arg: &OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    arg.as_bytes()
}

fn has_scheme(bytes: &[u8]) -> bool {
    let Some(scheme_end) = bytes.windows(3).position(|window| window == b"://") else {
        return false;
    };
    let scheme = &bytes[..scheme_end];
    matches!(scheme.first(), Some(byte) if byte.is_ascii_alphabetic())
        && scheme
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
}

fn is_scp_locator(bytes: &[u8]) -> bool {
    let Some(colon) = bytes.iter().position(|byte| *byte == b':') else {
        return false;
    };
    let host = &bytes[..colon];
    let repository = &bytes[colon + 1..];
    !host.is_empty()
        && !repository.is_empty()
        && !host
            .iter()
            .any(|byte| matches!(byte, b'/' | b'\\') || byte.is_ascii_whitespace())
        && !repository.iter().any(u8::is_ascii_whitespace)
        && host
            .rsplit(|byte| *byte == b'@')
            .next()
            .is_some_and(|host| !host.is_empty())
}

fn is_explicit_path(bytes: &[u8], path: &Path) -> bool {
    path.is_absolute()
        || bytes.starts_with(b"~")
        || bytes.starts_with(b"./")
        || bytes.starts_with(b"../")
}

fn is_real_directory(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_dir())
}

fn is_real_file(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_file())
}

fn read_path_file(path: &Path, prefix: &[u8]) -> Option<PathBuf> {
    use rustix::fs::{open, Mode, OFlags};
    use std::fs::File;
    use std::io::Read;
    use std::os::unix::ffi::OsStringExt;

    const MAX_PATH_FILE_BYTES: u64 = 4096;

    let file = open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.file_type().is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_PATH_FILE_BYTES
    {
        return None;
    }
    let mut contents = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_PATH_FILE_BYTES + 1)
        .read_to_end(&mut contents)
        .ok()?;
    if contents.len() as u64 > MAX_PATH_FILE_BYTES {
        return None;
    }
    let line = contents.strip_suffix(b"\n").unwrap_or(&contents);
    if line.is_empty()
        || line
            .iter()
            .any(|byte| matches!(byte, b'\0' | b'\n' | b'\r'))
    {
        return None;
    }
    let value = line.strip_prefix(prefix)?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(value.to_vec())))
}

fn resolve_path(base: &Path, path: PathBuf) -> PathBuf {
    let path: PathBuf = path.components().collect();
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn has_git_admin_structure(git_dir: &Path) -> bool {
    if !is_real_directory(git_dir) || !is_real_file(&git_dir.join("HEAD")) {
        return false;
    }
    if is_real_directory(&git_dir.join("objects")) {
        return true;
    }
    let Some(common_dir) = read_path_file(&git_dir.join("commondir"), b"") else {
        return false;
    };
    let common_dir = resolve_path(git_dir, common_dir);
    is_real_directory(&common_dir) && is_real_directory(&common_dir.join("objects"))
}

fn is_existing_repository(path: &Path) -> bool {
    if !is_real_directory(path) {
        return false;
    }
    let marker = path.join(".git");
    if has_git_admin_structure(&marker) || has_git_admin_structure(path) {
        return true;
    }
    let Some(admin_dir) = read_path_file(&marker, b"gitdir: ") else {
        return false;
    };
    has_git_admin_structure(&resolve_path(path, admin_dir))
}

fn looks_like_explicit_locator(arg: &OsStr) -> bool {
    let bytes = locator_bytes(arg);
    let path = Path::new(arg);
    has_scheme(bytes) || is_scp_locator(bytes) || is_explicit_path(bytes, path)
}

/// Apply the default-action rules before clap sees the arguments.
pub fn normalize(argv: Vec<OsString>) -> Result<Vec<OsString>> {
    normalize_with(argv, || {
        std::env::current_dir().map_err(|error| {
            GroveError::failure(format!("cannot determine current directory: {error}"))
        })
    })
}

#[cfg(test)]
fn normalize_from(argv: Vec<OsString>, cwd: &Path) -> Result<Vec<OsString>> {
    normalize_with(argv, || Ok(cwd.to_path_buf()))
}

fn normalize_with<F>(mut argv: Vec<OsString>, cwd: F) -> Result<Vec<OsString>>
where
    F: FnOnce() -> Result<PathBuf>,
{
    let command_index = argv
        .iter()
        .skip(1)
        .take_while(|arg| *arg == OsStr::new("--ignore-unsupported"))
        .count()
        + 1;
    match argv.get(command_index) {
        None => {
            Grove::discover(&cwd()?)?;
            argv.push(OsString::from("list"));
            Ok(argv)
        }
        Some(first) if first.as_encoded_bytes().starts_with(b"-") || is_known(first) => Ok(argv),
        Some(first) if looks_like_explicit_locator(first) => {
            argv.insert(command_index, OsString::from("clone"));
            Ok(argv)
        }
        Some(first) => {
            let cwd = cwd()?;
            let path = Path::new(first);
            let candidate = if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            };
            if is_existing_repository(&candidate) {
                argv.insert(command_index, OsString::from("clone"));
                return Ok(argv);
            }
            let display = first.to_string_lossy();
            Err(GroveError::usage(format!(
                "`{display}` is neither a command nor a repository location"
            ))
            .with_detail("write `git grove clone <url>` if you meant to clone it"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::ffi::OsString;
    use std::path::Path;

    fn argv(args: &[&str]) -> Vec<OsString> {
        std::iter::once("git-grove")
            .chain(args.iter().copied())
            .map(OsString::from)
            .collect()
    }

    fn norm(cwd: &Path, args: &[&str]) -> Vec<OsString> {
        normalize_from(argv(args), cwd).unwrap()
    }

    #[test]
    fn keeps_lifecycle_commands_tooling_commands_and_aliases() {
        let cwd = tempfile::tempdir().unwrap();
        for command in [
            "clone",
            "plant",
            "init",
            "seed",
            "add",
            "sprout",
            "list",
            "survey",
            "completion",
            "help",
        ] {
            assert_eq!(norm(cwd.path(), &[command]), argv(&[command]));
        }
    }

    #[test]
    fn expands_recognisable_locators_into_clone() {
        let cwd = tempfile::tempdir().unwrap();
        for locator in [
            "https://host/x.git",
            "ssh+git://host/x.git",
            "git@host:o/r.git",
            "host:o/r.git",
            "/srv/git/r.git",
            "./r",
            "../r",
            "~/src/r",
            "~other/src/r",
        ] {
            assert_eq!(
                norm(cwd.path(), &[locator]),
                argv(&["clone", locator]),
                "locator {locator}"
            );
        }
    }

    #[test]
    fn expands_an_existing_repository_path_into_clone() {
        let cwd = tempfile::tempdir().unwrap();
        let repository = cwd.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        std::fs::create_dir(repository.join(".git")).unwrap();
        std::fs::write(repository.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir(repository.join(".git/objects")).unwrap();
        let input = vec![OsString::from("git-grove"), OsString::from("repository")];
        let mut expected = input.clone();
        expected.insert(1, OsString::from("clone"));

        assert_eq!(normalize_from(input, cwd.path()).unwrap(), expected);
    }

    #[test]
    fn expands_a_relative_gitfile_repository_path_into_clone() {
        let cwd = tempfile::tempdir().unwrap();
        let repository = cwd.path().join("repository");
        let admin = cwd.path().join("admin");
        std::fs::create_dir(&repository).unwrap();
        std::fs::create_dir(&admin).unwrap();
        std::fs::write(repository.join(".git"), "gitdir: ../admin/\n").unwrap();
        std::fs::write(admin.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir(admin.join("objects")).unwrap();

        assert_eq!(
            norm(cwd.path(), &["repository"]),
            argv(&["clone", "repository"])
        );
    }

    #[test]
    fn expands_a_real_linked_worktree_path_into_clone() {
        fn git(cwd: &Path, args: &[&str]) {
            let output = std::process::Command::new("git")
                .current_dir(cwd)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let cwd = tempfile::tempdir().unwrap();
        let primary = cwd.path().join("primary");
        let linked = cwd.path().join("linked");
        git(
            cwd.path(),
            &["init", "--quiet", "--initial-branch=main", "primary"],
        );
        git(
            &primary,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "initial",
            ],
        );
        git(
            &primary,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "linked",
                linked.to_str().unwrap(),
            ],
        );

        assert_eq!(norm(cwd.path(), &["linked"]), argv(&["clone", "linked"]));
    }

    #[test]
    fn refuses_malformed_or_dangling_gitfile_markers() {
        let cwd = tempfile::tempdir().unwrap();
        let admin = cwd.path().join("admin");
        std::fs::create_dir(&admin).unwrap();
        std::fs::write(admin.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir(admin.join("objects")).unwrap();

        for (name, marker) in [
            ("empty", b"".as_slice()),
            ("missing-prefix", b"../admin\n".as_slice()),
            ("missing-path", b"gitdir: \n".as_slice()),
            ("extra-line", b"gitdir: ../admin\nextra\n".as_slice()),
            ("embedded-nul", b"gitdir: ../admin\0\n".as_slice()),
            ("dangling", b"gitdir: ../missing\n".as_slice()),
        ] {
            let repository = cwd.path().join(name);
            std::fs::create_dir(&repository).unwrap();
            std::fs::write(repository.join(".git"), marker).unwrap();
            let err = normalize_from(
                vec![OsString::from("git-grove"), OsString::from(name)],
                cwd.path(),
            )
            .unwrap_err();
            assert_eq!(err.class, crate::error::ExitClass::Usage, "marker {name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_gitfile_targeting_a_symlink_with_a_trailing_separator() {
        let cwd = tempfile::tempdir().unwrap();
        let repository = cwd.path().join("repository");
        let real_admin = cwd.path().join("real-admin");
        std::fs::create_dir(&repository).unwrap();
        std::fs::create_dir(&real_admin).unwrap();
        std::fs::write(real_admin.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir(real_admin.join("objects")).unwrap();
        std::os::unix::fs::symlink(&real_admin, cwd.path().join("admin-link")).unwrap();
        std::fs::write(repository.join(".git"), "gitdir: ../admin-link/\n").unwrap();

        let err = normalize_from(
            vec![OsString::from("git-grove"), OsString::from("repository")],
            cwd.path(),
        )
        .unwrap_err();
        assert_eq!(err.class, crate::error::ExitClass::Usage);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_common_dir_symlink_hidden_by_a_trailing_separator() {
        let cwd = tempfile::tempdir().unwrap();
        let repository = cwd.path().join("repository");
        let admin = cwd.path().join("admin");
        let real_common = cwd.path().join("real-common");
        std::fs::create_dir(&repository).unwrap();
        std::fs::create_dir(&admin).unwrap();
        std::fs::create_dir(&real_common).unwrap();
        std::fs::write(repository.join(".git"), "gitdir: ../admin\n").unwrap();
        std::fs::write(admin.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(admin.join("commondir"), "../common-link/\n").unwrap();
        std::fs::create_dir(real_common.join("objects")).unwrap();
        std::os::unix::fs::symlink(&real_common, cwd.path().join("common-link")).unwrap();

        let err = normalize_from(
            vec![OsString::from("git-grove"), OsString::from("repository")],
            cwd.path(),
        )
        .unwrap_err();
        assert_eq!(err.class, crate::error::ExitClass::Usage);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_empty_and_symlinked_git_markers_as_existing_repositories() {
        let cwd = tempfile::tempdir().unwrap();

        let empty = cwd.path().join("empty");
        std::fs::create_dir_all(empty.join(".git")).unwrap();
        let empty_err = normalize_from(
            vec![OsString::from("git-grove"), OsString::from("empty")],
            cwd.path(),
        )
        .unwrap_err();
        assert_eq!(empty_err.class, crate::error::ExitClass::Usage);

        let target = cwd.path().join("git-target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir(target.join("objects")).unwrap();
        let linked = cwd.path().join("linked");
        std::fs::create_dir(&linked).unwrap();
        std::os::unix::fs::symlink(&target, linked.join(".git")).unwrap();
        let linked_err = normalize_from(
            vec![OsString::from("git-grove"), OsString::from("linked")],
            cwd.path(),
        )
        .unwrap_err();
        assert_eq!(linked_err.class, crate::error::ExitClass::Usage);
    }

    #[test]
    fn expands_no_arguments_into_list_only_inside_a_grove() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir(cwd.path().join(".bare")).unwrap();
        std::fs::write(
            cwd.path().join(".git"),
            crate::grove::layout::POINTER_CONTENTS,
        )
        .unwrap();

        assert_eq!(norm(cwd.path(), &[]), argv(&["list"]));

        let outside = tempfile::tempdir().unwrap();
        let err = normalize_from(argv(&[]), outside.path()).unwrap_err();
        assert_eq!(err.class, crate::error::ExitClass::Usage);
        assert!(err.message.contains("not inside a grove"));
    }

    #[test]
    fn refuses_an_ambiguous_bare_word_with_an_explicit_clone_hint() {
        let cwd = tempfile::tempdir().unwrap();
        let err = normalize_from(argv(&["clnoe"]), cwd.path()).unwrap_err();
        assert_eq!(err.class, crate::error::ExitClass::Usage);
        assert!(err.message.contains("clnoe"));
        assert!(err.detail.unwrap().contains("git grove clone <url>"));
    }

    #[test]
    fn leaves_flags_to_clap() {
        let cwd = tempfile::tempdir().unwrap();
        assert_eq!(norm(cwd.path(), &["--version"]), argv(&["--version"]));
        assert_eq!(norm(cwd.path(), &["-h"]), argv(&["-h"]));
    }

    #[test]
    fn help_and_version_do_not_look_up_the_current_directory() {
        for flag in ["--help", "-h", "--version", "-V"] {
            let normalized = normalize_with(argv(&[flag]), || {
                panic!("help and version must not inspect the current directory")
            })
            .unwrap();
            assert_eq!(normalized, argv(&[flag]));
        }
    }

    #[test]
    fn expands_implicit_actions_after_the_leading_global_override() {
        let cwd = tempfile::tempdir().unwrap();
        assert_eq!(
            norm(cwd.path(), &["--ignore-unsupported", "https://host/x.git"]),
            argv(&["--ignore-unsupported", "clone", "https://host/x.git"])
        );

        std::fs::create_dir(cwd.path().join(".bare")).unwrap();
        std::fs::write(
            cwd.path().join(".git"),
            crate::grove::layout::POINTER_CONTENTS,
        )
        .unwrap();
        assert_eq!(
            norm(cwd.path(), &["--ignore-unsupported"]),
            argv(&["--ignore-unsupported", "list"])
        );
    }

    #[test]
    fn accepts_the_global_policy_override_for_every_lifecycle_command() {
        for args in [
            vec!["clone", "origin", "--ignore-unsupported"],
            vec!["init", "--ignore-unsupported"],
            vec!["add", "--ignore-unsupported"],
            vec!["list", "--ignore-unsupported"],
        ] {
            let parsed =
                Cli::try_parse_from(std::iter::once("git-grove").chain(args.iter().copied()))
                    .unwrap();
            assert!(parsed.ignore_unsupported, "arguments {args:?}");
        }
    }

    #[test]
    fn limits_runtime_completion_to_the_supported_shells() {
        for shell in ["zsh", "bash", "fish"] {
            assert!(Cli::try_parse_from(["git-grove", "completion", shell]).is_ok());
        }
        assert!(Cli::try_parse_from(["git-grove", "completion", "powershell"]).is_err());
    }

    fn parsed_add(args: &[&str]) -> AddArgs {
        let parsed =
            Cli::try_parse_from(std::iter::once("git-grove").chain(args.iter().copied())).unwrap();
        match parsed.command {
            Command::Add(args) => args,
            other => panic!("parsed the wrong command: {other:?}"),
        }
    }

    #[test]
    fn resolves_branch_add_with_an_optional_directory() {
        assert_eq!(
            parsed_add(&["add", "topic", "worktrees/topic"])
                .resolve()
                .unwrap(),
            AddMode::Branch {
                branch: OsString::from("topic"),
                dir: Some(PathBuf::from("worktrees/topic")),
                start: None,
            }
        );
    }

    #[test]
    fn resolves_detached_add_without_consuming_its_directory_as_a_branch() {
        assert_eq!(
            parsed_add(&["add", "--detach", "HEAD~1", "inspections/previous"])
                .resolve()
                .unwrap(),
            AddMode::Detached {
                revision: OsString::from("HEAD~1"),
                dir: Some(PathBuf::from("inspections/previous")),
            }
        );
    }

    #[test]
    fn rejects_missing_or_ambiguous_add_positionals() {
        let missing_branch = parsed_add(&["add"]).resolve().unwrap_err();
        assert_eq!(missing_branch.class, crate::error::ExitClass::Usage);

        let detached_excess = parsed_add(&["add", "--detach", "HEAD", "one", "two"])
            .resolve()
            .unwrap_err();
        assert_eq!(detached_excess.class, crate::error::ExitClass::Usage);

        assert!(Cli::try_parse_from(["git-grove", "add", "one", "two", "three"]).is_err());
    }

    #[test]
    fn add_help_explains_both_positional_forms_and_limits() {
        use clap::CommandFactory;

        let help = Cli::command()
            .find_subcommand_mut("add")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(help.contains("add <branch> [dir]"));
        assert!(help.contains("add --detach <revision> [dir]"));
        assert!(help.contains("at most two positional arguments"));
    }

    #[cfg(unix)]
    #[test]
    fn accepts_non_utf8_git_values() {
        use std::os::unix::ffi::OsStringExt;

        let locator = OsString::from_vec(vec![b'r', 0xff, b'p', b'o']);
        let branch = OsString::from_vec(vec![b'b', 0xfe]);
        let parsed = Cli::try_parse_from([
            OsString::from("git-grove"),
            OsString::from("clone"),
            locator.clone(),
            OsString::from("--branch"),
            branch.clone(),
        ])
        .unwrap();

        match parsed.command {
            Command::Clone {
                url,
                branch: parsed_branch,
                ..
            } => {
                assert_eq!(url, locator);
                assert_eq!(parsed_branch, Some(branch));
            }
            other => panic!("parsed the wrong command: {other:?}"),
        }
    }
}
