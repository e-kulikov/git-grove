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

/// Whether the unquoted `{` at byte offset `open` in `command` opens a
/// *live* Bash brace expansion — one Bash will actually expand, not
/// merely literal use of the character (`find ... -exec cmd {} \;`'s
/// placeholder, or a shell function/compound-command body's own `{
/// ...; }`, neither of which contains what Bash requires: at least one
/// unquoted comma, or a `..` range, somewhere between a matching pair of
/// braces). Scans forward from `open`, tracking brace nesting depth and
/// quote state the same way the rest of this file does, until the
/// matching close brace at depth zero; returns whether a comma was seen
/// at any nesting level within that span, or the span's text contains
/// `..` (a coarse but safely conservative stand-in for Bash's exact
/// `{x..y}`/`{x..y..z}` sequence grammar — over-matching here only costs
/// an unnecessary denial, never a miss). `false` if the brace is never
/// closed at all: an unmatched `{` with no comma/range inside is simply
/// literal text to Bash either way, and an unterminated *quote* opened
/// while scanning for the match is separately, unconditionally denied by
/// this function's caller already.
fn looks_like_live_brace_expansion(command: &str, open: usize) -> bool {
    let bytes = command.as_bytes();
    let mut depth: usize = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut has_comma = false;
    let mut index = open;
    while index < bytes.len() {
        match bytes[index] {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'\\' if !in_single => index += 1,
            b'{' if !in_single && !in_double => depth += 1,
            b'}' if !in_single && !in_double => {
                depth -= 1;
                if depth == 0 {
                    let inner = &command[open + 1..index];
                    return has_comma || inner.contains("..");
                }
            }
            b',' if !in_single && !in_double && depth >= 1 => has_comma = true,
            _ => {}
        }
        index += 1;
    }
    false
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
    // Whether the character about to be matched is a position Bash would
    // tilde-expand from — tracked only for `~` below, since unlike every
    // other character this scan flags anywhere in a word, Bash only
    // tilde-expands from specific positions, never arbitrarily mid-word:
    // the start of a word (`~/x`), or — strictly within a word that is
    // *itself* shaped like a `NAME=value` assignment, not any word that
    // merely contains `=`/`:` — right after the first `=` or any later
    // unquoted `:` (`FOO=~/x`, a `PATH`-like `FOO=~/a:~/b`). Both
    // `git log HEAD~3..HEAD` (a bare `~` mid-word, no assignment shape at
    // all) and `scp host:~/file` (a `:` inside an ordinary argument that
    // is not a `NAME=value` word — confirmed against real Bash: this `~`
    // does not expand) are real commands whose `~` must not be flagged;
    // `assignment_prefix_ok`/`assignment_confirmed` track, per word,
    // whether it still could be — or already is — assignment-shaped, the
    // same shape [`is_assignment_word`] recognizes, so the `:`-triggered
    // boundary only ever fires inside one.
    let mut tilde_boundary = true;
    let mut assignment_prefix_ok = true;
    let mut assignment_confirmed = false;
    let mut chars = command.char_indices().peekable();
    while let Some((byte_index, character)) = chars.next() {
        let word_start = tilde_boundary;
        if in_single || in_double {
            tilde_boundary = false;
        } else if matches!(
            character,
            ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>'
        ) {
            // A fresh word starts right after any of these — reset the
            // per-word assignment-shape tracking too.
            tilde_boundary = true;
            assignment_prefix_ok = true;
            assignment_confirmed = false;
        } else if assignment_confirmed {
            tilde_boundary = character == ':';
        } else if assignment_prefix_ok {
            if character == '=' {
                assignment_confirmed = true;
                tilde_boundary = true;
            } else {
                assignment_prefix_ok = character.is_ascii_alphanumeric() || character == '_';
                tilde_boundary = false;
            }
        } else {
            tilde_boundary = false;
        }
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
            '*' | '?' | '[' if !in_single && !in_double => {
                // Unlike `$`/backtick, Bash pathname expansion is inert
                // inside *either* quote style, not just single quotes — so
                // this is the one character class in this scan gated on
                // both flags at once. Left unquoted, any of the three can
                // make the token that contains it expand, at Bash's own
                // hand, into a filesystem-dependent path this scan never
                // sees: `rm .b?re/config`, `cat /usr/bin/g[i]t`, `echo *`.
                // Nothing here can predict a glob's expansion without
                // walking the real filesystem the same way Bash would, so
                // rather than trying, this is denied the same way every
                // other construct outside this scan's bounded vocabulary
                // is: as unmodeled, not as safe by default.
                return Some(format!(
                    "an unquoted `{character}` (Bash pathname expansion, whose result this scan cannot predict)"
                ));
            }
            '{' if !in_single
                && !in_double
                && looks_like_live_brace_expansion(command, byte_index) =>
            {
                // Brace expansion (`.{b,b}are/config`, `.b{,}are`) is, like
                // pathname expansion, inert inside either quote style, and
                // for the same reason denied outright rather than modeled:
                // predicting its expanded words would mean re-implementing
                // Bash's own brace-expansion grammar, not just recognizing
                // a construct exists. Gated on
                // `looks_like_live_brace_expansion`, unlike the pathname
                // characters above: Bash itself only expands a `{...}`
                // group that contains a comma or a `..` range directly
                // inside it — a bare `{}`/`{single}` is left completely
                // literal, and `find ... -exec cmd {} \;`'s placeholder is
                // exactly that shape, denying it outright was a real
                // false-positive regression on an extremely common
                // command. This also subsumes the compound-command-group
                // reading of a bare `{` this scan already denies as a
                // command word (`UNSAFE_COMMAND_WORDS`) only when it's
                // actually live — a `{ cmd; }` group's own body always has
                // more than a bare pair of braces, so the existing
                // command-word check still catches it independently, and
                // is unaffected by this gate.
                return Some(
                    "an unquoted `{...}` (Bash brace expansion, whose result this scan cannot predict)"
                        .to_string(),
                );
            }
            '~' if !in_single && !in_double && word_start => {
                // Tilde expansion substitutes a user's home directory (or,
                // as `~+`/`~-`, the shell's own $PWD/$OLDPWD) — but, unlike
                // every other character flagged in this function, only
                // when it appears at the front of a word (or right after
                // `=`/`:`, see `at_word_start` above), never mid-word: a
                // `~` anywhere else (`HEAD~3`, a git revision range) is
                // ordinary text with no shell meaning at all. Denied
                // outright rather than resolved, same as the others, since
                // this scan has no access to the environment Bash itself
                // would expand a live one against.
                return Some(
                    "an unquoted `~` (Bash tilde expansion, whose result this scan cannot predict)"
                        .to_string(),
                );
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

/// Whether `token` is `-C` itself, `-C` with its value glued directly onto
/// it (`-C..`, `-Cdir`), or `-C` clustered together with one or more other
/// single-character flags in the same getopt-style token (`-iC/path`,
/// `-sC/`) — GNU getopt (and every program in
/// [`DIRECTORY_CHANGING_SHORT_C_FLAG_PROGRAMS`] uses it) treats a bundle of
/// short options as equivalent to writing each separately, with a
/// value-taking option's own value glued directly onto whatever follows it
/// in the bundle exactly as if it had been written alone (`env -iC/path` is
/// `env -i -C /path`). Matched as "any `-`-prefixed, non-`--` token
/// containing an uppercase `C` anywhere after the leading dash" rather than
/// modeling each program's exact short-option set: this check only ever
/// runs against the arguments of the four known directory-changing
/// programs, so the broader match costs nothing but an occasional
/// unnecessary denial on a flag bundle that happens to contain a `C` for an
/// unrelated reason — the same fail-closed trade this whole scan makes
/// everywhere else.
fn is_short_c_flag(token: &str) -> bool {
    token
        .strip_prefix('-')
        .filter(|rest| !rest.starts_with('-'))
        .is_some_and(|rest| rest.contains('C'))
}

/// Programs that transparently re-exec their remaining arguments as another
/// command, without altering how any later program's own flags are parsed
/// — scheduling and process-group wrappers, not shells or interpreters. A
/// directory-changing program named right after one of these is exactly as
/// live as if the wrapper were not there, so [`resolve_directory_flag`]
/// must see past it to find the real command word, instead of mistaking
/// the wrapper's own name — or, worse, one of its option values — for it.
/// `env` and `timeout` are handled separately, not listed here: `env` also
/// has its own `-C`/`--chdir` that can directly precede the command it
/// execs, and `timeout`'s grammar has a mandatory positional `DURATION`
/// this generic skip does not account for — see their own arms of
/// [`resolve_directory_flag`].
const TRANSPARENT_WRAPPER_PROGRAMS: &[&str] = &["nice", "nohup", "setsid"];

/// [`TRANSPARENT_WRAPPER_PROGRAMS`]'s own short and long options that take
/// their value as a separate following word rather than only glued on:
/// `nice -n 10 cmd`/`nice --adjustment 10 cmd` alongside `nice -n10 cmd`.
/// `nohup` and `setsid` take no value-bearing option before the command
/// they run.
const WRAPPER_OPTIONS_WITH_SEPARATE_VALUE: &[&str] = &["-n", "--adjustment"];

/// `env`'s own one-letter short options that take no value at all
/// (`-i`/`--ignore-environment`, `-0`/`--null`, `-v`/`--debug`) — see
/// [`short_option_cluster_needs_separate_value`], which this classifies
/// GNU-style clustering against. `-C` is deliberately excluded even
/// though it is also a short option: [`resolve_directory_flag`]'s `env`
/// arm checks for it, via [`is_short_c_flag`], before consulting this at
/// all, since finding it is the match that arm is looking for, not
/// something to classify and skip past.
const ENV_NO_VALUE_SHORT_FLAGS: &str = "i0v";

/// `env`'s own one-letter short options that take a value, either glued
/// on (`-uFOO`) or as a separate following word (`-u FOO`) — see
/// [`short_option_cluster_needs_separate_value`]. Confirmed against the
/// installed `env --help`: `-a`/`--argv0=ARG` (pass a different argv[0] to
/// the command), `-u`/`--unset=NAME`, `-S`/`--split-string=S` are the
/// three that require one, unlike `-C` (handled separately, see above).
const ENV_VALUE_SHORT_FLAGS: &str = "auS";

/// `env`'s own long options that take no value at all — including the
/// three signal-handling ones whose argument, per `env --help`'s own
/// `[=SIG]` bracket notation, is optional and only ever glued on via `=`
/// (`--block-signal=PIPE`), never a separate word, the same shape
/// `--exec-path` has on `git` — so, like that one, they belong here, not
/// in [`ENV_VALUE_LONG_OPTIONS`].
const ENV_NO_VALUE_LONG_OPTIONS: &[&str] = &[
    "--ignore-environment",
    "--null",
    "--debug",
    "--help",
    "--version",
    "--block-signal",
    "--default-signal",
    "--ignore-signal",
    "--list-signal-handling",
];

/// `env`'s own long options that always take a value, either glued on via
/// `=` (`--unset=FOO`) or as a separate following word (`--unset FOO`).
const ENV_VALUE_LONG_OPTIONS: &[&str] = &["--argv0", "--unset", "--split-string"];

/// `timeout`'s own one-letter short options that take no value at all
/// (`-f`/`--foreground`, `-p`/`--preserve-status`, `-v`/`--verbose`).
const TIMEOUT_NO_VALUE_SHORT_FLAGS: &str = "fpv";

/// `timeout`'s own one-letter short options that take a value, either
/// glued on (`-k5`) or as a separate following word (`-k 5`).
const TIMEOUT_VALUE_SHORT_FLAGS: &str = "ks";

/// `timeout`'s own long options that take no value at all.
const TIMEOUT_NO_VALUE_LONG_OPTIONS: &[&str] = &[
    "--foreground",
    "--preserve-status",
    "--verbose",
    "--help",
    "--version",
];

/// `timeout`'s own long options that take a value, either glued on via
/// `=` (`--kill-after=5`) or as a separate following word.
const TIMEOUT_VALUE_LONG_OPTIONS: &[&str] = &["--kill-after", "--signal"];

/// Classify one `-`-prefixed, non-`--` option token — a single short flag
/// or several bundled together, GNU-getopt style (`-i`, `-iv`, `-uFOO`,
/// `-ivuFOO`) — against a wrapper's own complete, fixed set of one-letter
/// options of each kind. Only the *last* letter in a bundle may carry a
/// value, exactly like real getopt short-option clustering: `env -iuFOO`
/// is `-i -u FOO`(glued), not `-i -u -F -O -O`. Returns:
///
/// - `Some(true)` if the bundle is fully recognized and its last letter is
///   a value-taking flag with *nothing* glued after it, so the value is
///   the next separate word (`env -iu FOO` → `-i`, `-u` needing `FOO`).
/// - `Some(false)` if the bundle is fully recognized and needs no separate
///   word — every letter is a no-value flag, or the last is value-taking
///   with its value already glued on (`env -iuFOO` → `-i`, `-u` with `FOO`
///   glued).
/// - `None` if any letter in the bundle is not one of the two given sets
///   at all: an unrecognized option must never be silently treated as
///   "no value" (that is exactly the miscount that let `env -iu FOO -C /
///   cmd` slip through before this function existed — `-iu`, treated as a
///   bare flag, consumed nothing, so `FOO` was mistaken for `env`'s
///   command instead of `-u`'s value), so callers must fail closed on
///   `None` instead of guessing.
fn short_option_cluster_needs_separate_value(
    token: &str,
    no_value_flags: &str,
    value_flags: &str,
) -> Option<bool> {
    let rest = token.strip_prefix('-')?;
    if rest.is_empty() || rest.starts_with('-') {
        return None;
    }
    let mut chars = rest.chars();
    while let Some(letter) = chars.next() {
        if value_flags.contains(letter) {
            return Some(chars.next().is_none());
        }
        if !no_value_flags.contains(letter) {
            return None;
        }
    }
    Some(false)
}

/// Classify one `--`-prefixed long option token — with an optional glued
/// `=value` — against a wrapper's own complete, fixed set of long
/// options of each kind. Same three-way result as
/// [`short_option_cluster_needs_separate_value`], for the same reason:
/// `None` (unrecognized) must never be treated the same as `Some(false)`
/// (recognized, needs nothing more).
fn long_option_needs_separate_value(
    token: &str,
    no_value_options: &[&str],
    value_options: &[&str],
) -> Option<bool> {
    let name = token.split_once('=').map_or(token, |(name, _)| name);
    if value_options.contains(&name) {
        return Some(!token.contains('='));
    }
    if no_value_options.contains(&name) {
        return Some(false);
    }
    None
}

/// Whether `token` is one of `git`'s own long-standing global options that
/// *always* takes its value as a separate following word, rather than
/// only glued onto the flag itself — the same question
/// [`short_option_cluster_needs_separate_value`]/[`long_option_needs_separate_value`]
/// answer for `env`/`timeout`. This only matters for
/// [`resolve_directory_flag`]'s stop-at-first-non-option-token rule for
/// `git`: without skipping the value too, `git -c alias.v=version -C / v`'s
/// `alias.v=version` (the *value* of `-c`, not a subcommand) would wrongly
/// look like the boundary and hide the `-C` straight after it — a false
/// negative, exactly the kind of regression the boundary rule exists to
/// avoid introducing.
///
/// Deliberately does *not* include `--exec-path`, even though it is one of
/// `git`'s own long-standing global options too: `git`'s own usage string
/// documents it as `--exec-path[=<path>]` — an *optional* argument, valid
/// bare with no value at all (confirmed: `git --exec-path` alone prints
/// the path and exits) — so unlike `-c`/`--git-dir`/`--work-tree`/
/// `--namespace` (each confirmed mandatory: bare, each errors with "no ...
/// given" rather than treating the next word as anything) it must never
/// unconditionally consume the following word; doing so once
/// unconditionally consumed `git --exec-path -C / status`'s real `-C` as
/// if it were `--exec-path`'s value, missing it entirely. Since
/// `--exec-path` (like every other git global option not listed here) is
/// simply left out, this scan neither skips past it specially nor treats
/// it as the subcommand-boundary positional — it is just one more
/// `-`-prefixed token the surrounding loop passes over unchanged, which is
/// exactly correct for an optional-argument flag: whether or not a value
/// happens to be glued on, nothing after it needs skipping.
fn option_consumes_separate_value(command_word: &str, token: &str) -> bool {
    let name = Path::new(command_word)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command_word);
    name == "git" && matches!(token, "-c" | "--git-dir" | "--work-tree" | "--namespace")
}

/// Resolve one simple command's directory-changing exposure, starting at
/// `index` — a command word, either the segment's own or a wrapper's exec
/// target, reached by recursing into this same function: whether it, or
/// any program it transparently execs in turn, contains a live directory-
/// changing flag. `None` once `index` runs off the end of `tokens` (a
/// wrapper whose own flags ran out with no command word following it has
/// nothing left to resolve).
///
/// - A [`TRANSPARENT_WRAPPER_PROGRAMS`] name: skip its own leading flags
///   (a bare flag, one with a separate value from
///   [`WRAPPER_OPTIONS_WITH_SEPARATE_VALUE`], or a literal `--`, which
///   stops the skip) and recurse into whatever command word follows —
///   `nice nohup git -C / status` must be seen as naming `git`.
/// - `timeout`/`env`: each walks its own option grammar directly via
///   [`short_option_cluster_needs_separate_value`] (bundled short flags —
///   `env -iu FOO` is `-i`, then `-u` needing `FOO` as its separate value,
///   not one bare flag `-iu` that consumes nothing) and
///   [`long_option_needs_separate_value`], denying outright on any option
///   token neither recognizes rather than guessing how many words it
///   spans — silently guessing "no value" for an unrecognized option is
///   exactly the miscount that let `env -iu FOO -C / cmd` slip through
///   before these functions existed. `timeout` additionally has one more
///   step `TRANSPARENT_WRAPPER_PROGRAMS` never needs: a mandatory
///   `DURATION` positional between its own options and the command it
///   execs, unconditionally skipped once (not gated on looking
///   option-like, since `timeout`'s grammar always has exactly one there
///   whenever a command follows at all) before recursing — `timeout 2 git
///   -C / status` must be seen as naming `git`, not `2`. `env` also has
///   its own bare `-C`/`--chdir`, checked before consulting either
///   classifier at all, since finding it is itself exactly the live flag
///   this whole function exists to find; a leading `NAME=VALUE` (see
///   [`is_assignment_word`]) is skipped as one more of `env`'s own leading
///   tokens. Either way, the first token that is none of those is the
///   command the wrapper execs, recursed into the same way (`env git -C /
///   status` reaches `git`'s own `-C` this way; `env FOO=bar mytool -C /`
///   correctly does *not* treat that `-C` as `env`'s own, since it is only
///   found after recursing into `mytool`).
/// - Any other program: `None` unless it names one of
///   [`DIRECTORY_CHANGING_SHORT_C_FLAG_PROGRAMS`], in which case its own
///   arguments are scanned for a live `-C`, stopping at a literal `--`
///   unconditionally, and — for `git` specifically, via
///   [`option_consumes_separate_value`] — at the first token that is
///   neither `-C` nor one of `git`'s own separate-value options nor
///   otherwise `-`-prefixed, since that is `git`'s subcommand and anything
///   after belongs to the subcommand's own argument grammar, not `git`'s
///   (`git grep -C 1 pattern`'s `-C` belongs to `grep`). `make`/`tar` are
///   deliberately never stopped early this way: both legitimately repeat
///   `-C` after a non-option argument already went by (`tar -cf out.tar
///   file1 -C dir2 file2`), so no subcommand-like boundary is assumed for
///   either.
fn resolve_directory_flag(tokens: &[String], index: usize) -> Option<String> {
    let command_word = tokens.get(index)?;
    let name = Path::new(command_word.as_str())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command_word.as_str());

    if TRANSPARENT_WRAPPER_PROGRAMS.contains(&name) {
        let mut next = index + 1;
        while next < tokens.len() {
            let token = tokens[next].as_str();
            if token == "--" {
                next += 1;
                break;
            }
            if !token.starts_with('-') {
                break;
            }
            let consumes_next_word = WRAPPER_OPTIONS_WITH_SEPARATE_VALUE.contains(&token);
            next += 1;
            if consumes_next_word {
                next += 1;
            }
        }
        return resolve_directory_flag(tokens, next);
    }

    if name == "timeout" {
        let mut next = index + 1;
        while next < tokens.len() {
            let token = tokens[next].as_str();
            if token == "--" {
                next += 1;
                break;
            }
            if !token.starts_with('-') {
                break;
            }
            let needs_value = if token.starts_with("--") {
                long_option_needs_separate_value(
                    token,
                    TIMEOUT_NO_VALUE_LONG_OPTIONS,
                    TIMEOUT_VALUE_LONG_OPTIONS,
                )
            } else {
                short_option_cluster_needs_separate_value(
                    token,
                    TIMEOUT_NO_VALUE_SHORT_FLAGS,
                    TIMEOUT_VALUE_SHORT_FLAGS,
                )
            };
            let Some(needs_value) = needs_value else {
                return Some(format!(
                    "an unrecognized option (`{token}`) on `{command_word}`"
                ));
            };
            next += 1;
            if needs_value {
                next += 1;
            }
        }
        // The mandatory DURATION positional, between timeout's own options
        // and the command it execs — skipped unconditionally, unlike a
        // wrapper's optional flags, since timeout's own grammar always has
        // exactly one here whenever a command follows at all.
        if next < tokens.len() {
            next += 1;
        }
        return resolve_directory_flag(tokens, next);
    }

    if name == "env" {
        let mut next = index + 1;
        while next < tokens.len() {
            let token = tokens[next].as_str();
            if token == "--" {
                next += 1;
                break;
            }
            if is_short_c_flag(token) {
                return Some(format!(
                    "a directory-changing `-C` flag on `{command_word}`"
                ));
            }
            if token.starts_with("--") {
                let Some(needs_value) = long_option_needs_separate_value(
                    token,
                    ENV_NO_VALUE_LONG_OPTIONS,
                    ENV_VALUE_LONG_OPTIONS,
                ) else {
                    return Some(format!(
                        "an unrecognized option (`{token}`) on `{command_word}`"
                    ));
                };
                next += 1;
                if needs_value {
                    next += 1;
                }
                continue;
            }
            if token.starts_with('-') {
                let Some(needs_value) = short_option_cluster_needs_separate_value(
                    token,
                    ENV_NO_VALUE_SHORT_FLAGS,
                    ENV_VALUE_SHORT_FLAGS,
                ) else {
                    return Some(format!(
                        "an unrecognized option (`{token}`) on `{command_word}`"
                    ));
                };
                next += 1;
                if needs_value {
                    next += 1;
                }
                continue;
            }
            if is_assignment_word(token) {
                next += 1;
                continue;
            }
            break;
        }
        return resolve_directory_flag(tokens, next);
    }

    if !names_directory_changing_program(command_word) {
        return None;
    }
    let stop_at_first_positional = name == "git";
    let mut rest = tokens[index + 1..].iter();
    while let Some(token) = rest.next() {
        if token == "--" {
            break;
        }
        if is_short_c_flag(token) {
            return Some(format!(
                "a directory-changing `-C` flag on `{command_word}`"
            ));
        }
        if stop_at_first_positional {
            if option_consumes_separate_value(command_word, token) {
                rest.next();
                continue;
            }
            if !token.starts_with('-') {
                break;
            }
        }
    }
    None
}

/// Whether `command` contains one of [`UNSAFE_DIRECTORY_FLAGS`] (bare or
/// with a glued `=value`) anywhere among its plain tokens, or — via
/// [`resolve_directory_flag`], starting from each simple command's own
/// word — a live directory-changing `-C` on that command word itself, on
/// any program it transparently execs through [`TRANSPARENT_WRAPPER_PROGRAMS`]
/// or `env`, or in `env`'s own leading options.
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
        let command_word_index = match command_word_index_in_segment(&tokens) {
            Ok(Some(index)) => index,
            Ok(None) => continue,
            Err(token) => {
                return Some(format!(
                    "an assignment or redirection prefix (`{token}`) whose grammar is not fully recognized"
                ));
            }
        };
        if let Some(reason) = resolve_directory_flag(&tokens, command_word_index) {
            return Some(reason);
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
///
/// `Ok(None)` if the segment has no tokens at all past its own leading
/// assignments/redirections (genuinely nothing left to check). `Err` —
/// not folded into `Ok(None)` — if its first non-prefix token is an
/// unrecognized assignment/redirection prefix (see
/// [`looks_like_unrecognized_prefix`]), carrying that token back to the
/// caller to deny with: at the *top level*, this case is also separately
/// denied by `unsafe_bash_command_word`/`unsafe_command_word_in_segment`,
/// but `unsafe_bash_directory_flag` recurses into quoted content on its
/// own, unconditionally, unlike that construct-level scan (gated by
/// [`looks_like_shell_code`]) — so a quoted string whose own evidence
/// doesn't trip that gate (`"alias.x=!git -C / status"`, no operator or
/// evidence word in `looks_like_shell_code`'s narrower vocabulary) would
/// otherwise reach only this function, which previously just skipped the
/// whole segment on the same unrecognized-prefix token and silently
/// missed a live `-C` later in it. Returning `Err` here, rather than
/// `Ok(None)`, lets this scan deny that case itself instead of silently
/// relying on an invariant ("already denied elsewhere") that only holds
/// for the top-level call.
fn command_word_index_in_segment(tokens: &[String]) -> Result<Option<usize>, &str> {
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
            return Err(token);
        }
        return Ok(Some(index));
    }
    Ok(None)
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

    /// exec-reviewer's seventh-round findings on the scoped `-C` check,
    /// found once it was taken back for final approval: a clustered short
    /// option (`env -iC/path`, GNU getopt's "-i -C /path" written as one
    /// token), a transparent wrapper (`nice`) hiding the real, directory-
    /// changing command word from command-word-scoped inspection, and two
    /// false positives from scanning `-C` with no regard for option/command
    /// boundaries at all (`git grep -C 1 pattern`'s `-C` belongs to `grep`,
    /// not `git`; nothing after a literal `--` is an option for any of the
    /// four programs).
    #[test]
    fn unsafe_bash_directory_flag_covers_clustered_short_options_and_transparent_wrappers() {
        for command in [
            "env -iC/path touch .bare/config",
            "env -iC../.. touch .bare/config",
            "nice /usr/bin/git -C / status harmless",
            "nice -n 10 git -C / status",
            "nice --adjustment 10 git -C / status",
            "nice nohup git -C / status",
        ] {
            assert!(unsafe_bash_directory_flag(command).is_some(), "{command:?}");
        }
    }

    /// `env` with no `-C` of its own still transparently execs whatever
    /// command follows its leading options/assignments — `env git -C /
    /// status` must be seen as reaching `git`'s own `-C`, exactly as if
    /// `env` were not there, the same way `nice`/`nohup`/`setsid` already
    /// are. `env FOO=bar mytool -C /` must *not* treat that `-C` as
    /// `env`'s own (it is only live once recursed into `mytool`), so this
    /// also doubles as another option/command-boundary regression check.
    #[test]
    fn unsafe_bash_directory_flag_treats_env_as_a_transparent_wrapper_too() {
        for command in [
            "env git -C / status",
            "env FOO=bar git -C / status",
            "env -i FOO=bar BAZ=qux git -C / status",
            "nice env -C / git status",
        ] {
            assert!(unsafe_bash_directory_flag(command).is_some(), "{command:?}");
        }
    }

    /// `timeout`'s mandatory `DURATION` positional (`timeout [OPTION]
    /// DURATION COMMAND [ARG]...`) sits between its own options and the
    /// command it execs, unlike `nice`/`nohup`/`setsid` — it needs its own
    /// arm in `resolve_directory_flag`, not just membership in
    /// `TRANSPARENT_WRAPPER_PROGRAMS`, or the duration itself gets
    /// mistaken for the command word.
    #[test]
    fn unsafe_bash_directory_flag_treats_timeout_as_a_wrapper_with_a_duration_positional() {
        for command in [
            "timeout 2 git -C / status",
            "timeout -k 5 2 git -C / status",
            "timeout --signal=KILL 2 git -C / status",
        ] {
            assert!(unsafe_bash_directory_flag(command).is_some(), "{command:?}");
        }
    }

    #[test]
    fn unsafe_bash_directory_flag_respects_option_and_command_boundaries() {
        for command in ["git grep -C 1 setup README.md", "git -- -C harmless"] {
            assert_eq!(
                unsafe_bash_directory_flag(command),
                None,
                "{command:?} must not be denied by the scoped -C check"
            );
        }
    }

    /// A `git -c`/`env -u`-style separate-value global option, appearing
    /// before a real `-C`, must not be mistaken for the subcommand boundary
    /// [`unsafe_bash_directory_flag_respects_option_and_command_boundaries`]
    /// checks for — its *value* is not a subcommand and must be skipped
    /// over, or the live `-C` right after it goes unseen.
    #[test]
    fn unsafe_bash_directory_flag_still_finds_short_c_past_a_separate_value_option() {
        for command in [
            "git -c alias.v=version -C / v",
            "git --git-dir /somewhere -C / status",
            "env -u FOO -C / pwd",
            "env -S 'a b' -C / pwd",
        ] {
            assert!(unsafe_bash_directory_flag(command).is_some(), "{command:?}");
        }
    }

    /// `git --exec-path` is documented (and confirmed by running it) as an
    /// *optional*-argument long option (`--exec-path[=<path>]`, valid bare
    /// with no value and no error) — unlike `-c`/`--git-dir`/`--work-tree`/
    /// `--namespace`, each confirmed mandatory. Treating it as always
    /// consuming a separate word, the way an earlier version of
    /// `option_consumes_separate_value` did, mistook a real following `-C`
    /// for `--exec-path`'s own value and missed it entirely.
    #[test]
    fn unsafe_bash_directory_flag_does_not_swallow_c_after_bare_exec_path() {
        assert!(unsafe_bash_directory_flag("git --exec-path -C / status").is_some());
        assert_eq!(unsafe_bash_directory_flag("git --exec-path status"), None);
    }

    /// exec-reviewer's own probe (`env -iu FOO ...`, `timeout -vk 1 2
    /// ...`): a value-taking short option bundled together with other
    /// short flags in one GNU-getopt-style cluster (`-iu`, `-vk`) was
    /// previously matched only by its bare exact form (`-u`, `-k`), so the
    /// cluster was wrongly treated as a no-value flag that consumed
    /// nothing — silently hiding the real command word (and its own live
    /// `-C`) one position too early. `short_option_cluster_needs_separate_value`
    /// closes this by classifying the whole bundle, not just an exact
    /// match.
    #[test]
    fn unsafe_bash_directory_flag_finds_short_c_past_a_clustered_separate_value_option() {
        for command in [
            "env -iu FOO -C / git status",
            "timeout -vk 1 2 git -C / status",
            "env -iuFOO -C / git status",
        ] {
            assert!(unsafe_bash_directory_flag(command).is_some(), "{command:?}");
        }
    }

    /// The same clustering, with no `-C` anywhere, must still be allowed —
    /// `short_option_cluster_needs_separate_value` correctly recognizing a
    /// bundle must not turn into over-denial of the ordinary case.
    #[test]
    fn unsafe_bash_directory_flag_allows_ordinary_clustered_options_with_no_c() {
        for command in ["env -iu FOO sh -c 'true'", "timeout -vk 1 2 sh -c 'true'"] {
            assert_eq!(
                unsafe_bash_directory_flag(command),
                None,
                "{command:?} must not be denied"
            );
        }
    }

    /// An option this scan does not recognize at all — on `env` or
    /// `timeout`'s own leading-option grammar — must fail closed rather
    /// than be silently treated as a no-value flag that consumes nothing:
    /// treating an unrecognized option as "no value" is exactly the kind
    /// of miscounted skip that hid a live `-C` in the clustering bypass
    /// above.
    #[test]
    fn unsafe_bash_directory_flag_denies_an_unrecognized_option_on_env_or_timeout() {
        for command in ["env -zzz FOO cmd", "timeout --bogus 2 cmd"] {
            assert!(unsafe_bash_directory_flag(command).is_some(), "{command:?}");
        }
    }

    /// exec-reviewer's own regression probe (`env -a fake git --version`
    /// ran successfully, confirming `-a`/`--argv0=ARG` is a real,
    /// documented `env` option this scan had missed) found it denied as
    /// "unrecognized" by the fail-closed check above — safe, but an
    /// unnecessary denial on a legitimate option. Confirmed against the
    /// installed `env --help`: `-a` takes a mandatory value; the three
    /// signal-handling long options take only an optional, always-glued
    /// `=value` (the same shape `git --exec-path` has), never a separate
    /// word.
    #[test]
    fn unsafe_bash_directory_flag_recognizes_env_argv0_and_signal_options() {
        assert_eq!(unsafe_bash_directory_flag("env -a fake git status"), None);
        assert_eq!(
            unsafe_bash_directory_flag("env --block-signal=PIPE git status"),
            None
        );
        assert!(unsafe_bash_directory_flag("env -a fake -C / git status").is_some());
    }

    /// `tar`/`make` are deliberately not scoped to leading options only
    /// (see [`resolve_directory_flag`]'s `stop_at_first_positional` rule):
    /// `tar` in particular legitimately repeats `-C` after a non-option
    /// filename already went by, so this must still be caught in full.
    #[test]
    fn unsafe_bash_directory_flag_still_scans_tar_and_make_past_a_non_option_argument() {
        for command in ["tar -cf out.tar file1 -C dir2 file2", "make target -C dir"] {
            assert!(unsafe_bash_directory_flag(command).is_some(), "{command:?}");
        }
    }

    /// exec-reviewer's seventh-round finding: unquoted Bash pathname
    /// expansion (`*`, `?`, `[`) can make a token resolve, at Bash's own
    /// hand, to a path or program name this scan's literal, textual
    /// containment and basename checks never see — `/usr/bin/g[i]t`
    /// expands to `/usr/bin/git` in any shell that has that file, while
    /// never containing the literal substring `git` itself.
    #[test]
    fn decide_denies_unquoted_glob_metacharacters() {
        let (root, canonical_bare, canonical_git) = grove();
        for command in ["cat /usr/bin/g[i]t", "cat .b?re/config", "cat .bare/*"] {
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

    /// Glob metacharacters are inert, in either quote style, so ordinary
    /// quoted prose and data that happens to contain them must still be
    /// allowed — the same fail-closed-on-unquoted-only trade `$`/backtick
    /// already make, just gated on both quote flags instead of only single
    /// quotes since quoting of *either* kind suppresses pathname expansion.
    #[test]
    fn unsafe_bash_character_allows_glob_metacharacters_when_quoted() {
        for command in [
            r#"git commit -m "what is this?""#,
            r#"echo '[a-z]* matches lowercase'"#,
            r#"printf "%s" "a[b]c?d*e""#,
        ] {
            assert_eq!(
                unsafe_bash_character(command),
                None,
                "{command:?} must not be denied"
            );
        }
    }

    /// The same discovery as `decide_denies_unquoted_glob_metacharacters`,
    /// for Bash's two other unquoted-word expansions: brace expansion
    /// (`.{b,b}are/config` becomes the literal path `.bare/config`, twice
    /// over, while never containing that substring itself) and tilde
    /// expansion (substitutes `$HOME`/`$PWD`/`$OLDPWD`, none of which this
    /// scan has access to).
    #[test]
    fn decide_denies_unquoted_brace_and_tilde_expansion() {
        let (root, canonical_bare, canonical_git) = grove();
        for command in ["cat .b{,}are/config", "cat ~/.bare/config"] {
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

    /// exec-reviewer's own regression probe: `find ... -exec cmd {} \;`'s
    /// placeholder is an extremely common idiom, and Bash never expands a
    /// bare `{}` (or any single, comma-/range-free `{word}`) at all —
    /// unconditional denial on the mere presence of `{` was a real
    /// false-positive regression, the same shape as the tilde one.
    /// `looks_like_live_brace_expansion` must still catch the cases that
    /// really do expand.
    #[test]
    fn unsafe_bash_character_allows_non_expanding_braces_but_still_denies_live_ones() {
        for command in [
            r#"find . -name "*.txt" -exec cat {} \;"#,
            "echo {}",
            "echo {single}",
        ] {
            assert_eq!(
                unsafe_bash_character(command),
                None,
                "{command:?} must not be denied"
            );
        }
        for command in ["cat .b{,}are/config", "echo {1..5}", "echo {a..z}"] {
            assert!(
                unsafe_bash_character(command).is_some(),
                "{command:?} must be denied"
            );
        }
    }

    #[test]
    fn unsafe_bash_character_allows_brace_and_tilde_when_quoted() {
        for command in [r#"echo "a set of {choices}""#, r#"echo '~ marks the spot'"#] {
            assert_eq!(
                unsafe_bash_character(command),
                None,
                "{command:?} must not be denied"
            );
        }
    }

    /// exec-reviewer's own regression probe: a `~` only tilde-expands at
    /// the start of a word (or right after `=`/`:`) — `HEAD~1`, `HEAD~3`
    /// in ordinary git revision syntax is ordinary text with no shell
    /// meaning at all, and the first version of the tilde check (flagging
    /// `~` anywhere, unconditionally) denied these extremely common
    /// commands outright. Word-start tracking must both fix this and
    /// still catch a genuinely live tilde (word-start, or right after `=`
    /// in an assignment value).
    #[test]
    fn unsafe_bash_character_allows_tilde_mid_word_but_still_denies_it_at_a_word_boundary() {
        for command in [
            "git rev-parse HEAD~1",
            "git log HEAD~3..HEAD",
            "git diff HEAD~1 HEAD",
            "echo a~b",
        ] {
            assert_eq!(
                unsafe_bash_character(command),
                None,
                "{command:?} must not be denied"
            );
        }
        for command in [
            "cat ~/somewhere",
            "cat ~root/somewhere",
            "FOO=~/bar echo hi",
        ] {
            assert!(
                unsafe_bash_character(command).is_some(),
                "{command:?} must be denied"
            );
        }
    }

    /// exec-reviewer's own follow-up regression probe: Bash only expands a
    /// `~` right after a `:` when the *whole word* is itself a
    /// `NAME=value` assignment (`FOO=x:~`, `PATH`-like) — not in an
    /// ordinary argument that merely contains a colon. `scp host:~/file`
    /// is real, common remote-path syntax whose `~` does not expand
    /// (confirmed directly against Bash), and the first version of the
    /// `:`-boundary rule denied it and `echo x:~` outright regardless of
    /// assignment shape.
    #[test]
    fn unsafe_bash_character_only_treats_colon_as_a_tilde_boundary_inside_an_assignment_word() {
        for command in ["scp host:~/somewhere .", "echo x:~", "echo a:b:~c"] {
            assert_eq!(
                unsafe_bash_character(command),
                None,
                "{command:?} must not be denied"
            );
        }
        for command in ["FOO=x:~ echo hi", "FOO=a:b:~ echo hi"] {
            assert!(
                unsafe_bash_character(command).is_some(),
                "{command:?} must be denied"
            );
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

    /// exec-reviewer's own discovery: a git alias whose value starts with
    /// `!` runs its remainder as a shell command via `sh -c`, so `git -c
    /// "alias.x=!git -C / status" x` hides a real, later `-C` behind a
    /// leading token (`alias.x=!git`) that isn't a valid Bash assignment (a
    /// `.` in the name) and isn't a recognized redirection either —
    /// [`looks_like_unrecognized_prefix`]'s exact case. The bug was
    /// structural, not specific to git aliases:
    /// `command_word_index_in_segment` used to fold "segment empty" and
    /// "unrecognized prefix" into the same `None`, so
    /// `unsafe_bash_directory_flag`, when `unsafe_bash_construct` recurses
    /// it directly into a quoted token's own text, silently skipped the
    /// *entire* segment — including any real `-C` later in it — relying on
    /// an invariant ("already denied elsewhere") that only holds for a
    /// top-level call, not for this recursive one. Any quoted string with
    /// the same shape reproduces it, with no alias or `!` involved at all
    /// — tested directly against the quoted content's own text, the same
    /// way `unsafe_bash_construct`'s recursion invokes this function, not
    /// against the outer command (whose own `-c` legitimately consumes the
    /// whole quoted string as its own value, correctly, and is not the
    /// bug here).
    #[test]
    fn unsafe_bash_directory_flag_denies_rather_than_skips_an_unrecognized_prefix_hiding_a_later_c()
    {
        for quoted_content in ["alias.x=!git -C / status", "not.valid=weird -C / status"] {
            assert!(
                unsafe_bash_directory_flag(quoted_content).is_some(),
                "{quoted_content:?}"
            );
        }
    }

    #[test]
    fn decide_denies_a_git_alias_hiding_a_c_flag_behind_an_unrecognized_prefix() {
        let (root, canonical_bare, canonical_git) = grove();
        let payload = NormalizedPayload {
            tool: Tool::Bash {
                command: r#"git -c "alias.x=!git -C / status" x"#.to_string(),
            },
            cwd: Some(root.path().to_path_buf()),
        };
        let verdict = decide(&payload, &canonical_bare, &canonical_git, root.path());
        assert!(matches!(verdict, Verdict::Deny(_)), "{verdict:?}");
    }
}
