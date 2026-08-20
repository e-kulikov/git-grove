//! The publication transaction against real git and real local remotes.
//!
//! Every test here uses throwaway repositories and applies the hermetic
//! environment per child; nothing mutates this process's environment.

mod harness;

use harness::Sandbox;
use predicates::prelude::PredicateBooleanExt;
use std::path::{Path, PathBuf};

/// A grove created by `init`, with one commit on its default branch.
fn grove_with_a_commit(sandbox: &Sandbox, name: &str, branch: &str) -> PathBuf {
    sandbox
        .grove(&["init", name, "--branch", branch])
        .assert()
        .success();
    let root = sandbox.root().join(name);
    let worktree = root.join(branch);
    // Deliberately not the content `Sandbox::bare_origin` seeds. Identical
    // content, message, author and second would produce the identical commit
    // object, and the "unrelated history" cases would silently share history.
    std::fs::write(worktree.join("GROVE.md"), format!("grove {name}\n")).unwrap();
    sandbox.git(&worktree, &["add", "GROVE.md"]);
    sandbox.git(&worktree, &["commit", "--quiet", "-m", "grove seed"]);
    root
}

fn bare_of(root: &Path) -> PathBuf {
    root.join(".bare")
}

fn config_of(sandbox: &Sandbox, root: &Path, key: &str) -> Option<String> {
    sandbox.repo_config(&bare_of(root), key)
}

fn probe_refs(sandbox: &Sandbox, root: &Path) -> Vec<String> {
    sandbox
        .remote_refs(&bare_of(root))
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| name.starts_with("refs/grove/"))
        .collect()
}

/// Every regular file under `directory`, recursively. Empty after a probe ref
/// and its reflog are deleted together.
fn reflog_files(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(reflog_files(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// Add a commit on `branch` in `worktree` and return the new tip.
fn advance(sandbox: &Sandbox, worktree: &Path, contents: &str) -> String {
    std::fs::write(worktree.join("README.md"), contents).unwrap();
    sandbox.git(worktree, &["add", "README.md"]);
    sandbox.git(worktree, &["commit", "--quiet", "-m", "advance"]);
    sandbox.oid(worktree, "HEAD")
}

// ---- Step 1: the happy path against an empty remote --------------------

#[test]
fn publishes_a_fresh_grove_to_an_empty_remote_and_leaves_it_syncable() {
    let sandbox = Sandbox::new();
    let origin = sandbox.empty_origin("origin");
    let root = grove_with_a_commit(&sandbox, "g", "main");
    let local = sandbox.oid(&root.join("main"), "HEAD");

    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicates::str::contains("published main to origin"));

    assert_eq!(
        sandbox.remote_refs(&origin),
        vec![("refs/heads/main".to_string(), local.clone())]
    );
    assert_eq!(
        sandbox.remote_head_symref(&origin).as_deref(),
        Some("refs/heads/main")
    );

    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("published")
    );
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishRemote").as_deref(),
        Some("origin")
    );
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishUrl").as_deref(),
        Some(origin.to_str().unwrap())
    );
    assert_eq!(
        config_of(&sandbox, &root, "grove.remote").as_deref(),
        Some("origin")
    );
    assert_eq!(
        config_of(&sandbox, &root, "remote.origin.fetch").as_deref(),
        Some("+refs/heads/*:refs/remotes/origin/*")
    );
    assert_eq!(
        config_of(&sandbox, &root, "worktree.guessRemote").as_deref(),
        Some("true")
    );
    assert_eq!(
        config_of(&sandbox, &root, "branch.main.remote").as_deref(),
        Some("origin")
    );
    assert_eq!(
        config_of(&sandbox, &root, "branch.main.merge").as_deref(),
        Some("refs/heads/main")
    );

    let head = sandbox.git(
        &bare_of(&root),
        &["symbolic-ref", "refs/remotes/origin/HEAD"],
    );
    assert_eq!(
        String::from_utf8(head.stdout).unwrap().trim_end(),
        "refs/remotes/origin/main"
    );
    assert!(probe_refs(&sandbox, &root).is_empty());

    // Publication must leave a grove `sync` can drive.
    sandbox
        .grove_in(&root, &["list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("origin/main"));
    sandbox.grove_in(&root, &["sync"]).assert().success();
}

// ---- Step 2: a non-empty, strictly-behind remote ----------------------

#[test]
fn publishes_onto_a_remote_that_is_strictly_behind() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("origin");
    let root = sandbox.root().join("g");
    // Build a grove whose history extends the origin's.
    sandbox
        .grove(&["clone", origin.to_str().unwrap(), "seeded"])
        .assert()
        .success();
    let seeded = sandbox.root().join("seeded");
    let ahead = advance(&sandbox, &seeded.join("main"), "ahead\n");

    // Re-home that history in a fresh, unpublished grove.
    sandbox
        .grove(&["init", "g", "--branch", "main"])
        .assert()
        .success();
    sandbox.git(
        &root.join("main"),
        &[
            "fetch",
            "--quiet",
            seeded.join("main").to_str().unwrap(),
            "main",
        ],
    );
    sandbox.git(
        &root.join("main"),
        &["reset", "--quiet", "--hard", "FETCH_HEAD"],
    );

    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .success();

    assert!(sandbox
        .remote_refs(&origin)
        .contains(&("refs/heads/main".to_string(), ahead)));
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("published")
    );
    assert!(probe_refs(&sandbox, &root).is_empty());
}

