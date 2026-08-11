use crate::error::{GroveError, Result};
use bstr::ByteSlice;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArgKind {
    None,
    Required,
    Optional,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Effect {
    Other,
    Config,
    Depth,
    Dissociate,
    Origin,
    Reference,
    ReferenceIfAble,
    ShallowExclude,
    ShallowSince,
    SingleBranch,
}

#[derive(Clone, Copy)]
struct Alias {
    name: &'static str,
    enabled: bool,
}

#[derive(Clone, Copy)]
struct OptionSpec {
    canonical: &'static str,
    aliases: &'static [Alias],
    argument: ArgKind,
    negatable: bool,
    effect: Effect,
    refused: Option<&'static str>,
}

const fn alias(name: &'static str) -> Alias {
    Alias {
        name,
        enabled: true,
    }
}

const fn inverse(name: &'static str) -> Alias {
    Alias {
        name,
        enabled: false,
    }
}

const OPTIONS: &[OptionSpec] = &[
    OptionSpec {
        canonical: "verbose",
        aliases: &[alias("verbose")],
        argument: ArgKind::None,
        negatable: true,
        effect: Effect::Other,
        refused: None,
    },
    OptionSpec {
        canonical: "quiet",
        aliases: &[alias("quiet")],
        argument: ArgKind::None,
        negatable: true,
        effect: Effect::Other,
        refused: None,
    },
    OptionSpec {
        canonical: "progress",
        aliases: &[alias("progress")],
        argument: ArgKind::None,
        negatable: true,
        effect: Effect::Other,
        refused: None,
    },
    OptionSpec {
        canonical: "reject-shallow",
        aliases: &[alias("reject-shallow")],
        argument: ArgKind::None,
        negatable: true,
        effect: Effect::Other,
        refused: None,
    },
    OptionSpec {
        canonical: "checkout",
        aliases: &[alias("checkout"), inverse("no-checkout")],
        argument: ArgKind::None,
        negatable: true,
        effect: Effect::Other,
        refused: Some("a bare clone has no working tree to check out"),
    },
    OptionSpec {
        canonical: "bare",
        aliases: &[alias("bare")],
        argument: ArgKind::None,
        negatable: true,
        effect: Effect::Other,
        refused: Some("the grove already clones bare into .bare"),
    },
    OptionSpec {
        canonical: "mirror",
        aliases: &[alias("mirror")],
        argument: ArgKind::None,
        negatable: true,
        effect: Effect::Other,
        refused: Some("a mirror refspec can force-update branches checked out in worktrees"),
    },
    OptionSpec {
        canonical: "local",
        aliases: &[alias("local")],
        argument: ArgKind::None,
        negatable: true,
        effect: Effect::Other,
        refused: None,
    },
    OptionSpec {
        canonical: "hardlinks",
        aliases: &[alias("hardlinks"), inverse("no-hardlinks")],
        argument: ArgKind::None,
        negatable: true,
        effect: Effect::Other,
        refused: None,
    },
    OptionSpec {
        canonical: "shared",
        aliases: &[alias("shared")],
        argument: ArgKind::None,
        negatable: true,
        effect: Effect::Other,
        refused: Some("the grove's objects must not live in another repository"),
    },
    OptionSpec {
        canonical: "recurse-submodules",
        aliases: &[alias("recurse-submodules"), alias("recursive")],
        argument: ArgKind::Optional,
        negatable: true,
        effect: Effect::Other,
        refused: None,
    },
    OptionSpec {
        canonical: "jobs",
        aliases: &[alias("jobs")],
        argument: ArgKind::Required,
        negatable: true,
        effect: Effect::Other,
        refused: None,
    },
    OptionSpec {
        canonical: "template",
        aliases: &[alias("template")],
        argument: ArgKind::Required,
        negatable: true,
        effect: Effect::Other,
        refused: None,
    },
    OptionSpec {
        canonical: "reference",
        aliases: &[alias("reference")],
        argument: ArgKind::Required,
        negatable: true,
        effect: Effect::Reference,
        refused: None,
    },
    OptionSpec {
        canonical: "reference-if-able",
        aliases: &[alias("reference-if-able")],
        argument: ArgKind::Required,
        negatable: true,
        effect: Effect::ReferenceIfAble,
        refused: None,
    },
    OptionSpec {
        canonical: "dissociate",
        aliases: &[alias("dissociate")],
        argument: ArgKind::None,
        negatable: true,
        effect: Effect::Dissociate,
        refused: None,
    },
    OptionSpec {
        canonical: "origin",
        aliases: &[alias("origin")],
        argument: ArgKind::Required,
        negatable: true,
        effect: Effect::Origin,
        refused: None,
    },
    OptionSpec {
        canonical: "branch",
        aliases: &[alias("branch")],
        argument: ArgKind::Required,
        negatable: true,
        effect: Effect::Other,
        refused: Some("-b/--branch selects the first worktree of git-grove"),
    },
    OptionSpec {
        canonical: "upload-pack",
        aliases: &[alias("upload-pack")],
        argument: ArgKind::Required,
        negatable: true,
        effect: Effect::Other,
        refused: None,
    },
    OptionSpec {
        canonical: "depth",
        aliases: &[alias("depth")],
        argument: ArgKind::Required,
        negatable: true,
        effect: Effect::Depth,
        refused: None,
    },
    OptionSpec {
        canonical: "shallow-since",
        aliases: &[alias("shallow-since")],
        argument: ArgKind::Required,
        negatable: true,
        effect: Effect::ShallowSince,
        refused: None,
    },
    OptionSpec {
        canonical: "shallow-exclude",
        aliases: &[alias("shallow-exclude")],
        argument: ArgKind::Required,
        negatable: true,
        effect: Effect::ShallowExclude,
        refused: None,
    },
    OptionSpec {
        canonical: "single-branch",
        aliases: &[alias("single-branch")],
        argument: ArgKind::None,
        negatable: true,
        effect: Effect::SingleBranch,
        refused: None,
    },
    OptionSpec {
        canonical: "tags",
        aliases: &[alias("tags"), inverse("no-tags")],
        argument: ArgKind::None,
        negatable: true,
        effect: Effect::Other,
        refused: None,
    },
    OptionSpec {
        canonical: "shallow-submodules",
        aliases: &[alias("shallow-submodules")],
        argument: ArgKind::None,
        negatable: true,
        effect: Effect::Other,
        refused: None,
    },
    OptionSpec {
        canonical: "separate-git-dir",
        aliases: &[alias("separate-git-dir")],
        argument: ArgKind::Required,
        negatable: true,
        effect: Effect::Other,
        refused: Some("the repository must live in .bare"),
    },
    OptionSpec {
        canonical: "ref-format",
        aliases: &[alias("ref-format")],
        argument: ArgKind::Required,
        negatable: true,
        effect: Effect::Other,
        refused: None,
    },
    OptionSpec {
        canonical: "config",
        aliases: &[alias("config")],
        argument: ArgKind::Required,
        negatable: true,
        effect: Effect::Config,
        refused: None,
    },
    OptionSpec {
        canonical: "server-option",
        aliases: &[alias("server-option")],
        argument: ArgKind::Required,
        negatable: true,
        effect: Effect::Other,
        refused: None,
    },
    OptionSpec {
        canonical: "ipv4",
        aliases: &[alias("ipv4")],
        argument: ArgKind::None,
        negatable: false,
        effect: Effect::Other,
        refused: None,
    },
    OptionSpec {
        canonical: "ipv6",
        aliases: &[alias("ipv6")],
        argument: ArgKind::None,
        negatable: false,
        effect: Effect::Other,
        refused: None,
    },
    OptionSpec {
        canonical: "filter",
        aliases: &[alias("filter")],
        argument: ArgKind::Required,
        negatable: true,
        effect: Effect::Other,
        refused: None,
    },
    OptionSpec {
        canonical: "also-filter-submodules",
        aliases: &[alias("also-filter-submodules")],
        argument: ArgKind::None,
        negatable: true,
        effect: Effect::Other,
        refused: None,
    },
    OptionSpec {
        canonical: "remote-submodules",
        aliases: &[alias("remote-submodules")],
        argument: ArgKind::None,
        negatable: true,
        effect: Effect::Other,
        refused: None,
    },
    OptionSpec {
        canonical: "sparse",
        aliases: &[alias("sparse")],
        argument: ArgKind::None,
        negatable: true,
        effect: Effect::Other,
        refused: Some("a bare clone has no working tree to make sparse"),
    },
    OptionSpec {
        canonical: "bundle-uri",
        aliases: &[alias("bundle-uri")],
        argument: ArgKind::Required,
        negatable: true,
        effect: Effect::Other,
        refused: None,
    },
];

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Verdict {
    pub forwarded: Vec<OsString>,
    pub narrowed: bool,
    pub remote_name: OsString,
}

fn display_bytes(bytes: &[u8]) -> String {
    bytes.escape_bytes().to_string()
}

fn candidate_names(spec: &OptionSpec) -> impl Iterator<Item = (String, bool)> + '_ {
    spec.aliases.iter().flat_map(move |candidate| {
        let literal = std::iter::once((candidate.name.to_string(), candidate.enabled));
        let negated = spec
            .negatable
            .then(|| (format!("no-{}", candidate.name), !candidate.enabled));
        literal.chain(negated)
    })
}

