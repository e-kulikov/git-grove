//! Read-only inspection of a publication target, plus the transaction-owned
//! probe refs that make the inspection possible before a remote exists.
//!
//! A publication target is inspected by URL, not through a configured remote,
//! because `publish` must decide whether to configure one at all. That leaves
//! no `refs/remotes/<remote>/<branch>` to compare against, so the branch is
//! fetched into `refs/grove/publish-probe/<nonce>` — a namespace this
//! transaction owns outright — and the comparison is made there.

use crate::error::{GroveError, Result};
use crate::fsx;
use crate::git::runner::{GitRunner, Invocation};
use crate::grove::discover::Grove;
use bstr::{BString, ByteSlice};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

/// The namespace every publication probe ref lives in. Nothing else in this
/// tool writes under `refs/grove`, so anything found here is by construction
/// the debris of an interrupted run.
pub const PROBE_PREFIX: &[u8] = b"refs/grove/publish-probe/";

/// What `ls-remote --symref` advertised about a publication target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAdvert {
    /// The remote advertised nothing at all. Measured: an empty repository
    /// prints zero bytes and exits `0` — it does not advertise its unborn
    /// `HEAD` symref, so the name it intends to use is invisible until
    /// something is pushed.
    pub empty: bool,
    /// The full ref `HEAD` resolves to on the remote, e.g. `refs/heads/main`.
    pub head_symref: Option<BString>,
    /// The object `HEAD` resolves to on the remote.
    pub head_oid: Option<BString>,
    /// Every advertised head, as `(full ref, oid)`, in advertised order.
    pub heads: Vec<(BString, BString)>,
}

impl RemoteAdvert {
    /// The advertised object for `refs/heads/<branch>`, if the remote has it.
    pub fn head_oid_for(&self, branch: &[u8]) -> Option<&BString> {
        let mut wanted = b"refs/heads/".to_vec();
        wanted.extend_from_slice(branch);
        self.heads
            .iter()
            .find(|(name, _)| name.as_slice() == wanted.as_slice())
            .map(|(_, oid)| oid)
    }
}

/// The verdict of `merge-base --is-ancestor`, which has three outcomes and not
/// two. Measured: `0` is an ancestor, `1` is not an ancestor — including
/// unrelated histories — and `128` means the named object does not exist.
/// Folding `128` into "not an ancestor" would turn a probe ref that vanished
/// under a racing peer into a bogus divergence verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ancestry {
    Ancestor,
    NotAncestor,
    MissingObject,
}

/// A probe ref that has been fetched and not yet deleted.
///
/// Deliberately a plain value and **not** a `Drop` guard: deletion is an
/// explicit step, so a failure to delete is a reported error rather than
/// something swallowed in a destructor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeRef {
    pub name: BString,
}

fn escaped(bytes: &[u8]) -> String {
    bytes.escape_bytes().to_string()
}

fn invalid(detail: impl Into<String>) -> GroveError {
    GroveError::failure("git returned an invalid remote advertisement").with_detail(detail)
}

/// Ask the remote at `url` what it has, without configuring anything locally.
pub fn advertise(runner: &dyn GitRunner, url: &OsStr) -> Result<RemoteAdvert> {
    let output = runner.run(Invocation::new().args([
        OsStr::new("ls-remote"),
        OsStr::new("--symref"),
        OsStr::new("--"),
        url,
        OsStr::new("HEAD"),
        OsStr::new("refs/heads/*"),
    ]))?;
    if !output.ok() {
        return Err(GroveError::failure(format!(
            "cannot read the publication target {}",
            escaped(url.as_bytes())
        ))
        .with_detail(escaped(&output.stderr)));
    }
    parse_advertisement(&output.stdout)
}