// ---- Step 3: refusals, each leaving the remote untouched --------------

#[test]
fn refuses_an_unborn_grove_without_touching_the_remote() {
    let sandbox = Sandbox::new();
    let origin = sandbox.empty_origin("origin");
    sandbox
        .grove(&["init", "g", "--branch", "main"])
        .assert()
        .success();
    let root = sandbox.root().join("g");

    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("no commit to publish"));

    assert!(sandbox.remote_refs(&origin).is_empty());
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("unpublished")
    );
}

#[test]
fn refuses_a_remote_whose_head_names_a_different_branch() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("origin");
    sandbox.git(&origin, &["branch", "-m", "main", "trunk"]);
    sandbox.git(&origin, &["symbolic-ref", "HEAD", "refs/heads/trunk"]);
    let root = grove_with_a_commit(&sandbox, "g", "main");
    let before = sandbox.remote_refs(&origin);

    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("refs/heads/trunk"));

    assert_eq!(sandbox.remote_refs(&origin), before);
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("unpublished")
    );
}

#[test]
fn refuses_a_diverged_remote_and_never_forces() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("origin");
    let peer = sandbox.peer_clone(&origin, "peer");
    sandbox.commit_and_push(&peer, "peer.txt", b"peer\n", "origin", "main");
    let before = sandbox.remote_refs(&origin);

    // A grove whose `main` shares the origin's root commit but has diverged.
    sandbox
        .grove(&["clone", origin.to_str().unwrap(), "g"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    sandbox.git(
        &root.join("main"),
        &["reset", "--quiet", "--hard", "HEAD~1"],
    );
    advance(&sandbox, &root.join("main"), "diverged\n");
    // Start from an unpublished grove that already holds the diverged history.
    let fresh = sandbox.root().join("fresh");
    sandbox
        .grove(&["init", "fresh", "--branch", "main"])
        .assert()
        .success();
    sandbox.git(
        &fresh.join("main"),
        &[
            "fetch",
            "--quiet",
            root.join("main").to_str().unwrap(),
            "main",
        ],
    );
    sandbox.git(
        &fresh.join("main"),
        &["reset", "--quiet", "--hard", "FETCH_HEAD"],
    );

    let assertion = sandbox
        .grove_in(&fresh, &["publish", origin.to_str().unwrap()])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("diverged"));
    assert!(!String::from_utf8_lossy(&assertion.get_output().stderr).contains("--force"));

    assert_eq!(sandbox.remote_refs(&origin), before);
    assert_eq!(
        config_of(&sandbox, &fresh, "grove.publishState").as_deref(),
        Some("unpublished")
    );
    assert!(probe_refs(&sandbox, &fresh).is_empty());
}

/// Measured M6: unrelated histories are exit 1 from `merge-base
/// --is-ancestor`, i.e. `NotAncestor` — not an error.
#[test]
fn refuses_a_remote_with_unrelated_history() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("origin");
    let before = sandbox.remote_refs(&origin);
    let root = grove_with_a_commit(&sandbox, "g", "main");

    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("diverged"));

    assert_eq!(sandbox.remote_refs(&origin), before);
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("unpublished")
    );
}

