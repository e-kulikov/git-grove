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

/// The subset of `SHELL_OPERATORS` that starts a fresh command, per Bash
/// grammar — the token right after one of these is a command word. The
/// redirection operators are not command separators: `cmd > file` still
/// has `file` as an argument of `cmd`, not a new command.
const COMMAND_SEPARATORS: &[&str] = &["&&", "||", ";", "|", "&"];

/// Command words `unsafe_bash_command_word` refuses outright wherever one
/// appears as a command's first word: each one invalidates the
/// single-fixed-cwd model `decide` resolves every other candidate
/// against. `cd`/`pushd`/`popd` change the directory a later command in
/// the same compound runs in, which nothing here tracks; `eval`/`exec` can
/// run an arbitrary computed string as a command in its own right, which
/// nothing here can see into.
const UNSAFE_COMMAND_WORDS: &[&str] = &["cd", "pushd", "popd", "eval", "exec"];

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

/// Whether `command` contains a construct that makes resolving every path
/// candidate against one fixed, unchanging cwd unsound to reason about
/// statically, and if so, a short human-readable description of which one
/// — for the denial message, not for further parsing. `decide` denies the
/// whole command outright when this returns `Some`, without attempting to
/// resolve any of its path candidates: correctly tracking a `cd`'s effect
/// on later path resolution, or looking inside a subshell, command
/// substitution, or `eval`'d string, is an unbounded problem (the next
/// bypass is always one more construct away), so this closes the class by
/// refusing to reason past the point where reasoning would have to start
/// guessing, rather than chasing individual bypass patterns one at a time.
///
/// Checks two independent things, in order: [`unsafe_bash_character`]
/// (unquoted syntax that changes what a later token means or lets Bash run
/// a computed string) and [`unsafe_bash_command_word`] (a command word
/// that changes the cwd, or hands off execution, for a *later* command in
/// the same compound).
fn unsafe_bash_construct(command: &str) -> Option<String> {
    unsafe_bash_character(command).or_else(|| unsafe_bash_command_word(command))
}

/// Scan `command` character by character, tracking the same quote state
/// [`shell_tokens`] tracks (deliberately re-derived here, not shared,
/// since this check must not itself decide where tokens split — see
/// [`unsafe_bash_construct`]), for a character that is live Bash syntax
/// wherever it is found unquoted or inside a double-quoted string —
/// only single quotes fully neutralize `$`, a backtick, and (for command
/// substitution) parentheses in Bash:
///
/// - `(` or `)` outside any quoting: a subshell, or the parenthesis half
///   of `$(...)`/`<(...)`/`>(...)` — flagging the bare parenthesis covers
///   all of command substitution, process substitution, and a subshell
///   without needing to separately recognize each spelling.
/// - `` ` `` anywhere not inside single quotes: backtick command
///   substitution, live even inside double quotes.
/// - `$` anywhere not inside single quotes: `$(...)` command substitution
///   or a `$NAME`/`${NAME}` variable expansion, live even inside double
///   quotes. Flagged globally rather than only "inside a path-looking
///   token": `bash_candidates` already treats nearly every non-operator
///   token as a path candidate, so scoping this check narrower would not
///   meaningfully reduce false positives while adding a second, easy-to-get-
///   wrong notion of "looks like a path" for a bypass to hide in the gap
///   of.
///
/// A character immediately after an unquoted backslash is consumed as a
/// literal escaped character by the same rule [`shell_tokens`] uses, and
/// is never checked here — `\$file` is a literal dollar sign, not live
/// syntax, exactly because escaping it is what makes it one.
///
/// An unterminated quote at the end of `command` is flagged too: whatever
/// [`shell_tokens`] made of a command whose own quoting is unbalanced is
/// not something this scan's quote-state tracking agrees with either, so
/// nothing tokenized under it can be trusted.
fn unsafe_bash_character(command: &str) -> Option<String> {
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = command.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '\\' if !in_single => {
                chars.next();
            }
            '(' | ')' if !in_single && !in_double => {
                return Some(format!(
                    "an unquoted `{character}` (a subshell, or command/process substitution)"
                ));
            }
            '`' if !in_single => {
                return Some("a backtick command substitution".to_string());
            }
            '$' if !in_single => {
                return Some("a `$` variable expansion or command substitution".to_string());
            }
            _ => {}
        }
    }
    if in_single || in_double {
        return Some("an unterminated quote".to_string());
    }
    None
}