fn resolve_long(name: &[u8]) -> Result<(&'static OptionSpec, bool)> {
    if name.is_empty() || !name.is_ascii() {
        return Err(GroveError::usage(format!(
            "`--{}` is not an ASCII git clone option name",
            display_bytes(name)
        )));
    }
    let name = std::str::from_utf8(name).expect("ASCII is UTF-8");

    let collect = |exact: bool| {
        let mut matches = Vec::<(usize, bool)>::new();
        for (index, spec) in OPTIONS.iter().enumerate() {
            for (candidate, enabled) in candidate_names(spec) {
                let matched = if exact {
                    candidate == name
                } else {
                    candidate.starts_with(name)
                };
                if matched && !matches.contains(&(index, enabled)) {
                    matches.push((index, enabled));
                }
            }
        }
        matches
    };

    let exact = collect(true);
    let matches = if exact.is_empty() {
        collect(false)
    } else {
        exact
    };
    match matches.as_slice() {
        [(index, enabled)] => Ok((&OPTIONS[*index], *enabled)),
        [] => Err(GroveError::usage(format!(
            "`--{name}` is not a git clone option git-grove knows"
        ))
        .with_detail(
            "git-grove refuses spellings it cannot classify rather than forwarding them blindly",
        )),
        _ => Err(GroveError::usage(format!("`--{name}` is ambiguous"))),
    }
}

