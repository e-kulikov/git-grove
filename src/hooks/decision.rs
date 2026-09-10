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

/// Command words `unsafe_bash_command_word` refuses outright wherever one
/// appears as a command's first word: each one invalidates the
/// single-fixed-cwd model `decide` resolves every other candidate
/// against, or wraps/dispatches a *nested* command word this scan does not
/// look inside of. `cd`/`pushd`/`popd` change the directory a later
/// command in the same compound runs in, which nothing here tracks;
/// `eval`/`exec`/`command`/`builtin`/`time`/`coproc` can all run another,
/// arbitrary command word in their own right, which nothing here can see
/// into; `if`/`while`/`until`/`for`/`case`/`select`/`function`/`{`/`!`
/// each start compound-command or negated-pipeline grammar whose body can
/// run `cd` (or any of the above) without ever appearing as this simple
/// command's own first word (`if cd ..; then :; fi`, `! cd ..`) — denying
/// the reserved word itself, rather than trying to look inside the
/// compound command it introduces, is the same fail-closed choice this
/// whole scan makes everywhere else. `trap` registers a string to be run
/// later, on a signal or a debug/exit event, exactly as unseen as
/// `eval`'s string; `source`/`.` read and run an entire file's contents as
/// shell commands, which can contain any of the above just as easily as
/// the top-level command line can. `alias`/`shopt` can redefine what a
/// later, ordinary-looking word in the *same* command means (`shopt -s
/// expand_aliases; alias up="cd .."; up` really does change the cwd,
/// non-interactively, once both are set in the same script this scan
/// already sees) — refusing either as a command word closes that without
/// having to simulate alias expansion.
///
/// Deliberately does *not* extend to enumerating every external program by
/// name that could itself interpret an embedded command string
/// (`bash -c '...'`, `python -c '...'`, and so on indefinitely) — recognizing
/// every program on the system that offers a "run this string" flag is the
/// unbounded, non-terminating version of exactly the problem denying by
/// *construct* was chosen to close instead of chasing. That class is
/// closed a different way instead: [`unsafe_bash_construct`] recurses into
/// every quoted token's content as its own nested command line, so a
/// `cd`/`eval`/subshell/substitution hidden inside a quoted argument is
/// still caught regardless of which program the string was headed for,
/// without this scan ever needing to know its name. A directory-changing
/// *flag* on an ordinary program (`env -C dir cmd`, `git -C dir cmd`) is
/// narrower still — see [`unsafe_bash_directory_flag`] for what is and is
/// not covered there, and why.
const UNSAFE_COMMAND_WORDS: &[&str] = &[
    "cd", "pushd", "popd", "eval", "exec", "command", "builtin", "time", "coproc", "if", "while",
    "until", "for", "case", "select", "function", "{", "!", "trap", "source", ".", "alias",
    "shopt",
];

/// Minimal, intentionally forgiving shell tokenizer: honors single/double
/// quoting and backslash escapes, splits on unquoted whitespace. It is not
/// a POSIX shell parser and is not meant to be one — see `bash_candidates`.
fn shell_tokens(command: &str) -> Vec<String> {
    shell_tokens_scanned(command)
        .into_iter()
        .map(|token| token.text)
        .collect()
}

/// One token as [`shell_tokens`] produces it, alongside whether any part of
/// its content came from inside a quoted region (single or double) — see
/// [`bash_candidates`] for why that matters.
struct ScannedToken {
    text: String,
    quoted: bool,
}

/// The tokenizer [`shell_tokens`] exposes the plain text of. Kept as its
/// own function, rather than folded directly into `shell_tokens`, so every
/// other caller of `shell_tokens` (the command-word scan, its own tests)
/// keeps working against `Vec<String>` unchanged; only [`bash_candidates`]
/// needs the extra quote-provenance bit.
fn shell_tokens_scanned(command: &str) -> Vec<ScannedToken> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut current_quoted = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = command.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\'' if !in_double => {
                in_single = !in_single;
                current_quoted = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                current_quoted = true;
            }
            // A backslash-newline is a Bash line continuation: both
            // characters vanish, joining the following line onto this one
            // with nothing inserted between them (`c\<newline>d` becomes
            // the single word `cd`, not two words or a word containing a
            // literal newline) — distinct from every other backslash
            // escape, which keeps the escaped character as a literal.
            '\\' if !in_single && chars.peek() == Some(&'\n') => {
                chars.next();
            }
            '\\' if !in_single => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            character if character.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    tokens.push(ScannedToken {
                        text: std::mem::take(&mut current),
                        quoted: std::mem::take(&mut current_quoted),
                    });
                }
            }
            character => current.push(character),
        }
    }
    if !current.is_empty() {
        tokens.push(ScannedToken {
            text: current,
            quoted: current_quoted,
        });
    }
    tokens
}

/// The subset of [`UNSAFE_COMMAND_WORDS`] specific enough, as an exact
/// standalone token, to be real evidence of shell code rather than
/// ordinary English prose — unlike most of that list. `cd`, `if`, `while`,
/// `until`, `for`, `case`, `select`, `function`, `time`, `command`,
/// `source`, `trap`, `alias`, `!`, `.`, and `{` are all common ordinary
/// words or punctuation (`"cd into src before building"`, `"save time"`,
/// `"the source of truth"`, an exclamation mark ending any sentence, a
/// lone `.` ending one) that would make [`looks_like_shell_code`]
/// false-positive on completely ordinary quoted text constantly if used
/// as evidence the same way — an earlier version of this list did exactly
/// that, and was caught by this file's own test suite before it shipped.
/// This subset is what remains once every word plausible as ordinary
/// prose is excluded: shell/dispatch jargon that essentially never
/// appears as a bare, standalone lowercase word outside of actual shell
/// code.
const SHELL_CODE_EVIDENCE_WORDS: &[&str] = &[
    "exec", "eval", "pushd", "popd", "coproc", "shopt", "builtin",
];

