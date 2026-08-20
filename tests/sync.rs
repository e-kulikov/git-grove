mod harness;

use harness::Sandbox;

#[test]
fn sync_fast_forwards_a_clean_behind_worktree_to_the_upstream_tip() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("origin");
    sandbox
        .grove(&["clone", origin.to_str().unwrap(), "g"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    let worktree = root.join("main");

    let peer = sandbox.root().join("peer");
    sandbox.git(
        sandbox.root(),
        &[
            "clone",
            "--quiet",
            origin.to_str().unwrap(),
            peer.to_str().unwrap(),
        ],
    );
    std::fs::write(peer.join("advance.txt"), b"advance\n").unwrap();
    sandbox.git(&peer, &["add", "advance.txt"]);
    sandbox.git(&peer, &["commit", "--quiet", "-m", "advance"]);
    sandbox.git(&peer, &["push", "--quiet", "origin", "main"]);

    sandbox
        .grove_in(&root, &["sync"])
        .assert()
        .success()
        .stdout(predicates::str::contains("UP-TO-DATE"));

    let worktree_head = sandbox.git(&worktree, &["rev-parse", "HEAD"]);
    let origin_main = sandbox.git(&worktree, &["rev-parse", "refs/remotes/origin/main"]);
    assert_eq!(worktree_head.stdout, origin_main.stdout);
    assert_eq!(
        std::fs::read(worktree.join("advance.txt")).unwrap(),
        b"advance\n"
    );
}

// --- Fetch-boundary matrix ---

#[test]
fn sync_with_no_remote_performs_no_fetch_and_succeeds() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["init", "g", "--branch", "main"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    let worktree = root.join("main");
    std::fs::write(worktree.join("tracked"), b"one\n").unwrap();
    sandbox.git(&worktree, &["add", "tracked"]);
    sandbox.git(&worktree, &["commit", "--quiet", "-m", "one"]);
    let before = sandbox.oid(&worktree, "HEAD");

    sandbox
        .grove_in(&root, &["sync"])
        .assert()
        .success()
        .stdout(predicates::str::contains("LOCAL"));

    assert_eq!(sandbox.oid(&worktree, "HEAD"), before);
}

#[test]
fn sync_still_explicitly_fetches_origin_when_skip_fetch_all_is_set() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("origin");
    sandbox
        .grove(&["clone", origin.to_str().unwrap(), "g"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    let worktree = root.join("main");
    sandbox.git(&worktree, &["config", "remote.origin.skipFetchAll", "true"]);

    let peer = sandbox.peer_clone(&origin, "peer");
    sandbox.commit_and_push(&peer, "advance.txt", b"advance\n", "origin", "main");
    let origin_main = sandbox.oid(&peer, "HEAD");

    sandbox
        .grove_in(&root, &["sync"])
        .assert()
        .success()
        .stdout(predicates::str::contains("UP-TO-DATE"));

    assert_eq!(sandbox.oid(&worktree, "HEAD"), origin_main);
}

#[test]
fn sync_fetches_and_fast_forwards_two_worktrees_tracking_two_remotes() {
    let sandbox = Sandbox::new();
    let origin_a = sandbox.bare_origin("origin-a");
    let origin_b = sandbox.bare_origin("origin-b");
    sandbox
        .grove(&["clone", origin_a.to_str().unwrap(), "g"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    let bare = root.join(".bare");

    sandbox.git(
        &root,
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "remote",
            "add",
            "origin-b",
            origin_b.to_str().unwrap(),
        ],
    );
    sandbox.git(
        &root,
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "fetch",
            "--quiet",
            "origin-b",
        ],
    );
    let topic = root.join("topic");
    sandbox.git(
        &root,
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "worktree",
            "add",
            "--quiet",
            "-b",
            "topic",
            "--track",
            topic.to_str().unwrap(),
            "origin-b/main",
        ],
    );

    let peer_a = sandbox.peer_clone(&origin_a, "peer-a");
    sandbox.commit_and_push(&peer_a, "a.txt", b"a\n", "origin", "main");
    let peer_b = sandbox.peer_clone(&origin_b, "peer-b");
    sandbox.commit_and_push(&peer_b, "b.txt", b"b\n", "origin", "main");

    sandbox.grove_in(&root, &["sync"]).assert().success();

    assert_eq!(
        sandbox.oid(&root.join("main"), "HEAD"),
        sandbox.oid(&peer_a, "HEAD")
    );
    assert_eq!(sandbox.oid(&topic, "HEAD"), sandbox.oid(&peer_b, "HEAD"));
    assert!(root.join("main/a.txt").exists());
    assert!(topic.join("b.txt").exists());
}

#[test]
fn a_later_fetch_failure_exits_one_and_changes_no_worktree_head() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("origin");
    sandbox
        .grove(&["clone", origin.to_str().unwrap(), "g"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    let worktree = root.join("main");
    let bare = root.join(".bare");
    let before = sandbox.oid(&worktree, "HEAD");

    // "zzz-broken" sorts after "origin" in raw-byte remote order, so the
    // first (origin) fetch may already succeed and advance tracking refs
    // before the second fetch fails and the whole sync aborts.
    let broken = sandbox.root().join("does-not-exist.git");
    sandbox.git(
        &root,
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "remote",
            "add",
            "zzz-broken",
            broken.to_str().unwrap(),
        ],
    );
    let second_worktree = root.join("second");
    sandbox.git(
        &root,
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "branch",
            "second",
            "main",
        ],
    );
    sandbox.git(
        &root,
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "config",
            "branch.second.remote",
            "zzz-broken",
        ],
    );
    sandbox.git(
        &root,
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "config",
            "branch.second.merge",
            "refs/heads/main",
        ],
    );
    sandbox.git(
        &root,
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "worktree",
            "add",
            "--quiet",
            second_worktree.to_str().unwrap(),
            "second",
        ],
    );

    let peer = sandbox.peer_clone(&origin, "peer");
    sandbox.commit_and_push(&peer, "advance.txt", b"advance\n", "origin", "main");

    sandbox
        .grove_in(&root, &["sync"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("fetch"));

    assert_eq!(sandbox.oid(&worktree, "HEAD"), before);
    assert!(!worktree.join("advance.txt").exists());
}