#[test]
fn a_nonexistent_url_is_a_failure_carrying_escaped_git_stderr() {
    let sandbox = Sandbox::new();
    let root = grove_with_a_commit(&sandbox, "g", "main");
    let missing = sandbox.root().join("absent.git");

    sandbox
        .grove_in(&root, &["publish", missing.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicates::str::contains(
            r"does\x20not\x20appear\x20to\x20be\x20a\x20git\x20repository",
        ));

    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("unpublished")
    );
}

#[test]
fn refuses_a_remote_name_already_configured_without_writing_a_receipt() {
    let sandbox = Sandbox::new();
    let origin = sandbox.empty_origin("origin");
    let other = sandbox.empty_origin("other");
    let root = grove_with_a_commit(&sandbox, "g", "main");
    sandbox.git(
        &bare_of(&root),
        &["remote", "add", "origin", other.to_str().unwrap()],
    );

    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("already exists"));

    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("unpublished")
    );
    assert_eq!(config_of(&sandbox, &root, "grove.publishUrl"), None);
    assert!(sandbox.remote_refs(&origin).is_empty());
}

// ---- Step 4: a remote that does not advertise atomic push -------------

/// Measured M8: `receive.advertiseAtomic=false` makes git refuse pre-flight
/// with exit 128, leaving the remote with zero refs.
#[test]
fn refuses_a_remote_that_does_not_advertise_atomic_push() {
    let sandbox = Sandbox::new();
    let origin = sandbox.empty_origin("origin");
    sandbox.set_repo_config(&origin, "receive.advertiseAtomic", "false");
    let root = grove_with_a_commit(&sandbox, "g", "main");
    sandbox.git(&root.join("main"), &["branch", "topic"]);

    sandbox
        .grove_in(
            &root,
            &["publish", "--all-branches", origin.to_str().unwrap()],
        )
        .assert()
        .code(2)
        .stderr(predicates::str::contains("atomic push"))
        .stderr(predicates::str::contains("nothing was published"));

    assert!(sandbox.remote_refs(&origin).is_empty());
}

/// The regression test for the locale pin. It asserts the grove's own
/// diagnostic rather than git's raw stderr, so it passes whether or not a
/// German message catalog is installed on the machine.
#[test]
fn classifies_a_non_atomic_remote_identically_under_a_translated_locale() {
    let sandbox = Sandbox::new();
    let origin = sandbox.empty_origin("origin");
    sandbox.set_repo_config(&origin, "receive.advertiseAtomic", "false");
    let root = grove_with_a_commit(&sandbox, "g", "main");

    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .env("LANGUAGE", "de")
        .assert()
        .code(2)
        .stderr(predicates::str::contains("atomic push"))
        .stderr(predicates::str::contains("nothing was published"));

    assert!(sandbox.remote_refs(&origin).is_empty());
}

// ---- Step 5: `--all-branches` atomicity -------------------------------

/// Measured M9: one non-fast-forward ref in an `--atomic` push rejects the
/// whole push, and the new refs are not created. The identical push *without*
/// `--atomic` would have published the other two — the partial-publication
/// hazard `--atomic` exists to prevent.
#[test]
fn all_branches_publishes_nothing_when_one_branch_is_not_fast_forward() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("origin");
    let peer = sandbox.peer_clone(&origin, "peer");
    sandbox.commit_and_push(&peer, "peer.txt", b"peer\n", "origin", "main");
    let before = sandbox.remote_refs(&origin);

    let root = grove_with_a_commit(&sandbox, "g", "main");
    sandbox.git(&root.join("main"), &["branch", "topic"]);
    sandbox.git(&root.join("main"), &["branch", "other"]);
    // `main` here has unrelated history, so it cannot fast-forward the origin.
    sandbox.git(
        &bare_of(&root),
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    sandbox.set_repo_config(&bare_of(&root), "grove.publishState", "publishing");
    sandbox.set_repo_config(&bare_of(&root), "grove.publishRemote", "origin");
    sandbox.set_repo_config(
        &bare_of(&root),
        "grove.publishUrl",
        origin.to_str().unwrap(),
    );

    sandbox
        .grove_in(
            &root,
            &["publish", "--all-branches", origin.to_str().unwrap()],
        )
        .assert()
        .failure();

    assert_eq!(
        sandbox.remote_refs(&origin),
        before,
        "no ref was created or moved"
    );
}

// ---- Step 6: a remote rejection hook ----------------------------------

