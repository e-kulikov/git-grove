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
    config_key, config_values, escaped, required, set_config, trim_one_line,
    validate_refspec_destinations,
};
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
            }
        }

        (PublishState::Publishing, None) => {
            unreachable!("publishing without a receipt is rejected by receipt()")
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
    let output = runner.run(Invocation::new().args([
        OsStr::new("check-ref-format"),
        OsStr::new("--"),
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

/// Run the publication transaction.
pub fn run(
    runner: &dyn GitRunner,
    grove: &Grove,
    metadata: &Metadata,
    request: &Request,
) -> Result<PublishReport> {
    let live = live_remote_url(runner, grove, &request.remote)?;
    let resume = accept_rerun(metadata, live.as_ref(), request)?;
    let preflight = preflight(runner, grove, metadata, request)?;

    if resume == Resume::RepairPublished {
        return Err(GroveError::failure(
            "repairing an already published grove is not implemented yet",
        ));
    }

    let mut diagnostics = Vec::new();
    inspect_target(runner, grove, request, &preflight, &mut diagnostics)?;

    // The first durable write. Everything above is read-only against the remote
    // and self-healing locally.
    metadata::write_receipt(
        runner,
        grove,
        PublishState::Publishing,
        &Receipt {
            remote: BString::from(request.remote.as_bytes().to_vec()),
            url: BString::from(request.url.as_bytes().to_vec()),
        },
    )?;

    configure_local_remote(runner, grove, request, live.is_some())?;

    Ok(PublishReport {
        class: ExitClass::Ok,
        lines: Vec::new(),
        diagnostics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
            ["check-ref-format", "--", "refs/remotes/a b/HEAD"]
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
            ["check-ref-format", "--", "refs/remotes/a/b/HEAD"]
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
}