#[test]
fn sync_never_contacts_a_remote_not_configured_as_any_branchs_upstream() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("origin");
    sandbox
        .grove(&["clone", origin.to_str().unwrap(), "g"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    let worktree = root.join("main");
    let bare = root.join(".bare");

    let unreachable = sandbox.root().join("unreachable.git");
    sandbox.git(
        &root,
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "remote",
            "add",
            "extra",
            unreachable.to_str().unwrap(),
        ],
    );

    let peer = sandbox.peer_clone(&origin, "peer");
    sandbox.commit_and_push(&peer, "advance.txt", b"advance\n", "origin", "main");

    sandbox
        .grove_in(&root, &["sync"])
        .assert()
        .success()
        .stdout(predicates::str::contains("UP-TO-DATE"));

    assert_eq!(sandbox.oid(&worktree, "HEAD"), sandbox.oid(&peer, "HEAD"));
}

// --- Final-state table: exit 0 ---

#[test]
fn sync_exits_zero_for_an_ahead_worktree_and_does_not_touch_history() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("origin");
    sandbox
        .grove(&["clone", origin.to_str().unwrap(), "g"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    let worktree = root.join("main");
    std::fs::write(worktree.join("local.txt"), b"local\n").unwrap();
    sandbox.git(&worktree, &["add", "local.txt"]);
    sandbox.git(&worktree, &["commit", "--quiet", "-m", "local ahead"]);
    let before = sandbox.oid(&worktree, "HEAD");

    sandbox
        .grove_in(&root, &["sync"])
        .assert()
        .success()
        .stdout(predicates::str::contains("AHEAD"));

    assert_eq!(sandbox.oid(&worktree, "HEAD"), before);
}

#[test]
fn sync_exits_zero_for_a_local_only_worktree() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["init", "g", "--branch", "main"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    let worktree = root.join("main");
    std::fs::write(worktree.join("tracked"), b"one\n").unwrap();
    sandbox.git(&worktree, &["add", "tracked"]);
    sandbox.git(&worktree, &["commit", "--quiet", "-m", "one"]);

    sandbox
        .grove_in(&root, &["sync"])
        .assert()
        .success()
        .stdout(predicates::str::contains("LOCAL"));
}

#[test]
fn sync_exits_zero_for_a_detached_worktree() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["init", "g", "--branch", "main"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    std::fs::write(root.join("main/tracked"), b"one\n").unwrap();
    sandbox.git(&root.join("main"), &["add", "tracked"]);
    sandbox.git(&root.join("main"), &["commit", "--quiet", "-m", "one"]);
    sandbox
        .grove_in(&root, &["add", "--detach", "HEAD", "review"])
        .assert()
        .success();

    sandbox
        .grove_in(&root, &["sync"])
        .assert()
        .success()
        .stdout(predicates::str::contains("DETACHED"));
}

#[test]
fn sync_exits_zero_for_a_fresh_unborn_grove() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["init", "g", "--branch", "main"])
        .assert()
        .success();
    let root = sandbox.root().join("g");

    sandbox
        .grove_in(&root, &["sync"])
        .assert()
        .success()
        .stdout(predicates::str::contains("UNBORN"));
}