#[test]
fn a_pre_receive_hook_rejection_is_a_failure_that_leaves_the_state_publishing() {
    let sandbox = Sandbox::new();
    let origin = sandbox.empty_origin("origin");
    let hook = origin.join("hooks/pre-receive");
    std::fs::create_dir_all(origin.join("hooks")).unwrap();
    std::fs::write(&hook, "#!/bin/sh\necho grove-hook-refused >&2\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let root = grove_with_a_commit(&sandbox, "g", "main");

    // Hook text is hook output rather than gettext, so the locale pin neither
    // helps nor harms here: the message survives verbatim either way.
    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("grove-hook-refused"));

    assert!(sandbox.remote_refs(&origin).is_empty());
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("publishing"),
        "the receipt stays publishing so a rerun reconciles"
    );
}

// ---- Step 7: server-side HEAD verification ----------------------------

/// Measured M12: pushing into a target whose unborn `HEAD` names `master`
/// succeeds, leaves `HEAD` pointing at `refs/heads/master` and dangling, and
/// makes `ls-remote --symref <url> HEAD` print nothing.
#[test]
fn keeps_publishing_when_the_hosting_side_default_branch_is_not_confirmed() {
    let sandbox = Sandbox::new();
    let origin = sandbox.empty_origin_with_head("origin", "master");
    let root = grove_with_a_commit(&sandbox, "g", "main");
    let local = sandbox.oid(&root.join("main"), "HEAD");

    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .code(2)
        .stdout(predicates::str::contains("published main to origin"))
        .stderr(predicates::str::contains("set it by hand"));

    assert!(sandbox
        .remote_refs(&origin)
        .contains(&("refs/heads/main".to_string(), local)));
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("publishing"),
        "not published, and not rolled back"
    );
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishUrl").as_deref(),
        Some(origin.to_str().unwrap())
    );

    // Fix the hosting side by hand, then rerun: the receipt makes it resumable.
    sandbox.git(&origin, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .success();
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("published")
    );
}

// ---- Step 8: interruption before and after the local configuration ----

#[test]
fn resumes_from_a_receipt_written_before_the_remote_was_configured() {
    let sandbox = Sandbox::new();
    let origin = sandbox.empty_origin("origin");
    let root = grove_with_a_commit(&sandbox, "g", "main");
    sandbox.set_repo_config(&bare_of(&root), "grove.publishState", "publishing");
    sandbox.set_repo_config(&bare_of(&root), "grove.publishRemote", "origin");
    sandbox.set_repo_config(
        &bare_of(&root),
        "grove.publishUrl",
        origin.to_str().unwrap(),
    );

    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .success();

    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("published")
    );
}

/// Measured M13: a second `remote add` on an existing name is exit 3, so the
/// resumed run must read and compare rather than re-add.
#[test]
fn resumes_from_a_receipt_and_a_configured_remote_without_adding_it_again() {
    let sandbox = Sandbox::new();
    let origin = sandbox.empty_origin("origin");
    let root = grove_with_a_commit(&sandbox, "g", "main");
    sandbox.set_repo_config(&bare_of(&root), "grove.publishState", "publishing");
    sandbox.set_repo_config(&bare_of(&root), "grove.publishRemote", "origin");
    sandbox.set_repo_config(
        &bare_of(&root),
        "grove.publishUrl",
        origin.to_str().unwrap(),
    );
    sandbox.git(
        &bare_of(&root),
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );

    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .success();

    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("published")
    );
    assert_eq!(
        config_of(&sandbox, &root, "remote.origin.url").as_deref(),
        Some(origin.to_str().unwrap())
    );
}

#[test]
fn refuses_a_request_that_differs_from_the_recorded_receipt() {
    let sandbox = Sandbox::new();
    let recorded = sandbox.empty_origin("recorded");
    let requested = sandbox.empty_origin("requested");
    let root = grove_with_a_commit(&sandbox, "g", "main");
    sandbox.set_repo_config(&bare_of(&root), "grove.publishState", "publishing");
    sandbox.set_repo_config(&bare_of(&root), "grove.publishRemote", "origin");
    sandbox.set_repo_config(
        &bare_of(&root),
        "grove.publishUrl",
        recorded.to_str().unwrap(),
    );

    sandbox
        .grove_in(&root, &["publish", requested.to_str().unwrap()])
        .assert()
        .code(2)
        .stderr(predicates::str::contains(recorded.to_str().unwrap()))
        .stderr(predicates::str::contains(requested.to_str().unwrap()));

    assert_eq!(config_of(&sandbox, &root, "remote.origin.url"), None);
    assert!(sandbox.remote_refs(&recorded).is_empty());
    assert!(sandbox.remote_refs(&requested).is_empty());
}

