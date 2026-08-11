use crate::error::{GroveError, Result};
use crate::grove::discover::Grove;
use clap::{Parser, Subcommand, ValueEnum};
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
    /// Report incompatible options and environment, then continue
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
    #[command(visible_alias = "sprout")]
    Add {
        branch: Option<OsString>,
        dir: Option<PathBuf>,
        /// Start point for a branch that does not exist yet
        #[arg(long = "start")]
        start: Option<OsString>,
        /// Check out a revision without a branch
        #[arg(long = "detach", conflicts_with = "start")]
        detach: Option<OsString>,
    },
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
        || bytes == b"~"
        || bytes.starts_with(b"~/")
        || bytes.starts_with(b"./")
        || bytes.starts_with(b"../")
}

fn is_existing_repository(path: &Path) -> bool {
    path.join(".git").exists() || (path.join("HEAD").is_file() && path.join("objects").is_dir())
}

fn looks_like_locator(arg: &OsStr, cwd: &Path) -> bool {
    let bytes = locator_bytes(arg);
    let path = Path::new(arg);
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    has_scheme(bytes)
        || is_scp_locator(bytes)
        || is_explicit_path(bytes, path)
        || is_existing_repository(&candidate)
}

/// Apply the default-action rules before clap sees the arguments.
pub fn normalize(argv: Vec<OsString>) -> Result<Vec<OsString>> {
    let cwd = std::env::current_dir().map_err(|error| {
        GroveError::failure(format!("cannot determine current directory: {error}"))
    })?;
    normalize_from(argv, &cwd)
}

fn normalize_from(mut argv: Vec<OsString>, cwd: &Path) -> Result<Vec<OsString>> {
    match argv.get(1) {
        None => {
            Grove::discover(cwd)?;
            argv.push(OsString::from("list"));
            Ok(argv)
        }
        Some(first) if first.as_encoded_bytes().starts_with(b"-") || is_known(first) => Ok(argv),
        Some(first) if looks_like_locator(first, cwd) => {
            argv.insert(1, OsString::from("clone"));
            Ok(argv)
        }
        Some(first) => {
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
        let input = vec![
            OsString::from("git-grove"),
            repository.as_os_str().to_owned(),
        ];
        let mut expected = input.clone();
        expected.insert(1, OsString::from("clone"));

        assert_eq!(normalize_from(input, cwd.path()).unwrap(), expected);
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