fn option_by_canonical(canonical: &str) -> &'static OptionSpec {
    OPTIONS
        .iter()
        .find(|spec| spec.canonical == canonical)
        .expect("short option table names a known long option")
}

fn valid_remote_name(remote: &OsStr) -> bool {
    let remote = remote.as_bytes();
    if remote.is_empty()
        || remote.starts_with(b"/")
        || remote.starts_with(b"-")
        || remote.ends_with(b"/")
        || remote
            .windows(2)
            .any(|bytes| bytes == b"//" || bytes == b".." || bytes == b"@{")
        || remote.iter().any(|byte| {
            *byte <= b' '
                || *byte == 0x7f
                || matches!(*byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
    {
        return false;
    }

    remote
        .split(|byte| *byte == b'/')
        .all(|component| !component.starts_with(b".") && !component.ends_with(b".lock"))
}

fn check_config_assignment(assignment: &OsStr) -> Result<()> {
    let bytes = assignment.as_bytes();
    let Some(equals) = bytes.iter().position(|byte| *byte == b'=') else {
        return Err(GroveError::usage(format!(
            "`--config {}` is not a key=value assignment",
            display_bytes(bytes)
        )));
    };
    let key = &bytes[..equals];
    if key.is_empty()
        || !key.is_ascii()
        || key.first() == Some(&b'.')
        || key.last() == Some(&b'.')
        || !key.contains(&b'.')
        || key
            .iter()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(GroveError::usage(format!(
            "`--config {}` has a malformed key",
            display_bytes(bytes)
        )));
    }

    let key = key.to_ascii_lowercase();
    let remote_layout_key = key.starts_with(b"remote.")
        && [b".url".as_slice(), b".pushurl", b".fetch"]
            .iter()
            .any(|suffix| key.ends_with(suffix));
    if key == b"core.bare"
        || key == b"core.worktree"
        || key == b"extensions.worktreeconfig"
        || remote_layout_key
    {
        return Err(GroveError::usage(format!(
            "`--config {}` would rewrite the grove layout",
            display_bytes(bytes)
        )));
    }
    Ok(())
}

fn canonical_name(spec: &OptionSpec, enabled: bool) -> OsString {
    if enabled {
        OsString::from(format!("--{}", spec.canonical))
    } else {
        OsString::from(format!("--no-{}", spec.canonical))
    }
}

fn canonical_display(spec: &OptionSpec, enabled: bool) -> String {
    if enabled {
        format!("--{}", spec.canonical)
    } else {
        format!("--no-{}", spec.canonical)
    }
}

#[derive(Default)]
struct EffectiveState {
    depth: bool,
    dissociate: bool,
    origin: Option<OsString>,
    reference: bool,
    reference_if_able: bool,
    shallow_exclude: bool,
    shallow_since: bool,
    single_branch: Option<bool>,
}

impl EffectiveState {
    fn observe(&mut self, spec: &OptionSpec, enabled: bool, value: Option<&OsStr>) -> Result<()> {
        match spec.effect {
            Effect::Other => {}
            Effect::Config if enabled => {
                check_config_assignment(value.expect("enabled config requires a value"))?
            }
            Effect::Config => {}
            Effect::Depth => self.depth = enabled,
            Effect::Dissociate => self.dissociate = enabled,
            Effect::Origin if enabled => {
                self.origin = Some(
                    value
                        .expect("enabled origin requires a value")
                        .to_os_string(),
                )
            }
            Effect::Origin => self.origin = None,
            Effect::Reference => self.reference = enabled,
            Effect::ReferenceIfAble => self.reference_if_able = enabled,
            Effect::ShallowExclude => self.shallow_exclude = enabled,
            Effect::ShallowSince => self.shallow_since = enabled,
            Effect::SingleBranch => self.single_branch = Some(enabled),
        }
        Ok(())
    }
}

fn take_separate_value(
    options: &[OsString],
    index: &mut usize,
    option: &OsStr,
) -> Result<OsString> {
    *index += 1;
    options.get(*index).cloned().ok_or_else(|| {
        GroveError::usage(format!(
            "`{}` requires a value",
            display_bytes(option.as_bytes())
        ))
    })
}

fn process_option(
    spec: &'static OptionSpec,
    enabled: bool,
    inline: Option<OsString>,
    options: &[OsString],
    index: &mut usize,
    forwarded: &mut Vec<OsString>,
    state: &mut EffectiveState,
) -> Result<()> {
    if let Some(reason) = spec.refused {
        return Err(GroveError::usage(format!(
            "`--{}` contradicts the grove layout",
            spec.canonical
        ))
        .with_detail(reason));
    }

    let value = match (enabled, spec.argument, inline) {
        (false, _, Some(_)) | (true, ArgKind::None, Some(_)) => {
            return Err(GroveError::usage(format!(
                "`{}` takes no value",
                canonical_display(spec, enabled)
            )))
        }
        (false, _, None) | (true, ArgKind::None, None) | (true, ArgKind::Optional, None) => None,
        (true, ArgKind::Required, None) => Some(take_separate_value(
            options,
            index,
            OsStr::new(&canonical_name(spec, enabled)),
        )?),
        (true, ArgKind::Required | ArgKind::Optional, Some(value)) => Some(value),
    };

    state.observe(spec, enabled, value.as_deref())?;
    if spec.argument == ArgKind::Optional {
        let mut canonical = canonical_name(spec, enabled).into_vec();
        if let Some(value) = value {
            canonical.push(b'=');
            canonical.extend_from_slice(value.as_bytes());
        }
        forwarded.push(OsString::from_vec(canonical));
    } else {
        forwarded.push(canonical_name(spec, enabled));
        if let Some(value) = value {
            forwarded.push(value);
        }
    }
    Ok(())
}

pub fn classify(options: &[OsString]) -> Result<Verdict> {
    let mut forwarded = Vec::new();
    let mut state = EffectiveState::default();
    let mut index = 0;

    while index < options.len() {
        let raw = &options[index];
        let bytes = raw.as_bytes();
        if let Some(body) = bytes.strip_prefix(b"--") {
            if body.is_empty() {
                return Err(GroveError::usage(
                    "a forwarded `--` could replace git-grove's pinned clone operands",
                ));
            }
            let (name, inline) = match body.iter().position(|byte| *byte == b'=') {
                Some(equals) => (
                    &body[..equals],
                    Some(OsString::from_vec(body[equals + 1..].to_vec())),
                ),
                None => (body, None),
            };
            let (spec, enabled) = resolve_long(name)?;
            process_option(
                spec,
                enabled,
                inline,
                options,
                &mut index,
                &mut forwarded,
                &mut state,
            )?;
        } else if bytes.starts_with(b"-") && bytes.len() >= 2 {
            let (canonical, takes_value) = match bytes[1] {
                b'v' => ("verbose", false),
                b'q' => ("quiet", false),
                b'n' => ("checkout", false),
                b'l' => ("local", false),
                b's' => ("shared", false),
                b'j' => ("jobs", true),
                b'o' => ("origin", true),
                b'b' => ("branch", true),
                b'u' => ("upload-pack", true),
                b'c' => ("config", true),
                b'4' => ("ipv4", false),
                b'6' => ("ipv6", false),
                _ => {
                    return Err(GroveError::usage(format!(
                        "`{}` is not a supported git clone short option",
                        display_bytes(bytes)
                    )))
                }
            };
            if !takes_value && bytes.len() != 2 {
                return Err(GroveError::usage(format!(
                    "short option clusters such as `{}` are not forwarded",
                    display_bytes(bytes)
                )));
            }
            let inline = (bytes.len() > 2).then(|| OsString::from_vec(bytes[2..].to_vec()));
            process_option(
                option_by_canonical(canonical),
                true,
                inline,
                options,
                &mut index,
                &mut forwarded,
                &mut state,
            )?;
        } else {
            return Err(GroveError::usage(format!(
                "forwarded clone operand `{}` is not allowed",
                display_bytes(bytes)
            ))
            .with_detail("git-grove pins the repository URL and .bare destination separately"));
        }
        index += 1;
    }

    if (state.reference || state.reference_if_able) && !state.dissociate {
        return Err(GroveError::usage("--reference requires --dissociate").with_detail(
            "a grove whose objects live in another repository breaks when that repository is pruned",
        ));
    }

    let remote_name = state.origin.unwrap_or_else(|| OsString::from("origin"));
    if !valid_remote_name(&remote_name) {
        return Err(GroveError::usage(format!(
            "`{}` is not a safe remote name",
            display_bytes(remote_name.as_bytes())
        )));
    }

    let narrowed = match state.single_branch {
        Some(enabled) => enabled,
        None => state.depth || state.shallow_since || state.shallow_exclude,
    };
    Ok(Verdict {
        forwarded,
        narrowed,
        remote_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ExitClass;
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::process::Command;
    use tempfile::TempDir;

    fn opts(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn strings(args: &[&str]) -> Vec<OsString> {
        opts(args)
    }

    fn assert_forwarded(input: &[&str], expected: &[&str]) -> Verdict {
        let verdict = classify(&opts(input)).unwrap_or_else(|error| {
            panic!("classifying {input:?} failed: {error}");
        });
        assert_eq!(verdict.forwarded, strings(expected), "input {input:?}");
        verdict
    }

    #[test]
    fn classifies_every_git_2_47_long_option_and_negation() {
        let accepted: &[(&[&str], &[&str])] = &[
            (&["--verbose"], &["--verbose"]),
            (&["--no-verbose"], &["--no-verbose"]),
            (&["--quiet"], &["--quiet"]),
            (&["--no-quiet"], &["--no-quiet"]),
            (&["--progress"], &["--progress"]),
            (&["--no-progress"], &["--no-progress"]),
            (&["--reject-shallow"], &["--reject-shallow"]),
            (&["--no-reject-shallow"], &["--no-reject-shallow"]),
            (&["--local"], &["--local"]),
            (&["--no-local"], &["--no-local"]),
            (&["--hardlinks"], &["--hardlinks"]),
            (&["--no-hardlinks"], &["--no-hardlinks"]),
            (&["--no-no-hardlinks"], &["--hardlinks"]),
            (&["--recurse-submodules"], &["--recurse-submodules"]),
            (&["--recurse-submodules=lib"], &["--recurse-submodules=lib"]),
            (&["--no-recurse-submodules"], &["--no-recurse-submodules"]),
            (&["--recursive=lib"], &["--recurse-submodules=lib"]),
            (&["--no-recursive"], &["--no-recurse-submodules"]),
            (&["--jobs", "2"], &["--jobs", "2"]),
            (&["--no-jobs"], &["--no-jobs"]),
            (&["--template=/tmp/t"], &["--template", "/tmp/t"]),
            (&["--no-template"], &["--no-template"]),
            (&["--dissociate"], &["--dissociate"]),
            (&["--no-dissociate"], &["--no-dissociate"]),
            (&["--origin", "upstream"], &["--origin", "upstream"]),
            (&["--no-origin"], &["--no-origin"]),
            (
                &["--upload-pack=/bin/git-upload-pack"],
                &["--upload-pack", "/bin/git-upload-pack"],
            ),
            (&["--no-upload-pack"], &["--no-upload-pack"]),
            (&["--depth=1"], &["--depth", "1"]),
            (&["--no-depth"], &["--no-depth"]),
            (
                &["--shallow-since", "yesterday"],
                &["--shallow-since", "yesterday"],
            ),
            (&["--no-shallow-since"], &["--no-shallow-since"]),
            (
                &["--shallow-exclude=main~1"],
                &["--shallow-exclude", "main~1"],
            ),
            (&["--no-shallow-exclude"], &["--no-shallow-exclude"]),
            (&["--single-branch"], &["--single-branch"]),
            (&["--no-single-branch"], &["--no-single-branch"]),
            (&["--tags"], &["--tags"]),
            (&["--no-tags"], &["--no-tags"]),
            (&["--no-no-tags"], &["--tags"]),
            (&["--shallow-submodules"], &["--shallow-submodules"]),
            (&["--no-shallow-submodules"], &["--no-shallow-submodules"]),
            (&["--ref-format=files"], &["--ref-format", "files"]),
            (&["--no-ref-format"], &["--no-ref-format"]),
            (
                &["--config", "http.proxy=http://p"],
                &["--config", "http.proxy=http://p"],
            ),
            (&["--no-config"], &["--no-config"]),
            (&["--server-option=x"], &["--server-option", "x"]),
            (&["--no-server-option"], &["--no-server-option"]),
            (&["--ipv4"], &["--ipv4"]),
            (&["--ipv6"], &["--ipv6"]),
            (&["--filter=blob:none"], &["--filter", "blob:none"]),
            (&["--no-filter"], &["--no-filter"]),
            (&["--also-filter-submodules"], &["--also-filter-submodules"]),
            (
                &["--no-also-filter-submodules"],
                &["--no-also-filter-submodules"],
            ),
            (&["--remote-submodules"], &["--remote-submodules"]),
            (&["--no-remote-submodules"], &["--no-remote-submodules"]),
            (
                &["--bundle-uri=https://example.invalid/b"],
                &["--bundle-uri", "https://example.invalid/b"],
            ),
            (&["--no-bundle-uri"], &["--no-bundle-uri"]),
        ];
        for (input, expected) in accepted {
            assert_forwarded(input, expected);
        }

        for input in [
            "--checkout",
            "--no-checkout",
            "--no-no-checkout",
            "--bare",
            "--no-bare",
            "--mirror",
            "--no-mirror",
            "--shared",
            "--no-shared",
            "--branch=main",
            "--no-branch",
            "--separate-git-dir=/tmp/git",
            "--no-separate-git-dir",
            "--sparse",
            "--no-sparse",
        ] {
            let error = classify(&opts(&[input])).unwrap_err();
            assert_eq!(error.class, ExitClass::Usage, "accepted {input}");
        }

        for input in ["--no-ipv4", "--no-ipv6"] {
            assert_eq!(
                classify(&opts(&[input])).unwrap_err().class,
                ExitClass::Usage
            );
        }

        assert_forwarded(
            &["--reference=/other", "--dissociate"],
            &["--reference", "/other", "--dissociate"],
        );
        assert_forwarded(
            &["--reference-if-able", "/other", "--dissociate"],
            &["--reference-if-able", "/other", "--dissociate"],
        );
    }

    #[test]
    fn resolves_exact_names_semantic_aliases_and_unique_abbreviations() {
        assert_forwarded(&["--verb"], &["--verbose"]);
        assert_forwarded(&["--rec=lib"], &["--recurse-submodules=lib"]);
        assert_forwarded(&["--no-rec"], &["--no-recurse-submodules"]);
        assert_forwarded(&["--hard"], &["--hardlinks"]);
        assert_forwarded(&["--no-hard"], &["--no-hardlinks"]);
        assert_forwarded(&["--no-no-hard"], &["--hardlinks"]);
        assert_forwarded(&["--tag"], &["--tags"]);
        assert_forwarded(&["--no-tag"], &["--no-tags"]);

        for input in [
            "--s",
            "--sh",
            "--shallow-s",
            "--re",
            "--ref",
            "--refer",
            "--ip",
        ] {
            let error = classify(&opts(&[input])).unwrap_err();
            assert_eq!(error.class, ExitClass::Usage, "accepted {input}");
            assert!(error.message.contains("ambiguous"), "{error}");
        }
    }

    #[test]
    fn keeps_optional_pathspecs_attached_in_canonical_and_real_git_argv() {
        let ascii_verdict = classify(&opts(&["--rec=lib"])).unwrap();
        assert_eq!(ascii_verdict.forwarded, opts(&["--recurse-submodules=lib"]));

        let raw = OsString::from_vec(b"--recurse-submodules=lib-\xff".to_vec());
        let raw_verdict = classify(&[raw]).unwrap();
        assert_eq!(
            raw_verdict.forwarded[0].as_bytes(),
            b"--recurse-submodules=lib-\xff"
        );

        let sandbox = TempDir::new().unwrap();
        let origin = sandbox.path().join("origin.git");
        let target = sandbox.path().join("clone");
        let path = std::env::var_os("PATH").expect("PATH must be set for the Git probe");
        let configure = |command: &mut Command| {
            command
                .env_clear()
                .env("PATH", &path)
                .env("HOME", sandbox.path())
                .env("XDG_CONFIG_HOME", sandbox.path().join("config"))
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("LC_ALL", "C");
        };

        let mut init = Command::new("git");
        configure(&mut init);
        let init = init
            .args(["init", "--quiet", "--bare"])
            .arg(&origin)
            .output()
            .unwrap();
        assert!(init.status.success(), "{}", display_bytes(&init.stderr));

        let mut clone = Command::new("git");
        configure(&mut clone);
        let clone = clone
            .arg("clone")
            .args(&ascii_verdict.forwarded)
            .arg("--")
            .arg(&origin)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            clone.status.success(),
            "canonical optional argument was rejected by Git: {}",
            display_bytes(&clone.stderr)
        );
    }

    #[test]
    fn refuses_all_layout_spellings_even_when_negated_or_abbreviated() {
        for input in [
            "--bar",
            "--no-bar",
            "--mirr",
            "--no-mirr",
            "--spar",
            "--no-spar",
            "--check",
            "--no-check",
            "--no-no-check",
            "--sep=/tmp/git",
            "--no-sep",
            "--sha",
        ] {
            let error = classify(&opts(&[input])).unwrap_err();
            assert_eq!(error.class, ExitClass::Usage, "accepted {input}");
        }
    }

    #[test]
    fn handles_long_and_short_values_without_accepting_operand_injection() {
        assert_forwarded(&["--filter", "blob:none"], &["--filter", "blob:none"]);
        assert_forwarded(&["--filter=blob:none"], &["--filter", "blob:none"]);
        assert_forwarded(&["--filter="], &["--filter", ""]);
        assert_forwarded(&["-j", "2"], &["--jobs", "2"]);
        assert_forwarded(&["-j2"], &["--jobs", "2"]);
        assert_forwarded(&["-o", "up"], &["--origin", "up"]);
        assert_forwarded(&["-oup"], &["--origin", "up"]);
        assert_forwarded(
            &["-u/path/git-upload-pack"],
            &["--upload-pack", "/path/git-upload-pack"],
        );
        assert_forwarded(&["-c", "http.proxy=x"], &["--config", "http.proxy=x"]);
        assert_forwarded(&["-chttp.proxy=x"], &["--config", "http.proxy=x"]);
        for (input, expected) in [
            ("-v", "--verbose"),
            ("-q", "--quiet"),
            ("-l", "--local"),
            ("-4", "--ipv4"),
            ("-6", "--ipv6"),
        ] {
            assert_forwarded(&[input], &[expected]);
        }

        for input in [
            &["--jobs"][..],
            &["--template"][..],
            &["--reference"][..],
            &["--reference-if-able"][..],
            &["--origin"][..],
            &["--upload-pack"][..],
            &["--depth"][..],
            &["--shallow-since"][..],
            &["--shallow-exclude"][..],
            &["--ref-format"][..],
            &["--config"][..],
            &["--server-option"][..],
            &["--filter"][..],
            &["--bundle-uri"][..],
            &["--quiet=x"][..],
            &["--no-filter=x"][..],
            &["--recurse-submodules", "lib"][..],
            &["--", "--mirror"][..],
            &["main"][..],
            &["-vq"][..],
            &["-ql"][..],
            &["-46"][..],
            &["-x"][..],
        ] {
            assert_eq!(
                classify(&opts(input)).unwrap_err().class,
                ExitClass::Usage,
                "accepted {input:?}"
            );
        }

        for input in ["-n", "-s", "-b", "-bmain"] {
            assert_eq!(
                classify(&opts(&[input])).unwrap_err().class,
                ExitClass::Usage,
                "accepted layout short {input}"
            );
        }
    }

    #[test]
    fn computes_effective_narrowing_after_cancellations() {
        for input in [
            &["--single-branch"][..],
            &["--depth", "1"][..],
            &["--shallow-since=yesterday"][..],
            &["--shallow-exclude", "old"][..],
            &["--depth", "1", "--single-branch"][..],
        ] {
            assert!(
                classify(&opts(input)).unwrap().narrowed,
                "not narrowed: {input:?}"
            );
        }
        for input in [
            &[][..],
            &["--depth", "1", "--no-depth"][..],
            &["--depth", "1", "--no-single-branch"][..],
            &["--no-single-branch", "--depth", "1"][..],
            &["--single-branch", "--no-single-branch"][..],
            &["--shallow-since=x", "--no-shallow-since"][..],
            &["--shallow-exclude=x", "--no-shallow-exclude"][..],
        ] {
            assert!(
                !classify(&opts(input)).unwrap().narrowed,
                "narrowed: {input:?}"
            );
        }
    }

    #[test]
    fn requires_effective_dissociation_for_effective_references() {
        for input in [
            &["--reference", "/other"][..],
            &["--reference-if-able=/other"][..],
            &["--reference=/other", "--dissociate", "--no-dissociate"][..],
        ] {
            assert_eq!(classify(&opts(input)).unwrap_err().class, ExitClass::Usage);
        }
        for input in [
            &["--reference=/other", "--dissociate"][..],
            &["--reference=/other", "--no-reference"][..],
            &["--reference-if-able=/other", "--no-reference-if-able"][..],
            &[
                "--reference=/a",
                "--no-reference",
                "--reference=/b",
                "--dissociate",
            ][..],
        ] {
            classify(&opts(input)).unwrap_or_else(|error| panic!("rejected {input:?}: {error}"));
        }
    }

    #[test]
    fn validates_config_assignments_without_case_or_subsection_evasions() {
        for assignment in [
            "core.bare=false",
            "CORE.WORKTREE=/tmp/w",
            "Extensions.WorktreeConfig=true",
            "remote.origin.url=x",
            "REMOTE.up.PushUrl=x",
            "remote.team.origin.fetch=+refs/x:refs/y",
            "remote.\"odd.name\".URL=x",
        ] {
            let error = classify(&opts(&["--config", assignment])).unwrap_err();
            assert_eq!(error.class, ExitClass::Usage, "accepted {assignment}");
        }
        for malformed in ["", "novariable", "=value", "section.=value", ".name=value"] {
            let error = classify(&opts(&["--config", malformed])).unwrap_err();
            assert_eq!(error.class, ExitClass::Usage, "accepted {malformed:?}");
        }
        assert_forwarded(&["-c", "http.proxy="], &["--config", "http.proxy="]);
        assert_forwarded(&["--no-config"], &["--no-config"]);
    }

    #[test]
    fn keeps_the_last_safe_remote_name_as_raw_bytes() {
        let verdict = classify(&opts(&["--origin=first", "-osecond", "--no-origin"])).unwrap();
        assert_eq!(verdict.remote_name, OsString::from("origin"));
        assert_forwarded(&["--origin=team/backend"], &["--origin", "team/backend"]);

        for remote in [
            "",
            ".",
            "..",
            "/bad",
            "bad/",
            "bad..name",
            "bad name",
            "bad@{name",
        ] {
            let mut input = vec![OsString::from("--origin"), OsString::from(remote)];
            let error = classify(&input).unwrap_err();
            assert_eq!(error.class, ExitClass::Usage, "accepted {remote:?}");
            input.clear();
        }

        let raw = OsString::from_vec(b"team-\xff".to_vec());
        let verdict = classify(&[OsString::from("--origin"), raw.clone()]).unwrap();
        assert_eq!(verdict.remote_name.as_bytes(), raw.as_bytes());
        assert_eq!(verdict.forwarded[1].as_bytes(), raw.as_bytes());
    }

    #[test]
    fn refuses_remote_names_that_can_be_reparsed_as_options() {
        for input in [
            &["--origin=-help"][..],
            &["--origin", "--help"][..],
            &["-o-help"][..],
            &["-o", "--help"][..],
        ] {
            let error = classify(&opts(input)).unwrap_err();
            assert_eq!(error.class, ExitClass::Usage, "accepted {input:?}");
            assert!(error.message.contains("-help"), "{error}");
        }

        let raw = OsString::from_vec(b"--origin=-\xff".to_vec());
        let error = classify(&[raw]).unwrap_err();
        assert_eq!(error.class, ExitClass::Usage);
        assert!(error.message.contains(r"-\xFF"), "{error}");
    }

    #[test]
    fn preserves_non_utf8_values_and_rejects_non_ascii_option_names_reversibly() {
        let raw = OsString::from_vec(b"blob:\xff".to_vec());
        let verdict = classify(&[OsString::from("--filter"), raw.clone()]).unwrap();
        assert_eq!(verdict.forwarded[1].as_bytes(), raw.as_bytes());

        let error = classify(&[OsString::from_vec(b"--filt\xff=x".to_vec())]).unwrap_err();
        assert_eq!(error.class, ExitClass::Usage);
        assert!(error.message.contains(r"\xFF"), "{}", error.message);
    }
}