// ---- Step 9: the published rerun, against real git --------------------

/// A grove published successfully by the Step 1 path.
fn published_grove(sandbox: &Sandbox) -> (PathBuf, PathBuf) {
    let origin = sandbox.empty_origin("origin");
    let root = grove_with_a_commit(sandbox, "g", "main");
    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .success();
    (root, origin)
}

#[test]
fn a_published_rerun_repairs_each_damaged_key_and_pushes_nothing() {
    for damaged in [
        "branch.main.merge",
        "branch.main.remote",
        "remote.origin.fetch",
        "worktree.guessRemote",
        "grove.remote",
    ] {
        let sandbox = Sandbox::new();
        let (root, origin) = published_grove(&sandbox);
        let before = sandbox.remote_refs(&origin);
        let intended = config_of(&sandbox, &root, damaged).expect("key must be set");
        sandbox.unset_repo_config(&bare_of(&root), damaged);

        sandbox
            .grove_in(&root, &["publish", origin.to_str().unwrap()])
            .assert()
            .success();

        assert_eq!(
            config_of(&sandbox, &root, damaged).as_deref(),
            Some(intended.as_str()),
            "{damaged} must be restored to exactly its intended value"
        );
        assert_eq!(
            config_of(&sandbox, &root, "grove.publishState").as_deref(),
            Some("published")
        );
        assert_eq!(
            config_of(&sandbox, &root, "grove.publishUrl").as_deref(),
            Some(origin.to_str().unwrap())
        );
        assert_eq!(sandbox.remote_refs(&origin), before, "nothing to push");
        assert!(probe_refs(&sandbox, &root).is_empty());
    }
}

#[test]
fn a_published_rerun_widens_a_narrowed_fetch_refspec() {
    let sandbox = Sandbox::new();
    let (root, origin) = published_grove(&sandbox);
    sandbox.set_repo_config(
        &bare_of(&root),
        "remote.origin.fetch",
        "+refs/heads/main:refs/remotes/origin/main",
    );

    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .success();

    assert_eq!(
        config_of(&sandbox, &root, "remote.origin.fetch").as_deref(),
        Some("+refs/heads/*:refs/remotes/origin/*")
    );
}

#[test]
fn a_published_rerun_restores_a_deleted_remote_head() {
    let sandbox = Sandbox::new();
    let (root, origin) = published_grove(&sandbox);
    sandbox.git(
        &bare_of(&root),
        &["update-ref", "-d", "refs/remotes/origin/HEAD"],
    );

    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .success();

    let head = sandbox.git(
        &bare_of(&root),
        &["symbolic-ref", "refs/remotes/origin/HEAD"],
    );
    assert_eq!(
        String::from_utf8(head.stdout).unwrap().trim_end(),
        "refs/remotes/origin/main"
    );
}

#[test]
fn a_published_rerun_fast_forwards_a_remote_this_grove_has_advanced_past() {
    let sandbox = Sandbox::new();
    let (root, origin) = published_grove(&sandbox);
    let advanced = advance(&sandbox, &root.join("main"), "advanced\n");

    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicates::str::contains("advanced main"));

    assert!(sandbox
        .remote_refs(&origin)
        .contains(&("refs/heads/main".to_string(), advanced)));
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("published")
    );
}

#[test]
fn a_published_rerun_refuses_a_remote_a_peer_advanced_past_this_grove() {
    let sandbox = Sandbox::new();
    let (root, origin) = published_grove(&sandbox);
    let peer = sandbox.peer_clone(&origin, "peer");
    sandbox.commit_and_push(&peer, "peer.txt", b"peer\n", "origin", "main");
    let before = sandbox.remote_refs(&origin);

    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("nothing was pushed"));

    assert_eq!(sandbox.remote_refs(&origin), before);
}

// ---- Step 10: a probe-stage refusal leaves the grove recoverable ------

