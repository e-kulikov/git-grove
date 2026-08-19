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
