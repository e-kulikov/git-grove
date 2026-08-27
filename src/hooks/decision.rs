use crate::hooks::payload::{NormalizedPayload, Tool};
use crate::hooks::Verdict;
use std::path::{Component, Path, PathBuf};

/// Resolve one candidate path to an absolute, containment-comparable form,
/// without requiring the path to exist. Hook targets frequently do not
/// exist yet (a file about to be created), so `Path::canonicalize` alone is
/// wrong — it fails outright on any nonexistent path.
///
/// Algorithm (the plan's binding correction to the approved spec): make the
/// candidate absolute against `base`; walk upward from the full path to the
/// deepest ancestor that actually exists; canonicalize that existing prefix
/// (following symlinks, exactly like any other containment check in this
/// codebase); reject the candidate outright if the remaining, nonexistent
/// tail still contains a `..` component (its target cannot be determined
/// without a real directory to resolve it against); otherwise lexically
/// re-append the tail's non-`.` components to the canonical prefix.
fn resolve(candidate: &Path, base: &Path) -> Result<PathBuf, ()> {
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        base.join(candidate)
    };

    let components: Vec<Component> = absolute.components().collect();
    let mut boundary = components.len();
    let existing_prefix = loop {
        let candidate: PathBuf = components[..boundary].iter().collect();
        if candidate.exists() {
            break candidate;
        }
        if boundary == 0 {
            // Nothing on the path exists, not even a root — cannot happen on
            // a real filesystem with an absolute path, but fail closed
            // rather than loop forever if it somehow does.
            return Err(());
        }
        boundary -= 1;
    };
    let tail = &components[boundary..];

    if tail
        .iter()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(());
    }

    let canonical_prefix = existing_prefix.canonicalize().map_err(|_| ())?;
    let mut resolved = canonical_prefix;
    for component in tail {
        if let Component::Normal(part) = component {
            resolved.push(part);
        }
        // `Component::CurDir` (a literal `.`) contributes nothing; `RootDir`/
        // `Prefix` cannot appear in the tail once `boundary` has advanced
        // past the absolute path's own root.
    }
    Ok(resolved)
}

/// Whether `resolved` is the grove's bare repository (or anything under it)
/// or the exact root pointer file. `canonical_bare`/`canonical_git` are the
/// yardstick, not a second candidate to resolve: callers pass
/// `Grove::discover`'s own `root` (already canonicalized —
/// `grove::discover::Grove::discover` canonicalizes `start` before walking
/// up) joined with `.bare`/`.git`, without re-canonicalizing here. That is
/// sound, not an asymmetry with `resolve`'s own canonicalization above:
/// `validate_signature` opens `.bare` with `O_NOFOLLOW` and proves `.git` is
/// a regular file, not a symlink, so both are already exactly what a second
/// canonicalize would produce.
fn is_protected(resolved: &Path, canonical_bare: &Path, canonical_git: &Path) -> bool {
    resolved == canonical_bare || resolved.starts_with(canonical_bare) || resolved == canonical_git
}

/// Shell operators and bare redirection tokens `bash_candidates` treats as
/// syntax, never as a path candidate.
const SHELL_OPERATORS: &[&str] = &["&&", "||", ";", "|", ">", ">>", "<", "<<", "&", "2>&1"];

/// Minimal, intentionally forgiving shell tokenizer: honors single/double
/// quoting and backslash escapes, splits on unquoted whitespace. It is not
/// a POSIX shell parser and is not meant to be one — see `bash_candidates`.
fn shell_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = command.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '\\' if !in_single => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            character if character.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            character => current.push(character),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Conservative, best-effort path-candidate extraction for a Bash command:
/// every token that is not a recognized shell operator, with a glued
/// leading redirection operator stripped — `>file`, `>>file`, `<file`, and
/// the same three with a leading file-descriptor number glued on
/// (`1>file`, `2>>file`, `0<file`; exec-reviewer caught `1>../.bare/config`
/// surviving as one untouched candidate token in an earlier round). A token
/// that looks like a long or short option with its value glued on
/// (`--flag=path`, `-o=path`) additionally yields the value half as its own
/// candidate — otherwise `--output=../.git` resolves as the single
/// nonexistent path `<base>/--output=../.git`, `..` fused into a directory
/// name rather than a real parent-directory component, and never matches
/// containment at all. Shell commands have no single canonical "the path"
/// the way a structured tool call does — a prior `cd`, a loop, or command
/// substitution can all reach a protected path without one clean token
/// containing it — so this deliberately over-collects candidates rather
/// than under-collects: a false positive costs an annoying rephrase, a
/// false negative is the hole this feature exists to close.
fn bash_candidates(command: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    for token in shell_tokens(command) {
        if SHELL_OPERATORS.contains(&token.as_str()) {
            continue;
        }
        let mut rest = token.as_str();
        let digit_end = rest
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(rest.len());
        let after_digits = &rest[digit_end..];
        for operator in [">>", "<<", ">", "<"] {
            if let Some(stripped) = after_digits.strip_prefix(operator) {
                rest = stripped;
                break;
            }
        }
        if rest.is_empty() {
            continue;
        }
        candidates.push(rest.to_string());
        if rest.starts_with('-') {
            if let Some((_, value)) = rest.split_once('=') {
                if !value.is_empty() {
                    candidates.push(value.to_string());
                }
            }
        }
    }
    candidates
}