/// Whether `text` shows evidence, once its own quoting is considered, of
/// being intended as a shell command rather than ordinary quoted string
/// data. [`unsafe_bash_construct`]'s recursion into quoted content uses
/// this to decide whether a quoted token is worth recursing into at all;
/// each signal below is specific enough that ordinary free-text data (a
/// commit message, a grep pattern, an echoed string) essentially never
/// produces it, while a string actually meant to run as shell code (most
/// commonly an interpreter's own `-c`/`-e` argument) very often does:
///
/// - An unquoted Bash separator (`;`, `&`, `|`, or a newline — see
///   [`split_bash_segments`]) or redirection character (`>`/`<`, bare or
///   glued onto a word — see [`find_redirection`]) appearing anywhere in
///   it: chaining or redirecting is the entire reason to hand a
///   multi-word shell command to another program in the first place. This
///   alone already covers a hidden `cd` combined with anything
///   consequential (`cd ..; printf x > file`) — `cd` on its own, with no
///   separator or redirect anywhere in the same quoted string, has no
///   observable effect once the interpreter it was handed to exits, so
///   `cd` itself is deliberately *not* also evidence on its own; see
///   [`SHELL_CODE_EVIDENCE_WORDS`] for why it (and most of
///   [`UNSAFE_COMMAND_WORDS`]) is excluded from the next signal too.
/// - One of [`SHELL_CODE_EVIDENCE_WORDS`] or [`UNSAFE_DIRECTORY_FLAGS`]
///   appearing as its own whole token anywhere in it: a single dispatch
///   word like `exec` with no separator around it at all (`exec env
///   --chdir .. touch .bare/config`) still needs to trigger recursion for
///   `unsafe_bash_command_word`/`unsafe_bash_directory_flag` to ever see
///   it, and unlike the excluded majority of `UNSAFE_COMMAND_WORDS`,
///   this narrower vocabulary is not plausible as ordinary prose.
///
/// Deliberately does *not* also trigger on the presence of `(`, `)`, `` ` ``,
/// or `$` alone: those are common in ordinary prose and data with no shell
/// meaning at all (a parenthetical aside, a literal dollar amount, a grep
/// pattern with a literal `(group)`), and recursing on their presence
/// alone reintroduces exactly the false-positive regression an earlier
/// version of this function caused — seeing them just means Bash would not
/// treat this content specially were it unquoted, not that some other
/// language given the same text wouldn't. This leaves one acknowledged,
/// not-yet-closed gap: a quoted payload written in a *different*
/// language's syntax that both avoids every signal above and still
/// resolves a relative path against a directory it changed to itself
/// (most concretely, a `python3 -c` argument using `os.chdir`/`open(...)`
/// with no semicolon-separated statements and no call to a flagged
/// command) is not recognized as code by this function and is not
/// recursed into. Reliably distinguishing "this quoted string is
/// executable code" from "this quoted string is data that happens to
/// contain code-shaped punctuation" for an unbounded set of possible
/// target languages is the same kind of open-ended problem denying by
/// enumerated construct was chosen to avoid chasing in the first place;
/// see the design note on [`UNSAFE_COMMAND_WORDS`].
fn looks_like_shell_code(text: &str) -> bool {
    split_bash_segments(text).len() > 1
        || shell_tokens(text).iter().any(|token| {
            find_redirection(token).is_some()
                || SHELL_CODE_EVIDENCE_WORDS.contains(&token.as_str())
                || UNSAFE_DIRECTORY_FLAGS.contains(&token.as_str())
        })
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
    if let Some(reason) = unsafe_bash_character(command) {
        return Some(reason);
    }
    if let Some(reason) = unsafe_bash_command_word(command) {
        return Some(reason);
    }
    if let Some(reason) = unsafe_bash_directory_flag(command) {
        return Some(reason);
    }
    // Bash keeps a quoted region as one word — including, inside single
    // quotes, one that itself contains further `"`-quoting, `$(...)`,
    // backticks, or a `cd`/`eval`/… command word, none of which mean
    // anything to *this* shell while still inside the outer quoting. But
    // many programs turn straight around and parse that word as a command
    // line of their own — most commonly an interpreter's own `-c`/`-e`
    // flag. Scanning a quoted token's content with exactly this same
    // function, recursively, closes that class without needing to know
    // which program is on the receiving end: a quote nested inside a
    // quote is unwound the same way, one recursive call at a time, and
    // recursion terminates because each nested string is strictly shorter
    // than the one that contained it.
    //
    // Gated on `looks_like_shell_code`, not run unconditionally over every
    // quoted token: ordinary quoted *data* -- a commit message, a grep
    // pattern, an echoed string -- routinely contains `$`, `(`, or the bare
    // word `cd` with no shell meaning whatsoever (`git commit -m "cd into
    // src before building"`), and recursing into it unconditionally would
    // deny that as readily as it denies actual embedded shell code. See
    // `looks_like_shell_code` for the narrower, operator-based evidence
    // this scan requires before treating quoted content as code instead of
    // data.
    for token in shell_tokens_scanned(command) {
        if !token.quoted {
            continue;
        }
        // `unsafe_bash_directory_flag` alone always recurses into quoted
        // content, unlike the rest of this recursion below: it only
        // denies on an exact match against a small, specific vocabulary
        // (`--chdir`/`--directory`, or `-C` on one of four well-known
        // programs), so it carries essentially none of the false-positive
        // risk `looks_like_shell_code` exists to gate the broader
        // character/command-word recursion against.
        if let Some(reason) = unsafe_bash_directory_flag(&token.text) {
            return Some(reason);
        }
        if looks_like_shell_code(&token.text) {
            if let Some(reason) = unsafe_bash_construct(&token.text) {
                return Some(reason);
            }
        }
    }
    None
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

/// GNU-convention flags, shared by name across several common external
/// programs (`env --chdir dir cmd`, `git --directory dir cmd`), that
/// change the effective working directory a later argument or the
/// program's own child process resolves paths against — the same
/// invalidation of the single-fixed-cwd model a `cd` command word causes,
/// just spelled as an ordinary argument instead of a shell builtin.
/// Checked anywhere in the command, not only in a command-word position,
/// since a flag like this can appear after any command name.
///
/// Deliberately only these unambiguous long forms at the top level — see
/// [`DIRECTORY_CHANGING_SHORT_C_FLAG_PROGRAMS`] for how the collision-prone
/// short form (`-C`) is instead scoped to the specific programs where it
/// unambiguously means this, rather than either enumerated here (denying
/// `grep -C`/`diff -C`'s unrelated context-line count everywhere) or
/// ignored entirely.
const UNSAFE_DIRECTORY_FLAGS: &[&str] = &["--directory", "--chdir"];

/// External programs, by long-standing Unix convention, whose `-C`
/// argument unambiguously means "change directory to the following value
/// before doing anything else" — the exact same effect as
/// [`UNSAFE_DIRECTORY_FLAGS`]'s long forms, just spelled with the short
/// form that collides with `grep -C`/`diff -C`'s unrelated context-line
/// count everywhere else. Scoping the check to only these specific command
/// words (rather than a blanket `-C` ban) removes that collision entirely,
/// since neither `grep` nor `diff` appears here.
const DIRECTORY_CHANGING_SHORT_C_FLAG_PROGRAMS: &[&str] = &["env", "git", "make", "tar"];

/// The command word that names a program in
/// [`DIRECTORY_CHANGING_SHORT_C_FLAG_PROGRAMS`], whether spelled bare
/// (`env`) or via an absolute or relative path to it (`/usr/bin/env`,
/// `./env`) — matched on the final path component, the same way a shell
/// itself resolves which program a path invokes regardless of where it
/// lives.
fn names_directory_changing_program(command_word: &str) -> bool {
    let name = Path::new(command_word)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command_word);
    DIRECTORY_CHANGING_SHORT_C_FLAG_PROGRAMS.contains(&name)
}