// --- Final-state table: exit 2 ---

#[test]
fn sync_exits_two_for_an_invalid_worktree_and_continues_to_a_later_one() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["init", "g", "--branch", "main"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    std::fs::write(root.join("main/tracked"), b"one\n").unwrap();
    sandbox.git(&root.join("main"), &["add", "tracked"]);
    sandbox.git(&root.join("main"), &["commit", "--quiet", "-m", "one"]);
    std::fs::rename(root.join("main"), root.join("moved-by-user")).unwrap();
    std::os::unix::fs::symlink("moved-by-user", root.join("main")).unwrap();

    sandbox
        .grove_in(&root, &["sync"])
        .assert()
        .code(2)
        .stdout(predicates::str::contains("INVALID"));
}

#[test]
fn sync_exits_two_for_a_missing_worktree() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["init", "g", "--branch", "main"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    std::fs::rename(root.join("main"), root.join("moved-by-user")).unwrap();

    sandbox
        .grove_in(&root, &["sync"])
        .assert()
        .code(2)
        .stdout(predicates::str::contains("MISSING"));
}

#[test]
fn sync_exits_two_and_never_merges_a_locked_worktree_even_when_behind() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("origin");
    sandbox
        .grove(&["clone", origin.to_str().unwrap(), "g"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    let worktree = root.join("main");
    let bare = root.join(".bare");
    sandbox.git(
        &root,
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "worktree",
            "lock",
            "--reason",
            "human review",
            worktree.to_str().unwrap(),
        ],
    );
    let before = sandbox.oid(&worktree, "HEAD");

    let peer = sandbox.peer_clone(&origin, "peer");
    sandbox.commit_and_push(&peer, "advance.txt", b"advance\n", "origin", "main");

    sandbox
        .grove_in(&root, &["sync"])
        .assert()
        .code(2)
        .stdout(predicates::str::contains("LOCKED"));

    assert_eq!(sandbox.oid(&worktree, "HEAD"), before);
}