fn parse_advertisement(stdout: &[u8]) -> Result<RemoteAdvert> {
    let mut lines: Vec<&[u8]> = stdout.split(|byte| *byte == b'\n').collect();
    if lines.last() == Some(&(&[] as &[u8])) {
        lines.pop();
    }

    let mut advert = RemoteAdvert {
        empty: lines.is_empty(),
        head_symref: None,
        head_oid: None,
        heads: Vec::new(),
    };

    for line in lines {
        let Some(tab) = line.iter().position(|byte| *byte == b'\t') else {
            return Err(invalid(format!(
                "a line carries no tab separator: {}",
                escaped(line)
            )));
        };
        let (left, right) = (&line[..tab], &line[tab + 1..]);
        if left.is_empty() || right.is_empty() {
            return Err(invalid(format!(
                "a line has an empty field: {}",
                escaped(line)
            )));
        }

        if let Some(target) = left.strip_prefix(b"ref: ") {
            if target.is_empty() {
                return Err(invalid(format!(
                    "a symref line names no target: {}",
                    escaped(line)
                )));
            }
            if right != b"HEAD" {
                // Only `HEAD` is asked for as a symref; anything else is extra
                // information this decision does not use.
                continue;
            }
            if advert.head_symref.is_some() {
                return Err(invalid("HEAD is advertised as a symref twice"));
            }
            advert.head_symref = Some(BString::from(target));
            continue;
        }

        if right == b"HEAD" {
            if advert.head_oid.is_some() {
                return Err(invalid("HEAD is advertised twice"));
            }
            advert.head_oid = Some(BString::from(left));
            continue;
        }

        if advert
            .heads
            .iter()
            .any(|(name, _)| name.as_slice() == right)
        {
            return Err(invalid(format!("{} is advertised twice", escaped(right))));
        }
        advert
            .heads
            .push((BString::from(right), BString::from(left)));
    }

    Ok(advert)
}

/// Remove every probe ref left behind by an interrupted run, returning the
/// names removed so the caller can report them.
///
/// Measured: `update-ref -d` on an absent ref exits `0`, so this is idempotent
/// and safe to run unconditionally before every publication attempt.
pub fn purge_probe_refs(runner: &dyn GitRunner, grove: &Grove) -> Result<Vec<BString>> {
    let mut pattern = PROBE_PREFIX.to_vec();
    pattern.push(b'*');
    let output = runner.run(Invocation::new().git_dir(grove.bare_dir()).args([
        OsStr::new("for-each-ref"),
        OsStr::new("--format=%(refname)"),
        OsStr::new("--"),
        OsStr::from_bytes(&pattern),
    ]))?;
    if !output.ok() {
        return Err(
            GroveError::failure("cannot enumerate leftover publication probe refs")
                .with_detail(escaped(&output.stderr)),
        );
    }

    let mut purged = Vec::new();
    for line in output.stdout.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        if !line.starts_with(PROBE_PREFIX) {
            return Err(GroveError::failure(
                "git listed a ref outside the publication probe namespace",
            )
            .with_detail(escaped(line)));
        }
        delete_probe_ref(runner, grove, line)?;
        purged.push(BString::from(line));
    }
    Ok(purged)
}

/// Fetch `branch` from `url` into a fresh probe ref, by URL, with no remote
/// configured and nothing else touched.
///
/// The flag set matches `git::fetch::FetchPlan::execute`, minus `--prune` and
/// `--no-prune-tags` (which need a configured remote) and plus `--no-tags` and
/// `--no-write-fetch-head`. The three maintenance-suppressing flags are not
/// optional: background maintenance spawned by a probe can outlive a refused
/// run and leave lock files behind that a later `adopt` would refuse on.
///
/// Measured: fetching a branch the remote does not advertise is exit `128`, so
/// callers must call [`advertise`] first and only probe a branch it reported.
pub fn fetch_probe(
    runner: &dyn GitRunner,
    grove: &Grove,
    url: &OsStr,
    branch: &[u8],
) -> Result<ProbeRef> {
    let mut name = PROBE_PREFIX.to_vec();
    name.extend_from_slice(fsx::hex_nonce()?.as_bytes());

    let mut refspec = b"+refs/heads/".to_vec();
    refspec.extend_from_slice(branch);
    refspec.push(b':');
    refspec.extend_from_slice(&name);

    let output = runner.run(Invocation::new().git_dir(grove.bare_dir()).args([
        OsStr::new("fetch"),
        OsStr::new("--atomic"),
        OsStr::new("--no-tags"),
        OsStr::new("--no-write-fetch-head"),
        OsStr::new("--no-recurse-submodules"),
        OsStr::new("--no-auto-maintenance"),
        OsStr::new("--no-write-commit-graph"),
        OsStr::new("--"),
        url,
        OsStr::from_bytes(&refspec),
    ]))?;
    if !output.ok() {
        return Err(GroveError::failure(format!(
            "cannot inspect branch {} on the publication target",
            escaped(branch)
        ))
        .with_detail(escaped(&output.stderr)));
    }
    Ok(ProbeRef {
        name: BString::from(name),
    })
}