/// Whether `token` is `-C` itself, or `-C` with its value glued directly
/// onto it (`-C..`, `-Cdir`) — the same short-option-with-glued-value
/// convention `env`/`git`/`make`/`tar` (and getopt-style parsing
/// generally) all accept as equivalent to a separate following word.
fn is_short_c_flag(token: &str) -> bool {
    token == "-C" || (token.starts_with("-C") && token.len() > "-C".len())
}

/// Whether `command` contains one of [`UNSAFE_DIRECTORY_FLAGS`] (bare or
/// with a glued `=value`) anywhere among its plain tokens, or a `-C`
/// (bare or with its value glued on — see [`is_short_c_flag`]) among the
/// arguments of a simple command whose own command word names one of
/// [`DIRECTORY_CHANGING_SHORT_C_FLAG_PROGRAMS`] (see
/// [`names_directory_changing_program`] for how an absolute or relative
/// path to that program is matched too).
fn unsafe_bash_directory_flag(command: &str) -> Option<String> {
    for token in shell_tokens(command) {
        let name = token
            .split_once('=')
            .map_or(token.as_str(), |(name, _)| name);
        if UNSAFE_DIRECTORY_FLAGS.contains(&name) {
            return Some(format!("a directory-changing flag (`{token}`)"));
        }
    }
    for segment in split_bash_segments(command) {
        let tokens = shell_tokens(&segment);
        let Some(command_word_index) = command_word_index_in_segment(&tokens) else {
            continue;
        };
        let command_word = tokens[command_word_index].as_str();
        if names_directory_changing_program(command_word)
            && tokens[command_word_index + 1..]
                .iter()
                .any(|token| is_short_c_flag(token))
        {
            return Some(format!(
                "a directory-changing `-C` flag on `{command_word}`"
            ));
        }
    }
    None
}

/// Find the command word of one simple command (a [`split_bash_segments`]
/// slice, already tokenized), returning its index into `tokens` — skipping
/// past assignment words and prefix redirections exactly like
/// [`unsafe_command_word_in_segment`] does, since this helper answers the
/// same question ("which token is the real command word") for
/// [`unsafe_bash_directory_flag`]'s narrower, per-program `-C` check.
/// `None` if the segment has no tokens, or its first non-prefix token is
/// an unrecognized assignment/redirection prefix (see
/// [`looks_like_unrecognized_prefix`]) — that case is already denied by
/// `unsafe_command_word_in_segment` separately, so this helper's callers
/// can simply skip the segment rather than duplicate that denial.
fn command_word_index_in_segment(tokens: &[String]) -> Option<usize> {
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        if is_assignment_word(token) {
            index += 1;
            continue;
        }
        if let Some(bare) = redirection_prefix(token) {
            index += 1;
            if bare {
                index += 1;
            }
            continue;
        }
        if looks_like_unrecognized_prefix(token) {
            return None;
        }
        return Some(index);
    }
    None
}

/// Find the command word of every simple command in `command` (splitting
/// on `;`/`&`/`|`/newline via [`split_bash_segments`], since `cd` after a
/// separator glued directly onto the previous word — `true;cd ..` — is
/// exactly as much a fresh command word as `cd` after one with spaces
/// around it) and check each against [`UNSAFE_COMMAND_WORDS`] — see
/// [`unsafe_command_word_in_segment`] for how a command word is found
/// within one simple command once assignment and prefix-redirection words
/// are skipped.
fn unsafe_bash_command_word(command: &str) -> Option<String> {
    split_bash_segments(command)
        .iter()
        .find_map(|segment| unsafe_command_word_in_segment(segment))
}