/// The case that makes the declared receipt ordering worth taking. Under the
/// rejected alternative — writing the receipt before the read-only inspection —
/// the second half of this test would be exit 2 forever.
#[test]
fn a_probe_stage_refusal_leaves_the_grove_untouched_and_republishable() {
    for log_all_ref_updates in ["true", "always"] {
        let sandbox = Sandbox::new();
        let root = grove_with_a_commit(&sandbox, "g", "main");
        sandbox.set_repo_config(
            &bare_of(&root),
            "core.logAllRefUpdates",
            log_all_ref_updates,
        );

        // A nonexistent URL, and a target whose HEAD names another branch.
        let missing = sandbox.root().join("absent.git");
        sandbox
            .grove_in(&root, &["publish", missing.to_str().unwrap()])
            .assert()
            .code(1);

        let wrong_head = sandbox.bare_origin("wrong");
        sandbox.git(&wrong_head, &["branch", "-m", "main", "trunk"]);
        sandbox.git(&wrong_head, &["symbolic-ref", "HEAD", "refs/heads/trunk"]);
        sandbox
            .grove_in(&root, &["publish", wrong_head.to_str().unwrap()])
            .assert()
            .code(2);

        // A target with unrelated history is the divergence refusal.
        let unrelated = sandbox.bare_origin("unrelated");
        sandbox
            .grove_in(&root, &["publish", unrelated.to_str().unwrap()])
            .assert()
            .code(2);

        assert_eq!(
            config_of(&sandbox, &root, "grove.publishState").as_deref(),
            Some("unpublished")
        );
        assert_eq!(config_of(&sandbox, &root, "grove.publishRemote"), None);
        assert_eq!(config_of(&sandbox, &root, "grove.publishUrl"), None);
        assert_eq!(config_of(&sandbox, &root, "remote.origin.url"), None);
        assert_eq!(config_of(&sandbox, &root, "grove.remote"), None);
        assert!(probe_refs(&sandbox, &root).is_empty());
        // Measured M17: `update-ref -d` removes the ref and its reflog file
        // together; only the now-empty directory is left, which is not a gc
        // root. Assert on files, not on the directory.
        assert!(
            reflog_files(&bare_of(&root).join("logs/refs/grove")).is_empty(),
            "no probe reflog survives under core.logAllRefUpdates={log_all_ref_updates}"
        );
        assert!(
            !bare_of(&root).join("FETCH_HEAD").exists(),
            "the probe fetch writes no FETCH_HEAD"
        );

        // The same grove publishes cleanly to a corrected URL.
        let good = sandbox.empty_origin("good");
        sandbox
            .grove_in(&root, &["publish", good.to_str().unwrap()])
            .assert()
            .success();
        assert_eq!(
            config_of(&sandbox, &root, "grove.publishState").as_deref(),
            Some("published")
        );
    }
}

// ---- Step 11: probe-ref debris ---------------------------------------

#[test]
fn purges_stale_probe_refs_reports_them_and_still_succeeds() {
    let sandbox = Sandbox::new();
    let origin = sandbox.empty_origin("origin");
    let root = grove_with_a_commit(&sandbox, "g", "main");
    let tip = sandbox.oid(&root.join("main"), "HEAD");
    sandbox.git(
        &bare_of(&root),
        &["update-ref", "refs/grove/publish-probe/stale", &tip],
    );

    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .success();

    assert!(probe_refs(&sandbox, &root).is_empty());
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("published")
    );
}

// ---- Step 12: byte safety --------------------------------------------

#[test]
fn publishes_a_default_branch_whose_name_contains_a_slash() {
    let sandbox = Sandbox::new();
    let origin = sandbox.empty_origin_with_head("origin", "release/1.0");
    let root = grove_with_a_commit(&sandbox, "g", "release/1.0");
    let local = sandbox.oid(&root.join("release/1.0"), "HEAD");

    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .success();

    assert_eq!(
        sandbox.remote_refs(&origin),
        vec![("refs/heads/release/1.0".to_string(), local)]
    );
    assert_eq!(
        config_of(&sandbox, &root, "branch.release/1.0.merge").as_deref(),
        Some("refs/heads/release/1.0")
    );
    assert_eq!(
        config_of(&sandbox, &root, "remote.origin.fetch").as_deref(),
        Some("+refs/heads/*:refs/remotes/origin/*")
    );
}