/// Classify `candidate` against `descendant`, three ways.
pub fn is_ancestor(
    runner: &dyn GitRunner,
    grove: &Grove,
    candidate: &[u8],
    descendant: &[u8],
) -> Result<Ancestry> {
    let output = runner.run(Invocation::new().git_dir(grove.bare_dir()).args([
        OsStr::new("merge-base"),
        OsStr::new("--is-ancestor"),
        OsStr::new("--end-of-options"),
        OsStr::from_bytes(candidate),
        OsStr::from_bytes(descendant),
    ]))?;
    match output.status {
        0 => Ok(Ancestry::Ancestor),
        1 => Ok(Ancestry::NotAncestor),
        128 => Ok(Ancestry::MissingObject),
        status => Err(GroveError::failure(format!(
            "git merge-base --is-ancestor failed with exit status {status}"
        ))
        .with_detail(escaped(&output.stderr))),
    }
}

/// Delete one probe ref. Idempotent: deleting an absent ref exits `0`.
pub fn delete_probe_ref(runner: &dyn GitRunner, grove: &Grove, name: &[u8]) -> Result<()> {
    if !name.starts_with(PROBE_PREFIX) {
        return Err(GroveError::failure(
            "refused to delete a ref outside the publication probe namespace",
        )
        .with_detail(escaped(name)));
    }
    let output = runner.run(Invocation::new().git_dir(grove.bare_dir()).args([
        OsString::from("update-ref"),
        OsString::from("-d"),
        OsString::from("--"),
        OsString::from_vec(name.to_vec()),
    ]))?;
    if !output.ok() {
        return Err(GroveError::failure(format!(
            "cannot delete the publication probe ref {}",
            escaped(name)
        ))
        .with_detail(escaped(&output.stderr)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ExitClass;
    use crate::git::runner::{GitOutput, RecordingFake};

    fn grove() -> Grove {
        Grove { root: "/g".into() }
    }

    const BARE: &str = "--git-dir=/g/.bare";

    fn output(status: i32, stdout: &[u8], stderr: &[u8]) -> GitOutput {
        GitOutput {
            status,
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    const OID: &str = "c3d445388f83a72043990aeaf22af9ba74aa4797";
    const OTHER_OID: &str = "1111111111111111111111111111111111111111";

    /// Measured: an empty repository prints zero bytes and exits `0`.
    #[test]
    fn zero_bytes_means_the_remote_is_empty() {
        let fake = RecordingFake::new();
        fake.push_response(output(0, b"", b""));

        let advert = advertise(&fake, OsStr::new("/srv/r.git")).unwrap();

        assert!(advert.empty);
        assert_eq!(advert.head_symref, None);
        assert_eq!(advert.head_oid, None);
        assert!(advert.heads.is_empty());
    }

    #[test]
    fn asks_the_remote_with_an_exact_argument_vector() {
        let fake = RecordingFake::new();
        fake.push_response(output(0, b"", b""));

        advertise(&fake, OsStr::new("-weird-url")).unwrap();

        assert_eq!(
            fake.calls()[0].argv_for_test(),
            [
                "ls-remote",
                "--symref",
                "--",
                "-weird-url",
                "HEAD",
                "refs/heads/*"
            ]
        );
    }

    #[test]
    fn passes_the_url_through_as_raw_bytes() {
        let fake = RecordingFake::new();
        fake.push_response(output(0, b"", b""));
        let url = OsString::from_vec(b"/srv/r-\xff.git".to_vec());

        advertise(&fake, &url).unwrap();

        assert_eq!(fake.calls()[0].argv_os()[3], url);
    }

    /// Measured: `ref: refs/heads/main\tHEAD`, then `<oid>\tHEAD`, then one
    /// `<oid>\t<ref>` per head.
    #[test]
    fn parses_the_measured_non_empty_advertisement() {
        let fake = RecordingFake::new();
        fake.push_response(output(
            0,
            format!(
                "ref: refs/heads/main\tHEAD\n{OID}\tHEAD\n{OID}\trefs/heads/main\n{OTHER_OID}\trefs/heads/topic/x\n"
            )
            .as_bytes(),
            b"",
        ));

        let advert = advertise(&fake, OsStr::new("/srv/r.git")).unwrap();

        assert!(!advert.empty);
        assert_eq!(advert.head_symref, Some(BString::from("refs/heads/main")));
        assert_eq!(advert.head_oid, Some(BString::from(OID)));
        assert_eq!(
            advert.heads,
            vec![
                (BString::from("refs/heads/main"), BString::from(OID)),
                (
                    BString::from("refs/heads/topic/x"),
                    BString::from(OTHER_OID)
                ),
            ]
        );
        assert_eq!(advert.head_oid_for(b"main"), Some(&BString::from(OID)));
        assert_eq!(
            advert.head_oid_for(b"topic/x"),
            Some(&BString::from(OTHER_OID))
        );
        assert_eq!(advert.head_oid_for(b"absent"), None);
    }

    #[test]
    fn round_trips_a_non_utf8_branch_name_as_raw_bytes() {
        let fake = RecordingFake::new();
        let mut stdout = format!("ref: refs/heads/main\tHEAD\n{OID}\tHEAD\n{OID}\t").into_bytes();
        stdout.extend_from_slice(b"refs/heads/odd-\xff\n");
        fake.push_response(output(0, &stdout, b""));

        let advert = advertise(&fake, OsStr::new("/srv/r.git")).unwrap();

        assert_eq!(
            advert.heads,
            vec![(
                BString::from(b"refs/heads/odd-\xff".to_vec()),
                BString::from(OID)
            )]
        );
        assert_eq!(advert.head_oid_for(b"odd-\xff"), Some(&BString::from(OID)));
    }

    /// A remote that advertises only `HEAD`, or only tags, is not empty. The
    /// emptiness test is "no lines at all", never "no heads".
    #[test]
    fn a_remote_advertising_only_head_is_not_empty() {
        let fake = RecordingFake::new();
        fake.push_response(output(0, format!("{OID}\tHEAD\n").as_bytes(), b""));

        let advert = advertise(&fake, OsStr::new("/srv/r.git")).unwrap();

        assert!(!advert.empty);
        assert!(advert.heads.is_empty());
        assert_eq!(advert.head_oid, Some(BString::from(OID)));
    }

    #[test]
    fn a_malformed_advertisement_is_a_failure_not_a_best_guess() {
        for stdout in [
            b"no-tab-here\n".to_vec(),
            b"ref: \tHEAD\n".to_vec(),
            b"ref: refs/heads/main\tHEAD\nref: refs/heads/other\tHEAD\n".to_vec(),
            format!("{OID}\trefs/heads/main\n{OTHER_OID}\trefs/heads/main\n").into_bytes(),
            format!("{OID}\tHEAD\n{OTHER_OID}\tHEAD\n").into_bytes(),
            b"\tHEAD\n".to_vec(),
        ] {
            let fake = RecordingFake::new();
            fake.push_response(output(0, &stdout, b""));

            let error = advertise(&fake, OsStr::new("/srv/r.git")).unwrap_err();

            assert_eq!(
                error.class,
                ExitClass::Failure,
                "stdout: {}",
                stdout.escape_bytes()
            );
        }
    }

    /// Measured: a nonexistent repository is a non-zero exit with
    /// `fatal: '<path>' does not appear to be a git repository`.
    #[test]
    fn a_non_zero_ls_remote_exit_is_a_failure_carrying_escaped_stderr() {
        let fake = RecordingFake::new();
        fake.push_response(output(
            128,
            b"",
            b"fatal: '/srv/nope.git' does not appear to be a git repository\n\xff",
        ));

        let error = advertise(&fake, OsStr::new("/srv/nope.git")).unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
        assert!(error.message.contains("/srv/nope.git"));
        assert_eq!(
            error.detail.as_deref(),
            Some(
                r"fatal:\x20'/srv/nope.git'\x20does\x20not\x20appear\x20to\x20be\x20a\x20git\x20repository\n\xFF"
            )
        );
    }

    #[test]
    fn the_probe_fetch_uses_the_measured_flag_set() {
        let fake = RecordingFake::new();

        let probe = fetch_probe(&fake, &grove(), OsStr::new("/srv/r.git"), b"main").unwrap();

        let argv = fake.calls()[0].argv_for_test();
        let (head, tail) = argv.split_at(argv.len() - 3);
        assert_eq!(
            head,
            [
                BARE,
                "fetch",
                "--atomic",
                "--no-tags",
                "--no-write-fetch-head",
                "--no-recurse-submodules",
                "--no-auto-maintenance",
                "--no-write-commit-graph",
            ]
        );
        assert_eq!(tail[0], "--");
        assert_eq!(tail[1], "/srv/r.git");
        assert_eq!(tail[2], format!("+refs/heads/main:{}", probe.name));
        assert!(probe.name.starts_with(PROBE_PREFIX));
        assert_eq!(probe.name.len(), PROBE_PREFIX.len() + 32);
    }

    /// The probe's flags are `FetchPlan::execute`'s, minus the two that need a
    /// configured remote and plus the two the probe needs. Pinned so the two
    /// cannot drift apart.
    fn fetch_flags(argv: Vec<String>) -> Vec<String> {
        argv.into_iter()
            .skip_while(|arg| arg != "fetch")
            .take_while(|arg| arg != "--")
            .filter(|arg| arg.starts_with("--"))
            .collect()
    }

    #[test]
    fn the_probe_flag_set_is_the_sync_fetch_flag_set_by_construction() {
        let fake = RecordingFake::new();
        fetch_probe(&fake, &grove(), OsStr::new("/srv/r.git"), b"main").unwrap();
        let probe = fetch_flags(fake.calls()[0].argv_for_test());

        let sync_fake = RecordingFake::new();
        crate::git::fetch::FetchPlan {
            remotes: vec![BString::from("origin")],
        }
        .execute(&sync_fake, &grove())
        .unwrap();
        let mut expected = fetch_flags(sync_fake.calls()[0].argv_for_test());

        assert!(expected.contains(&"--prune".to_string()));
        assert!(expected.contains(&"--no-prune-tags".to_string()));
        expected.retain(|flag| flag != "--prune" && flag != "--no-prune-tags");
        expected.insert(1, "--no-tags".to_string());
        expected.insert(2, "--no-write-fetch-head".to_string());
        assert_eq!(probe, expected);
    }

    #[test]
    fn each_probe_ref_gets_a_fresh_nonce() {
        let fake = RecordingFake::new();

        let first = fetch_probe(&fake, &grove(), OsStr::new("/srv/r.git"), b"main").unwrap();
        let second = fetch_probe(&fake, &grove(), OsStr::new("/srv/r.git"), b"main").unwrap();

        assert_ne!(first.name, second.name);
    }

    #[test]
    fn a_probe_fetch_failure_names_the_branch_and_escapes_stderr() {
        let fake = RecordingFake::new();
        fake.push_response(output(
            128,
            b"",
            b"fatal: couldn't find remote ref refs/heads/main\n",
        ));

        let error = fetch_probe(&fake, &grove(), OsStr::new("/srv/r.git"), b"main").unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
        assert!(error.message.contains("main"));
        assert_eq!(
            error.detail.as_deref(),
            Some(r"fatal:\x20couldn't\x20find\x20remote\x20ref\x20refs/heads/main\n")
        );
    }

    #[test]
    fn a_non_utf8_branch_reaches_the_refspec_as_raw_bytes() {
        let fake = RecordingFake::new();

        let probe = fetch_probe(&fake, &grove(), OsStr::new("/srv/r.git"), b"odd-\xff").unwrap();

        let argv = fake.calls()[0].argv_os();
        let mut expected = b"+refs/heads/odd-\xff:".to_vec();
        expected.extend_from_slice(&probe.name);
        assert_eq!(argv[argv.len() - 1], OsString::from_vec(expected));
    }

    /// Measured M6: `0` ancestor, `1` not an ancestor including unrelated
    /// histories, `128` the named object does not exist.
    #[test]
    fn ancestry_is_classified_three_ways() {
        for (status, expected) in [
            (0, Ancestry::Ancestor),
            (1, Ancestry::NotAncestor),
            (128, Ancestry::MissingObject),
        ] {
            let fake = RecordingFake::new();
            fake.push_response(output(status, b"", b""));

            let verdict = is_ancestor(&fake, &grove(), b"probe", b"refs/heads/main").unwrap();

            assert_eq!(verdict, expected);
        }
    }

    #[test]
    fn ancestry_asks_git_with_end_of_options_before_the_revisions() {
        let fake = RecordingFake::new();

        is_ancestor(&fake, &grove(), b"-probe", b"refs/heads/main").unwrap();

        assert_eq!(
            fake.calls()[0].argv_for_test(),
            [
                BARE,
                "merge-base",
                "--is-ancestor",
                "--end-of-options",
                "-probe",
                "refs/heads/main",
            ]
        );
    }

    #[test]
    fn an_unexpected_ancestry_exit_is_a_failure() {
        let fake = RecordingFake::new();
        fake.push_response(output(3, b"", b"broken"));

        let error = is_ancestor(&fake, &grove(), b"a", b"b").unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
        assert!(error.message.contains("exit status 3"));
    }

    #[test]
    fn purging_deletes_every_listed_probe_ref_and_reports_the_names() {
        let fake = RecordingFake::new();
        fake.push_response(output(
            0,
            b"refs/grove/publish-probe/aa\nrefs/grove/publish-probe/bb\n",
            b"",
        ));

        let purged = purge_probe_refs(&fake, &grove()).unwrap();

        assert_eq!(
            purged,
            vec![
                BString::from("refs/grove/publish-probe/aa"),
                BString::from("refs/grove/publish-probe/bb"),
            ]
        );
        let calls: Vec<Vec<String>> = fake
            .calls()
            .iter()
            .map(|call| call.argv_for_test())
            .collect();
        assert_eq!(
            calls[0],
            [
                BARE,
                "for-each-ref",
                "--format=%(refname)",
                "--",
                "refs/grove/publish-probe/*",
            ]
        );
        assert_eq!(
            calls[1],
            [
                BARE,
                "update-ref",
                "-d",
                "--",
                "refs/grove/publish-probe/aa"
            ]
        );
        assert_eq!(
            calls[2],
            [
                BARE,
                "update-ref",
                "-d",
                "--",
                "refs/grove/publish-probe/bb"
            ]
        );
    }

    #[test]
    fn purging_an_empty_namespace_deletes_nothing() {
        let fake = RecordingFake::new();
        fake.push_response(output(0, b"", b""));

        let purged = purge_probe_refs(&fake, &grove()).unwrap();

        assert!(purged.is_empty());
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn purging_refuses_a_ref_listed_outside_the_probe_namespace() {
        let fake = RecordingFake::new();
        fake.push_response(output(0, b"refs/heads/main\n", b""));

        let error = purge_probe_refs(&fake, &grove()).unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn deleting_refuses_a_ref_outside_the_probe_namespace() {
        let fake = RecordingFake::new();

        let error = delete_probe_ref(&fake, &grove(), b"refs/heads/main").unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn a_failed_probe_ref_delete_is_reported() {
        let fake = RecordingFake::new();
        fake.push_response(output(1, b"", b"error: cannot lock ref\n"));

        let error = delete_probe_ref(&fake, &grove(), b"refs/grove/publish-probe/aa").unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
        assert_eq!(
            error.detail.as_deref(),
            Some(r"error:\x20cannot\x20lock\x20ref\n")
        );
    }
}