/// Split `command` into the substrings between every unquoted Bash command
/// separator — `;`, `&`, `|` (a bare `&`/`|` and their doubled forms
/// `&&`/`||` are both split on the same way, since finding every boundary
/// is all `unsafe_bash_command_word` needs; it does not need `&&`'s own
/// short-circuit semantics), or a literal newline. Unlike
/// [`shell_tokens`], this splits on these separators wherever they occur
/// — including glued directly onto adjacent text with no surrounding
/// whitespace (`true;cd ..`), which real Bash still treats as two
/// commands — because [`unsafe_command_word_in_segment`]'s reasoning about
/// where one simple command's word begins depends on getting that
/// boundary right independent of spacing. Quote characters are kept in
/// each segment (not stripped), since every segment is re-tokenized with
/// [`shell_tokens`] afterward, which needs them for its own quote
/// tracking.
fn split_bash_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = command.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(character);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(character);
            }
            '\\' if !in_single => {
                current.push(character);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            // `&`/`|` glued directly onto a preceding `>`/`<` (`>&`, `<&`,
            // `>|`) or `&` immediately followed by a `>` at the start of a
            // fresh word (`&>`, `&>>`) is part of a redirect operator
            // token, not a command separator — splitting here would hand
            // `unsafe_command_word_in_segment` a truncated token
            // (`>`/`<` alone) it would then wrongly treat as fully
            // recognized, exactly defeating the fail-closed check that
            // token was supposed to trigger.
            '&' | '|'
                if !in_single
                    && !in_double
                    && (current.ends_with(['>', '<'])
                        || (character == '&'
                            && current.is_empty()
                            && chars.peek() == Some(&'>'))) =>
            {
                current.push(character);
            }
            ';' | '&' | '|' | '\n' if !in_single && !in_double => {
                segments.push(std::mem::take(&mut current));
            }
            character => current.push(character),
        }
    }
    segments.push(current);
    segments
}

/// Find the command word of one simple command (a `split_bash_segments`
/// slice with no unquoted `;`/`&`/`|`/newline left in it) and check it
/// against [`UNSAFE_COMMAND_WORDS`], skipping past every leading
/// assignment word (`NAME=value`, Bash allows any number before the real
/// command: `X=1 Y=2 cd ..`) and prefix redirection (`>out cd ..`, `2>&1
/// cd ..`) first — neither is the command word itself. Stops at (and
/// returns `None` for) the first token that is neither, since that is the
/// real command word and this function only needs to know whether *that
/// one* is unsafe: a redirection or filename appearing *after* the real
/// command word (`printf x > cd`) is an argument, not a fresh command.
///
/// A token that merely *looks* like an assignment or a redirection but
/// does not match [`is_assignment_word`]/[`redirection_prefix`] exactly
/// (`A+=x`, `a[0]=x`, a herestring `<<<`, `>&`, `<>`, `>|`, …) is denied
/// outright rather than treated as the real command word: falling through
/// to "it must be the command word, and it's not on the unsafe list" for a
/// token this scan does not actually understand is exactly the kind of
/// silent, ungrounded assumption the fail-closed design exists to refuse.
/// See [`looks_like_unrecognized_prefix`].
fn unsafe_command_word_in_segment(segment: &str) -> Option<String> {
    let tokens = shell_tokens(segment);
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        if is_assignment_word(token) {
            index += 1;
            continue;
        }
        if let Some(bare) = redirection_prefix(token) {
            index += 1;
            if bare {
                // A bare operator with no target glued on (`>`, `2>>`)
                // takes the *next* word as its target; that word is not
                // the command word either.
                index += 1;
            }
            continue;
        }
        if looks_like_unrecognized_prefix(token) {
            return Some(format!(
                "an assignment or redirection prefix (`{token}`) whose grammar is not fully recognized"
            ));
        }
        return UNSAFE_COMMAND_WORDS
            .contains(&token)
            .then(|| format!("`{token}` as a command word"));
    }
    None
}