#[test]
fn sync_exits_two_and_never_merges_an_in_progress_worktree_even_when_behind() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("origin");
    sandbox
        .grove(&["clone", origin.to_str().unwrap(), "g"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    let worktree = root.join("main");
    let admin = sandbox.worktree_admin(&worktree);
    std::fs::write(admin.join("index.lock"), b"").unwrap();
    let before = sandbox.oid(&worktree, "HEAD");

    let peer = sandbox.peer_clone(&origin, "peer");
    sandbox.commit_and_push(&peer, "advance.txt", b"advance\n", "origin", "main");

    sandbox
        .grove_in(&root, &["sync"])
        .assert()
        .code(2)
        .stdout(predicates::str::contains("IN-PROGRESS"));

    std::fs::remove_file(admin.join("index.lock")).unwrap();
    assert_eq!(sandbox.oid(&worktree, "HEAD"), before);
}

#[test]
fn sync_exits_two_for_an_upstream_gone_worktree() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("origin");
    sandbox
        .grove(&["clone", origin.to_str().unwrap(), "g"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    let worktree = root.join("main");
    // A bogus upstream branch name that never existed on `origin`; unlike
    // deleting the tracking ref, a fetch during `sync` cannot resurrect it.
    sandbox.git(
        &worktree,
        &["config", "branch.main.merge", "refs/heads/never-existed"],
    );

    sandbox
        .grove_in(&root, &["sync"])
        .assert()
        .code(2)
        .stdout(predicates::str::contains("UPSTREAM-GONE"));
}

#[test]
fn sync_exits_two_and_never_merges_a_dirty_behind_worktree() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("origin");
    sandbox
        .grove(&["clone", origin.to_str().unwrap(), "g"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    let worktree = root.join("main");
    let before = sandbox.oid(&worktree, "HEAD");

    let peer = sandbox.peer_clone(&origin, "peer");
    sandbox.commit_and_push(&peer, "advance.txt", b"advance\n", "origin", "main");
    std::fs::write(worktree.join("dirty.txt"), b"dirty\n").unwrap();

    sandbox
        .grove_in(&root, &["sync"])
        .assert()
        .code(2)
        .stdout(predicates::str::contains("DIRTY-BEHIND"));

    assert_eq!(sandbox.oid(&worktree, "HEAD"), before);
    assert_eq!(
        std::fs::read(worktree.join("dirty.txt")).unwrap(),
        b"dirty\n"
    );
}

#[test]
fn sync_exits_two_and_never_merges_a_diverged_worktree() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("origin");
    sandbox
        .grove(&["clone", origin.to_str().unwrap(), "g"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    let worktree = root.join("main");

    let peer = sandbox.peer_clone(&origin, "peer");
    sandbox.commit_and_push(&peer, "advance.txt", b"advance\n", "origin", "main");
    std::fs::write(worktree.join("local.txt"), b"local\n").unwrap();
    sandbox.git(&worktree, &["add", "local.txt"]);
    sandbox.git(&worktree, &["commit", "--quiet", "-m", "local divergence"]);
    let before = sandbox.oid(&worktree, "HEAD");

    sandbox
        .grove_in(&root, &["sync"])
        .assert()
        .code(2)
        .stdout(predicates::str::contains("DIVERGED"));

    assert_eq!(sandbox.oid(&worktree, "HEAD"), before);
}

#[test]
fn sync_reports_blocked_and_exits_two_when_an_ignored_local_file_obstructs_the_merge() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("origin");
    // Commit a tracked .gitignore rule for "ignored.txt" to the shared
    // history before cloning, so it is present without making the cloned
    // worktree dirty.
    let seeder = sandbox.peer_clone(&origin, "ignore-seeder");
    sandbox.commit_and_push(&seeder, ".gitignore", b"ignored.txt\n", "origin", "main");

    sandbox
        .grove(&["clone", origin.to_str().unwrap(), "g"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    let worktree = root.join("main");
    let before = sandbox.oid(&worktree, "HEAD");

    // A local, untracked, git-ignored file already occupies the path the
    // remote is about to introduce as a tracked file.
    std::fs::write(worktree.join("ignored.txt"), b"local content\n").unwrap();

    let peer = sandbox.peer_clone(&origin, "peer");
    std::fs::write(peer.join("ignored.txt"), b"from remote\n").unwrap();
    sandbox.git(&peer, &["add", "--force", "ignored.txt"]);
    sandbox.git(&peer, &["commit", "--quiet", "-m", "add ignored.txt"]);
    sandbox.git(&peer, &["push", "--quiet", "origin", "main"]);

    sandbox
        .grove_in(&root, &["sync"])
        .env("GIT_TRACE", "1")
        .assert()
        .code(2)
        .stdout(predicates::str::contains("BLOCKED"))
        .stderr(predicates::str::contains("git\\x20merge"))
        .stderr(predicates::str::contains("--ff-only"))
        .stderr(predicates::str::contains("--no-autostash"))
        .stderr(predicates::str::contains("--no-overwrite-ignore"));

    assert_eq!(sandbox.oid(&worktree, "HEAD"), before);
    assert_eq!(
        std::fs::read(worktree.join("ignored.txt")).unwrap(),
        b"local content\n"
    );
}

#[test]
fn sync_never_rewrites_an_existing_generated_guide() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("origin");
    sandbox
        .grove(&["clone", origin.to_str().unwrap(), "g"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    let worktree = root.join("main");

    // An existing, 0.1-style AGENTS.md and its CLAUDE.md link, as if this
    // grove had been cloned by an older release.
    let old_style = "# Grove checkout layout\n\nA pre-0.2 guide.\n";
    std::fs::write(root.join("AGENTS.md"), old_style).unwrap();
    std::fs::remove_file(root.join("CLAUDE.md")).ok();
    std::os::unix::fs::symlink("AGENTS.md", root.join("CLAUDE.md")).unwrap();

    let peer = sandbox.peer_clone(&origin, "peer");
    sandbox.commit_and_push(&peer, "advance.txt", b"advance\n", "origin", "main");

    sandbox
        .grove_in(&root, &["sync"])
        .assert()
        .success()
        .stdout(predicates::str::contains("UP-TO-DATE"));

    assert_eq!(
        std::fs::read_to_string(root.join("AGENTS.md")).unwrap(),
        old_style
    );
    assert_eq!(
        std::fs::read_link(root.join("CLAUDE.md")).unwrap(),
        std::path::Path::new("AGENTS.md")
    );
    assert_eq!(sandbox.oid(&worktree, "HEAD"), sandbox.oid(&peer, "HEAD"));
}