#[cfg(unix)]
#[test]
fn publishes_a_default_branch_whose_name_is_not_utf8() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let sandbox = Sandbox::new();
    let branch = OsString::from_vec(b"topic-\xff".to_vec());
    let origin = sandbox.empty_origin("origin");
    // `--initial-branch` cannot carry these bytes through `&str`, so point the
    // unborn HEAD at the branch afterwards, with raw bytes.
    let mut head_target = OsString::from("refs/heads/");
    head_target.push(&branch);
    sandbox.git_os(
        &origin,
        &[
            OsString::from("symbolic-ref"),
            OsString::from("HEAD"),
            head_target,
        ],
    );
    sandbox
        .grove(&["init", "g", "--branch"])
        .arg(&branch)
        .assert()
        .success();
    let root = sandbox.root().join("g");
    let worktree = root.join(&branch);
    std::fs::write(worktree.join("GROVE.md"), "grove\n").unwrap();
    sandbox.git(&worktree, &["add", "GROVE.md"]);
    sandbox.git(&worktree, &["commit", "--quiet", "-m", "grove seed"]);
    let local = sandbox.oid(&worktree, "HEAD");

    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .success()
        // The report escapes the byte reversibly rather than losing it.
        .stdout(predicates::str::contains(r"topic-\xFF"));

    // `remote_refs` decodes as UTF-8, so compare raw bytes here instead.
    let refs = sandbox.git(
        &origin,
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    );
    let mut expected = b"refs/heads/topic-\xff ".to_vec();
    expected.extend_from_slice(local.as_bytes());
    expected.push(b'\n');
    assert_eq!(refs.stdout, expected);

    let head = sandbox.git(&origin, &["symbolic-ref", "HEAD"]);
    assert_eq!(head.stdout, b"refs/heads/topic-\xff\n");

    let merge = sandbox.git(
        &bare_of(&root),
        &["config", "--get-regexp", "^branch\\..*\\.merge$"],
    );
    assert_eq!(
        merge.stdout,
        b"branch.topic-\xff.merge refs/heads/topic-\xff\n"
    );

    let fetch = sandbox.git(&bare_of(&root), &["config", "--get", "remote.origin.fetch"]);
    assert_eq!(fetch.stdout, b"+refs/heads/*:refs/remotes/origin/*\n");
}

/// Per the specification's `## Testing` note, use a *path* with a space rather
/// than a branch with a space: `git check-ref-format` rejects the latter.
#[test]
fn publishes_through_a_path_containing_a_space() {
    let sandbox = Sandbox::new();
    let origin = sandbox.empty_origin("an origin");
    let root = grove_with_a_commit(&sandbox, "g", "main");

    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .success();

    assert_eq!(
        config_of(&sandbox, &root, "grove.publishUrl").as_deref(),
        Some(origin.to_str().unwrap())
    );
    assert_eq!(
        sandbox.remote_head_symref(&origin).as_deref(),
        Some("refs/heads/main")
    );
}

#[test]
fn escapes_a_non_utf8_url_reversibly_in_its_diagnostics() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let sandbox = Sandbox::new();
    let root = grove_with_a_commit(&sandbox, "g", "main");
    let mut raw = sandbox.root().join("absent-").into_os_string().into_vec();
    raw.push(0xff);
    let url = OsString::from_vec(raw);

    sandbox
        .grove_os(&[
            OsString::from("-C"),
            root.clone().into_os_string(),
            OsString::from("publish"),
            url,
        ])
        .assert()
        .code(64);
}

/// A publication whose remote name is not the default still records that name
/// everywhere the receipt and the configuration reach.
#[test]
fn publishes_under_a_two_level_remote_name() {
    let sandbox = Sandbox::new();
    let origin = sandbox.empty_origin("origin");
    let root = grove_with_a_commit(&sandbox, "g", "main");

    sandbox
        .grove_in(
            &root,
            &["publish", "--remote", "hosts/one", origin.to_str().unwrap()],
        )
        .assert()
        .success();

    assert_eq!(
        config_of(&sandbox, &root, "grove.publishRemote").as_deref(),
        Some("hosts/one")
    );
    assert_eq!(
        config_of(&sandbox, &root, "remote.hosts/one.fetch").as_deref(),
        Some("+refs/heads/*:refs/remotes/hosts/one/*")
    );
    assert_eq!(
        config_of(&sandbox, &root, "grove.remote").as_deref(),
        Some("hosts/one")
    );
}

