//! `git grove publish <url>` — give an unpublished grove a remote and push it.
//!
//! Publication is a three-state transaction: `unpublished` -> `publishing` ->
//! `published`. Its durable record is the receipt in the grove configuration
//! (`grove.publishState`, `grove.publishRemote`, `grove.publishUrl`), written
//! in one step immediately before the first step that **mutates** the remote or
//! the local remote configuration.
//!
//! That is narrower than "before the first step that touches the remote", and
//! deliberately so. A receipt has no reader during the read-only inspection:
//! its only function is to let a *later* run reconcile durable state, and the
//! inspection creates none. Writing it earlier would make a mistyped URL
//! poison the grove permanently — this release ships no `--abort`, and the
//! acceptance matrix would then refuse the corrected URL — leaving only
//! `git config --unset` surgery inside the one directory the grove contract
//! tells everyone never to touch. Every exit path out of the inspection stage
//! therefore leaves `grove.publishState` untouched.
//!
//! URL and remote-name comparison is **exact byte equality**. No
//! canonicalisation of any kind: not trailing slashes, not `.git` suffixes, not
//! scheme or case. A false "equal" would let a rerun adopt a different
//! publication target; a false "conflict" lands exactly on what exit `2` means.

use crate::error::{ExitClass, GroveError, Result};
use crate::git::config::{
    config_key, config_values, configure_upstreams, escaped, list_local_heads, remote_head_branch,
    required, set_config, trim_one_line, validate_refspec_destinations,
};
use crate::git::fetch::FetchPlan;
use crate::git::provider::{self, Provider, ProviderOutput, ProviderRunner};
use crate::git::query;
use crate::git::remote::{self, Ancestry};
use crate::git::runner::{GitRunner, Invocation};
use crate::grove::discover::Grove;
use crate::grove::metadata::{self, Metadata, PublishState, Receipt};
use bstr::BString;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

/// What the user asked for. Raw bytes throughout; never parsed or normalised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub url: OsString,
    pub remote: OsString,
    pub all_branches: bool,
}

/// What the durable state says this run is: a first attempt, the continuation
/// of an interrupted one, or a repair of a completed one.
// `ResumePublishing` repeats the type name by design: it is the plan's
// published interface for this decision, and renaming it here would make the
// code and the plan disagree.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resume {
    Fresh,
    ResumePublishing,
    RepairPublished,
}

/// Publishing has no per-worktree state to render, so the report is a small
/// ordered list of lines rather than `list`/`sync` rows. There is no porcelain
/// output for `publish` in 0.3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishReport {
    pub class: ExitClass,
    pub lines: Vec<String>,
    pub diagnostics: Vec<String>,
}

/// The byte string recorded in `grove.publishProvider` for each provider —
/// the same spelling `--host <github|gitlab>` accepts, not the CLI binary
/// name (`gh`/`glab`).
fn provider_slug(provider: Provider) -> &'static [u8] {
    match provider {
        Provider::GitHub => b"github",
        Provider::GitLab => b"gitlab",
    }
}

fn conflict(message: impl Into<String>, detail: impl Into<String>) -> GroveError {
    GroveError::needs_decision(message).with_detail(detail)
}

/// Decide, from durable state alone and before anything is mutated, whether
/// this run may proceed and as what.
///
/// `live_remote_url` is the current value of `remote.<name>.url`, read from the
/// grove configuration. Every refusal names both values it compared, escaped.
pub fn accept_rerun(
    metadata: &Metadata,
    live_remote_url: Option<&BString>,
    request: &Request,
) -> Result<Resume> {
    let receipt = metadata::receipt(metadata)?;
    let requested_remote = request.remote.as_bytes();
    let requested_url = request.url.as_bytes();

    match (metadata.publish_state, receipt) {
        (PublishState::Unpublished, _) => match live_remote_url {
            None => Ok(Resume::Fresh),
            Some(live) => Err(conflict(
                format!(
                    "a remote named {} already exists in this grove",
                    escaped(requested_remote)
                ),
                format!(
                    "it points at {}; this grove records no publication, so publishing would adopt a remote it did not create",
                    escaped(live)
                ),
            )),
        },

        // `published` with no receipt is a grove created by `git grove clone`,
        // including every grove created by the shipped v0.2.0. It is not torn;
        // see `metadata::receipt`.
        (PublishState::Published, None) => match live_remote_url {
            Some(live) => Err(conflict(
                "this grove is already published",
                format!(
                    "{} points at {}; `publish` gives an *unpublished* grove a remote",
                    escaped(requested_remote),
                    escaped(live)
                ),
            )),
            // The remote was removed by hand. Refusing would leave no 0.3
            // command able to repair it, so treat it as a first publication.
            None => Ok(Resume::Fresh),
        },

        (state, Some(recorded)) => {
            if recorded.remote != requested_remote {
                return Err(conflict(
                    "this grove records a publication to a different remote name",
                    format!(
                        "recorded {}, requested {}",
                        escaped(&recorded.remote),
                        escaped(requested_remote)
                    ),
                ));
            }
            if recorded.url != requested_url {
                return Err(conflict(
                    "this grove records a publication to a different URL",
                    format!(
                        "recorded {}, requested {}",
                        escaped(&recorded.url),
                        escaped(requested_url)
                    ),
                ));
            }
            match (state, live_remote_url) {
                (PublishState::Publishing, None) => Ok(Resume::ResumePublishing),
                (PublishState::Publishing, Some(live)) if *live == recorded.url => {
                    Ok(Resume::ResumePublishing)
                }
                (PublishState::Published, Some(live)) if *live == recorded.url => {
                    Ok(Resume::RepairPublished)
                }
                (_, Some(live)) => Err(conflict(
                    "the configured remote does not match this grove's publication receipt",
                    format!(
                        "receipt records {}, {} points at {}",
                        escaped(&recorded.url),
                        escaped(requested_remote),
                        escaped(live)
                    ),
                )),
                (PublishState::Published, None) => Err(conflict(
                    "this grove records a completed publication but has no configured remote",
                    format!(
                        "the receipt records {} at {}",
                        escaped(&recorded.remote),
                        escaped(&recorded.url)
                    ),
                )),
                (PublishState::Unpublished, _) => {
                    unreachable!("an unpublished state carrying a receipt is rejected by receipt()")
                }
                // `metadata::receipt` rejects a `creating` grove that also
                // carries a classic URL as torn (`Err`, never `Ok(Some(_))`),
                // so this arm should not be reachable from a well-formed
                // grove — but it is not `unreachable!()`: a malformed
                // `.bare/config` edited by hand can still reach it, and a
                // decision this file already treats as unconditionally exit
                // `2` deserves the same treatment here rather than a panic.
                (PublishState::Creating, None) => Err(GroveError::failure(
                    "this grove's publish state is creating but its metadata is inconsistent",
                )
                .with_detail(
                    "a creating grove must never carry a classic publication receipt; this should be rejected by metadata::receipt",
                )),
            }
        }

        (PublishState::Publishing, None) => {
            unreachable!("publishing without a receipt is rejected by receipt()")
        }

        // `creating` sits strictly before a remote exists. `metadata::receipt`
        // always returns `None` here (a creating grove has no classic URL
        // yet), so a bare `publish <url>` never silently resumes or
        // reinterprets an in-flight `--create`: it refuses and points at the
        // command that owns this state.
        (PublishState::Creating, None) => {
            let creating = metadata::creating_receipt(metadata)?.ok_or_else(|| {
                GroveError::failure(
                    "this grove's publish state is creating but its creation receipt is missing",
                )
            })?;
            Err(conflict(
                format!(
                    "this grove is already creating a repository on {}",
                    escaped(&creating.provider)
                ),
                format!(
                    "recorded {}/{}; finish or abandon it with `git grove publish --create {}/{}`, not a bare `publish <url>`",
                    escaped(&creating.owner),
                    escaped(&creating.name),
                    escaped(&creating.owner),
                    escaped(&creating.name)
                ),
            ))
        }
    }
}

/// What the preflight established, before anything was mutated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preflight {
    /// The short branch name, raw bytes.
    pub default_branch: BString,
}

impl Preflight {
    /// `refs/heads/<default branch>`.
    fn default_ref(&self) -> Vec<u8> {
        let mut reference = b"refs/heads/".to_vec();
        reference.extend_from_slice(&self.default_branch);
        reference
    }
}

/// Validate the request and the grove before touching anything.
///
/// The two argument checks are decided first, because nothing may mutate before
/// an exit `64` is decided and they are the cheapest way to reach one.
fn preflight(
    runner: &dyn GitRunner,
    grove: &Grove,
    metadata: &Metadata,
    request: &Request,
) -> Result<Preflight> {
    if request.url.as_bytes().is_empty() {
        return Err(GroveError::usage("`publish` requires a URL"));
    }
    validate_remote_name(runner, &request.remote)?;
    check_publishable_repository(runner, grove, metadata)
}

/// The URL-independent half of [`preflight`]: this grove has a commit and a
/// default branch it can push, regardless of whether it has a remote yet.
/// `--create` runs this before ever touching a provider, with no URL in
/// hand at all.
fn check_publishable_repository(
    runner: &dyn GitRunner,
    grove: &Grove,
    metadata: &Metadata,
) -> Result<Preflight> {
    if !query::has_any_commit(runner, grove)? {
        return Err(conflict(
            "this grove has no commit to publish",
            "commit on the default branch first, then run `git grove publish <url>` again",
        ));
    }

    let default_branch = match &metadata.default_branch {
        Some(branch) => branch.clone(),
        None => derive_default_branch(runner, grove)?,
    };

    let branch = OsString::from_vec(default_branch.to_vec());
    if !query::local_branch_exists(runner, grove, &branch)? {
        return Err(conflict(
            format!(
                "this grove's default branch {} does not exist",
                escaped(&default_branch)
            ),
            "set grove.defaultBranch to a branch this grove has, or create that branch",
        ));
    }

    Ok(Preflight { default_branch })
}

/// Delegate the remote-name rule to git rather than re-implementing it.
///
/// `git remote add` accepts exactly the names for which
/// `refs/remotes/<name>/HEAD` is a valid ref name — measured to agree on every
/// case tried, including the ones that matter here: `a/b` is valid and must be
/// accepted, an empty name is not, and a name beginning with `-` is valid and
/// must be passed after `--`.
fn validate_remote_name(runner: &dyn GitRunner, remote: &OsStr) -> Result<()> {
    let mut candidate = b"refs/remotes/".to_vec();
    candidate.extend_from_slice(remote.as_bytes());
    candidate.extend_from_slice(b"/HEAD");
    // `check-ref-format` takes its refname positionally and accepts neither
    // `--` nor `--end-of-options` (both are exit 129). No guard is needed: the
    // argument always begins `refs/remotes/`, so it can never look like an
    // option however the remote name is spelled.
    let output = runner.run(Invocation::new().args([
        OsStr::new("check-ref-format"),
        OsStr::from_bytes(&candidate),
    ]))?;
    if output.ok() {
        Ok(())
    } else {
        Err(GroveError::usage(format!(
            "{} is not a valid remote name",
            escaped(remote.as_bytes())
        )))
    }
}

/// Derive the default branch from the bare repository's `HEAD`, and only when
/// the derivation is unambiguous.
fn derive_default_branch(runner: &dyn GitRunner, grove: &Grove) -> Result<BString> {
    let output = runner.run(Invocation::new().git_dir(grove.bare_dir()).args([
        "symbolic-ref",
        "--quiet",
        "HEAD",
    ]))?;
    let ambiguous = || {
        conflict(
            "this grove does not record a default branch",
            "set grove.defaultBranch in the grove configuration, then publish again",
        )
    };
    if !output.ok() {
        return Err(ambiguous());
    }
    let target = trim_one_line(output.stdout, "HEAD symref")?;
    let Some(branch) = target.strip_prefix(b"refs/heads/") else {
        return Err(ambiguous());
    };
    if branch.is_empty() {
        return Err(ambiguous());
    }
    Ok(BString::from(branch.to_vec()))
}

/// Specification steps 2 and 3: inspect the target read-only, and decide.
///
/// Leaves nothing durable behind on any path — no `FETCH_HEAD`, no surviving
/// reflog, and no probe ref — so a refusal here is perfectly recoverable by
/// retrying with a corrected URL.
fn inspect_target(
    runner: &dyn GitRunner,
    grove: &Grove,
    request: &Request,
    preflight: &Preflight,
    diagnostics: &mut Vec<String>,
) -> Result<()> {
    for purged in remote::purge_probe_refs(runner, grove)? {
        diagnostics.push(format!(
            "removed a leftover publication probe ref {}",
            escaped(&purged)
        ));
    }

    let advert = remote::advertise(runner, &request.url)?;
    if advert.empty {
        // Measured: an empty repository advertises nothing at all, so there is
        // nothing to compare and nothing to probe. Its intended default branch
        // is invisible until something is pushed, which is exactly why the
        // server-side `HEAD` verification exists.
        return Ok(());
    }

    let default_ref = preflight.default_ref();
    match &advert.head_symref {
        Some(symref) if symref.as_slice() == default_ref.as_slice() => {}
        Some(symref) => {
            return Err(conflict(
                "the publication target's default branch is not this grove's",
                format!(
                    "the target's HEAD names {}, this grove's default branch is {}",
                    escaped(symref),
                    escaped(&default_ref)
                ),
            ))
        }
        None => {
            return Err(conflict(
                "the publication target does not resolve its default branch",
                format!(
                    "it advertises no HEAD symref, so it cannot be confirmed to use {}",
                    escaped(&default_ref)
                ),
            ))
        }
    }

    if advert.head_oid_for(&preflight.default_branch).is_none() {
        return Err(conflict(
            "the publication target does not have this grove's default branch",
            format!(
                "it does not advertise {}, so it cannot be compared with this grove",
                escaped(&default_ref)
            ),
        ));
    }

    let probe = remote::fetch_probe(runner, grove, &request.url, &preflight.default_branch)?;
    let verdict = remote::is_ancestor(runner, grove, &probe.name, &default_ref);
    let deleted = remote::delete_probe_ref(runner, grove, &probe.name);
    let verdict = verdict?;
    deleted?;

    match verdict {
        Ancestry::Ancestor => Ok(()),
        Ancestry::NotAncestor => Err(conflict(
            "the publication target has diverged from this grove",
            format!(
                "its {} is not an ancestor of this grove's; publish never force-pushes and never merges",
                escaped(&default_ref)
            ),
        )),
        // Measured: a missing object is exit 128, distinct from exit 1. A
        // vanished probe ref is a racing peer, not a divergence verdict.
        Ancestry::MissingObject => Err(conflict(
            "the publication probe ref vanished while it was being compared",
            "another `git grove publish` may be running in this grove; retry when it has finished",
        )),
    }
}

/// The single fetch refspec a published grove is expected to carry.
fn wildcard_refspec(remote: &OsStr) -> Vec<u8> {
    let mut value = b"+refs/heads/*:refs/remotes/".to_vec();
    value.extend_from_slice(remote.as_bytes());
    value.extend_from_slice(b"/*");
    value
}

fn unset_all(runner: &dyn GitRunner, config: &Path, key: &OsStr) -> Result<()> {
    let output = runner.run(Invocation::new().args([
        OsStr::new("config"),
        OsStr::new("--file"),
        config.as_os_str(),
        OsStr::new("--unset-all"),
        key,
    ]))?;
    // Exit 5 is "the key does not exist", which is the state this asks for.
    if output.ok() || output.status == 5 {
        Ok(())
    } else {
        Err(GroveError::failure(format!(
            "cannot clear {} in the grove configuration",
            escaped(key.as_bytes())
        ))
        .with_detail(escaped(&output.stderr)))
    }
}

/// Write one single-valued key and read it back. Used for the tool's own
/// writes and to repair the ones git performs on its behalf.
fn set_and_verify(runner: &dyn GitRunner, config: &Path, key: &OsStr, value: &[u8]) -> Result<()> {
    let want = vec![value.to_vec()];
    if config_values(runner, config, key)? != want {
        set_config(runner, config, key, OsStr::from_bytes(value), false)?;
        if config_values(runner, config, key)? != want {
            return Err(GroveError::failure(format!(
                "{} verification failed",
                escaped(key.as_bytes())
            )));
        }
    }
    Ok(())
}