/// Scan `command`'s tokens (via the unmodified [`shell_tokens`]) for a
/// command word — the first token, or the token right after one of
/// [`COMMAND_SEPARATORS`] — that names one of [`UNSAFE_COMMAND_WORDS`]. A
/// redirection operator (`>`, `<`, …) is not a command separator, so the
/// token after one is still an argument, not a fresh command word:
/// `printf x > cd` does not flag `cd`.
fn unsafe_bash_command_word(command: &str) -> Option<String> {
    let mut command_word_next = true;
    for token in shell_tokens(command) {
        if SHELL_OPERATORS.contains(&token.as_str()) {
            command_word_next = COMMAND_SEPARATORS.contains(&token.as_str());
            continue;
        }
        if command_word_next && UNSAFE_COMMAND_WORDS.contains(&token.as_str()) {
            return Some(format!("`{token}` as a command word"));
        }
        command_word_next = false;
    }
    None
}

/// Find the earliest unquoted redirection operator (`>`, `>>`, `<`, `<<`)
/// inside `token`, returning its byte offset and length. `>`/`<` are ASCII,
/// so a byte-index split on them can never land inside a multi-byte UTF-8
/// sequence.
fn find_redirection(token: &str) -> Option<(usize, usize)> {
    let bytes = token.as_bytes();
    for (index, &byte) in bytes.iter().enumerate() {
        if byte == b'>' || byte == b'<' {
            let doubled = bytes.get(index + 1) == Some(&byte);
            return Some((index, if doubled { 2 } else { 1 }));
        }
    }
    None
}

/// Split one shell token on every redirection operator it contains,
/// wherever it appears — not only at the token's start. Bash glues a
/// redirection onto a preceding word with no separating space just as
/// readily as it glues one onto a following path (`echo x>../.bare/config`
/// tokenizes as the single token `x>../.bare/config`, not `x`, `>`,
/// `../.bare/config`), so a start-anchored strip alone leaves that whole
/// token as one unrecognized, non-matching path candidate — a real bypass
/// of this feature's protection. The word immediately before an operator is
/// dropped, not kept as a candidate, when it is entirely ASCII digits: bash
/// treats a bare numeric word glued to a redirection as the file descriptor
/// (`1>file`, `2>>file`, `0<file`), never as a word of its own.
fn split_redirections(token: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut rest = token;
    while let Some((offset, length)) = find_redirection(rest) {
        let before = &rest[..offset];
        if !before.is_empty() && !before.bytes().all(|byte| byte.is_ascii_digit()) {
            parts.push(before.to_string());
        }
        rest = &rest[offset + length..];
    }
    if !rest.is_empty() {
        parts.push(rest.to_string());
    }
    parts
}