/// The guard every `--` in this command exists for, exercised end to end.
///
/// Measured M14: `git remote add -x <url>` is exit 129 while
/// `git remote add -- -x <url>` succeeds, and `a/b` is a valid remote name — so
/// a dash-leading name is legal and must be passed after `--`. The same applies
/// to `remote set-head --auto -- <remote>`, where the mode must come *before*
/// the separator: with `--` first git reads `--auto` as the positional
/// `<branch>` and fails with `Not a valid ref: refs/remotes/<remote>/--auto`.
///
/// Every other test here uses `origin`, for which the separators are inert.
/// This one fails if any of them is dropped or misplaced.
#[test]
fn publishes_under_a_remote_name_beginning_with_a_dash() {
    let sandbox = Sandbox::new();
    let origin = sandbox.empty_origin("origin");
    let root = grove_with_a_commit(&sandbox, "g", "main");
    let local = sandbox.oid(&root.join("main"), "HEAD");

    sandbox
        .grove_in(&root, &["publish", "--remote=-x", origin.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicates::str::contains("published main to -x"));

    assert_eq!(
        sandbox.remote_refs(&origin),
        vec![("refs/heads/main".to_string(), local)]
    );
    assert_eq!(
        sandbox.remote_head_symref(&origin).as_deref(),
        Some("refs/heads/main")
    );
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishRemote").as_deref(),
        Some("-x")
    );
    assert_eq!(
        config_of(&sandbox, &root, "remote.-x.fetch").as_deref(),
        Some("+refs/heads/*:refs/remotes/-x/*")
    );
    assert_eq!(
        config_of(&sandbox, &root, "branch.main.remote").as_deref(),
        Some("-x")
    );
    // `remote set-head --auto -- -x` wrote this; the plan's literal argv would
    // instead have tried to point HEAD at a branch called `--auto`.
    let head = sandbox.git(&bare_of(&root), &["symbolic-ref", "refs/remotes/-x/HEAD"]);
    assert_eq!(
        String::from_utf8(head.stdout).unwrap().trim_end(),
        "refs/remotes/-x/main"
    );
}

#[test]
fn all_branches_publishes_every_local_branch_to_an_empty_remote() {
    let sandbox = Sandbox::new();
    let origin = sandbox.empty_origin("origin");
    let root = grove_with_a_commit(&sandbox, "g", "main");
    sandbox.git(&root.join("main"), &["branch", "topic/x"]);
    sandbox.git(&root.join("main"), &["branch", "zeta"]);
    let tip = sandbox.oid(&root.join("main"), "HEAD");

    sandbox
        .grove_in(
            &root,
            &["publish", "--all-branches", origin.to_str().unwrap()],
        )
        .assert()
        .success()
        .stdout(predicates::str::contains("3 branches in one atomic push"));

    let mut names: Vec<String> = sandbox
        .remote_refs(&origin)
        .into_iter()
        .map(|(name, oid)| {
            assert_eq!(oid, tip);
            name
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "refs/heads/main".to_string(),
            "refs/heads/topic/x".to_string(),
            "refs/heads/zeta".to_string(),
        ]
    );
}

/// `publish` never leaves a probe ref behind, on any exit path this file
/// exercises.
#[test]
fn never_leaves_a_probe_ref_behind_on_any_exit_path() {
    let sandbox = Sandbox::new();
    let root = grove_with_a_commit(&sandbox, "g", "main");
    let unrelated = sandbox.bare_origin("unrelated");
    let empty = sandbox.empty_origin("empty");

    for (url, code) in [
        (sandbox.root().join("absent.git"), 1),
        (unrelated, 2),
        (empty, 0),
    ] {
        sandbox
            .grove_in(&root, &["publish", url.to_str().unwrap()])
            .assert()
            .code(code);
        assert!(
            probe_refs(&sandbox, &root).is_empty(),
            "a probe ref survived a run that exited {code}"
        );
    }
}

#[test]
fn publish_never_rewrites_the_generated_guide_but_reports_it_as_stale() {
    let sandbox = Sandbox::new();
    let origin = sandbox.empty_origin("origin");
    let root = grove_with_a_commit(&sandbox, "g", "main");
    let guide = root.join("AGENTS.md");
    let before = std::fs::read(&guide).unwrap();

    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicates::str::contains("not published"))
        .stdout(predicates::str::contains("does not rewrite it"));

    assert_eq!(std::fs::read(&guide).unwrap(), before);
}

#[test]
fn a_cloned_grove_is_refused_with_a_decision_rather_than_a_torn_receipt() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("origin");
    sandbox
        .grove(&["clone", origin.to_str().unwrap(), "g"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("published"),
        "clone records published with no receipt; that shape is not torn"
    );

    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("already published"))
        .stderr(predicates::str::contains("receipt").not());
}