/// Extract every path an `apply_patch` payload names, or an error if the
/// grammar itself is malformed. Grammar: the first nonblank line is
/// `*** Begin Patch`, the last is `*** End Patch`; recognized headers are
/// `*** Add File: `, `*** Delete File: `, `*** Update File: `, and
/// `*** Move to: `; any other `*** `-prefixed line is an unknown control
/// line and rejects the whole patch; every other line (diff body: context,
/// `+`/`-`, `@@`) is never parsed as a path.
fn apply_patch_candidates(patch: &str) -> Result<Vec<String>, String> {
    let lines: Vec<&str> = patch.lines().collect();
    let nonblank: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if nonblank.first() != Some(&"*** Begin Patch") {
        return Err("apply_patch payload does not start with `*** Begin Patch`".to_string());
    }
    if nonblank.last() != Some(&"*** End Patch") {
        return Err("apply_patch payload does not end with `*** End Patch`".to_string());
    }

    let mut candidates = Vec::new();
    for line in &lines {
        if *line == "*** Begin Patch" || *line == "*** End Patch" {
            continue;
        }
        let mut matched = false;
        for header in [
            "*** Add File: ",
            "*** Delete File: ",
            "*** Update File: ",
            "*** Move to: ",
        ] {
            if let Some(path) = line.strip_prefix(header) {
                candidates.push(path.to_string());
                matched = true;
                break;
            }
        }
        if matched {
            continue;
        }
        if line.starts_with("*** ") {
            return Err(format!("unknown apply_patch control line: {line}"));
        }
    }
    Ok(candidates)
}