/// Specification step 4: configure the remote locally, verifying every write
/// git performs on this tool's behalf. Every step is idempotent under rerun.
fn configure_local_remote(
    runner: &dyn GitRunner,
    grove: &Grove,
    request: &Request,
    remote_already_configured: bool,
) -> Result<()> {
    let bare = grove.bare_dir();
    let config = bare.join("config");

    if !remote_already_configured {
        // Measured: repeating `remote add` on an existing name is exit 3, so a
        // rerun reads and compares instead of blindly re-adding. `--` before
        // the name, because a name beginning with `-` is valid and would
        // otherwise be read as an option.
        required(
            runner,
            Invocation::new().git_dir(&bare).args([
                OsStr::new("remote"),
                OsStr::new("add"),
                OsStr::new("--"),
                request.remote.as_os_str(),
                request.url.as_os_str(),
            ]),
            "remote add",
        )?;
    }

    let url_key = config_key(b"remote.", &request.remote, b".url");
    let urls = config_values(runner, &config, &url_key)?;
    if urls.as_slice() != [request.url.as_bytes()] {
        return Err(GroveError::failure(format!(
            "the configured URL for remote {} is not the one requested",
            escaped(request.remote.as_bytes())
        ))
        .with_detail(format!(
            "expected {}, found {}",
            escaped(request.url.as_bytes()),
            urls.iter()
                .map(|value| escaped(value))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    // `remote add` writes the fetch refspec itself. That is a git-performed
    // write, so it is verified rather than assumed, and rewritten if git left
    // something else.
    let fetch_key = config_key(b"remote.", &request.remote, b".fetch");
    let want = wildcard_refspec(&request.remote);
    let mut refspecs = config_values(runner, &config, &fetch_key)?;
    if refspecs.as_slice() != [want.clone()] {
        unset_all(runner, &config, &fetch_key)?;
        set_config(runner, &config, &fetch_key, OsStr::from_bytes(&want), true)?;
        refspecs = config_values(runner, &config, &fetch_key)?;
        if refspecs.as_slice() != [want.clone()] {
            return Err(GroveError::failure("fetch refspec verification failed"));
        }
    }
    validate_refspec_destinations(&refspecs, &request.remote)?;

    // An `init`-created grove does not have this key; `clone` writes it.
    set_and_verify(runner, &config, OsStr::new("worktree.guessRemote"), b"true")?;
    set_and_verify(
        runner,
        &config,
        OsStr::new("grove.remote"),
        request.remote.as_bytes(),
    )?;
    Ok(())
}

/// The current value of `remote.<name>.url`, or `None` when the remote is not
/// configured. A multi-valued URL is a state a human has to resolve.
fn live_remote_url(
    runner: &dyn GitRunner,
    grove: &Grove,
    remote: &OsStr,
) -> Result<Option<BString>> {
    let config = grove.bare_dir().join("config");
    let key = config_key(b"remote.", remote, b".url");
    let mut values = config_values(runner, &config, &key)?;
    match values.len() {
        0 => Ok(None),
        1 => Ok(Some(BString::from(values.remove(0)))),
        _ => Err(conflict(
            format!(
                "remote {} has more than one configured URL",
                escaped(remote.as_bytes())
            ),
            "leave exactly one remote.<name>.url before publishing",
        )),
    }
}

/// `refs/remotes/<remote>/<branch>`.
fn tracking_ref(remote: &OsStr, branch: &[u8]) -> Vec<u8> {
    let mut reference = b"refs/remotes/".to_vec();
    reference.extend_from_slice(remote.as_bytes());
    reference.push(b'/');
    reference.extend_from_slice(branch);
    reference
}

/// The refspecs specification step 5 pushes: the default branch alone, or —
/// under `--all-branches` — every local head, in git's `for-each-ref` order,
/// still as **one** atomic push.
fn push_refspecs(
    runner: &dyn GitRunner,
    grove: &Grove,
    request: &Request,
    flight: &Preflight,
) -> Result<Vec<Vec<u8>>> {
    let sources = if request.all_branches {
        list_local_heads(runner, &grove.bare_dir())?
    } else {
        vec![flight.default_ref()]
    };
    Ok(sources
        .into_iter()
        .map(|source| {
            let mut refspec = source.clone();
            refspec.push(b':');
            refspec.extend_from_slice(&source);
            refspec
        })
        .collect())
}

/// Specification step 5: one explicit, non-forced, atomic push.
///
/// There is no `--force`, no `--force-with-lease`, no `--set-upstream`, no
/// `--all`, no `--mirror` and no `--tags`. `--all-branches` widens the refspec
/// list, never the number of pushes: a partial publication is exactly the
/// hazard `--atomic` exists to prevent.
///
/// This is the one child whose stderr the tool branches on, so it goes through
/// `run_classified`. Accepted cost: its *own* unrelated failures render in
/// English too (`error: failed to push some refs`, `! [remote rejected]`).
/// Server-side rejection-hook text is hook output rather than gettext, so hook
/// messages reach the user unchanged either way.
fn push(
    runner: &dyn GitRunner,
    grove: &Grove,
    request: &Request,
    refspecs: &[Vec<u8>],
) -> Result<()> {
    let mut args = vec![
        OsString::from("push"),
        OsString::from("--atomic"),
        OsString::from("--"),
        request.remote.to_os_string(),
    ];
    args.extend(
        refspecs
            .iter()
            .map(|refspec| OsString::from_vec(refspec.clone())),
    );

    let output = runner.run_classified(Invocation::new().git_dir(grove.bare_dir()).args(args))?;
    if output.ok() {
        return Ok(());
    }

    // Measured: git refuses a non-advertising receiving end pre-flight, with
    // exit 128 and zero refs updated on the remote — so "refused before any ref
    // is updated" is satisfied by git itself, and no separate `--dry-run`
    // capability probe is issued. `atomic` is a receive-pack capability, so
    // `ls-remote` could not probe it even in principle.
    if output
        .stderr
        .windows(ATOMIC_UNSUPPORTED.len())
        .any(|window| window == ATOMIC_UNSUPPORTED)
    {
        return Err(conflict(
            "the publication target does not support atomic push",
            "nothing was published; git refused before sending any ref update",
        ));
    }

    Err(GroveError::failure("cannot push to the publication target")
        .with_detail(escaped(&output.stderr)))
}

const ATOMIC_UNSUPPORTED: &[u8] = b"the receiving end does not support --atomic push";

/// Whether `reference` exists in the bare repository.
fn ref_exists(runner: &dyn GitRunner, grove: &Grove, reference: &[u8]) -> Result<bool> {
    let output = runner.run(Invocation::new().git_dir(grove.bare_dir()).args([
        OsStr::new("show-ref"),
        OsStr::new("--verify"),
        OsStr::new("--quiet"),
        OsStr::new("--"),
        OsStr::from_bytes(reference),
    ]))?;
    match output.status {
        0 => Ok(true),
        1 => Ok(false),
        status => Err(GroveError::failure(format!(
            "git show-ref failed with exit status {status} while verifying {}",
            escaped(reference)
        ))
        .with_detail(escaped(&output.stderr))),
    }
}

/// The object `reference` resolves to.
fn resolve(runner: &dyn GitRunner, grove: &Grove, reference: &[u8]) -> Result<BString> {
    let mut revision = OsString::from_vec(reference.to_vec());
    revision.push("^{commit}");
    let output = required(
        runner,
        Invocation::new().git_dir(grove.bare_dir()).args([
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new("--end-of-options"),
            revision.as_os_str(),
        ]),
        "rev-parse",
    )?;
    Ok(BString::from(trim_one_line(output.stdout, "object ID")?))
}

/// Specification step 6, first half: the tracking ref the push is expected to
/// have created.
///
/// Measured: the step-5 push creates `refs/remotes/<remote>/<branch>` itself,
/// because step 4 wrote `remote.<name>.fetch` before it — so this verifies the
/// tracking ref rather than assuming it, and fetches only as an explicit
/// fallback if it is somehow absent.
fn verify_tracking_ref(
    runner: &dyn GitRunner,
    grove: &Grove,
    request: &Request,
    flight: &Preflight,
) -> Result<()> {
    let tracking = tracking_ref(&request.remote, &flight.default_branch);
    if ref_exists(runner, grove, &tracking)? {
        return Ok(());
    }
    FetchPlan {
        remotes: vec![BString::from(request.remote.as_bytes().to_vec())],
    }
    .execute(runner, grove)?;
    if !ref_exists(runner, grove, &tracking)? {
        return Err(GroveError::failure(format!(
            "the push did not create {}",
            escaped(&tracking)
        )));
    }
    Ok(())
}

/// Specification step 6, second half: point the local `refs/remotes/<r>/HEAD`
/// at the remote's default branch and verify what git wrote.
///
/// Runs only after step 7 has confirmed the hosting side resolves `HEAD` to
/// this grove's default branch. `remote set-head --auto` asks the remote the
/// same question and can only answer it as an exit code — measured, it exits 1
/// with `error: Cannot determine remote HEAD` against a dangling remote `HEAD`,
/// which is precisely the state the specification says to report as a decision.
/// Asking structurally first turns that into the exit `2` the specification
/// names; it changes no mutation ordering, since both steps are local.
fn point_remote_head(
    runner: &dyn GitRunner,
    grove: &Grove,
    request: &Request,
    flight: &Preflight,
) -> Result<()> {
    // `--auto` is the mode argument and must precede `--`: with `--` first, git
    // reads `--auto` as the positional <branch> and fails with
    // `error: Not a valid ref: refs/remotes/<remote>/--auto`. The `--` still
    // guards a remote name beginning with a dash.
    required(
        runner,
        Invocation::new().git_dir(grove.bare_dir()).args([
            OsStr::new("remote"),
            OsStr::new("set-head"),
            OsStr::new("--auto"),
            OsStr::new("--"),
            request.remote.as_os_str(),
        ]),
        "remote set-head --auto",
    )?;

    // Verify what git wrote locally. `remote set-head` never changes the server.
    let written = remote_head_branch(runner, &grove.bare_dir(), &request.remote)?;
    if written.as_bytes() != flight.default_branch.as_slice() {
        return Err(GroveError::failure(format!(
            "the remote HEAD was set to {} rather than {}",
            escaped(written.as_bytes()),
            escaped(&flight.default_branch)
        )));
    }
    Ok(())
}

/// Specification step 7: ask the hosting side, over the wire, whether `HEAD`
/// now resolves to this grove's default branch.
fn server_head_matches(
    runner: &dyn GitRunner,
    request: &Request,
    flight: &Preflight,
) -> Result<bool> {
    let advert = remote::advertise(runner, &request.url)?;
    Ok(advert
        .head_symref
        .is_some_and(|symref| symref.as_slice() == flight.default_ref().as_slice()))
}

/// The sentence a guide generated for an unpublished grove carries, which
/// publishing makes false.
const UNPUBLISHED_GUIDE_SENTENCE: &str =
    "This grove is **not published**: it has no upstream branch.";

/// `publish` never rewrites `AGENTS.md`: the guide is written once with
/// `write_atomic_if_absent` and is the user's file thereafter. When it still
/// says the grove is unpublished, say so and let the user decide.
fn stale_guide_line(grove: &Grove) -> Option<String> {
    let guide = grove.root.join("AGENTS.md");
    let contents = std::fs::read(&guide).ok()?;
    let stale = contents
        .windows(UNPUBLISHED_GUIDE_SENTENCE.len())
        .any(|window| window == UNPUBLISHED_GUIDE_SENTENCE.as_bytes());
    stale.then(|| {
        format!(
            "{} still says `{UNPUBLISHED_GUIDE_SENTENCE}`; publish does not rewrite it",
            guide.display()
        )
    })
}

fn published_receipt(request: &Request) -> Receipt {
    Receipt {
        remote: BString::from(request.remote.as_bytes().to_vec()),
        url: BString::from(request.url.as_bytes().to_vec()),
    }
}

/// The report for a run that pushed but could not confirm the hosting side's
/// default branch. The state stays `publishing`, so a rerun reconciles.
fn unconfirmed_head_report(
    request: &Request,
    flight: &Preflight,
    lines: Vec<String>,
    mut diagnostics: Vec<String>,
) -> PublishReport {
    diagnostics.push(format!(
        "the hosting side's default branch is not {}; set it by hand, then run `git grove publish {}` again",
        escaped(&flight.default_branch),
        escaped(request.url.as_bytes())
    ));
    PublishReport {
        class: ExitClass::NeedsDecision,
        lines,
        diagnostics,
    }
}

/// Steps 5 to 7, shared by a fresh publication and by resuming an interrupted
/// one. The receipt is already `publishing` and the remote already configured.
///
/// `provider_runner` is `Some` only on the `--create` hand-off path (never
/// for a bare `publish <url>`, which always passes `None` through
/// [`run`]'s unmodified signature); the `gh`-default-branch repair below is a
/// no-op whenever it is `None`, regardless of what `metadata` records.
#[allow(clippy::too_many_arguments)]
fn publish_and_verify(
    runner: &dyn GitRunner,
    grove: &Grove,
    metadata: &Metadata,
    request: &Request,
    flight: &Preflight,
    diagnostics: Vec<String>,
    provider_runner: Option<&dyn ProviderRunner>,
) -> Result<PublishReport> {
    let refspecs = push_refspecs(runner, grove, request, flight)?;
    push(runner, grove, request, &refspecs)?;

    // Upstreams are written explicitly through `config --file` and verified,
    // never through `push --set-upstream`.
    let bare = grove.bare_dir();
    configure_upstreams(
        runner,
        &bare,
        &request.remote,
        &[wildcard_refspec(&request.remote)],
    )?;
    verify_tracking_ref(runner, grove, request, flight)?;

    let mut lines = vec![format!(
        "published {} to {} at {}",
        escaped(&flight.default_branch),
        escaped(request.remote.as_bytes()),
        escaped(request.url.as_bytes())
    )];
    if request.all_branches {
        lines.push(format!(
            "pushed {} branches in one atomic push",
            refspecs.len()
        ));
    }

    if !server_head_matches(runner, request, flight)? {
        attempt_gh_default_branch_repair(provider_runner, metadata, request, flight)?;
        if !server_head_matches(runner, request, flight)? {
            return Ok(unconfirmed_head_report(request, flight, lines, diagnostics));
        }
    }
    point_remote_head(runner, grove, request, flight)?;

    lines.extend(stale_guide_line(grove));

    metadata::write_receipt(
        runner,
        grove,
        PublishState::Published,
        &published_receipt(request),
    )?;
    Ok(PublishReport {
        class: ExitClass::Ok,
        lines,
        diagnostics,
    })
}

/// A rerun on a grove this tool already published: repair the local
/// configuration, push only if the remote is strictly behind, and re-verify the
/// hosting side's `HEAD`.
///
/// No probe ref is used here. A configured remote makes
/// `refs/remotes/<remote>/<default>` the authoritative comparison point that a
/// probe ref exists only to synthesise when there is no remote yet.
fn repair_published(
    runner: &dyn GitRunner,
    grove: &Grove,
    metadata: &Metadata,
    request: &Request,
    flight: &Preflight,
    provider_runner: Option<&dyn ProviderRunner>,
) -> Result<PublishReport> {
    FetchPlan {
        remotes: vec![BString::from(request.remote.as_bytes().to_vec())],
    }
    .execute(runner, grove)?;

    configure_local_remote(runner, grove, request, true)?;
    let bare = grove.bare_dir();
    configure_upstreams(
        runner,
        &bare,
        &request.remote,
        &[wildcard_refspec(&request.remote)],
    )?;

    let local_ref = flight.default_ref();
    let tracking = tracking_ref(&request.remote, &flight.default_branch);
    let mut lines = Vec::new();

    match remote::is_ancestor(runner, grove, &tracking, &local_ref)? {
        Ancestry::Ancestor => {
            let remote_oid = resolve(runner, grove, &tracking)?;
            let local_oid = resolve(runner, grove, &local_ref)?;
            if remote_oid == local_oid {
                lines.push(format!(
                    "{} is already at {}",
                    escaped(request.remote.as_bytes()),
                    escaped(&local_oid)
                ));
            } else {
                let refspecs = push_refspecs(runner, grove, request, flight)?;
                push(runner, grove, request, &refspecs)?;
                lines.push(format!(
                    "advanced {} on {} to {}",
                    escaped(&flight.default_branch),
                    escaped(request.remote.as_bytes()),
                    escaped(&local_oid)
                ));
            }
        }
        Ancestry::NotAncestor => {
            return Err(conflict(
                "the publication target is ahead of this grove or has diverged",
                format!(
                    "{} is not behind this grove's {}; nothing was pushed",
                    escaped(&tracking),
                    escaped(&local_ref)
                ),
            ))
        }
        // A configured remote's tracking ref vanishing mid-run is not a
        // decision a user can make.
        Ancestry::MissingObject => {
            return Err(GroveError::failure(format!(
                "{} vanished while this grove was being repaired",
                escaped(&tracking)
            )))
        }
    }

    verify_tracking_ref(runner, grove, request, flight)?;

    if !server_head_matches(runner, request, flight)? {
        attempt_gh_default_branch_repair(provider_runner, metadata, request, flight)?;
        if !server_head_matches(runner, request, flight)? {
            // The same condition specification step 7 prescribes `publishing`
            // and exit 2 for on a first publish, so a rerun applies the same
            // rule. The receipt is untouched, so the acceptance matrix still
            // resumes cleanly.
            metadata::write_receipt(
                runner,
                grove,
                PublishState::Publishing,
                &published_receipt(request),
            )?;
            return Ok(unconfirmed_head_report(request, flight, lines, Vec::new()));
        }
    }
    point_remote_head(runner, grove, request, flight)?;

    Ok(PublishReport {
        class: ExitClass::Ok,
        lines,
        diagnostics: Vec::new(),
    })
}

/// Decision 8: a repository `--create` itself made gets `gh`'s configured
/// default branch name, not necessarily this grove's — `glab` never needs
/// this, since `--defaultBranch` at creation time already closed the gap.
/// Gated on this grove's own creating keys being present, recorded as
/// `github`, and naming the same remote as `request`: never applied to a
/// repository published by a bare `publish <url>`, and never to `gitlab`.
/// Best-effort: `gh repo edit`'s own failure is not surfaced here, since the
/// caller always re-checks the server-side `HEAD` afterward and falls back to
/// its existing exit-`2` report if the repair did not help.
fn attempt_gh_default_branch_repair(
    provider_runner: Option<&dyn ProviderRunner>,
    metadata: &Metadata,
    request: &Request,
    flight: &Preflight,
) -> Result<()> {
    let Some(provider_runner) = provider_runner else {
        return Ok(());
    };
    let Some(creating) = metadata::creating_receipt(metadata)? else {
        return Ok(());
    };
    if creating.provider.as_slice() != provider_slug(Provider::GitHub)
        || creating.remote.as_slice() != request.remote.as_bytes()
    {
        return Ok(());
    }

    let mut target = OsString::from_vec(creating.owner.to_vec());
    target.push("/");
    target.push(OsString::from_vec(creating.name.to_vec()));
    let branch = OsString::from_vec(flight.default_branch.to_vec());

    let _ = provider_runner.run(
        Provider::GitHub,
        &[
            OsStr::new("repo"),
            OsStr::new("edit"),
            &target,
            OsStr::new("--default-branch"),
            &branch,
        ],
    );
    Ok(())
}

/// A grove left in `creating` with an incomplete four-key set — some but not
/// all of provider/owner/name/remote present — by a process killed mid-write
/// or mid-rollback. Not a torn-grove failure: clear whatever partial keys
/// exist and write `unpublished` (Decision 2's clear-then-state-last
/// ordering, via [`metadata::rollback_creating_receipt`]), then let the
/// caller proceed as an ordinary fresh attempt in the same invocation.
///
/// Runs on `publish`'s own path, under the exclusive lock it already holds,
/// **before** [`metadata::creating_receipt`] is ever asked to classify this
/// grove — never inside `metadata::read`, which every command calls,
/// including read-only ones holding only a shared lock. Both a bare
/// `publish <url>` and `publish --create` call this identically.
///
/// A *complete* four-key receipt is untouched here: that case is not a
/// repair, it is `accept_rerun`'s `(Creating, None)` arm for a bare
/// `publish <url>`, or `reconcile_create`'s own classification for
/// `--create`.
fn heal_incomplete_creating_receipt(
    runner: &dyn GitRunner,
    grove: &Grove,
    metadata: &Metadata,
) -> Result<Metadata> {
    if metadata.publish_state != PublishState::Creating {
        return Ok(metadata.clone());
    }
    if metadata.publish_url.is_some() {
        // Decision 1 forbids `publishUrl` outright while `creating`; this is
        // never the "some but not all of the four keys present" crash shape
        // this repair exists for, and `rollback_creating_receipt` does not
        // clear `publishUrl` (by design — it is the classic receipt's own
        // key, untouched by a creating-receipt rollback). Rolling back here
        // would only trade one torn shape (`creating` + a forbidden URL) for
        // another (`unpublished` + a stray URL) instead of actually healing
        // anything. Leave it for `creating_receipt`/`receipt` to report as
        // the `Failure` it is.
        return Ok(metadata.clone());
    }
    if metadata::creating_receipt(metadata).is_ok() {
        return Ok(metadata.clone());
    }
    metadata::rollback_creating_receipt(runner, grove)?;
    metadata::read(runner, grove)
}

/// Run the publication transaction.
///
/// This signature and its behaviour are unchanged by `--create`: it always
/// runs with no `ProviderRunner`, so [`attempt_gh_default_branch_repair`] is
/// always a no-op here, regardless of what `metadata` records. `--create`'s
/// own hand-off calls [`run_with_provider`] directly.
pub fn run(
    runner: &dyn GitRunner,
    grove: &Grove,
    metadata: &Metadata,
    request: &Request,
) -> Result<PublishReport> {
    run_with_provider(runner, grove, metadata, request, None)
}

fn run_with_provider(
    runner: &dyn GitRunner,
    grove: &Grove,
    metadata: &Metadata,
    request: &Request,
    provider_runner: Option<&dyn ProviderRunner>,
) -> Result<PublishReport> {
    let metadata = &heal_incomplete_creating_receipt(runner, grove, metadata)?;
    let live = live_remote_url(runner, grove, &request.remote)?;
    let resume = accept_rerun(metadata, live.as_ref(), request)?;
    let flight = preflight(runner, grove, metadata, request)?;

    if resume == Resume::RepairPublished {
        return repair_published(runner, grove, metadata, request, &flight, provider_runner);
    }

    let mut diagnostics = Vec::new();
    inspect_target(runner, grove, request, &flight, &mut diagnostics)?;

    // The first durable write. Everything above is read-only against the remote
    // and self-healing locally.
    metadata::write_receipt(
        runner,
        grove,
        PublishState::Publishing,
        &published_receipt(request),
    )?;

    configure_local_remote(runner, grove, request, live.is_some())?;
    publish_and_verify(
        runner,
        grove,
        metadata,
        request,
        &flight,
        diagnostics,
        provider_runner,
    )
}

// ============================================================================
// `publish --create`
// ============================================================================

/// What `--create <owner>/<name> --host <provider>` asked for. Raw bytes
/// throughout, like [`Request`]; never parsed or normalised beyond the
/// `owner`/`name` split `cli::parse_create_target` already validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateRequest {
    pub owner: OsString,
    pub name: OsString,
    pub provider: Provider,
    pub public: bool,
    pub remote: OsString,
    pub all_branches: bool,
}

/// What the durable state says a `--create` run is, decided from whatever is
/// already recorded, before any provider CLI is ever touched.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CreateResume {
    /// No creating receipt at all, and `accept_rerun` (with a synthetic
    /// request) confirms nothing else is in the way.
    Fresh,
    /// A creating receipt matching this exact request, state `creating`: a
    /// real continuation of an earlier, interrupted `--create`.
    Continue,
    /// A creating receipt matching this exact request, state `publishing` or
    /// `published`: the repository was already created (and possibly already
    /// published); hand off to the existing, unmodified `run()` machinery
    /// using the classic receipt already on record.
    ResumeExisting(Request),
}

/// `--create`'s own state-0 reconciliation against whatever is already
/// recorded (Decision 4 / the spec's "`--create` against every other state").
/// Never re-derives `accept_rerun`'s own rules: when no creating receipt is
/// recorded at all, this calls the real, unmodified `accept_rerun` with a
/// synthetic request scoped to this one check.
fn reconcile_create(
    metadata: &Metadata,
    live_remote_url: Option<&BString>,
    create_request: &CreateRequest,
) -> Result<CreateResume> {
    let creating = metadata::creating_receipt(metadata)?;

    let Some(recorded) = creating else {
        let synthetic_url = metadata::receipt(metadata)?
            .map(|receipt| OsString::from_vec(receipt.url.to_vec()))
            .unwrap_or_default();
        let synthetic = Request {
            url: synthetic_url,
            remote: create_request.remote.clone(),
            all_branches: create_request.all_branches,
        };
        return match accept_rerun(metadata, live_remote_url, &synthetic) {
            Ok(Resume::Fresh) => Ok(CreateResume::Fresh),
            Ok(Resume::ResumePublishing | Resume::RepairPublished) => Err(conflict(
                "this grove already has a publication in progress or complete",
                "`--create` only applies before a remote exists",
            )),
            Err(error) => Err(error),
        };
    };

    if recorded.provider.as_slice() != provider_slug(create_request.provider)
        || recorded.owner.as_slice() != create_request.owner.as_bytes()
        || recorded.name.as_slice() != create_request.name.as_bytes()
        || recorded.remote.as_slice() != create_request.remote.as_bytes()
    {
        return Err(conflict(
            "this grove records a different `--create` request",
            format!(
                "recorded {}/{} on {} via remote {}; requested {}/{} on {} via remote {}",
                escaped(&recorded.owner),
                escaped(&recorded.name),
                escaped(&recorded.provider),
                escaped(&recorded.remote),
                escaped(create_request.owner.as_bytes()),
                escaped(create_request.name.as_bytes()),
                escaped(provider_slug(create_request.provider)),
                escaped(create_request.remote.as_bytes()),
            ),
        ));
    }

    match metadata.publish_state {
        PublishState::Creating => Ok(CreateResume::Continue),
        PublishState::Publishing | PublishState::Published => {
            let classic = metadata::receipt(metadata)?.ok_or_else(|| {
                GroveError::failure(
                    "this grove's creation receipt is complete but its classic publication receipt is missing",
                )
            })?;
            Ok(CreateResume::ResumeExisting(Request {
                url: OsString::from_vec(classic.url.to_vec()),
                remote: OsString::from_vec(classic.remote.to_vec()),
                all_branches: create_request.all_branches,
            }))
        }
        PublishState::Unpublished => {
            unreachable!("creating_receipt never returns Some for Unpublished")
        }
    }
}

/// `<owner>/<name>`, built without assuming either half is valid UTF-8.
fn create_target(create_request: &CreateRequest) -> OsString {
    let mut target = create_request.owner.clone();
    target.push("/");
    target.push(&create_request.name);
    target
}

/// The ground truth `repo view` established for a target: the two URL forms
/// and whether the target is empty, plus whether the query's own report of
/// the target's identity agrees with what was requested (defence against a
/// provider-side rename/redirect; see `reconcile_create`, which already
/// refuses a mismatch between what is *recorded* and what was *requested*
/// before this is ever reached — this is a second, independent check against
/// what the *provider* itself reports for the query).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoView {
    https_url: BString,
    ssh_url: BString,
    is_empty: bool,
    matches_target: bool,
}

#[derive(serde::Deserialize)]
struct GhRepoView {
    url: String,
    #[serde(rename = "sshUrl")]
    ssh_url: String,
    #[serde(rename = "isEmpty")]
    is_empty: bool,
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
}

#[derive(serde::Deserialize)]
struct GlabRepoView {
    http_url_to_repo: String,
    ssh_url_to_repo: String,
    empty_repo: bool,
    path_with_namespace: String,
}

fn parse_repo_view(
    provider: Provider,
    stdout: &[u8],
    create_request: &CreateRequest,
) -> Result<RepoView> {
    let requested = create_target(create_request);
    match provider {
        Provider::GitHub => {
            let parsed: GhRepoView = serde_json::from_slice(stdout).map_err(|error| {
                GroveError::failure(format!("cannot parse `gh repo view` output: {error}"))
            })?;
            Ok(RepoView {
                matches_target: parsed.name_with_owner.as_bytes() == requested.as_bytes(),
                https_url: BString::from(parsed.url),
                ssh_url: BString::from(parsed.ssh_url),
                is_empty: parsed.is_empty,
            })
        }
        Provider::GitLab => {
            let parsed: GlabRepoView = serde_json::from_slice(stdout).map_err(|error| {
                GroveError::failure(format!("cannot parse `glab repo view` output: {error}"))
            })?;
            Ok(RepoView {
                matches_target: parsed.path_with_namespace.as_bytes() == requested.as_bytes(),
                https_url: BString::from(parsed.http_url_to_repo),
                ssh_url: BString::from(parsed.ssh_url_to_repo),
                is_empty: parsed.empty_repo,
            })
        }
    }
}

/// The three ways `repo view <owner>/<name>` can leave the sequencing
/// algorithm: it confirms the target is there, confirms it is not, or the
/// query itself could not be classified at all.
///
/// The last case collapses to almost-never-reachable in practice: by the
/// construction below, `Missing` is *every* non-zero exit from `repo view`,
/// regardless of its status or stderr text, and `Err` (this function's
/// `Result`) is reserved for the query never producing a `ProviderOutput` at
/// all (a spawn failure). **This is a plan defect, not a specified rule**:
/// neither `gh help exit-codes` nor `glab`'s undocumented convention gives a
/// semantic "not found" exit code distinct from any other non-auth failure,
/// and the plan's own measurement provenance never observed `repo view`
/// against a genuinely nonexistent target (doing so would be exactly the
/// real provider call the executor brief forbids). Consensus with
/// `exec-advisor` (`.superpowers/sdd/2026-08-22-git-grove-publish-create/exec-advisor-repo-view-classification.md`):
/// classify by the `Result`/`ProviderOutput` type boundary alone, never by
/// exit-code value or stderr text — the only signal that isn't a guess.
enum RepoViewOutcome {
    Found(RepoView),
    Missing,
}

fn indeterminate_repo_view_failure(
    create_request: &CreateRequest,
    cause: &GroveError,
) -> GroveError {
    GroveError::failure(format!(
        "cannot confirm whether {} exists on {}",
        escaped(create_target(create_request).as_bytes()),
        create_request.provider.host_env().1,
    ))
    .with_detail(format!(
        "{cause}. This grove stays in `creating`; clear grove.publishProvider, grove.publishOwner, grove.publishName, and grove.publishRemote by hand to force a fresh attempt, or rerun once the failure is resolved",
    ))
}

fn query_repo_view(
    provider_runner: &dyn ProviderRunner,
    create_request: &CreateRequest,
) -> Result<RepoViewOutcome> {
    let target = create_target(create_request);
    let args: Vec<&OsStr> = match create_request.provider {
        Provider::GitHub => vec![
            OsStr::new("repo"),
            OsStr::new("view"),
            &target,
            OsStr::new("--json"),
            OsStr::new("url,sshUrl,isEmpty,nameWithOwner"),
        ],
        Provider::GitLab => vec![
            OsStr::new("repo"),
            OsStr::new("view"),
            &target,
            OsStr::new("-F"),
            OsStr::new("json"),
        ],
    };
    let output = provider_runner
        .run(create_request.provider, &args)
        .map_err(|error| indeterminate_repo_view_failure(create_request, &error))?;
    if !output.ok() {
        return Ok(RepoViewOutcome::Missing);
    }
    Ok(RepoViewOutcome::Found(parse_repo_view(
        create_request.provider,
        &output.stdout,
        create_request,
    )?))
}

/// `<provider> repo create <owner>/<name> --private|--public`, and for
/// `glab`, unconditionally `--skipGitInit --defaultBranch <branch>` too
/// (Decision 5/8). Never `--clone`/`--push`/`--source`/anything that gives
/// the repository an initial commit.
fn call_create(
    provider_runner: &dyn ProviderRunner,
    create_request: &CreateRequest,
    flight: &Preflight,
) -> Result<ProviderOutput> {
    let target = create_target(create_request);
    let visibility = if create_request.public {
        OsStr::new("--public")
    } else {
        OsStr::new("--private")
    };
    let branch = OsString::from_vec(flight.default_branch.to_vec());
    let args: Vec<&OsStr> = match create_request.provider {
        Provider::GitHub => vec![
            OsStr::new("repo"),
            OsStr::new("create"),
            &target,
            visibility,
        ],
        Provider::GitLab => vec![
            OsStr::new("repo"),
            OsStr::new("create"),
            &target,
            visibility,
            OsStr::new("--skipGitInit"),
            OsStr::new("--defaultBranch"),
            &branch,
        ],
    };
    provider_runner.run(create_request.provider, &args)
}

/// `gh`'s own documented "requires authentication" exit (`4`) maps to exit
/// `2`; everything else maps to `1` with the provider's raw stderr attached.
fn create_failure_error(create_request: &CreateRequest, output: &ProviderOutput) -> GroveError {
    let detail = output.diagnostic_detail();
    if create_request.provider == Provider::GitHub && output.status == 4 {
        conflict(
            format!(
                "could not create {} on {}",
                escaped(create_target(create_request).as_bytes()),
                create_request.provider.host_env().1
            ),
            detail,
        )
    } else {
        GroveError::failure(format!(
            "cannot create {} on {}",
            escaped(create_target(create_request).as_bytes()),
            create_request.provider.host_env().1
        ))
        .with_detail(detail)
    }
}

fn unrelated_existing_repository_error(create_request: &CreateRequest) -> GroveError {
    conflict(
        "the publication target is an unrelated existing repository",
        format!(
            "{} exists on {} but does not match what this grove recorded creating",
            escaped(create_target(create_request).as_bytes()),
            create_request.provider.host_env().1
        ),
    )
}

/// Confirm `view` belongs to this grove's own creating target, rolling the
/// receipt back and refusing otherwise (the "unrelated existing repository"
/// outcome — reachable defensively even though `reconcile_create` already
/// refused a mismatch between what was *recorded* and *requested*, since
/// this checks what the *provider* itself reports for the query).
fn finish_repo_view(
    runner: &dyn GitRunner,
    grove: &Grove,
    create_request: &CreateRequest,
    view: RepoView,
) -> Result<RepoView> {
    if view.matches_target {
        Ok(view)
    } else {
        metadata::rollback_creating_receipt(runner, grove)?;
        Err(unrelated_existing_repository_error(create_request))
    }
}

/// The spec's *Sequencing* steps 5–6 and their outcomes: a continuation
/// queries `repo view` before ever calling `create` again; a fresh attempt,
/// or a continuation that found the target missing, calls `create` and
/// resolves the *outcome* from a follow-up query — never from `create`'s own
/// stdout, which has no structured output and is a one-shot, unrepeatable
/// observation.
fn run_sequencing(
    runner: &dyn GitRunner,
    provider_runner: &dyn ProviderRunner,
    grove: &Grove,
    create_request: &CreateRequest,
    flight: &Preflight,
    continuing: bool,
) -> Result<RepoView> {
    if continuing {
        if let RepoViewOutcome::Found(view) = query_repo_view(provider_runner, create_request)? {
            return finish_repo_view(runner, grove, create_request, view);
        }
        // Missing: an earlier attempt never got far enough to create it.
        // Fall through to the same call-create step the fresh path takes.
    }

    let create_output = call_create(provider_runner, create_request, flight)?;
    if create_output.ok() {
        match query_repo_view(provider_runner, create_request)? {
            RepoViewOutcome::Found(view) => finish_repo_view(runner, grove, create_request, view),
            RepoViewOutcome::Missing => Err(GroveError::failure(
                "the hosting side reported success creating the repository, but a follow-up query cannot find it",
            )),
        }
    } else {
        match query_repo_view(provider_runner, create_request)? {
            RepoViewOutcome::Missing => {
                metadata::rollback_creating_receipt(runner, grove)?;
                Err(create_failure_error(create_request, &create_output))
            }
            RepoViewOutcome::Found(view) => finish_repo_view(runner, grove, create_request, view),
        }
    }
}

/// The provider's own configured `git_protocol`, looked up **scoped to the
/// pinned host** — never the unscoped form, which measurement showed
/// disagrees with the host-scoped value on the machine this was specified
/// on. Defaults to `https` on any failure or absence, exactly as the spec
/// requires: this call's own errors are never surfaced to the user.
fn configured_git_protocol(provider_runner: &dyn ProviderRunner, provider: Provider) -> BString {
    let host = provider.host_env().1;
    let args = [
        OsStr::new("config"),
        OsStr::new("get"),
        OsStr::new("git_protocol"),
        OsStr::new("--host"),
        OsStr::new(host),
    ];
    let Ok(output) = provider_runner.run(provider, &args) else {
        return BString::from("https");
    };
    if !output.ok() {
        return BString::from("https");
    }
    let trimmed = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    if trimmed.is_empty() {
        BString::from("https")
    } else {
        BString::from(trimmed.to_vec())
    }
}

/// Deriving the URL (never parsed from `create`'s stdout): the field chosen
/// by the host-scoped `git_protocol` lookup, recorded byte-verbatim into a
/// classic receipt via the existing, unmodified [`metadata::write_receipt`],
/// leaving the three creating keys untouched.
fn derive_and_publish(
    runner: &dyn GitRunner,
    provider_runner: &dyn ProviderRunner,
    grove: &Grove,
    create_request: &CreateRequest,
    view: RepoView,
) -> Result<Request> {
    let protocol = configured_git_protocol(provider_runner, create_request.provider);
    let url_bytes = if protocol.as_slice() == b"ssh" {
        view.ssh_url
    } else {
        view.https_url
    };

    metadata::write_receipt(
        runner,
        grove,
        PublishState::Publishing,
        &Receipt {
            remote: BString::from(create_request.remote.as_bytes().to_vec()),
            url: url_bytes.clone(),
        },
    )?;

    Ok(Request {
        url: OsString::from_vec(url_bytes.to_vec()),
        remote: create_request.remote.clone(),
        all_branches: create_request.all_branches,
    })
}

/// Checked read-only, after the local preflight and before any provider
/// mutation: `gh auth status` / `glab auth status`, judged by exit code
/// alone. A non-zero exit maps to exit `2`, phrased as "could not confirm
/// authentication" rather than "not authenticated" — both CLIs' `auth
/// status` validate the stored token over the network, so a non-zero exit
/// also covers an unreachable host.
fn check_provider_auth(provider_runner: &dyn ProviderRunner, provider: Provider) -> Result<()> {
    let output = provider_runner.run(provider, &[OsStr::new("auth"), OsStr::new("status")])?;
    if output.ok() {
        Ok(())
    } else {
        Err(conflict(
            format!(
                "could not confirm authentication for {}",
                provider.host_env().1
            ),
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ))
    }
}

/// Create the hosting-side repository through `gh`/`glab`, then publish to
/// it, reusing the existing `publish` push/verification machinery unchanged
/// from the point a URL exists (Decision 6's ordering: version gate → local
/// preflight → auth check → reconciliation → sequencing → hand-off).
pub fn run_create(
    git_runner: &dyn GitRunner,
    provider_runner: &dyn ProviderRunner,
    grove: &Grove,
    metadata: &Metadata,
    create_request: &CreateRequest,
) -> Result<PublishReport> {
    let metadata = &heal_incomplete_creating_receipt(git_runner, grove, metadata)?;

    provider::check_provider_version(provider_runner, create_request.provider)?;

    validate_remote_name(git_runner, &create_request.remote)?;
    let flight = check_publishable_repository(git_runner, grove, metadata)?;

    check_provider_auth(provider_runner, create_request.provider)?;

    let live = live_remote_url(git_runner, grove, &create_request.remote)?;
    let resume = reconcile_create(metadata, live.as_ref(), create_request)?;

    let request = match resume {
        CreateResume::ResumeExisting(request) => request,
        CreateResume::Fresh => {
            metadata::write_creating_receipt(
                git_runner,
                grove,
                &BString::from(provider_slug(create_request.provider)),
                &BString::from(create_request.owner.as_bytes().to_vec()),
                &BString::from(create_request.name.as_bytes().to_vec()),
                &BString::from(create_request.remote.as_bytes().to_vec()),
            )?;
            let view = run_sequencing(
                git_runner,
                provider_runner,
                grove,
                create_request,
                &flight,
                false,
            )?;
            derive_and_publish(git_runner, provider_runner, grove, create_request, view)?
        }
        CreateResume::Continue => {
            let view = run_sequencing(
                git_runner,
                provider_runner,
                grove,
                create_request,
                &flight,
                true,
            )?;
            derive_and_publish(git_runner, provider_runner, grove, create_request, view)?
        }
    };

    let metadata = metadata::read(git_runner, grove)?;
    run_with_provider(
        git_runner,
        grove,
        &metadata,
        &request,
        Some(provider_runner),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::provider::{ProviderOutput as PO, RecordingFake as ProviderFake};
    use crate::git::runner::{GitOutput, RecordingFake};

    fn grove() -> Grove {
        Grove { root: "/g".into() }
    }

    const CONFIG: &str = "/g/.bare/config";
    const BARE: &str = "--git-dir=/g/.bare";
    const URL: &str = "https://example.invalid/r.git";
    const OID: &str = "c3d445388f83a72043990aeaf22af9ba74aa4797";

    fn out(status: i32, stdout: &[u8]) -> GitOutput {
        GitOutput {
            status,
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        }
    }

    /// `git config --get-all` succeeding with these NUL-terminated values.
    fn values(values: &[&[u8]]) -> GitOutput {
        let mut stdout = Vec::new();
        for value in values {
            stdout.extend_from_slice(value);
            stdout.push(0);
        }
        out(0, &stdout)
    }

    /// `git config --get-all` reporting the key is absent.
    fn absent() -> GitOutput {
        out(1, b"")
    }

    fn request() -> Request {
        Request {
            url: OsString::from(URL),
            remote: OsString::from("origin"),
            all_branches: false,
        }
    }

    fn metadata_of(
        state: PublishState,
        publish_remote: Option<&str>,
        publish_url: Option<&str>,
    ) -> Metadata {
        Metadata {
            version: Some(1),
            default_branch: Some(BString::from("main")),
            remote: None,
            publish_state: state,
            publish_remote: publish_remote.map(BString::from),
            publish_url: publish_url.map(BString::from),
            publish_provider: None,
            publish_owner: None,
            publish_name: None,
        }
    }

    fn unpublished() -> Metadata {
        metadata_of(PublishState::Unpublished, None, None)
    }

    // ---- the rerun acceptance matrix -----------------------------------

    #[test]
    fn unpublished_with_no_remote_is_a_fresh_publication() {
        assert_eq!(
            accept_rerun(&unpublished(), None, &request()).unwrap(),
            Resume::Fresh
        );
    }

    #[test]
    fn unpublished_with_a_remote_already_at_that_name_is_a_decision() {
        let error = accept_rerun(
            &unpublished(),
            Some(&BString::from("https://other.invalid/r.git")),
            &request(),
        )
        .unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        assert!(error.message.contains("origin"));
        assert!(error
            .detail
            .unwrap()
            .contains("https://other.invalid/r.git"));
    }

    #[test]
    fn unpublished_carrying_receipt_keys_is_a_failure() {
        let metadata = metadata_of(PublishState::Unpublished, Some("origin"), Some(URL));

        let error = accept_rerun(&metadata, None, &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
    }

    #[test]
    fn publishing_with_a_matching_receipt_and_no_remote_resumes() {
        let metadata = metadata_of(PublishState::Publishing, Some("origin"), Some(URL));

        assert_eq!(
            accept_rerun(&metadata, None, &request()).unwrap(),
            Resume::ResumePublishing
        );
    }

    #[test]
    fn publishing_with_a_matching_receipt_and_a_matching_remote_resumes() {
        let metadata = metadata_of(PublishState::Publishing, Some("origin"), Some(URL));

        assert_eq!(
            accept_rerun(&metadata, Some(&BString::from(URL)), &request()).unwrap(),
            Resume::ResumePublishing
        );
    }

    #[test]
    fn publishing_whose_remote_drifted_from_the_receipt_is_a_decision() {
        let metadata = metadata_of(PublishState::Publishing, Some("origin"), Some(URL));

        let error = accept_rerun(
            &metadata,
            Some(&BString::from("https://other.invalid/r.git")),
            &request(),
        )
        .unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        let detail = error.detail.unwrap();
        assert!(detail.contains(URL));
        assert!(detail.contains("https://other.invalid/r.git"));
    }

    #[test]
    fn a_receipt_for_a_different_url_is_a_decision_naming_both() {
        let metadata = metadata_of(
            PublishState::Publishing,
            Some("origin"),
            Some("https://typo.invalid/r.git"),
        );

        let error = accept_rerun(&metadata, None, &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        let detail = error.detail.unwrap();
        assert!(detail.contains("https://typo.invalid/r.git"));
        assert!(detail.contains(URL));
    }

    #[test]
    fn a_receipt_for_a_different_remote_name_is_a_decision_naming_both() {
        let metadata = metadata_of(PublishState::Publishing, Some("upstream"), Some(URL));

        let error = accept_rerun(&metadata, None, &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        let detail = error.detail.unwrap();
        assert!(detail.contains("upstream"));
        assert!(detail.contains("origin"));
    }

    #[test]
    fn published_with_a_matching_receipt_and_remote_is_a_repair() {
        let metadata = metadata_of(PublishState::Published, Some("origin"), Some(URL));

        assert_eq!(
            accept_rerun(&metadata, Some(&BString::from(URL)), &request()).unwrap(),
            Resume::RepairPublished
        );
    }

    #[test]
    fn published_with_a_matching_receipt_but_no_remote_is_a_decision() {
        let metadata = metadata_of(PublishState::Published, Some("origin"), Some(URL));

        let error = accept_rerun(&metadata, None, &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
    }

    #[test]
    fn published_with_a_matching_receipt_but_a_differing_remote_is_a_decision() {
        let metadata = metadata_of(PublishState::Published, Some("origin"), Some(URL));

        let error = accept_rerun(
            &metadata,
            Some(&BString::from("https://other.invalid/r.git")),
            &request(),
        )
        .unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
    }

    #[test]
    fn published_with_a_differing_receipt_is_a_decision() {
        let metadata = metadata_of(
            PublishState::Published,
            Some("origin"),
            Some("https://other.invalid/r.git"),
        );

        let error = accept_rerun(
            &metadata,
            Some(&BString::from("https://other.invalid/r.git")),
            &request(),
        )
        .unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
    }

    /// A grove created by `git grove clone`, including every grove created by
    /// the shipped v0.2.0: `published`, no receipt, a live remote.
    #[test]
    fn a_cloned_grove_is_refused_with_a_decision_not_a_failure() {
        let metadata = metadata_of(PublishState::Published, None, None);

        let error = accept_rerun(&metadata, Some(&BString::from(URL)), &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        assert!(error.message.contains("already published"));
    }

    /// The same grove after its remote was removed by hand. Refusing would
    /// leave no 0.3 command able to repair it, which is the trap the receipt
    /// ordering exists to avoid.
    #[test]
    fn a_cloned_grove_whose_remote_was_removed_can_be_published_afresh() {
        let metadata = metadata_of(PublishState::Published, None, None);

        assert_eq!(
            accept_rerun(&metadata, None, &request()).unwrap(),
            Resume::Fresh
        );
    }

    /// The one new `accept_rerun` arm `PublishState::Creating` forces the
    /// compiler to cover: a bare `publish <url>` against a grove already in
    /// the middle of `--create` never silently resumes or reinterprets it.
    #[test]
    fn a_bare_publish_against_a_creating_grove_with_a_complete_receipt_is_a_decision() {
        let mut metadata = unpublished();
        metadata.publish_state = PublishState::Creating;
        metadata.publish_provider = Some(BString::from("github"));
        metadata.publish_owner = Some(BString::from("acme"));
        metadata.publish_name = Some(BString::from("widgets"));
        metadata.publish_remote = Some(BString::from("origin"));

        let error = accept_rerun(&metadata, None, &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        assert!(error.message.contains("github"));
        let detail = error.detail.unwrap();
        assert!(detail.contains("acme"));
        assert!(detail.contains("widgets"));
        assert!(detail.contains("--create"));
    }

    /// Regression test for a real, reachable panic (found by Copilot review
    /// against PR #5, verified independently before fixing): a `creating`
    /// grove whose metadata is corrupted to also carry a classic receipt
    /// (forbidden outright by Decision 1) used to drive this function into
    /// the `PublishState::Creating` inner-match arm's `unreachable!()`
    /// instead of failing cleanly. `metadata::receipt` itself now rejects
    /// that shape (see its own regression test), so this call fails at the
    /// `?` before ever reaching the match here — but the arm itself is also
    /// hardened into a real `Failure`, so this stays a structured error
    /// rather than a panic even if that upstream guard is ever weakened
    /// again.
    #[test]
    fn a_creating_grove_with_a_corrupted_classic_receipt_fails_cleanly_instead_of_panicking() {
        let mut metadata = unpublished();
        metadata.publish_state = PublishState::Creating;
        metadata.publish_provider = Some(BString::from("github"));
        metadata.publish_owner = Some(BString::from("acme"));
        metadata.publish_name = Some(BString::from("widgets"));
        metadata.publish_remote = Some(BString::from("origin"));
        metadata.publish_url = Some(BString::from(URL));

        let error = accept_rerun(&metadata, None, &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
    }

    // ---- self-healing an incomplete `creating` receipt ------------------

    fn creating_metadata(
        provider: Option<&str>,
        owner: Option<&str>,
        name: Option<&str>,
        remote: Option<&str>,
    ) -> Metadata {
        let mut metadata = unpublished();
        metadata.publish_state = PublishState::Creating;
        metadata.publish_provider = provider.map(BString::from);
        metadata.publish_owner = owner.map(BString::from);
        metadata.publish_name = name.map(BString::from);
        metadata.publish_remote = remote.map(BString::from);
        metadata
    }

    #[test]
    fn an_incomplete_creating_receipt_is_healed_before_accept_rerun_ever_sees_it() {
        for missing in 0..4 {
            let mut fields = [
                Some("github"),
                Some("acme"),
                Some("widgets"),
                Some("origin"),
            ];
            fields[missing] = None;
            let metadata = creating_metadata(fields[0], fields[1], fields[2], fields[3]);
            let fake = RecordingFake::new();
            // rollback_creating_receipt: 4 unsets + 1 state write.
            for _ in 0..5 {
                fake.push_response(out(0, b""));
            }
            // The re-read metadata::read performs, landing on a plain
            // unpublished grove with no receipt at all.
            fake.push_response(absent()); // grove.version
            fake.push_response(absent()); // grove.defaultBranch
            fake.push_response(absent()); // grove.remote
            fake.push_response(out(0, b"unpublished\n")); // grove.publishState
            fake.push_response(absent()); // grove.publishRemote
            fake.push_response(absent()); // grove.publishUrl
            fake.push_response(absent()); // grove.publishProvider
            fake.push_response(absent()); // grove.publishOwner
            fake.push_response(absent()); // grove.publishName

            let healed = heal_incomplete_creating_receipt(&fake, &grove(), &metadata).unwrap();

            assert_eq!(
                healed.publish_state,
                PublishState::Unpublished,
                "missing {missing}"
            );
            let calls = calls_of(&fake);
            assert_eq!(calls.len(), 14, "missing {missing}");
            assert_eq!(
                calls[0],
                vec![
                    "config",
                    "--file",
                    CONFIG,
                    "--unset-all",
                    "grove.publishProvider"
                ]
            );
            assert_eq!(
                calls[3],
                vec![
                    "config",
                    "--file",
                    CONFIG,
                    "--unset-all",
                    "grove.publishRemote"
                ]
            );
            assert_eq!(
                calls[4],
                vec![
                    "config",
                    "--file",
                    CONFIG,
                    "grove.publishState",
                    "unpublished"
                ]
            );
        }
    }

    #[test]
    fn a_complete_creating_receipt_is_never_healed() {
        let metadata = creating_metadata(
            Some("github"),
            Some("acme"),
            Some("widgets"),
            Some("origin"),
        );
        let fake = RecordingFake::new();

        let healed = heal_incomplete_creating_receipt(&fake, &grove(), &metadata).unwrap();

        assert_eq!(healed, metadata);
        assert!(fake.calls().is_empty());
    }

    /// Regression test for a Copilot review finding on PR #5: a `creating`
    /// grove that also carries the forbidden `publishUrl` (Decision 1) used
    /// to trigger `rollback_creating_receipt` here, which clears only the
    /// four creating keys and never touches `publishUrl` — trading one torn
    /// shape (`creating` + a URL) for another (`unpublished` + a stray URL)
    /// instead of actually healing anything. This shape is never this
    /// repair's concern, regardless of whether the four creating keys are
    /// themselves complete: `creating_receipt`/`receipt` report it as the
    /// `Failure` it is.
    #[test]
    fn a_creating_grove_with_a_forbidden_url_is_never_auto_healed() {
        for keys in [
            // All four creating keys complete, but url also present.
            creating_metadata(
                Some("github"),
                Some("acme"),
                Some("widgets"),
                Some("origin"),
            ),
            // Doubly corrupted: a missing creating key *and* url present.
            creating_metadata(Some("github"), None, Some("widgets"), Some("origin")),
        ] {
            let mut metadata = keys;
            metadata.publish_url = Some(BString::from(URL));
            let fake = RecordingFake::new();

            let healed = heal_incomplete_creating_receipt(&fake, &grove(), &metadata).unwrap();

            assert_eq!(healed, metadata);
            assert!(fake.calls().is_empty(), "no rollback may be attempted here");
        }
    }

    #[test]
    fn a_non_creating_state_is_never_healed_even_with_stray_creating_keys() {
        let fake = RecordingFake::new();
        // A state other than `Creating` is not this repair's concern at all —
        // `receipt()`/`creating_receipt()` classify whatever shape it carries.
        let metadata = unpublished();

        let healed = heal_incomplete_creating_receipt(&fake, &grove(), &metadata).unwrap();

        assert_eq!(healed, metadata);
        assert!(fake.calls().is_empty());
    }

    // ---- exact byte equality, with no canonicalisation whatsoever -------

    #[test]
    fn a_dot_git_suffix_is_a_difference_not_the_same_url() {
        let metadata = metadata_of(
            PublishState::Publishing,
            Some("origin"),
            Some("https://example.invalid/r"),
        );

        let error = accept_rerun(&metadata, None, &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
    }

    #[test]
    fn a_trailing_slash_is_a_difference_not_the_same_url() {
        let metadata = metadata_of(
            PublishState::Publishing,
            Some("origin"),
            Some("https://example.invalid/r.git/"),
        );

        let error = accept_rerun(&metadata, None, &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
    }

    #[test]
    fn a_case_difference_is_a_difference() {
        let metadata = metadata_of(
            PublishState::Publishing,
            Some("origin"),
            Some("https://Example.invalid/r.git"),
        );

        let error = accept_rerun(&metadata, None, &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
    }

    #[test]
    fn a_non_utf8_url_round_trips_through_the_matrix_by_bytes() {
        let mut raw = b"https://example.invalid/r-".to_vec();
        raw.push(0xff);
        let request = Request {
            url: OsString::from_vec(raw.clone()),
            remote: OsString::from("origin"),
            all_branches: false,
        };
        let mut metadata = metadata_of(PublishState::Publishing, Some("origin"), None);
        metadata.publish_url = Some(BString::from(raw));

        assert_eq!(
            accept_rerun(&metadata, None, &request).unwrap(),
            Resume::ResumePublishing
        );
    }

    // ---- preflight, decided before anything is mutated -----------------

    /// The responses `run` consumes before it reaches the inspection stage,
    /// for a fresh publication of a grove that has a commit on `main`.
    fn script_preflight(fake: &RecordingFake) {
        fake.push_response(absent()); // remote.origin.url
        fake.push_response(out(0, b"")); // check-ref-format
        fake.push_response(out(0, b"refs/heads/main\n")); // has_any_commit
        fake.push_response(out(0, b"")); // show-ref --verify refs/heads/main
    }

    fn calls_of(fake: &RecordingFake) -> Vec<Vec<String>> {
        fake.calls()
            .iter()
            .map(|call| call.argv_for_test())
            .collect()
    }

    fn wrote_no_publish_state(fake: &RecordingFake) -> bool {
        !calls_of(fake)
            .iter()
            .any(|call| call.iter().any(|arg| arg == "grove.publishState"))
    }

    #[test]
    fn an_empty_url_is_a_usage_error() {
        let fake = RecordingFake::new();
        fake.push_response(absent());
        let request = Request {
            url: OsString::new(),
            remote: OsString::from("origin"),
            all_branches: false,
        };

        let error = run(&fake, &grove(), &unpublished(), &request).unwrap_err();

        assert_eq!(error.class, ExitClass::Usage);
        assert!(wrote_no_publish_state(&fake));
    }

    #[test]
    fn a_remote_name_git_rejects_is_a_usage_error_decided_by_git() {
        let fake = RecordingFake::new();
        fake.push_response(absent());
        fake.push_response(out(1, b"")); // check-ref-format refuses
        let request = Request {
            url: OsString::from(URL),
            remote: OsString::from("a b"),
            all_branches: false,
        };

        let error = run(&fake, &grove(), &unpublished(), &request).unwrap_err();

        assert_eq!(error.class, ExitClass::Usage);
        assert_eq!(
            calls_of(&fake)[1],
            ["check-ref-format", "refs/remotes/a b/HEAD"]
        );
        assert!(wrote_no_publish_state(&fake));
    }

    /// Measured: `a/b` is a valid remote name and yields
    /// `+refs/heads/*:refs/remotes/a/b/*`. Do not require a single level.
    #[test]
    fn a_two_level_remote_name_is_accepted() {
        let fake = RecordingFake::new();

        validate_remote_name(&fake, OsStr::new("a/b")).unwrap();

        assert_eq!(
            calls_of(&fake)[0],
            ["check-ref-format", "refs/remotes/a/b/HEAD"]
        );
    }

    /// Measured M16: prefer `has_any_commit` over `rev-parse --verify HEAD`,
    /// which is exit 128 in an unborn bare repository and would be misread as
    /// a failure rather than a decision.
    #[test]
    fn an_unborn_grove_is_a_decision_to_commit_first() {
        let fake = RecordingFake::new();
        fake.push_response(absent());
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b"")); // has_any_commit: nothing

        let error = run(&fake, &grove(), &unpublished(), &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        assert!(error.detail.unwrap().contains("commit"));
        assert!(wrote_no_publish_state(&fake));
    }

    #[test]
    fn a_missing_default_branch_key_is_derived_from_the_bare_head() {
        let fake = RecordingFake::new();
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b"refs/heads/trunk\n"));
        fake.push_response(out(0, b"refs/heads/trunk\n")); // symbolic-ref HEAD
        fake.push_response(out(0, b"")); // show-ref --verify
        let mut metadata = unpublished();
        metadata.default_branch = None;

        let flight = preflight(&fake, &grove(), &metadata, &request()).unwrap();

        assert_eq!(flight.default_branch, BString::from("trunk"));
        assert_eq!(
            calls_of(&fake)[2],
            [BARE, "symbolic-ref", "--quiet", "HEAD"]
        );
    }

    #[test]
    fn an_ambiguous_default_branch_is_a_decision() {
        for response in [
            out(1, b""),
            out(0, b"refs/tags/v1\n"),
            out(0, b"refs/heads/\n"),
        ] {
            let fake = RecordingFake::new();
            fake.push_response(out(0, b""));
            fake.push_response(out(0, b"refs/heads/main\n"));
            fake.push_response(response);
            let mut metadata = unpublished();
            metadata.default_branch = None;

            let error = preflight(&fake, &grove(), &metadata, &request()).unwrap_err();

            assert_eq!(error.class, ExitClass::NeedsDecision);
            assert!(error.message.contains("default branch"));
        }
    }

    #[test]
    fn a_default_branch_that_does_not_exist_is_a_decision() {
        let fake = RecordingFake::new();
        fake.push_response(absent());
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b"refs/heads/other\n"));
        fake.push_response(out(1, b"")); // show-ref --verify: no refs/heads/main

        let error = run(&fake, &grove(), &unpublished(), &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        assert!(error.message.contains("main"));
        assert!(wrote_no_publish_state(&fake));
    }

    /// `check_publishable_repository` is `preflight`'s URL-independent half —
    /// `--create` calls it with no URL involved at all, and it must give the
    /// exact same refusals `preflight` already gives for "no commit"/"no
    /// default branch".
    #[test]
    fn check_publishable_repository_gives_the_same_refusals_as_preflight_with_no_url_at_all() {
        let fake = RecordingFake::new();
        fake.push_response(out(0, b"")); // has_any_commit: nothing

        let error = check_publishable_repository(&fake, &grove(), &unpublished()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        assert!(error.detail.unwrap().contains("commit"));

        let fake = RecordingFake::new();
        fake.push_response(out(0, b"refs/heads/main\n"));
        fake.push_response(out(1, b"")); // show-ref --verify: no refs/heads/main

        let error = check_publishable_repository(&fake, &grove(), &unpublished()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        assert!(error.message.contains("main"));
    }

    #[test]
    fn check_publishable_repository_succeeds_on_a_grove_ready_to_publish() {
        let fake = RecordingFake::new();
        fake.push_response(out(0, b"refs/heads/main\n"));
        fake.push_response(out(0, b"")); // show-ref --verify refs/heads/main

        let flight = check_publishable_repository(&fake, &grove(), &unpublished()).unwrap();

        assert_eq!(flight.default_branch, BString::from("main"));
    }

    // ---- the probe decision, specification steps 2 and 3 ---------------

    fn advert_of(symref: &str, heads: &[(&str, &str)]) -> GitOutput {
        let mut stdout = format!("ref: {symref}\tHEAD\n{OID}\tHEAD\n");
        for (name, oid) in heads {
            stdout.push_str(&format!("{oid}\t{name}\n"));
        }
        out(0, stdout.as_bytes())
    }

    /// Measured M1: an empty remote advertises zero bytes, so there is nothing
    /// to probe and the flow proceeds straight to the local configuration.
    #[test]
    fn an_empty_remote_is_never_probed() {
        let fake = RecordingFake::new();
        script_preflight(&fake);
        fake.push_response(out(0, b"")); // purge: no probe refs
        fake.push_response(out(0, b"")); // ls-remote: empty
        for _ in 0..3 {
            fake.push_response(out(0, b"")); // the three receipt writes
        }
        fake.push_response(out(0, b"")); // remote add
        fake.push_response(values(&[URL.as_bytes()]));
        fake.push_response(values(&[b"+refs/heads/*:refs/remotes/origin/*"]));
        fake.push_response(values(&[b"true"]));
        fake.push_response(values(&[b"origin"]));

        script_tail(&fake);

        run(&fake, &grove(), &unpublished(), &request()).unwrap();

        let calls = calls_of(&fake);
        assert!(!calls.iter().any(|call| call.contains(&"fetch".to_string())));
        assert!(!calls
            .iter()
            .any(|call| call.contains(&"merge-base".to_string())));
    }

    #[test]
    fn a_remote_whose_head_names_another_branch_is_refused_before_any_probe() {
        let fake = RecordingFake::new();
        script_preflight(&fake);
        fake.push_response(out(0, b""));
        fake.push_response(advert_of(
            "refs/heads/master",
            &[("refs/heads/master", OID)],
        ));

        let error = run(&fake, &grove(), &unpublished(), &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        assert!(error.detail.unwrap().contains("refs/heads/master"));
        let calls = calls_of(&fake);
        assert!(!calls.iter().any(|call| call.contains(&"fetch".to_string())));
        assert!(wrote_no_publish_state(&fake));
    }

    /// Measured M12: a target pushed into while its unborn `HEAD` named
    /// another branch leaves `HEAD` dangling, so `ls-remote --symref` prints
    /// no symref line at all.
    #[test]
    fn a_remote_with_a_dangling_head_is_refused_before_any_probe() {
        let fake = RecordingFake::new();
        script_preflight(&fake);
        fake.push_response(out(0, b""));
        fake.push_response(out(0, format!("{OID}\trefs/heads/main\n").as_bytes()));

        let error = run(&fake, &grove(), &unpublished(), &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        let calls = calls_of(&fake);
        assert!(!calls.iter().any(|call| call.contains(&"fetch".to_string())));
        assert!(wrote_no_publish_state(&fake));
    }

    /// Measured M5: probing a branch the remote does not advertise is exit
    /// 128, so the branch is checked against the advertisement first.
    #[test]
    fn a_remote_not_advertising_the_default_branch_is_refused_before_any_probe() {
        let fake = RecordingFake::new();
        script_preflight(&fake);
        fake.push_response(out(0, b""));
        fake.push_response(advert_of("refs/heads/main", &[("refs/heads/other", OID)]));

        let error = run(&fake, &grove(), &unpublished(), &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        let calls = calls_of(&fake);
        assert!(!calls.iter().any(|call| call.contains(&"fetch".to_string())));
        assert!(wrote_no_publish_state(&fake));
    }

    /// The probe responses for a target that advertises `main`, with
    /// `ancestry` deciding the comparison.
    fn script_probe(fake: &RecordingFake, ancestry: i32) {
        fake.push_response(advert_of("refs/heads/main", &[("refs/heads/main", OID)]));
        fake.push_response(out(0, b"")); // probe fetch
        fake.push_response(out(ancestry, b"")); // merge-base --is-ancestor
        fake.push_response(out(0, b"")); // update-ref -d
    }

    fn probe_ref_deleted(fake: &RecordingFake) -> bool {
        calls_of(fake).iter().any(|call| {
            call.contains(&"update-ref".to_string())
                && call.iter().any(|arg| {
                    arg.starts_with(&format!(
                        "{}",
                        String::from_utf8_lossy(remote::PROBE_PREFIX)
                    ))
                })
        })
    }

    #[test]
    fn a_strictly_behind_remote_is_probed_and_accepted() {
        let fake = RecordingFake::new();
        script_preflight(&fake);
        fake.push_response(out(0, b""));
        script_probe(&fake, 0);
        for _ in 0..3 {
            fake.push_response(out(0, b""));
        }
        fake.push_response(out(0, b"")); // remote add
        fake.push_response(values(&[URL.as_bytes()]));
        fake.push_response(values(&[b"+refs/heads/*:refs/remotes/origin/*"]));
        fake.push_response(values(&[b"true"]));
        fake.push_response(values(&[b"origin"]));

        script_tail(&fake);

        run(&fake, &grove(), &unpublished(), &request()).unwrap();

        assert!(probe_ref_deleted(&fake));
    }

    #[test]
    fn a_diverged_remote_is_refused_and_never_forced() {
        let fake = RecordingFake::new();
        script_preflight(&fake);
        fake.push_response(out(0, b""));
        script_probe(&fake, 1);

        let error = run(&fake, &grove(), &unpublished(), &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        assert!(error.message.contains("diverged"));
        assert!(probe_ref_deleted(&fake));
        assert!(wrote_no_publish_state(&fake));
        assert!(!calls_of(&fake)
            .iter()
            .any(|call| call.iter().any(|arg| arg.starts_with("--force"))));
    }

    /// Measured M6: a missing object is exit 128, not exit 1. A vanished probe
    /// ref gets its own diagnostic and is never reported as a divergence.
    #[test]
    fn a_vanished_probe_ref_is_not_a_divergence_verdict() {
        let fake = RecordingFake::new();
        script_preflight(&fake);
        fake.push_response(out(0, b""));
        script_probe(&fake, 128);

        let error = run(&fake, &grove(), &unpublished(), &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        assert!(error.message.contains("vanished"));
        assert!(!error.message.contains("diverged"));
        assert!(error
            .detail
            .unwrap()
            .contains("another `git grove publish` may be running"));
        assert!(wrote_no_publish_state(&fake));
    }

    #[test]
    fn a_failing_advertisement_leaves_the_grove_untouched() {
        let fake = RecordingFake::new();
        script_preflight(&fake);
        fake.push_response(out(0, b""));
        fake.push_response(GitOutput {
            status: 128,
            stdout: Vec::new(),
            stderr: b"fatal: '/srv/nope.git' does not appear to be a git repository\n".to_vec(),
        });

        let error = run(&fake, &grove(), &unpublished(), &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
        assert!(wrote_no_publish_state(&fake));
    }

    #[test]
    fn leftover_probe_refs_are_purged_first_and_reported() {
        let fake = RecordingFake::new();
        script_preflight(&fake);
        fake.push_response(out(0, b"refs/grove/publish-probe/stale\n"));
        fake.push_response(out(0, b"")); // update-ref -d for the stale ref
        fake.push_response(out(0, b"")); // ls-remote: empty target
        for _ in 0..3 {
            fake.push_response(out(0, b""));
        }
        fake.push_response(out(0, b""));
        fake.push_response(values(&[URL.as_bytes()]));
        fake.push_response(values(&[b"+refs/heads/*:refs/remotes/origin/*"]));
        fake.push_response(values(&[b"true"]));
        fake.push_response(values(&[b"origin"]));

        script_tail(&fake);

        let report = run(&fake, &grove(), &unpublished(), &request()).unwrap();

        assert_eq!(
            report.diagnostics,
            vec!["removed a leftover publication probe ref refs/grove/publish-probe/stale"]
        );
        let calls = calls_of(&fake);
        let purge = calls
            .iter()
            .position(|call| {
                call.contains(&"for-each-ref".to_string())
                    && call.iter().any(|arg| arg.contains("publish-probe"))
            })
            .unwrap();
        let advertise = calls
            .iter()
            .position(|call| call.contains(&"ls-remote".to_string()))
            .unwrap();
        assert!(purge < advertise);
    }

    /// The responses steps 5 to 7 consume on the happy path: the atomic push,
    /// the explicit upstream writes and their verification, the tracking-ref
    /// check and `set-head --auto` with its verification, the server-side
    /// `HEAD` re-advertisement, and the three `published` receipt writes.
    fn script_tail(fake: &RecordingFake) {
        fake.push_response(out(0, b"")); // push --atomic
        fake.push_response(out(0, b"refs/heads/main\n")); // list_local_heads
        fake.push_response(out(0, b"")); // show-ref refs/remotes/origin/main
        fake.push_response(out(0, b"")); // branch.main.remote write
        fake.push_response(out(0, b"")); // branch.main.merge write
        fake.push_response(values(&[b"origin"]));
        fake.push_response(values(&[b"refs/heads/main"]));
        fake.push_response(out(0, b"")); // ref_exists refs/remotes/origin/main
        fake.push_response(advert_of("refs/heads/main", &[("refs/heads/main", OID)]));
        fake.push_response(out(0, b"")); // remote set-head --auto
        fake.push_response(out(0, b"refs/remotes/origin/main\n")); // symbolic-ref
        fake.push_response(out(0, format!("{OID}\n").as_bytes())); // show-ref --hash
        fake.push_response(out(0, format!("{OID}\n").as_bytes())); // rev-parse
        for _ in 0..3 {
            fake.push_response(out(0, b"")); // the published receipt
        }
    }

    // ---- specification step 4: the local remote configuration ----------

    #[test]
    fn a_fresh_publication_adds_the_remote_after_dashes_and_verifies_every_write() {
        let fake = RecordingFake::new();
        script_preflight(&fake);
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b"")); // ls-remote: empty
        for _ in 0..3 {
            fake.push_response(out(0, b""));
        }
        fake.push_response(out(0, b"")); // remote add
        fake.push_response(values(&[URL.as_bytes()]));
        fake.push_response(values(&[b"+refs/heads/*:refs/remotes/origin/*"]));
        fake.push_response(values(&[b"true"]));
        fake.push_response(values(&[b"origin"]));

        script_tail(&fake);

        run(&fake, &grove(), &unpublished(), &request()).unwrap();

        let calls = calls_of(&fake);
        assert!(calls.contains(&vec![
            BARE.to_string(),
            "remote".into(),
            "add".into(),
            "--".into(),
            "origin".into(),
            URL.into(),
        ]));
        assert!(calls.contains(&vec![
            "config".to_string(),
            "--null".into(),
            "--file".into(),
            CONFIG.into(),
            "--get-all".into(),
            "remote.origin.fetch".into(),
        ]));
        assert!(calls.contains(&vec![
            "config".to_string(),
            "--null".into(),
            "--file".into(),
            CONFIG.into(),
            "--get-all".into(),
            "worktree.guessRemote".into(),
        ]));
    }

    /// The receipt is written immediately before `remote add`, and after the
    /// whole read-only inspection.
    #[test]
    fn the_receipt_is_written_immediately_before_the_first_mutation() {
        let fake = RecordingFake::new();
        script_preflight(&fake);
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b""));
        for _ in 0..3 {
            fake.push_response(out(0, b""));
        }
        fake.push_response(out(0, b""));
        fake.push_response(values(&[URL.as_bytes()]));
        fake.push_response(values(&[b"+refs/heads/*:refs/remotes/origin/*"]));
        fake.push_response(values(&[b"true"]));
        fake.push_response(values(&[b"origin"]));

        script_tail(&fake);

        run(&fake, &grove(), &unpublished(), &request()).unwrap();

        let calls = calls_of(&fake);
        let state = calls
            .iter()
            .position(|call| call.contains(&"grove.publishState".to_string()))
            .unwrap();
        let add = calls
            .iter()
            .position(|call| call.contains(&"add".to_string()))
            .unwrap();
        let advertise = calls
            .iter()
            .position(|call| call.contains(&"ls-remote".to_string()))
            .unwrap();
        assert_eq!(
            &calls[state..state + 3],
            [
                vec![
                    "config".to_string(),
                    "--file".into(),
                    CONFIG.into(),
                    "grove.publishState".into(),
                    "publishing".into()
                ],
                vec![
                    "config".to_string(),
                    "--file".into(),
                    CONFIG.into(),
                    "grove.publishRemote".into(),
                    "origin".into()
                ],
                vec![
                    "config".to_string(),
                    "--file".into(),
                    CONFIG.into(),
                    "grove.publishUrl".into(),
                    URL.into()
                ],
            ]
        );
        assert!(advertise < state, "the inspection precedes the receipt");
        assert_eq!(state + 3, add, "nothing separates the receipt from the add");
    }

    /// Measured M13: repeating `remote add` on an existing name is exit 3.
    #[test]
    fn a_resumed_publication_does_not_add_the_remote_again() {
        let fake = RecordingFake::new();
        fake.push_response(values(&[URL.as_bytes()])); // remote.origin.url is live
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b"refs/heads/main\n"));
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b"")); // ls-remote: empty
        for _ in 0..3 {
            fake.push_response(out(0, b""));
        }
        fake.push_response(values(&[URL.as_bytes()]));
        fake.push_response(values(&[b"+refs/heads/*:refs/remotes/origin/*"]));
        fake.push_response(values(&[b"true"]));
        fake.push_response(values(&[b"origin"]));
        let metadata = metadata_of(PublishState::Publishing, Some("origin"), Some(URL));

        script_tail(&fake);

        run(&fake, &grove(), &metadata, &request()).unwrap();

        assert!(!calls_of(&fake)
            .iter()
            .any(|call| call.contains(&"remote".to_string()) && call.contains(&"add".to_string())));
    }

    #[test]
    fn a_narrowed_or_missing_fetch_refspec_is_rewritten_and_reverified() {
        let fake = RecordingFake::new();
        script_preflight(&fake);
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b""));
        for _ in 0..3 {
            fake.push_response(out(0, b""));
        }
        fake.push_response(out(0, b"")); // remote add
        fake.push_response(values(&[URL.as_bytes()]));
        fake.push_response(values(&[b"+refs/heads/main:refs/remotes/origin/main"]));
        fake.push_response(out(0, b"")); // unset-all
        fake.push_response(out(0, b"")); // config --add
        fake.push_response(values(&[b"+refs/heads/*:refs/remotes/origin/*"]));
        fake.push_response(values(&[b"true"]));
        fake.push_response(values(&[b"origin"]));

        script_tail(&fake);

        run(&fake, &grove(), &unpublished(), &request()).unwrap();

        let calls = calls_of(&fake);
        assert!(calls.contains(&vec![
            "config".to_string(),
            "--file".into(),
            CONFIG.into(),
            "--unset-all".into(),
            "remote.origin.fetch".into(),
        ]));
        assert!(calls.contains(&vec![
            "config".to_string(),
            "--file".into(),
            CONFIG.into(),
            "--add".into(),
            "remote.origin.fetch".into(),
            "+refs/heads/*:refs/remotes/origin/*".into(),
        ]));
    }

    #[test]
    fn a_remote_url_git_did_not_store_verbatim_is_a_failure() {
        let fake = RecordingFake::new();
        script_preflight(&fake);
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b""));
        for _ in 0..3 {
            fake.push_response(out(0, b""));
        }
        fake.push_response(out(0, b"")); // remote add
        fake.push_response(values(&[b"https://example.invalid/r"])); // not verbatim

        let error = run(&fake, &grove(), &unpublished(), &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
        assert!(error.detail.unwrap().contains("https://example.invalid/r"));
    }

    #[test]
    fn an_absent_guess_remote_key_is_written_and_verified() {
        let fake = RecordingFake::new();
        script_preflight(&fake);
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b""));
        for _ in 0..3 {
            fake.push_response(out(0, b""));
        }
        fake.push_response(out(0, b""));
        fake.push_response(values(&[URL.as_bytes()]));
        fake.push_response(values(&[b"+refs/heads/*:refs/remotes/origin/*"]));
        fake.push_response(absent()); // worktree.guessRemote missing
        fake.push_response(out(0, b"")); // write it
        fake.push_response(values(&[b"true"])); // verify
        fake.push_response(absent()); // grove.remote missing
        fake.push_response(out(0, b""));
        fake.push_response(values(&[b"origin"]));

        script_tail(&fake);

        run(&fake, &grove(), &unpublished(), &request()).unwrap();

        let calls = calls_of(&fake);
        assert!(calls.contains(&vec![
            "config".to_string(),
            "--file".into(),
            CONFIG.into(),
            "worktree.guessRemote".into(),
            "true".into(),
        ]));
        assert!(calls.contains(&vec![
            "config".to_string(),
            "--file".into(),
            CONFIG.into(),
            "grove.remote".into(),
            "origin".into(),
        ]));
    }

    #[test]
    fn a_url_that_never_round_trips_is_a_verification_failure_not_a_loop() {
        let fake = RecordingFake::new();
        script_preflight(&fake);
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b""));
        for _ in 0..3 {
            fake.push_response(out(0, b""));
        }
        fake.push_response(out(0, b""));
        fake.push_response(values(&[URL.as_bytes()]));
        fake.push_response(values(&[b"+refs/heads/*:refs/remotes/origin/*"]));
        fake.push_response(absent());
        fake.push_response(out(0, b""));
        fake.push_response(absent()); // still absent after the write

        let error = run(&fake, &grove(), &unpublished(), &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
        assert!(error.message.contains("worktree.guessRemote"));
    }

    // ---- specification step 5: the push --------------------------------

    /// Everything a fresh publication consumes up to and including the local
    /// configuration, for a grove with a commit on `main` and an empty target.
    fn script_through_configuration(fake: &RecordingFake) {
        script_preflight(fake);
        fake.push_response(out(0, b"")); // purge
        fake.push_response(out(0, b"")); // ls-remote: empty target
        for _ in 0..3 {
            fake.push_response(out(0, b"")); // publishing receipt
        }
        fake.push_response(out(0, b"")); // remote add
        fake.push_response(values(&[URL.as_bytes()]));
        fake.push_response(values(&[b"+refs/heads/*:refs/remotes/origin/*"]));
        fake.push_response(values(&[b"true"]));
        fake.push_response(values(&[b"origin"]));
    }

    fn push_call(fake: &RecordingFake) -> Vec<String> {
        calls_of(fake)
            .into_iter()
            .find(|call| call.contains(&"push".to_string()))
            .expect("a push must have been issued")
    }

    #[test]
    fn the_default_push_is_one_explicit_non_forced_atomic_refspec() {
        let fake = RecordingFake::new();
        script_through_configuration(&fake);
        script_tail(&fake);

        run(&fake, &grove(), &unpublished(), &request()).unwrap();

        assert_eq!(
            push_call(&fake),
            [
                BARE,
                "push",
                "--atomic",
                "--",
                "origin",
                "refs/heads/main:refs/heads/main",
            ]
        );
    }

    #[test]
    fn no_publication_invocation_ever_forces_or_sets_upstream() {
        let fake = RecordingFake::new();
        script_through_configuration(&fake);
        script_tail(&fake);

        run(&fake, &grove(), &unpublished(), &request()).unwrap();

        for call in calls_of(&fake) {
            for forbidden in [
                "--force",
                "--force-with-lease",
                "--set-upstream",
                "-u",
                "--all",
                "--mirror",
                "--tags",
            ] {
                assert!(
                    !call.iter().any(|arg| arg == forbidden),
                    "{forbidden} appeared in {call:?}"
                );
            }
        }
    }

    #[test]
    fn the_push_child_is_locale_pinned_so_its_stderr_can_be_classified() {
        let fake = RecordingFake::new();
        script_through_configuration(&fake);
        script_tail(&fake);

        run(&fake, &grove(), &unpublished(), &request()).unwrap();

        let pinned: Vec<bool> = fake
            .calls()
            .iter()
            .filter(|call| call.argv_for_test().contains(&"push".to_string()))
            .map(|call| call.is_c_locale())
            .collect();
        assert_eq!(pinned, vec![true]);
    }

    #[test]
    fn all_branches_pushes_every_local_head_in_one_atomic_push() {
        let fake = RecordingFake::new();
        script_preflight(&fake);
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b""));
        for _ in 0..3 {
            fake.push_response(out(0, b""));
        }
        fake.push_response(out(0, b""));
        fake.push_response(values(&[URL.as_bytes()]));
        fake.push_response(values(&[b"+refs/heads/*:refs/remotes/origin/*"]));
        fake.push_response(values(&[b"true"]));
        fake.push_response(values(&[b"origin"]));
        // push_refspecs asks for every local head first.
        fake.push_response(out(
            0,
            b"refs/heads/main\nrefs/heads/topic/x\nrefs/heads/z\n",
        ));
        fake.push_response(out(0, b"")); // the single push
        fake.push_response(out(0, b"refs/heads/main\n")); // configure_upstreams
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b""));
        fake.push_response(values(&[b"origin"]));
        fake.push_response(values(&[b"refs/heads/main"]));
        fake.push_response(out(0, b"")); // verify_tracking_ref: ref_exists
        fake.push_response(advert_of("refs/heads/main", &[("refs/heads/main", OID)])); // server_head_matches
        fake.push_response(out(0, b"")); // remote set-head --auto
        fake.push_response(out(0, b"refs/remotes/origin/main\n")); // remote_head_branch: symbolic-ref
        fake.push_response(out(0, format!("{OID}\n").as_bytes())); // remote_head_branch: show-ref --hash
        fake.push_response(out(0, format!("{OID}\n").as_bytes())); // remote_head_branch: rev-parse
        for _ in 0..3 {
            fake.push_response(out(0, b"")); // the published receipt
        }
        let request = Request {
            url: OsString::from(URL),
            remote: OsString::from("origin"),
            all_branches: true,
        };

        let report = run(&fake, &grove(), &unpublished(), &request).unwrap();
        assert_eq!(report.class, ExitClass::Ok);

        let pushes: Vec<Vec<String>> = calls_of(&fake)
            .into_iter()
            .filter(|call| call.contains(&"push".to_string()))
            .collect();
        assert_eq!(pushes.len(), 1, "one atomic push, never one per branch");
        assert_eq!(
            pushes[0],
            [
                BARE,
                "push",
                "--atomic",
                "--",
                "origin",
                "refs/heads/main:refs/heads/main",
                "refs/heads/topic/x:refs/heads/topic/x",
                "refs/heads/z:refs/heads/z",
            ]
        );
    }

    /// Measured M8: git refuses pre-flight with exit 128 and updates no ref, so
    /// "nothing was published" is guaranteed by git itself and no separate
    /// `--dry-run` capability probe is needed. Measured M15: this match is only
    /// sound because the child is locale-pinned.
    #[test]
    fn a_target_that_does_not_advertise_atomic_push_is_a_named_decision() {
        let fake = RecordingFake::new();
        script_through_configuration(&fake);
        fake.push_response(GitOutput {
            status: 128,
            stdout: Vec::new(),
            stderr: b"fatal: the receiving end does not support --atomic push\nfatal: the remote end hung up unexpectedly\n".to_vec(),
        });

        let error = run(&fake, &grove(), &unpublished(), &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        assert!(error.message.contains("atomic push"));
        assert!(error.detail.unwrap().contains("nothing was published"));
    }

    #[test]
    fn any_other_push_failure_is_a_failure_that_leaves_the_state_publishing() {
        let fake = RecordingFake::new();
        script_through_configuration(&fake);
        fake.push_response(GitOutput {
            status: 1,
            stdout: Vec::new(),
            stderr: b"remote: rejected by policy\nerror: failed to push some refs\n".to_vec(),
        });

        let error = run(&fake, &grove(), &unpublished(), &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
        assert!(error.detail.unwrap().contains("rejected"));
        let states: Vec<String> = calls_of(&fake)
            .into_iter()
            .filter(|call| call.contains(&"grove.publishState".to_string()))
            .map(|call| call.last().unwrap().clone())
            .collect();
        assert_eq!(states, vec!["publishing"], "the state is never advanced");
    }

    // ---- steps 6 and 7 -------------------------------------------------

    #[test]
    fn upstreams_are_written_explicitly_and_the_happy_path_never_fetches() {
        let fake = RecordingFake::new();
        script_through_configuration(&fake);
        script_tail(&fake);

        run(&fake, &grove(), &unpublished(), &request()).unwrap();

        let calls = calls_of(&fake);
        assert!(calls.contains(&vec![
            "config".to_string(),
            "--file".into(),
            CONFIG.into(),
            "branch.main.remote".into(),
            "origin".into(),
        ]));
        assert!(calls.contains(&vec![
            "config".to_string(),
            "--file".into(),
            CONFIG.into(),
            "branch.main.merge".into(),
            "refs/heads/main".into(),
        ]));
        assert!(
            !calls.iter().any(|call| call.contains(&"fetch".to_string())),
            "the push creates the tracking ref itself; no fetch is needed"
        );
    }

    #[test]
    fn set_head_passes_auto_before_the_dashes_that_guard_the_remote_name() {
        let fake = RecordingFake::new();
        script_through_configuration(&fake);
        script_tail(&fake);

        run(&fake, &grove(), &unpublished(), &request()).unwrap();

        assert!(calls_of(&fake).contains(&vec![
            BARE.to_string(),
            "remote".into(),
            "set-head".into(),
            "--auto".into(),
            "--".into(),
            "origin".into(),
        ]));
    }

    #[test]
    fn an_absent_tracking_ref_is_fetched_as_an_explicit_fallback() {
        let fake = RecordingFake::new();
        script_through_configuration(&fake);
        fake.push_response(out(0, b"")); // push
        fake.push_response(out(0, b"refs/heads/main\n"));
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b""));
        fake.push_response(values(&[b"origin"]));
        fake.push_response(values(&[b"refs/heads/main"]));
        fake.push_response(out(1, b"")); // tracking ref absent
        fake.push_response(out(0, b"")); // fallback fetch
        fake.push_response(out(0, b"")); // present now
        fake.push_response(advert_of("refs/heads/main", &[("refs/heads/main", OID)]));
        fake.push_response(out(0, b"")); // set-head
        fake.push_response(out(0, b"refs/remotes/origin/main\n"));
        fake.push_response(out(0, format!("{OID}\n").as_bytes()));
        fake.push_response(out(0, format!("{OID}\n").as_bytes()));
        for _ in 0..3 {
            fake.push_response(out(0, b""));
        }

        run(&fake, &grove(), &unpublished(), &request()).unwrap();

        let fetches: Vec<Vec<String>> = calls_of(&fake)
            .into_iter()
            .filter(|call| call.contains(&"fetch".to_string()))
            .collect();
        assert_eq!(fetches.len(), 1);
        assert_eq!(fetches[0], sync_fetch_argv());
    }

    /// The repair and fallback fetches must stay byte-identical to `sync`'s.
    fn sync_fetch_argv() -> Vec<String> {
        let fake = RecordingFake::new();
        FetchPlan {
            remotes: vec![BString::from("origin")],
        }
        .execute(&fake, &grove())
        .unwrap();
        fake.calls()[0].argv_for_test()
    }

    /// Measured M12: pushing into a target whose unborn `HEAD` names another
    /// branch leaves `HEAD` dangling and silent.
    #[test]
    fn an_unconfirmed_server_head_keeps_publishing_and_asks_for_a_decision() {
        let fake = RecordingFake::new();
        script_through_configuration(&fake);
        fake.push_response(out(0, b"")); // push
        fake.push_response(out(0, b"refs/heads/main\n"));
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b""));
        fake.push_response(values(&[b"origin"]));
        fake.push_response(values(&[b"refs/heads/main"]));
        fake.push_response(out(0, b"")); // tracking ref exists
                                         // The target advertises the branch but no HEAD symref.
        fake.push_response(out(0, format!("{OID}\trefs/heads/main\n").as_bytes()));

        let report = run(&fake, &grove(), &unpublished(), &request()).unwrap();

        assert_eq!(report.class, ExitClass::NeedsDecision);
        assert!(report.lines[0].contains("published main"));
        assert!(report
            .diagnostics
            .last()
            .unwrap()
            .contains("set it by hand"));
        let states: Vec<String> = calls_of(&fake)
            .into_iter()
            .filter(|call| call.contains(&"grove.publishState".to_string()))
            .map(|call| call.last().unwrap().clone())
            .collect();
        assert_eq!(
            states,
            vec!["publishing"],
            "the state is kept at publishing, not advanced and not rolled back"
        );
    }

    /// The shape that makes running step 7 before the local `set-head`
    /// necessary rather than merely tidy. `--all-branches` into an empty target
    /// whose unborn `HEAD` names a branch this grove also has makes the server
    /// `HEAD` resolvable, so `set-head --auto` *succeeds* and points the local
    /// `refs/remotes/<r>/HEAD` at the wrong branch. In the plan's literal order
    /// the local verification would then fail as exit 1, for a condition the
    /// specification names as a decision at exit 2 — and there would be no
    /// stderr to classify, because git exited 0.
    #[test]
    fn a_server_head_naming_another_branch_is_a_decision_even_when_set_head_would_succeed() {
        let fake = RecordingFake::new();
        script_preflight(&fake);
        fake.push_response(out(0, b"")); // purge
        fake.push_response(out(0, b"")); // ls-remote: the target is empty
        for _ in 0..3 {
            fake.push_response(out(0, b""));
        }
        fake.push_response(out(0, b"")); // remote add
        fake.push_response(values(&[URL.as_bytes()]));
        fake.push_response(values(&[b"+refs/heads/*:refs/remotes/origin/*"]));
        fake.push_response(values(&[b"true"]));
        fake.push_response(values(&[b"origin"]));
        fake.push_response(out(0, b"refs/heads/main\nrefs/heads/master\n")); // push_refspecs
        fake.push_response(out(0, b"")); // the single push
        fake.push_response(out(0, b"refs/heads/main\n")); // configure_upstreams
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b""));
        fake.push_response(values(&[b"origin"]));
        fake.push_response(values(&[b"refs/heads/main"]));
        fake.push_response(out(0, b"")); // tracking ref exists
                                         // The push made the target's HEAD resolvable — to `master`, not `main`.
        fake.push_response(advert_of(
            "refs/heads/master",
            &[("refs/heads/main", OID), ("refs/heads/master", OID)],
        ));
        let request = Request {
            url: OsString::from(URL),
            remote: OsString::from("origin"),
            all_branches: true,
        };

        let report = run(&fake, &grove(), &unpublished(), &request).unwrap();

        assert_eq!(report.class, ExitClass::NeedsDecision);
        assert!(report
            .diagnostics
            .last()
            .unwrap()
            .contains("set it by hand"));
        assert!(
            !calls_of(&fake)
                .iter()
                .any(|call| call.contains(&"set-head".to_string())),
            "there is nothing sane to point the local remote HEAD at"
        );
        let states: Vec<String> = calls_of(&fake)
            .into_iter()
            .filter(|call| call.contains(&"grove.publishState".to_string()))
            .map(|call| call.last().unwrap().clone())
            .collect();
        assert_eq!(states, vec!["publishing"]);
    }

    #[test]
    fn a_confirmed_server_head_advances_the_receipt_to_published() {
        let fake = RecordingFake::new();
        script_through_configuration(&fake);
        script_tail(&fake);

        let report = run(&fake, &grove(), &unpublished(), &request()).unwrap();

        assert_eq!(report.class, ExitClass::Ok);
        let states: Vec<String> = calls_of(&fake)
            .into_iter()
            .filter(|call| call.contains(&"grove.publishState".to_string()))
            .map(|call| call.last().unwrap().clone())
            .collect();
        assert_eq!(states, vec!["publishing", "published"]);
    }

    // ---- the published rerun -------------------------------------------

    fn published_metadata() -> Metadata {
        metadata_of(PublishState::Published, Some("origin"), Some(URL))
    }

    /// Everything the repair path consumes up to its ancestry classification.
    fn script_repair_preamble(fake: &RecordingFake) {
        fake.push_response(values(&[URL.as_bytes()])); // live remote url
        fake.push_response(out(0, b"")); // check-ref-format
        fake.push_response(out(0, b"refs/heads/main\n")); // has_any_commit
        fake.push_response(out(0, b"")); // show-ref refs/heads/main
        fake.push_response(out(0, b"")); // the sync-shaped fetch
        fake.push_response(values(&[URL.as_bytes()])); // remote.origin.url
        fake.push_response(values(&[b"+refs/heads/*:refs/remotes/origin/*"]));
        fake.push_response(values(&[b"true"])); // worktree.guessRemote
        fake.push_response(values(&[b"origin"])); // grove.remote
        fake.push_response(out(0, b"refs/heads/main\n")); // configure_upstreams
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b""));
        fake.push_response(out(0, b""));
        fake.push_response(values(&[b"origin"]));
        fake.push_response(values(&[b"refs/heads/main"]));
    }

    fn script_repair_tail(fake: &RecordingFake) {
        fake.push_response(out(0, b"")); // tracking ref exists
        fake.push_response(advert_of("refs/heads/main", &[("refs/heads/main", OID)]));
        fake.push_response(out(0, b"")); // set-head
        fake.push_response(out(0, b"refs/remotes/origin/main\n"));
        fake.push_response(out(0, format!("{OID}\n").as_bytes()));
        fake.push_response(out(0, format!("{OID}\n").as_bytes()));
    }

    #[test]
    fn a_repair_run_fetches_exactly_as_sync_does_and_uses_no_probe_ref() {
        let fake = RecordingFake::new();
        script_repair_preamble(&fake);
        fake.push_response(out(0, b"")); // is_ancestor: tracking is an ancestor
        fake.push_response(out(0, format!("{OID}\n").as_bytes())); // remote oid
        fake.push_response(out(0, format!("{OID}\n").as_bytes())); // local oid
        script_repair_tail(&fake);

        let report = run(&fake, &grove(), &published_metadata(), &request()).unwrap();

        assert_eq!(report.class, ExitClass::Ok);
        assert!(report.lines[0].contains("already at"));
        let calls = calls_of(&fake);
        let fetches: Vec<&Vec<String>> = calls
            .iter()
            .filter(|call| call.contains(&"fetch".to_string()))
            .collect();
        assert_eq!(fetches.len(), 1);
        assert_eq!(*fetches[0], sync_fetch_argv());
        assert!(
            !calls
                .iter()
                .any(|call| call.iter().any(|arg| arg.contains("publish-probe"))),
            "the repair path compares against the tracking ref, never a probe ref"
        );
        assert!(!calls.iter().any(|call| call.contains(&"push".to_string())));
    }

    #[test]
    fn a_repair_run_pushes_when_the_target_is_strictly_behind() {
        let fake = RecordingFake::new();
        script_repair_preamble(&fake);
        fake.push_response(out(0, b"")); // is_ancestor: Ancestor
        fake.push_response(out(0, format!("{OID}\n").as_bytes())); // remote oid
        fake.push_response(out(0, b"aaaaaaa\n")); // local oid differs
        fake.push_response(out(0, b"")); // push
        script_repair_tail(&fake);

        let report = run(&fake, &grove(), &published_metadata(), &request()).unwrap();

        assert_eq!(report.class, ExitClass::Ok);
        assert!(report.lines[0].contains("advanced main"));
        assert_eq!(
            push_call(&fake),
            [
                BARE,
                "push",
                "--atomic",
                "--",
                "origin",
                "refs/heads/main:refs/heads/main",
            ]
        );
    }

    #[test]
    fn a_repair_run_refuses_when_the_target_is_ahead_or_diverged() {
        let fake = RecordingFake::new();
        script_repair_preamble(&fake);
        fake.push_response(out(1, b"")); // is_ancestor: NotAncestor

        let error = run(&fake, &grove(), &published_metadata(), &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        assert!(error.detail.unwrap().contains("nothing was pushed"));
        assert!(!calls_of(&fake)
            .iter()
            .any(|call| call.contains(&"push".to_string())));
    }

    /// A configured remote's tracking ref vanishing mid-run is not a decision a
    /// user can make, so it is exit 1 and not exit 2.
    #[test]
    fn a_repair_run_whose_tracking_ref_vanished_is_a_failure() {
        let fake = RecordingFake::new();
        script_repair_preamble(&fake);
        fake.push_response(out(128, b"")); // is_ancestor: MissingObject

        let error = run(&fake, &grove(), &published_metadata(), &request()).unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
        assert!(error.message.contains("vanished"));
    }

    #[test]
    fn a_repair_run_demotes_to_publishing_when_the_server_head_no_longer_matches() {
        let fake = RecordingFake::new();
        script_repair_preamble(&fake);
        fake.push_response(out(0, b""));
        fake.push_response(out(0, format!("{OID}\n").as_bytes()));
        fake.push_response(out(0, format!("{OID}\n").as_bytes()));
        fake.push_response(out(0, b"")); // tracking ref exists
        fake.push_response(advert_of("refs/heads/master", &[("refs/heads/main", OID)]));
        for _ in 0..3 {
            fake.push_response(out(0, b""));
        }

        let report = run(&fake, &grove(), &published_metadata(), &request()).unwrap();

        assert_eq!(report.class, ExitClass::NeedsDecision);
        let states: Vec<String> = calls_of(&fake)
            .into_iter()
            .filter(|call| call.contains(&"grove.publishState".to_string()))
            .map(|call| call.last().unwrap().clone())
            .collect();
        assert_eq!(states, vec!["publishing"]);
        let receipts: Vec<Vec<String>> = calls_of(&fake)
            .into_iter()
            .filter(|call| {
                call.iter()
                    .any(|arg| arg == "grove.publishUrl" || arg == "grove.publishRemote")
            })
            .collect();
        assert_eq!(
            receipts.last().unwrap().last().unwrap(),
            URL,
            "the receipt itself is untouched, so the matrix still resumes"
        );
    }

    // ==== `publish --create` ==============================================

    const OWNER: &str = "acme";
    const NAME: &str = "widgets";
    const HTTPS_URL: &str = "https://github.com/acme/widgets.git";
    const SSH_URL: &str = "git@github.com:acme/widgets.git";

    fn create_request() -> CreateRequest {
        CreateRequest {
            owner: OsString::from(OWNER),
            name: OsString::from(NAME),
            provider: Provider::GitHub,
            public: false,
            remote: OsString::from("origin"),
            all_branches: false,
        }
    }

    fn po(status: i32, stdout: &[u8]) -> PO {
        PO {
            status,
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        }
    }

    fn po_err(status: i32, stderr: &[u8]) -> PO {
        PO {
            status,
            stdout: Vec::new(),
            stderr: stderr.to_vec(),
        }
    }

    fn gh_view_json(name_with_owner: &str, is_empty: bool) -> Vec<u8> {
        format!(
            r#"{{"url":"{HTTPS_URL}","sshUrl":"{SSH_URL}","isEmpty":{is_empty},"nameWithOwner":"{name_with_owner}"}}"#
        )
        .into_bytes()
    }

    // ---- reconcile_create -------------------------------------------------

    #[test]
    fn reconcile_create_is_fresh_when_nothing_is_recorded_at_all() {
        let metadata = unpublished();

        assert_eq!(
            reconcile_create(&metadata, None, &create_request()).unwrap(),
            CreateResume::Fresh
        );
    }

    /// A Copilot review finding on PR #5 raised this exact shape as a
    /// possible bug: a `published` grove with **no** receipt at all (every
    /// `git grove clone`-created grove, including every shipped v0.2.0) whose
    /// remote was later removed by hand. Verified against the spec
    /// (`.superpowers/specs/2026-08-21-git-grove-publish-create.md`,
    /// "`--create` against every other state"), which explicitly lists this
    /// exact case among "no creating receipt is recorded at all" and
    /// documents `Ok(Resume::Fresh)` as the correct outcome — precisely
    /// because a bare `publish <url>` rerun already treats it identically
    /// (`a_cloned_grove_whose_remote_was_removed_can_be_published_afresh`
    /// above), and `reconcile_create` exists specifically to inherit that
    /// precedent unmodified rather than re-deriving it. This is not a bug;
    /// this test pins it so it is never "fixed" into a regression later.
    #[test]
    fn reconcile_create_is_fresh_for_a_published_grove_with_no_receipt_and_no_live_remote() {
        let metadata = metadata_of(PublishState::Published, None, None);

        assert_eq!(
            reconcile_create(&metadata, None, &create_request()).unwrap(),
            CreateResume::Fresh
        );
    }

    #[test]
    fn reconcile_create_refuses_a_stray_remote_on_an_unpublished_grove() {
        let metadata = unpublished();

        let error = reconcile_create(
            &metadata,
            Some(&BString::from("https://other.invalid/r.git")),
            &create_request(),
        )
        .unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
    }

    #[test]
    fn reconcile_create_refuses_a_grove_already_publishing_or_published_with_no_creating_receipt() {
        for state in [PublishState::Publishing, PublishState::Published] {
            let metadata = metadata_of(state, Some("origin"), Some(URL));

            let error = reconcile_create(&metadata, Some(&BString::from(URL)), &create_request())
                .unwrap_err();

            assert_eq!(error.class, ExitClass::NeedsDecision);
            assert!(error.detail.unwrap().contains("--create"));
        }
    }

    fn creating_receipt_metadata(
        owner: &str,
        name: &str,
        remote: &str,
        provider: &str,
    ) -> Metadata {
        creating_metadata(Some(provider), Some(owner), Some(name), Some(remote))
    }

    #[test]
    fn reconcile_create_continues_a_matching_creating_grove() {
        let metadata = creating_receipt_metadata(OWNER, NAME, "origin", "github");

        assert_eq!(
            reconcile_create(&metadata, None, &create_request()).unwrap(),
            CreateResume::Continue
        );
    }

    #[test]
    fn reconcile_create_resumes_existing_for_a_matching_publishing_grove() {
        let mut metadata = creating_receipt_metadata(OWNER, NAME, "origin", "github");
        metadata.publish_state = PublishState::Publishing;
        metadata.publish_remote = Some(BString::from("origin"));
        metadata.publish_url = Some(BString::from(URL));

        let resume = reconcile_create(&metadata, None, &create_request()).unwrap();

        assert_eq!(
            resume,
            CreateResume::ResumeExisting(Request {
                url: OsString::from(URL),
                remote: OsString::from("origin"),
                all_branches: false,
            })
        );
    }

    #[test]
    fn reconcile_create_refuses_a_mismatched_owner_or_name() {
        let metadata = creating_receipt_metadata("other-owner", NAME, "origin", "github");

        let error = reconcile_create(&metadata, None, &create_request()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        assert!(error.detail.unwrap().contains("other-owner"));
    }

    /// Regression test for the round-6 finding: a naive comparison must not
    /// silently discard a disagreeing `--remote`.
    #[test]
    fn reconcile_create_refuses_a_mismatched_remote_specifically() {
        let metadata = creating_receipt_metadata(OWNER, NAME, "upstream", "github");

        let error = reconcile_create(&metadata, None, &create_request()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        let detail = error.detail.unwrap();
        assert!(detail.contains("upstream"));
        assert!(detail.contains("origin"));
    }

    // ---- run_sequencing -----------------------------------------------

    #[test]
    fn fresh_sequencing_succeeds_when_create_returns_zero() {
        let git = RecordingFake::new();
        let provider = ProviderFake::new();
        provider.push_response(po(0, b"")); // create
        provider.push_response(po(0, &gh_view_json("acme/widgets", true))); // repo view

        let view = run_sequencing(
            &git,
            &provider,
            &grove(),
            &create_request(),
            &Preflight {
                default_branch: BString::from("main"),
            },
            false,
        )
        .unwrap();

        assert_eq!(view.https_url, BString::from(HTTPS_URL));
        assert!(view.is_empty);
        assert!(
            git.calls().is_empty(),
            "no git call happens during sequencing itself"
        );
    }

    #[test]
    fn fresh_sequencing_rolls_back_and_surfaces_the_original_failure_when_repo_view_confirms_missing(
    ) {
        let git = RecordingFake::new();
        let provider = ProviderFake::new();
        provider.push_response(po_err(1, b"some generic failure")); // create
        provider.push_response(po(1, b"")); // repo view: missing

        let error = run_sequencing(
            &git,
            &provider,
            &grove(),
            &create_request(),
            &flight(),
            false,
        )
        .unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
        assert!(error.detail.unwrap().contains("some generic failure"));
        assert_eq!(
            calls_of(&git)[0],
            vec![
                "config",
                "--file",
                CONFIG,
                "--unset-all",
                "grove.publishProvider"
            ]
        );
    }

    #[test]
    fn fresh_sequencing_maps_ghs_exit_4_to_a_decision() {
        let git = RecordingFake::new();
        let provider = ProviderFake::new();
        provider.push_response(po(4, b"authentication required")); // create
        provider.push_response(po(1, b"")); // repo view: missing

        let error = run_sequencing(
            &git,
            &provider,
            &grove(),
            &create_request(),
            &flight(),
            false,
        )
        .unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
    }

    /// Regression test for a Copilot review finding on PR #5:
    /// `create_failure_error` reported only `stderr`, so a provider CLI (or
    /// a wrapper in front of one) that puts its diagnostic on `stdout`
    /// instead produced an empty, unenlightening detail even though useful
    /// output existed.
    #[test]
    fn fresh_sequencing_surfaces_a_create_failures_stdout_when_stderr_is_empty() {
        let git = RecordingFake::new();
        let provider = ProviderFake::new();
        provider.push_response(po(1, b"rate limit exceeded")); // create: stdout only
        provider.push_response(po(1, b"")); // repo view: missing

        let error = run_sequencing(
            &git,
            &provider,
            &grove(),
            &create_request(),
            &flight(),
            false,
        )
        .unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
        assert_eq!(error.detail.as_deref(), Some("rate limit exceeded"));
    }

    #[test]
    fn fresh_sequencing_treats_create_failure_as_success_when_repo_view_confirms_it_exists_and_matches(
    ) {
        let git = RecordingFake::new();
        let provider = ProviderFake::new();
        provider.push_response(po(1, b"rate limited")); // create fails
        provider.push_response(po(0, &gh_view_json("acme/widgets", false))); // exists, matches, non-empty

        let view = run_sequencing(
            &git,
            &provider,
            &grove(),
            &create_request(),
            &flight(),
            false,
        )
        .unwrap();

        assert!(!view.is_empty);
        assert!(
            git.calls().is_empty(),
            "the original failure is not surfaced"
        );
    }

    #[test]
    fn fresh_sequencing_refuses_an_unrelated_existing_repository_and_rolls_back() {
        let git = RecordingFake::new();
        let provider = ProviderFake::new();
        provider.push_response(po(1, b"name already exists"));
        provider.push_response(po(0, &gh_view_json("someone-else/widgets", true)));

        let error = run_sequencing(
            &git,
            &provider,
            &grove(),
            &create_request(),
            &flight(),
            false,
        )
        .unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        assert!(error.message.contains("unrelated"));
        assert_eq!(
            calls_of(&git)[0],
            vec![
                "config",
                "--file",
                CONFIG,
                "--unset-all",
                "grove.publishProvider"
            ]
        );
    }

    #[test]
    fn continuation_sequencing_never_calls_create_when_repo_view_already_confirms_it() {
        for is_empty in [true, false] {
            let git = RecordingFake::new();
            let provider = ProviderFake::new();
            provider.push_response(po(0, &gh_view_json("acme/widgets", is_empty)));

            run_sequencing(
                &git,
                &provider,
                &grove(),
                &create_request(),
                &flight(),
                true,
            )
            .unwrap();

            assert_eq!(
                provider.calls().len(),
                1,
                "create must never be called on this path"
            );
            assert!(!provider.calls()[0].1.contains(&OsString::from("create")));
        }
    }

    #[test]
    fn continuation_sequencing_falls_through_to_create_when_repo_view_finds_nothing() {
        let git = RecordingFake::new();
        let provider = ProviderFake::new();
        provider.push_response(po(1, b"")); // step 5 probe: missing
        provider.push_response(po(0, b"")); // create
        provider.push_response(po(0, &gh_view_json("acme/widgets", true))); // step 6 follow-up

        let view = run_sequencing(
            &git,
            &provider,
            &grove(),
            &create_request(),
            &flight(),
            true,
        )
        .unwrap();

        assert!(view.is_empty);
        assert_eq!(provider.calls().len(), 3);
    }

    #[test]
    fn continuation_sequencing_refuses_an_unrelated_repo_view_result_without_ever_calling_create() {
        let git = RecordingFake::new();
        let provider = ProviderFake::new();
        provider.push_response(po(0, &gh_view_json("someone-else/widgets", true)));

        let error = run_sequencing(
            &git,
            &provider,
            &grove(),
            &create_request(),
            &flight(),
            true,
        )
        .unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        assert_eq!(provider.calls().len(), 1, "create must never be called");
    }

    #[test]
    fn repo_view_erroring_stays_creating_and_names_the_four_keys() {
        struct AlwaysFails;
        impl ProviderRunner for AlwaysFails {
            fn run(&self, _: Provider, _: &[&OsStr]) -> Result<ProviderOutput> {
                Err(GroveError::failure(
                    "cannot run gh: No such file or directory",
                ))
            }
        }
        let git = RecordingFake::new();

        let error = run_sequencing(
            &git,
            &AlwaysFails,
            &grove(),
            &create_request(),
            &flight(),
            true,
        )
        .unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
        let detail = error.detail.unwrap();
        assert!(detail.contains("grove.publishProvider"));
        assert!(
            detail.contains("cannot run gh: No such file or directory"),
            "the underlying spawn failure must not be discarded: {detail}"
        );
        assert!(
            git.calls().is_empty(),
            "the receipt is never rolled back here"
        );
    }

    fn flight() -> Preflight {
        Preflight {
            default_branch: BString::from("main"),
        }
    }

    // ---- derive_and_publish -------------------------------------------

    fn view_of(is_empty: bool) -> RepoView {
        RepoView {
            https_url: BString::from(HTTPS_URL),
            ssh_url: BString::from(SSH_URL),
            is_empty,
            matches_target: true,
        }
    }

    #[test]
    fn derive_and_publish_uses_https_by_default() {
        let git = RecordingFake::new();
        let provider = ProviderFake::new();
        provider.push_response(po(1, b"")); // git_protocol lookup fails -> default https

        let request =
            derive_and_publish(&git, &provider, &grove(), &create_request(), view_of(true))
                .unwrap();

        assert_eq!(request.url, OsString::from(HTTPS_URL));
        let calls = calls_of(&git);
        assert_eq!(
            calls[0],
            vec![
                "config",
                "--file",
                CONFIG,
                "grove.publishState",
                "publishing"
            ]
        );
        assert!(calls
            .iter()
            .any(|call| call.last() == Some(&HTTPS_URL.to_string())));
    }

    #[test]
    fn derive_and_publish_uses_ssh_when_git_protocol_is_scoped_to_ssh() {
        let git = RecordingFake::new();
        let provider = ProviderFake::new();
        provider.push_response(po(0, b"ssh\n"));

        let request =
            derive_and_publish(&git, &provider, &grove(), &create_request(), view_of(true))
                .unwrap();

        assert_eq!(request.url, OsString::from(SSH_URL));
        assert_eq!(
            provider.calls()[0].1,
            vec![
                OsString::from("config"),
                OsString::from("get"),
                OsString::from("git_protocol"),
                OsString::from("--host"),
                OsString::from("github.com"),
            ],
            "the git_protocol lookup must be host-scoped, never the unscoped form"
        );
    }

    #[test]
    fn derive_and_publish_leaves_the_creating_keys_untouched() {
        let git = RecordingFake::new();
        let provider = ProviderFake::new();
        provider.push_response(po(0, b"https\n"));

        derive_and_publish(&git, &provider, &grove(), &create_request(), view_of(true)).unwrap();

        let calls = calls_of(&git);
        assert!(!calls.iter().any(|call| call
            .iter()
            .any(|arg| arg.starts_with("grove.publishProvider")
                || arg.starts_with("grove.publishOwner")
                || arg.starts_with("grove.publishName"))));
    }

    // ---- check_provider_auth / version gate call sites -----------------

    #[test]
    fn check_provider_auth_accepts_a_zero_exit() {
        let provider = ProviderFake::new();
        provider.push_response(po(0, b""));

        check_provider_auth(&provider, Provider::GitHub).unwrap();
    }

    #[test]
    fn check_provider_auth_refuses_a_nonzero_exit_as_a_decision() {
        let provider = ProviderFake::new();
        provider.push_response(po(1, b"not logged in"));

        let error = check_provider_auth(&provider, Provider::GitHub).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        assert!(error.message.contains("github.com"));
    }

    // ---- the gh-default-branch repair -----------------------------------

    #[test]
    fn gh_default_branch_repair_is_a_noop_without_a_provider_runner() {
        let metadata = creating_receipt_metadata(OWNER, NAME, "origin", "github");
        attempt_gh_default_branch_repair(None, &metadata, &request(), &flight()).unwrap();
    }

    #[test]
    fn gh_default_branch_repair_is_a_noop_for_a_bare_published_grove() {
        let provider = ProviderFake::new();
        let metadata = unpublished();

        attempt_gh_default_branch_repair(Some(&provider), &metadata, &request(), &flight())
            .unwrap();

        assert!(provider.calls().is_empty());
    }

    #[test]
    fn gh_default_branch_repair_never_applies_to_gitlab() {
        let provider = ProviderFake::new();
        let metadata = creating_receipt_metadata(OWNER, NAME, "origin", "gitlab");

        attempt_gh_default_branch_repair(Some(&provider), &metadata, &request(), &flight())
            .unwrap();

        assert!(provider.calls().is_empty());
    }

    #[test]
    fn gh_default_branch_repair_runs_gh_repo_edit_for_a_matching_creating_grove() {
        let provider = ProviderFake::new();
        provider.push_response(po(0, b""));
        let metadata = creating_receipt_metadata(OWNER, NAME, "origin", "github");

        attempt_gh_default_branch_repair(Some(&provider), &metadata, &request(), &flight())
            .unwrap();

        assert_eq!(
            provider.calls()[0],
            (
                Provider::GitHub,
                vec![
                    OsString::from("repo"),
                    OsString::from("edit"),
                    OsString::from("acme/widgets"),
                    OsString::from("--default-branch"),
                    OsString::from("main"),
                ]
            )
        );
    }

    // ---- run_create end to end (unit level, both runners faked) --------

    /// The fresh path writes the creating receipt, runs sequencing, derives
    /// the URL, writes the classic receipt, and hands off into `run()`'s own
    /// machinery — proven here by a deterministic second preflight failure
    /// downstream, which only `run_with_provider`'s own re-entry into
    /// `preflight` could produce.
    #[test]
    fn run_create_fresh_writes_the_creating_receipt_then_hands_off_to_run() {
        let git = RecordingFake::new();
        let provider = ProviderFake::new();
        provider.push_response(po(0, b"gh version 2.97.0 (2026-07-31)\n")); // version gate
        provider.push_response(po(0, b"")); // auth status
        provider.push_response(po(0, b"")); // create
        provider.push_response(po(0, &gh_view_json("acme/widgets", true))); // repo view
        provider.push_response(po(1, b"")); // git_protocol -> default https

        git.push_response(out(0, b"")); // validate_remote_name: check-ref-format
                                        // check_publishable_repository
        git.push_response(out(0, b"refs/heads/main\n")); // has_any_commit
        git.push_response(out(0, b"")); // show-ref --verify refs/heads/main
                                        // live_remote_url (none configured yet)
        git.push_response(absent());
        // write_creating_receipt: state, provider, owner, name, remote.
        for _ in 0..5 {
            git.push_response(out(0, b""));
        }
        // derive_and_publish's classic write_receipt: state, remote, url.
        for _ in 0..3 {
            git.push_response(out(0, b""));
        }
        // metadata::read, re-reading after derive_and_publish's classic write.
        git.push_response(absent()); // grove.version
        git.push_response(out(0, b"main\n")); // grove.defaultBranch
        git.push_response(absent()); // grove.remote
        git.push_response(out(0, b"publishing\n")); // grove.publishState
        git.push_response(out(0, b"origin\n")); // grove.publishRemote
        git.push_response(out(0, format!("{HTTPS_URL}\n").as_bytes())); // grove.publishUrl
        git.push_response(out(0, b"github\n")); // grove.publishProvider
        git.push_response(out(0, format!("{OWNER}\n").as_bytes())); // grove.publishOwner
        git.push_response(out(0, format!("{NAME}\n").as_bytes())); // grove.publishName
                                                                   // run_with_provider: live_remote_url, then a second, deterministic
                                                                   // preflight failure stands in for the rest of the existing,
                                                                   // already-tested `run()` pipeline.
        git.push_response(absent());
        git.push_response(out(0, b"")); // check-ref-format
        git.push_response(out(0, b"")); // has_any_commit: nothing this time

        let metadata = unpublished();
        let error =
            run_create(&git, &provider, &grove(), &metadata, &create_request()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        assert!(error.detail.unwrap().contains("commit"));
        let calls = calls_of(&git);
        assert!(calls.iter().any(
            |call| call == &vec!["config", "--file", CONFIG, "grove.publishState", "creating"]
        ));
        assert!(calls.iter().any(|call| call
            == &vec![
                "config",
                "--file",
                CONFIG,
                "grove.publishState",
                "publishing"
            ]));
    }

    #[test]
    fn run_create_refuses_below_the_version_floor_before_touching_the_grove() {
        let git = RecordingFake::new();
        let provider = ProviderFake::new();
        provider.push_response(po(0, b"gh version 2.90.0 (2026-01-01)\n"));

        let metadata = unpublished();
        let error =
            run_create(&git, &provider, &grove(), &metadata, &create_request()).unwrap_err();

        assert_eq!(error.class, ExitClass::Usage);
        assert!(git.calls().is_empty());
    }

    #[test]
    fn run_create_refuses_an_unauthenticated_provider_after_the_local_preflight() {
        let git = RecordingFake::new();
        let provider = ProviderFake::new();
        provider.push_response(po(0, b"gh version 2.97.0 (2026-07-31)\n"));
        provider.push_response(po(1, b"not logged in"));
        git.push_response(out(0, b"")); // check-ref-format
        git.push_response(out(0, b"refs/heads/main\n"));
        git.push_response(out(0, b""));

        let metadata = unpublished();
        let error =
            run_create(&git, &provider, &grove(), &metadata, &create_request()).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
    }

    #[test]
    fn run_create_against_an_already_published_creating_grove_never_touches_the_provider() {
        let git = RecordingFake::new();
        let provider = ProviderFake::new();
        provider.push_response(po(0, b"gh version 2.97.0 (2026-07-31)\n")); // version gate
        provider.push_response(po(0, b"")); // auth status
        git.push_response(out(0, b"")); // check-ref-format
        git.push_response(out(0, b"refs/heads/main\n"));
        git.push_response(out(0, b""));
        git.push_response(values(&[URL.as_bytes()])); // live_remote_url

        let mut metadata = creating_receipt_metadata(OWNER, NAME, "origin", "github");
        metadata.publish_state = PublishState::Published;
        metadata.publish_remote = Some(BString::from("origin"));
        metadata.publish_url = Some(BString::from(URL));

        run_create(&git, &provider, &grove(), &metadata, &create_request()).unwrap_err();
        // A `Published` grove with a matching creating receipt hands off to
        // `run()`, which itself probes the remote before repairing — but the
        // fixture above supplies no further responses for that, so this
        // assertion only needs the provider side: `create`/`repo view` are
        // never invoked once `reconcile_create` resolves to `ResumeExisting`.
        assert_eq!(
            provider.calls().len(),
            2,
            "only the version gate and auth check ran"
        );
    }
}