/// Whether `token` is a Bash assignment word (`NAME=value`) that can
/// legitimately precede the real command word — `NAME` a valid shell
/// identifier: starts with a letter or underscore, and every character is
/// alphanumeric or an underscore.
fn is_assignment_word(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name.starts_with(|character: char| character.is_ascii_alphabetic() || character == '_')
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Whether `token` is a redirection operator this scan fully understands,
/// that can legitimately precede the real command word (`>out cd ..`,
/// `2>&1 cd ..`) — the same digit-prefix-then-operator shape
/// [`split_redirections`] strips a redirection off of, checked instead at
/// the *start* of the whole token, and restricted to exactly `<`, `<<`,
/// `>`, or `>>`. `None` if `token` does not match this precisely: neither
/// "not a redirection at all" nor "some other redirection form this scan
/// does not fully understand" (a herestring `<<<`, `>&`, `<>`, `>|`, …)
/// can safely be treated the same as "definitely not one" here — see
/// [`looks_like_unrecognized_prefix`], which the caller checks next to
/// tell those two apart. `Some(true)` if the matched operator is *bare*,
/// with no target glued onto it (`>`, `2>>`), meaning the following word
/// is its target and must be skipped too; `Some(false)` if the target is
/// already glued on (`>out`, `2>>log`) — glued text that itself starts
/// with another redirect-special character (`>&`'s `&`, a herestring's
/// third `<`) does not count as an ordinary glued target and falls
/// through to `None` instead, precisely because that shape is one of the
/// forms this function does not fully understand.
fn redirection_prefix(token: &str) -> Option<bool> {
    let digit_end = token
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(token.len());
    let rest = &token[digit_end..];
    for operator in [">>", "<<", ">", "<"] {
        if let Some(target) = rest.strip_prefix(operator) {
            if target.is_empty() {
                return Some(true);
            }
            return if target.starts_with(['<', '>', '&', '|']) {
                None
            } else {
                Some(false)
            };
        }
    }
    None
}

/// Whether `token` looks like it is attempting to be an assignment or
/// redirection prefix — starts (after an optional digit file-descriptor
/// prefix) with `<` or `>`, or contains an unquoted `=` that is not its
/// first character — without matching [`is_assignment_word`] or
/// [`redirection_prefix`] exactly. Bash has more assignment and
/// redirection grammar than those two functions fully model (compound
/// assignment `NAME+=value`, array-element assignment `name[0]=value`, a
/// herestring `<<<`, combined-stream and clobber-override redirects `>&`/
/// `<>`/`>|`, …), and each of those can precede the real command word
/// exactly as legitimately as the forms that are recognized — the point of
/// this check is to fail closed on all of them at once, rather than
/// enumerating every spelling one at a time.
fn looks_like_unrecognized_prefix(token: &str) -> bool {
    let digit_end = token
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(token.len());
    if matches!(
        token.as_bytes()[digit_end..].first(),
        Some(b'<') | Some(b'>')
    ) {
        return true;
    }
    token
        .char_indices()
        .skip(1)
        .any(|(_, character)| character == '=')
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
///
/// A quoted token gets one more pass beyond the plain-token extraction
/// every token gets: `shell_tokens` strips quotes but does not re-split
/// unquoted-looking whitespace *inside* what was a quoted region, since
/// real Bash does not either (`"cd / && printf x > .bare/config"`
/// tokenizes as one word, not nine, quotes and all). That is exactly right
/// for running the command, but wrong for finding every path this scan
/// needs to see: a multi-word command hidden inside a quoted argument —
/// most commonly the string handed to an external interpreter's own
/// `-c`/`-e` flag, which this scan does not and cannot enumerate by name
/// (see the design note on [`UNSAFE_COMMAND_WORDS`]) — would otherwise
/// present as one opaque, never-matching blob. Splitting a quoted token's
/// content on whitespace too, and running each resulting word through the
/// same extraction, finds the path without needing to know anything about
/// what program the quoted string was headed for.
fn bash_candidates(command: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    for token in shell_tokens_scanned(command) {
        extract_candidates_from_word(&token.text, &mut candidates);
        if token.quoted {
            // Recurse, the same way `unsafe_bash_construct` does and for
            // the same reason: re-parsing the quoted content as its own
            // command line (rather than a flat whitespace split) resolves
            // any further nested quoting correctly too, so a path that
            // was itself inside a nested quote is found in its clean,
            // unquoted form instead of the literal quote characters
            // corrupting it into a nonexistent candidate.
            candidates.extend(bash_candidates(&token.text));
        }
    }
    candidates
}

/// The extraction one plain Bash word contributes to `candidates`: skip a
/// recognized shell operator outright, otherwise split on every glued
/// redirection operator (see [`split_redirections`]) and, for a part that
/// looks like a long or short option with its value glued on, also yield
/// the value half. Shared by [`bash_candidates`]'s per-token pass and its
/// recursive pass into a quoted token's own content, so the two never
/// drift in what counts as a candidate.
fn extract_candidates_from_word(word: &str, candidates: &mut Vec<String>) {
    if SHELL_OPERATORS.contains(&word) {
        return;
    }
    for part in split_redirections(word) {
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
///
/// A second, structural limit, acknowledged rather than papered over: this
/// guard statically analyzes the *Bash* command line the tool call names
/// — its own syntax, and a bounded, enumerated set of constructs
/// (`cd`/`pushd`/`popd`, `eval`/`exec` and the other dispatch/reserved
/// words in `UNSAFE_COMMAND_WORDS`, subshells, command/process
/// substitution, a variable inside a path-looking token, a handful of
/// directory-changing flags) that would make resolving a path against one
/// fixed cwd unsound — and denies whatever it cannot fully account for.
/// It cannot prove what an arbitrary *external interpreter*, invoked with
/// a computed-string argument in a language this guard does not parse
/// (`python3 -c '...'`, `perl -e '...'`, and so on for every interpreter
/// that accepts one), will actually do with that string once it runs:
/// recursion into quoted Bash content closes the cases expressible in
/// Bash's own grammar (see `unsafe_bash_construct`'s recursion and
/// `looks_like_shell_code`), but a payload written in a different
/// language's syntax that both avoids every Bash-shaped signal this scan
/// looks for and still reaches a protected path is not something static
/// analysis of the *outer* command can rule out — doing so would require
/// parsing an open-ended set of target languages, which is the same kind
/// of unbounded, non-terminating problem this whole guard's construct-based
/// design was chosen to avoid chasing in the first place, not one more
/// construct to enumerate. Genuine defense against that class of bypass
/// needs a fundamentally different mechanism — OS-level sandboxing
/// (Landlock, seccomp, or similar) enforced on the process actually
/// performing the write, independent of what command line asked for it —
/// which is out of scope for a hook that only ever sees the tool call's
/// own text before anything runs. This guard is still real, meaningful
/// protection against every construct it does close; it is not, and does
/// not claim to be, a substitute for kernel-level enforcement against an
/// arbitrary interpreter's own behavior.
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

    /// exec-reviewer's finding: the first version of this scan located a
    /// command word only via whitespace-delimited `shell_tokens`, so a
    /// separator glued directly onto adjacent text with no surrounding
    /// whitespace, a newline separator, a leading assignment word, or a
    /// leading prefix redirection all hid the real command word from it.
    /// Each of these four forms reaches the real `.bare` with a `cd ..`
    /// the old scan would have missed entirely.
    #[test]
    fn unsafe_bash_command_word_finds_cd_past_glued_separators_assignments_and_prefix_redirections()
    {
        for (command, label) in [
            ("true;cd ..;printf x>.bare/config", "glued semicolons"),
            ("true\ncd ..\nprintf x>.bare/config", "newline separator"),
            ("X=1 cd .. && printf x>.bare/config", "assignment prefix"),
            (
                ">harmless cd .. && printf x>.bare/config",
                "redirection prefix",
            ),
        ] {
            let reason = unsafe_bash_command_word(command);
            assert!(
                reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("cd")),
                "{label} ({command:?}) should have flagged `cd`, got {reason:?}"
            );
        }
    }

    #[test]
    fn decide_denies_each_glued_separator_assignment_and_prefix_redirection_bypass() {
        for command in [
            "true;cd ..;printf x>.bare/config",
            "true\ncd ..\nprintf x>.bare/config",
            "X=1 cd .. && printf x>.bare/config",
            ">harmless cd .. && printf x>.bare/config",
        ] {
            let (root, canonical_bare, canonical_git) = grove();
            std::fs::create_dir(root.path().join("main")).unwrap();
            let payload = NormalizedPayload {
                tool: Tool::Bash {
                    command: command.to_string(),
                },
                cwd: Some(root.path().join("main")),
            };
            let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
            assert!(
                matches!(verdict, Verdict::Deny(_)),
                "{command:?}: {verdict:?}"
            );
        }
    }

    #[test]
    fn unsafe_bash_command_word_skips_multiple_assignments_and_a_bare_redirect_operator() {
        // Several assignments in a row, and a *bare* redirect operator
        // (its target a separate word, not glued on) both precede the
        // real command word.
        assert!(unsafe_bash_command_word("X=1 Y=2 cd ..")
            .as_deref()
            .is_some_and(|reason| reason.contains("cd")));
        assert!(unsafe_bash_command_word("> harmless cd ..")
            .as_deref()
            .is_some_and(|reason| reason.contains("cd")));
    }

    #[test]
    fn unsafe_bash_command_word_still_allows_ordinary_assignments_and_prefix_redirections() {
        // An assignment or prefix redirection ahead of an ordinary,
        // harmless command word must not itself trigger a deny.
        assert_eq!(unsafe_bash_command_word("X=1 git status"), None);
        assert_eq!(unsafe_bash_command_word(">out.log git status"), None);
        assert_eq!(unsafe_bash_command_word("true; git status"), None);
    }

    /// exec-reviewer's second-round finding on the command-word scan: a
    /// dispatch wrapper (`command`/`builtin`), a reserved word introducing
    /// compound grammar (`if ... fi`), a compound-assignment prefix
    /// (`A+=x`), and four redirection forms this scan does not fully
    /// model (a herestring, and the combined-stream/clobber-override/
    /// read-write spellings) each still hide `cd` from the previous
    /// design. Each must now be denied -- the dispatch/reserved words by
    /// being on the unsafe list directly, the rest by
    /// `looks_like_unrecognized_prefix` refusing to guess.
    #[test]
    fn unsafe_bash_command_word_denies_dispatch_wrappers_reserved_words_and_unrecognized_prefixes()
    {
        for command in [
            "command cd ..",
            "builtin cd ..",
            "if cd ..; then :; fi",
            "A+=x cd ..",
            "<<< x cd ..",
            ">& 2 cd ..",
            "<> file cd ..",
            ">| file cd ..",
        ] {
            assert!(
                unsafe_bash_command_word(command).is_some(),
                "expected {command:?} to be denied"
            );
        }
    }

    #[test]
    fn decide_denies_each_dispatch_reserved_word_and_unrecognized_prefix_bypass() {
        for command in [
            "command cd ..",
            "builtin cd ..",
            "if cd ..; then :; fi",
            "A+=x cd ..",
        ] {
            let (root, canonical_bare, canonical_git) = grove();
            std::fs::create_dir(root.path().join("main")).unwrap();
            let payload = NormalizedPayload {
                tool: Tool::Bash {
                    command: command.to_string(),
                },
                cwd: Some(root.path().join("main")),
            };
            let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
            assert!(
                matches!(verdict, Verdict::Deny(_)),
                "{command:?}: {verdict:?}"
            );
        }
    }

    /// exec-reviewer's third-round finding: a backslash-newline line
    /// continuation joins `c\<newline>d` into the single word `cd` in real
    /// Bash, but the scan's tokenizer previously kept the escaped newline
    /// as a literal character inside the token instead of performing the
    /// join, producing a token that matched neither the safe case nor
    /// `UNSAFE_COMMAND_WORDS`. Separately, `!` (pipeline negation),
    /// `trap` (registers a string to run later, unseen, on a signal or
    /// debug/exit event), and `source`/`.` (run an entire file's contents
    /// as shell commands) are reserved/dispatch words the previous list
    /// did not cover.
    #[test]
    fn unsafe_bash_command_word_finds_cd_past_a_line_continuation_and_denies_new_reserved_words() {
        for (command, needle) in [
            ("c\\\nd ..", "cd"),
            ("! cd ..", "!"),
            ("trap 'cd /' DEBUG", "trap"),
            ("source ./script.sh", "source"),
            (". ./script.sh", "."),
        ] {
            let reason = unsafe_bash_command_word(command);
            assert!(
                reason.is_some(),
                "expected {command:?} to be denied, got None"
            );
            assert!(
                reason.as_deref().unwrap().contains(needle),
                "command {command:?} gave reason {reason:?}, expected it to mention {needle:?}"
            );
        }
    }

    #[test]
    fn decide_denies_the_line_continuation_bypass() {
        let (root, canonical_bare, canonical_git) = grove();
        std::fs::create_dir(root.path().join("main")).unwrap();
        let payload = NormalizedPayload {
            tool: Tool::Bash {
                command: "c\\\nd .. && printf x>.bare/config".to_string(),
            },
            cwd: Some(root.path().join("main")),
        };
        let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
        assert!(matches!(verdict, Verdict::Deny(_)), "{verdict:?}");
    }

    #[test]
    fn shell_tokens_joins_a_backslash_newline_line_continuation() {
        assert_eq!(shell_tokens("c\\\nd .."), vec!["cd", ".."]);
    }

    /// exec-advisor's finding while reviewing the fourth-round fix: a
    /// quoted, multi-word argument (most commonly the string handed to an
    /// external interpreter's own `-c`/`-e` flag, which this scan
    /// deliberately does not enumerate by program name -- see
    /// `UNSAFE_COMMAND_WORDS`'s design note) tokenizes as one word with
    /// internal whitespace, not several, exactly like real Bash. Candidate
    /// extraction has to look inside that word too, or a path glued into
    /// the middle of it is invisible to every check downstream.
    #[test]
    fn bash_candidates_looks_inside_a_quoted_multi_word_argument() {
        let candidates = bash_candidates(r#"sh -c "cd / && printf x > .bare/config""#);
        for expected in ["sh", "-c", "cd", "printf", "x", ".bare/config"] {
            assert!(
                candidates.iter().any(|candidate| candidate == expected),
                "expected {expected:?} among {candidates:?}"
            );
        }
    }

    #[test]
    fn decide_denies_a_protected_path_hidden_inside_a_quoted_interpreter_argument() {
        let (root, canonical_bare, canonical_git) = grove();
        let payload = NormalizedPayload {
            tool: Tool::Bash {
                command: r#"sh -c "printf x > .bare/config""#.to_string(),
            },
            cwd: Some(root.path().to_path_buf()),
        };
        let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
        assert!(matches!(verdict, Verdict::Deny(_)), "{verdict:?}");
    }

    #[test]
    fn bash_candidates_still_allows_ordinary_quoted_arguments_with_no_hidden_path() {
        // A quoted argument with internal whitespace but no `.`/`/`
        // anywhere in it should not somehow start resolving to `.bare`
        // once it is also whitespace-split; every legitimate quoted-
        // multi-word case already covered by `decide` staying `Allow`
        // elsewhere in this suite must keep doing so.
        let (root, canonical_bare, canonical_git) = grove();
        for command in [
            "bash script.sh",
            r#"git commit -m "fix the thing""#,
            r#"grep -r "some pattern" src/"#,
            r#"echo "hello world""#,
        ] {
            let payload = NormalizedPayload {
                tool: Tool::Bash {
                    command: command.to_string(),
                },
                cwd: Some(root.path().to_path_buf()),
            };
            let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
            assert_eq!(verdict, Verdict::Allow, "{command:?}: {verdict:?}");
        }
    }

    /// exec-reviewer's fourth-round finding: the quoted-content extraction
    /// alone did not deny a `cd` hidden inside a quoted multi-word
    /// argument (only the *path* extraction recursed, not the unsafe-
    /// construct check), and did not correctly re-unquote a nested quote
    /// inside an outer quote. Both are now closed by
    /// `unsafe_bash_construct` recursing into quoted content that shows
    /// operator evidence of being shell code (see `looks_like_shell_code`).
    #[test]
    fn unsafe_bash_construct_denies_cd_hidden_inside_quoted_shell_code() {
        let reason = unsafe_bash_construct("bash -c 'cd ..; printf x > .bare/config'");
        assert!(
            reason
                .as_deref()
                .is_some_and(|reason| reason.contains("cd")),
            "{reason:?}"
        );
    }

    #[test]
    fn decide_denies_cd_hidden_inside_quoted_shell_code_passed_to_an_unnamed_interpreter() {
        let (root, canonical_bare, canonical_git) = grove();
        let payload = NormalizedPayload {
            tool: Tool::Bash {
                command: "bash -c 'cd ..; printf x > .bare/config'".to_string(),
            },
            cwd: Some(root.path().to_path_buf()),
        };
        let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
        assert!(matches!(verdict, Verdict::Deny(_)), "{verdict:?}");
    }

    #[test]
    fn decide_denies_a_bash_command_reaching_bare_through_a_nested_quote() {
        let (root, canonical_bare, canonical_git) = grove();
        let payload = NormalizedPayload {
            tool: Tool::Bash {
                command: r#"bash -c 'printf x > "../.bare/config"'"#.to_string(),
            },
            cwd: Some(root.path().join("main")),
        };
        std::fs::create_dir(root.path().join("main")).unwrap();
        let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
        assert!(matches!(verdict, Verdict::Deny(_)), "{verdict:?}");
    }

    #[test]
    fn decide_denies_a_directory_changing_flag() {
        let (root, canonical_bare, canonical_git) = grove();
        for command in [
            "env --chdir=.. touch .bare/config",
            "git --directory=.. status",
        ] {
            let payload = NormalizedPayload {
                tool: Tool::Bash {
                    command: command.to_string(),
                },
                cwd: Some(root.path().join("main")),
            };
            std::fs::create_dir_all(root.path().join("main")).unwrap();
            let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
            assert!(
                matches!(verdict, Verdict::Deny(_)),
                "{command:?}: {verdict:?}"
            );
        }
    }

    #[test]
    fn decide_denies_alias_and_shopt_as_command_words() {
        let (root, canonical_bare, canonical_git) = grove();
        for command in ["shopt -s expand_aliases", r#"alias up="cd .." "#] {
            let payload = NormalizedPayload {
                tool: Tool::Bash {
                    command: command.to_string(),
                },
                cwd: Some(root.path().to_path_buf()),
            };
            let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
            assert!(
                matches!(verdict, Verdict::Deny(_)),
                "{command:?}: {verdict:?}"
            );
        }
    }

    #[test]
    fn looks_like_shell_code_does_not_flag_ordinary_quoted_data() {
        // The exact false-positive risk of recursing into every quoted
        // token unconditionally: ordinary quoted text containing `$`,
        // `(`, or even the bare word `cd` with no shell meaning at all.
        for text in [
            "literal $HOME and (parens) are safe in single quotes",
            "cd into src before building",
            "fix the thing",
            "some pattern",
            "hello world",
        ] {
            assert!(
                !looks_like_shell_code(text),
                "expected {text:?} to not look like shell code"
            );
        }
    }

    #[test]
    fn looks_like_shell_code_flags_operator_evidence() {
        for text in [
            "cd ..; printf x > file",
            "printf x > file",
            "a && b",
            "a | b",
        ] {
            assert!(
                looks_like_shell_code(text),
                "expected {text:?} to look like shell code"
            );
        }
    }

    #[test]
    fn unsafe_bash_construct_still_allows_ordinary_quoted_text_containing_dollar_and_parens() {
        // The regression this change must not reintroduce: recursing into
        // quoted content only when it shows operator evidence, not
        // unconditionally, so a commit message or echoed string
        // containing `$`/`(`/`cd` with no shell meaning stays allowed.
        for command in [
            "echo 'literal $HOME and (parens) are safe in single quotes'",
            r#"git commit -m "cd into src before building""#,
        ] {
            assert_eq!(unsafe_bash_construct(command), None, "{command:?}");
        }
    }

    /// exec-reviewer's fifth-round finding: a dispatch word with no
    /// separator or redirect anywhere around it in the same quoted string
    /// (`exec env --chdir .. touch .bare/config` -- no `;`, `&`, `|`, `>`,
    /// or `<` at all) still needed to trigger recursion for
    /// `unsafe_bash_command_word` to ever see the `exec`. Closed by
    /// `SHELL_CODE_EVIDENCE_WORDS`/`UNSAFE_DIRECTORY_FLAGS` membership
    /// being its own trigger, alongside operator evidence.
    #[test]
    fn decide_denies_a_dispatch_word_with_no_operator_evidence_hidden_in_quoted_content() {
        let (root, canonical_bare, canonical_git) = grove();
        let payload = NormalizedPayload {
            tool: Tool::Bash {
                command: "bash -c 'exec env --chdir .. touch .bare/config'".to_string(),
            },
            cwd: Some(root.path().to_path_buf()),
        };
        let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
        assert!(matches!(verdict, Verdict::Deny(_)), "{verdict:?}");
    }

    /// exec-reviewer's fifth-round finding: `env -C ..` at the top level
    /// (no quoting at all) was never covered by the long-form-only
    /// directory-flag check. Closed by scoping the short `-C` form to the
    /// specific programs where it unambiguously means "change directory"
    /// (`DIRECTORY_CHANGING_SHORT_C_FLAG_PROGRAMS`), rather than a blanket
    /// ban that would also catch `grep -C`/`diff -C`'s unrelated
    /// context-line count.
    #[test]
    fn decide_denies_env_dash_capital_c_but_still_allows_grep_and_diff_context_flags() {
        let (root, canonical_bare, canonical_git) = grove();
        let payload = NormalizedPayload {
            tool: Tool::Bash {
                command: "env -C .. touch .bare/config".to_string(),
            },
            cwd: Some(root.path().to_path_buf()),
        };
        let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
        assert!(matches!(verdict, Verdict::Deny(_)), "{verdict:?}");

        for command in ["grep -C 3 pattern file", "diff -C 5 a b"] {
            assert_eq!(
                unsafe_bash_directory_flag(command),
                None,
                "{command:?} must not be denied by the scoped -C check"
            );
        }
    }

    #[test]
    fn unsafe_bash_directory_flag_covers_git_make_and_tar_short_c_form_too() {
        for command in [
            "git -C .. status",
            "make -C .. build",
            "tar -C .. -xf archive.tar",
        ] {
            assert!(unsafe_bash_directory_flag(command).is_some(), "{command:?}");
        }
    }

    /// exec-reviewer's sixth-round finding on the scoped `-C` check: a
    /// glued short-option value (`-C..`, the same convention the flag's
    /// own long form and getopt-style parsing generally accept), an
    /// absolute or relative path to the program instead of its bare name,
    /// and the flag hidden inside quoted content with no other shell-code
    /// evidence around it all still bypassed it.
    #[test]
    fn unsafe_bash_directory_flag_covers_glued_values_and_paths_to_the_program() {
        for command in [
            "env -C.. touch .bare/config",
            "/usr/bin/env -C .. touch .bare/config",
            "./env -C .. touch .bare/config",
        ] {
            assert!(unsafe_bash_directory_flag(command).is_some(), "{command:?}");
        }
    }

    #[test]
    fn decide_denies_the_short_c_flag_hidden_inside_quoted_content_with_no_other_evidence() {
        let (root, canonical_bare, canonical_git) = grove();
        let payload = NormalizedPayload {
            tool: Tool::Bash {
                command: "bash -c 'env -C .. touch .bare/config'".to_string(),
            },
            cwd: Some(root.path().to_path_buf()),
        };
        let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
        assert!(matches!(verdict, Verdict::Deny(_)), "{verdict:?}");
    }
}