/// Decide allow/deny for one normalized tool call against one grove.
/// `canonical_bare`/`canonical_git` must already be canonicalized (see
/// `is_protected`); `process_cwd` is used only when the payload's own `cwd`
/// is absent.
///
/// Known, bounded, deliberately deferred gap (same shape as the spec's own
/// v1 exclusions): the grove is fixed by the caller's process cwd, not by
/// this payload. An agent that `cd`s into a *different* grove B and writes
/// a relative path into B's own `.bare/` is not denied by a hook installed
/// for grove A, because only A is the yardstick here.
pub fn decide(
    payload: &NormalizedPayload,
    canonical_bare: &Path,
    canonical_git: &Path,
    process_cwd: &Path,
) -> Verdict {
    let candidates = match &payload.tool {
        Tool::Bash { command } => bash_candidates(command),
        Tool::Edit { path } | Tool::Write { path } => vec![path.clone()],
        Tool::ApplyPatch { patch } => match apply_patch_candidates(patch) {
            Ok(candidates) => candidates,
            Err(reason) => return Verdict::Deny(reason),
        },
    };

    let base = payload.cwd.as_deref().unwrap_or(process_cwd);
    for candidate in &candidates {
        match resolve(Path::new(candidate), base) {
            Ok(resolved) => {
                if is_protected(&resolved, canonical_bare, canonical_git) {
                    return Verdict::Deny(format!(
                        "grove invariant: `{}` resolves to protected grove metadata; refusing",
                        candidate
                    ));
                }
            }
            Err(()) => {
                return Verdict::Deny(format!(
                    "grove invariant: `{candidate}` cannot be unambiguously resolved; refusing"
                ));
            }
        }
    }
    Verdict::Allow
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::payload::NormalizedPayload;

    fn grove() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let bare = root.path().join(".bare");
        std::fs::create_dir(&bare).unwrap();
        let git = root.path().join(".git");
        std::fs::write(&git, "gitdir: ./.bare\n").unwrap();
        let canonical_bare = bare.canonicalize().unwrap();
        let canonical_git = git.canonicalize().unwrap();
        (root, canonical_bare, canonical_git)
    }

    #[test]
    fn resolve_makes_a_relative_nonexistent_path_absolute_against_base() {
        let base = tempfile::tempdir().unwrap();
        let resolved = resolve(Path::new("nested/new-file.txt"), base.path()).unwrap();
        assert_eq!(resolved, base.path().join("nested/new-file.txt"));
    }

    #[test]
    fn resolve_follows_a_symlink_in_the_existing_prefix() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = root.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let resolved = resolve(Path::new("link/new-file.txt"), root.path()).unwrap();
        assert_eq!(resolved, real.join("new-file.txt"));
    }

    #[test]
    fn resolve_rejects_a_parent_dir_component_in_the_nonexistent_tail() {
        let base = tempfile::tempdir().unwrap();
        assert!(resolve(Path::new("nested/../escape.txt"), base.path()).is_err());
    }

    #[test]
    fn decide_denies_a_write_whose_new_leaf_lands_under_bare() {
        let (root, canonical_bare, canonical_git) = grove();
        let payload = NormalizedPayload {
            tool: Tool::Write {
                path: root
                    .path()
                    .join(".bare/git-grove-hook-probe")
                    .to_str()
                    .unwrap()
                    .to_string(),
            },
            cwd: None,
        };
        let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
        assert!(matches!(verdict, Verdict::Deny(_)));
    }

    #[test]
    fn decide_denies_edit_of_the_exact_root_git_pointer() {
        let (root, canonical_bare, canonical_git) = grove();
        let payload = NormalizedPayload {
            tool: Tool::Edit {
                path: root.path().join(".git").to_str().unwrap().to_string(),
            },
            cwd: None,
        };
        let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
        assert!(matches!(verdict, Verdict::Deny(_)));
    }

    #[test]
    fn decide_allows_an_ordinary_new_file_elsewhere() {
        let (root, canonical_bare, canonical_git) = grove();
        let payload = NormalizedPayload {
            tool: Tool::Write {
                path: root
                    .path()
                    .join("main/new-file.txt")
                    .to_str()
                    .unwrap()
                    .to_string(),
            },
            cwd: None,
        };
        let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
        assert_eq!(verdict, Verdict::Allow);
    }

    #[test]
    fn decide_denies_a_bash_command_that_cats_bare_config() {
        let (root, canonical_bare, canonical_git) = grove();
        let payload = NormalizedPayload {
            tool: Tool::Bash {
                command: "cat .bare/config".to_string(),
            },
            cwd: Some(root.path().to_path_buf()),
        };
        let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
        assert!(matches!(verdict, Verdict::Deny(_)));
    }

    #[test]
    fn decide_allows_an_unrelated_bash_command() {
        let (root, canonical_bare, canonical_git) = grove();
        let payload = NormalizedPayload {
            tool: Tool::Bash {
                command: "git status".to_string(),
            },
            cwd: Some(root.path().to_path_buf()),
        };
        let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
        assert_eq!(verdict, Verdict::Allow);
    }

    #[test]
    fn decide_denies_apply_patch_touching_bare_via_move_to() {
        let (root, canonical_bare, canonical_git) = grove();
        let patch = format!(
            "*** Begin Patch\n*** Update File: elsewhere.txt\n*** Move to: {}/config\n*** End Patch",
            root.path().join(".bare").to_str().unwrap()
        );
        let payload = NormalizedPayload {
            tool: Tool::ApplyPatch { patch },
            cwd: Some(root.path().to_path_buf()),
        };
        let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
        assert!(matches!(verdict, Verdict::Deny(_)));
    }

    #[test]
    fn apply_patch_candidates_rejects_an_unknown_control_line() {
        assert!(
            apply_patch_candidates("*** Begin Patch\n*** Rename File: x\n*** End Patch").is_err()
        );
    }

    #[test]
    fn apply_patch_candidates_never_treats_a_diff_body_line_as_a_path() {
        let candidates = apply_patch_candidates(
            "*** Begin Patch\n*** Add File: x.rs\n@@\n+use .bare::config;\n*** End Patch",
        )
        .unwrap();
        assert_eq!(candidates, vec!["x.rs".to_string()]);
    }

    #[test]
    fn bash_candidates_strips_glued_redirection_operators() {
        assert_eq!(
            bash_candidates("echo hi >.bare/config"),
            vec!["echo", "hi", ".bare/config"]
        );
    }

    #[test]
    fn bash_candidates_strips_a_glued_file_descriptor_redirection() {
        assert_eq!(
            bash_candidates("tool 1>../.bare/config"),
            vec!["tool", "../.bare/config"]
        );
        assert_eq!(
            bash_candidates("tool 2>>../.bare/config"),
            vec!["tool", "../.bare/config"]
        );
        assert_eq!(
            bash_candidates("tool 0<../.bare/config"),
            vec!["tool", "../.bare/config"]
        );
    }

    #[test]
    fn decide_denies_a_bash_command_reaching_bare_through_an_fd_redirection() {
        let (root, canonical_bare, canonical_git) = grove();
        let payload = NormalizedPayload {
            tool: Tool::Bash {
                command: "tool 1>../.bare/config".to_string(),
            },
            cwd: Some(root.path().join("main")),
        };
        std::fs::create_dir(root.path().join("main")).unwrap();
        let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
        assert!(matches!(verdict, Verdict::Deny(_)));
    }

    #[test]
    fn bash_candidates_also_yields_the_value_half_of_a_glued_option() {
        assert_eq!(
            bash_candidates("tool --output=../.bare/config"),
            vec!["tool", "--output=../.bare/config", "../.bare/config"]
        );
    }

    #[test]
    fn decide_denies_a_bash_command_reaching_bare_through_a_glued_option_value() {
        let (root, canonical_bare, canonical_git) = grove();
        let payload = NormalizedPayload {
            tool: Tool::Bash {
                command: "tool --output=../.bare/config".to_string(),
            },
            cwd: Some(root.path().join("main")),
        };
        std::fs::create_dir(root.path().join("main")).unwrap();
        let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
        assert!(matches!(verdict, Verdict::Deny(_)));
    }
}