/// Conservative, best-effort path-candidate extraction for a Bash command:
/// every token that is not a recognized shell operator, split on every
/// redirection operator it contains (see [`split_redirections`]). A part
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
        for part in split_redirections(&token) {
            let value = if part.starts_with('-') {
                part.split_once('=')
                    .map(|(_, value)| value.to_string())
                    .filter(|value| !value.is_empty())
            } else {
                None
            };
            candidates.push(part);
            if let Some(value) = value {
                candidates.push(value);
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
        Tool::Bash { command } => match unsafe_bash_construct(command) {
            Some(reason) => {
                return Verdict::Deny(format!(
                    "grove invariant: this command contains {reason}, whose target path \
                     cannot be safely verified; split this into separate tool calls"
                ))
            }
            None => bash_candidates(command),
        },
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
    fn bash_candidates_splits_a_redirection_glued_to_a_preceding_word() {
        assert_eq!(
            bash_candidates("echo x>../.bare/config"),
            vec!["echo", "x", "../.bare/config"]
        );
    }

    #[test]
    fn decide_denies_a_bash_command_reaching_bare_through_a_redirection_glued_to_a_word() {
        let (root, canonical_bare, canonical_git) = grove();
        let payload = NormalizedPayload {
            tool: Tool::Bash {
                command: "echo x>../.bare/config".to_string(),
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

    fn denies_with_reason(verdict: &Verdict, needle: &str) -> bool {
        matches!(verdict, Verdict::Deny(message) if message.contains(needle))
    }

    #[test]
    fn decide_denies_the_reported_cd_bypass_because_it_refuses_to_reason_not_because_it_resolves() {
        // From a worktree cwd, this reaches the real .bare only once bash
        // actually executes `cd ..` first -- resolving the redirect target
        // against the stale, pre-cd cwd alone (the old behavior) would
        // wrongly see it as a harmless, nonexistent sibling path and allow
        // it. The fix is to refuse the whole command because `cd` breaks
        // the single-fixed-cwd model, not because this particular target
        // happens to resolve into `.bare`.
        let (root, canonical_bare, canonical_git) = grove();
        std::fs::create_dir(root.path().join("main")).unwrap();
        let payload = NormalizedPayload {
            tool: Tool::Bash {
                command: "cd .. && printf x > .bare/config".to_string(),
            },
            cwd: Some(root.path().join("main")),
        };
        let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
        assert!(denies_with_reason(&verdict, "cd"), "{verdict:?}");
    }

    #[test]
    fn decide_denies_a_subshell_wrapping_the_cd_bypass() {
        let (root, canonical_bare, canonical_git) = grove();
        std::fs::create_dir(root.path().join("main")).unwrap();
        let payload = NormalizedPayload {
            tool: Tool::Bash {
                command: "(cd .. && printf x > .bare/config)".to_string(),
            },
            cwd: Some(root.path().join("main")),
        };
        let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
        assert!(matches!(verdict, Verdict::Deny(_)));
    }

    #[test]
    fn decide_denies_command_substitution_used_as_a_redirect_target() {
        let (root, canonical_bare, canonical_git) = grove();
        std::fs::create_dir(root.path().join("main")).unwrap();
        let payload = NormalizedPayload {
            tool: Tool::Bash {
                command: r#"printf x > "$(echo ../.bare/config)""#.to_string(),
            },
            cwd: Some(root.path().join("main")),
        };
        let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
        assert!(matches!(verdict, Verdict::Deny(_)));
    }

    #[test]
    fn decide_denies_eval_of_a_computed_string() {
        let (root, canonical_bare, canonical_git) = grove();
        std::fs::create_dir(root.path().join("main")).unwrap();
        let payload = NormalizedPayload {
            tool: Tool::Bash {
                command: r#"eval "printf x > ../.bare/config""#.to_string(),
            },
            cwd: Some(root.path().join("main")),
        };
        let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
        assert!(denies_with_reason(&verdict, "eval"), "{verdict:?}");
    }

    #[test]
    fn decide_allows_a_compound_but_safe_command_with_no_cwd_changing_or_indirection() {
        // `&&` alone does not make path resolution unsound -- only a
        // construct that actually changes the cwd or hides the target
        // behind computation does. Plain sequential simple commands must
        // not be over-denied.
        let (root, canonical_bare, canonical_git) = grove();
        let payload = NormalizedPayload {
            tool: Tool::Bash {
                command: "mkdir foo && touch foo/bar".to_string(),
            },
            cwd: Some(root.path().join("main")),
        };
        std::fs::create_dir(root.path().join("main")).unwrap();
        let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
        assert_eq!(verdict, Verdict::Allow, "{verdict:?}");
    }

    #[test]
    fn unsafe_bash_construct_flags_each_named_construct() {
        for (command, needle) in [
            ("cd ..", "cd"),
            ("pushd ..", "pushd"),
            ("popd", "popd"),
            (r#"eval "cat file""#, "eval"),
            ("exec sh", "exec"),
            ("(echo hi)", "subshell"),
            ("echo )", "subshell"),
            ("echo $(echo hi)", "command substitution"),
            ("echo `echo hi`", "backtick"),
            ("echo $HOME", "variable"),
            ("echo ${HOME}", "variable"),
            ("cat <(echo hi)", "subshell"),
            ("tee >(cat)", "subshell"),
            ("echo 'unterminated", "unterminated quote"),
        ] {
            let reason = unsafe_bash_construct(command);
            assert!(reason.is_some(), "expected a deny reason for {command:?}");
            assert!(
                reason.as_deref().unwrap().contains(needle),
                "command {command:?} gave reason {reason:?}, expected it to mention {needle:?}"
            );
        }
    }

    #[test]
    fn unsafe_bash_construct_allows_ordinary_commands() {
        for command in [
            "git status",
            "mkdir foo && touch foo/bar",
            "echo 'literal $HOME and (parens) are safe in single quotes'",
            r#"echo "a literal (paren) with no dollar sign is safe in double quotes""#,
            "printf x > .bare/config",
            "echo \\$HOME",
            "cat .bare/config; echo done",
        ] {
            assert_eq!(
                unsafe_bash_construct(command),
                None,
                "expected {command:?} to be allowed"
            );
        }
    }

    #[test]
    fn unsafe_bash_command_word_ignores_a_redirection_target_that_looks_like_a_command_word() {
        // A redirection operator is not a command separator: the word
        // after `>` is still an argument, not a fresh command word.
        assert_eq!(unsafe_bash_command_word("printf x > cd"), None);
    }
}
