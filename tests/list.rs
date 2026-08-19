mod harness;

use harness::Sandbox;
use predicates::prelude::PredicateBooleanExt;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

fn admin_dir(sandbox: &Sandbox, worktree: &std::path::Path) -> PathBuf {
    let output = sandbox.git(worktree, &["rev-parse", "--git-dir"]);
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim_end())
}

#[test]
fn lists_a_fresh_grove_explicitly_and_implicitly_without_the_bare_row() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["init", "g", "--branch", "main"])
        .assert()
        .success();
    let root = sandbox.root().join("g");

    sandbox
        .grove_in(&root, &["list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("UNBORN"))
        .stdout(predicates::str::contains("main"));
    sandbox
        .grove_in(&root, &[])
        .assert()
        .success()
        .stdout(predicates::str::contains("UNBORN"));

    let output = sandbox
        .grove_in(&root, &["list", "--porcelain"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.starts_with(b"git-grove-list-v1\0"));
    assert!(!output.stdout.windows(6).any(|bytes| bytes == b"/.bare"));
    assert!(output.stdout.ends_with(b"\0\0"));
}

#[test]
fn list_outside_a_grove_keeps_the_usage_path() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["list"])
        .assert()
        .code(64)
        .stderr(predicates::str::contains("not inside a grove"));
    sandbox
        .grove(&[])
        .assert()
        .code(64)
        .stderr(predicates::str::contains("not inside a grove"));
}

#[test]
fn classifies_dirty_local_detached_and_locked_worktrees_without_exit_two() {
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
        .grove_in(&root, &["add", "topic", "--start", "HEAD"])
        .assert()
        .success();
    sandbox
        .grove_in(&root, &["add", "--detach", "HEAD", "review"])
        .assert()
        .success();
    sandbox.git(
        &root,
        &[
            "--git-dir",
            root.join(".bare").to_str().unwrap(),
            "worktree",
            "lock",
            "--reason",
            "human review",
            root.join("topic").to_str().unwrap(),
        ],
    );
    std::fs::write(root.join("main/untracked"), b"dirty\n").unwrap();

    sandbox
        .grove_in(&root, &["list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("LOCAL"))
        .stdout(predicates::str::contains("DETACHED"))
        .stdout(predicates::str::contains("LOCKED"))
        .stdout(predicates::str::contains("human\\x20review"));
}

#[test]
fn classifies_tracking_graph_states_from_pinned_queries() {
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
    let origin = sandbox.root().join("origin.git");
    sandbox.git(
        sandbox.root(),
        &[
            "init",
            "--quiet",
            "--bare",
            "--initial-branch=main",
            origin.to_str().unwrap(),
        ],
    );
    sandbox.git(
        &worktree,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    sandbox.git(
        &worktree,
        &["push", "--quiet", "--set-upstream", "origin", "main"],
    );

    sandbox
        .grove_in(&root, &["list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("UP-TO-DATE"));

    std::fs::write(worktree.join("tracked"), b"two\n").unwrap();
    sandbox.git(&worktree, &["commit", "--quiet", "-am", "ahead"]);
    sandbox
        .grove_in(&root, &["list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("AHEAD"));

    sandbox.git(&worktree, &["push", "--quiet", "origin", "main"]);
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
    std::fs::write(peer.join("remote"), b"remote\n").unwrap();
    sandbox.git(&peer, &["add", "remote"]);
    sandbox.git(&peer, &["commit", "--quiet", "-m", "remote"]);
    sandbox.git(&peer, &["push", "--quiet", "origin", "main"]);
    sandbox.git(&worktree, &["fetch", "--quiet", "origin"]);
    std::fs::write(worktree.join("dirty"), b"dirty\n").unwrap();
    sandbox
        .grove_in(&root, &["list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("DIRTY-BEHIND"));

    std::fs::write(worktree.join("tracked"), b"local-divergence\n").unwrap();
    sandbox.git(&worktree, &["commit", "--quiet", "-am", "local"]);
    sandbox
        .grove_in(&root, &["list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("DIVERGED"));

    sandbox.git(
        &root,
        &[
            "--git-dir",
            root.join(".bare").to_str().unwrap(),
            "update-ref",
            "-d",
            "refs/remotes/origin/main",
        ],
    );
    sandbox
        .grove_in(&root, &["list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("UPSTREAM-GONE"))
        .stdout(predicates::str::contains("origin/main"));
}

#[test]
fn an_in_progress_worktree_is_reported_and_stays_exit_zero() {
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
    let admin = admin_dir(&sandbox, &worktree);
    std::fs::write(admin.join("index.lock"), b"").unwrap();

    sandbox
        .grove_in(&root, &["list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("IN-PROGRESS"));
}

#[test]
fn a_locked_and_in_progress_worktree_remains_locked() {
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
    sandbox.git(
        &root,
        &[
            "--git-dir",
            root.join(".bare").to_str().unwrap(),
            "worktree",
            "lock",
            "--reason",
            "human review",
            worktree.to_str().unwrap(),
        ],
    );
    let admin = admin_dir(&sandbox, &worktree);
    std::fs::write(admin.join("index.lock"), b"").unwrap();

    sandbox
        .grove_in(&root, &["list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("LOCKED"))
        .stdout(predicates::str::contains("IN-PROGRESS").not());
}

#[test]
fn an_in_progress_detached_worktree_stays_in_progress() {
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
    let review = root.join("review");
    let admin = admin_dir(&sandbox, &review);
    std::fs::write(admin.join("index.lock"), b"").unwrap();

    sandbox
        .grove_in(&root, &["list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("IN-PROGRESS"));
}

#[test]
fn list_preserves_git_worktree_record_order_not_path_order() {
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
        .grove_in(&root, &["add", "aaa-later", "--start", "HEAD"])
        .assert()
        .success();

    let raw = sandbox.git(
        &root,
        &[
            "--git-dir",
            root.join(".bare").to_str().unwrap(),
            "worktree",
            "list",
            "--porcelain",
            "-z",
        ],
    );
    let raw_stdout = String::from_utf8(raw.stdout).unwrap();
    let git_order: Vec<&str> = raw_stdout
        .split('\0')
        .filter(|entry| entry.starts_with("worktree "))
        .map(|entry| entry.trim_start_matches("worktree "))
        .filter(|path| *path != root.join(".bare").to_str().unwrap())
        .collect();
    assert_eq!(
        git_order.len(),
        2,
        "expected exactly two non-bare worktrees"
    );

    // The two worktree names are chosen so raw git order and path-sorted
    // order coincide by default; assert the two candidate paths so the test
    // fails loudly (rather than silently passing) if that ever changes.
    let main_path = root.join("main").to_str().unwrap().to_string();
    let later_path = root.join("aaa-later").to_str().unwrap().to_string();
    assert!(git_order.contains(&main_path.as_str()));
    assert!(git_order.contains(&later_path.as_str()));

    let output = sandbox
        .grove_in(&root, &["list", "--porcelain"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let mut grove_positions: Vec<(usize, &str)> = git_order
        .iter()
        .map(|path| (stdout.find(path).unwrap(), *path))
        .collect();
    grove_positions.sort_by_key(|(position, _)| *position);
    let grove_order: Vec<&str> = grove_positions.into_iter().map(|(_, path)| path).collect();

    assert_eq!(
        grove_order, git_order,
        "grove list must preserve git's own worktree record order"
    );
}

#[test]
fn a_registered_missing_worktree_is_reported_and_exits_two() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["init", "g", "--branch", "main"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    std::fs::rename(root.join("main"), root.join("moved-by-user")).unwrap();

    let output = sandbox
        .grove_in(&root, &["list", "--porcelain"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output
        .stdout
        .windows(b"status\0MISSING\0".len())
        .any(|bytes| bytes == b"status\0MISSING\0"));
}

#[test]
fn a_replaced_registered_worktree_is_invalid_and_exits_two() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["init", "g", "--branch", "main"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    std::fs::rename(root.join("main"), root.join("moved-by-user")).unwrap();
    std::os::unix::fs::symlink("moved-by-user", root.join("main")).unwrap();

    sandbox
        .grove_in(&root, &["list"])
        .assert()
        .code(2)
        .stdout(predicates::str::contains("INVALID"));
}

#[test]
fn a_registered_worktree_outside_the_grove_is_invalid() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["init", "g", "--branch", "main"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    std::fs::write(root.join("main/tracked"), b"one\n").unwrap();
    sandbox.git(&root.join("main"), &["add", "tracked"]);
    sandbox.git(&root.join("main"), &["commit", "--quiet", "-m", "one"]);
    let outside = sandbox.root().join("outside");
    sandbox.git(
        &root,
        &[
            "--git-dir",
            root.join(".bare").to_str().unwrap(),
            "worktree",
            "add",
            "-b",
            "outside",
            outside.to_str().unwrap(),
            "main",
        ],
    );

    sandbox
        .grove_in(&root, &["list"])
        .assert()
        .code(2)
        .stdout(predicates::str::contains("INVALID"));
}

#[test]
fn porcelain_round_trips_non_utf8_branch_and_path_bytes() {
    let sandbox = Sandbox::new();
    let branch = OsString::from_vec(b"topic-\xff".to_vec());
    sandbox
        .grove(&["init", "g", "--branch"])
        .arg(&branch)
        .assert()
        .success();
    let root = sandbox.root().join("g");

    let output = sandbox
        .grove_in(&root, &["list", "--porcelain"])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output
        .stdout
        .windows(b"branch\0topic-\xff\0".len())
        .any(|bytes| bytes == b"branch\0topic-\xff\0"));
    let mut path = b"worktree\0".to_vec();
    path.extend_from_slice(root.join(&branch).as_os_str().as_encoded_bytes());
    path.push(0);
    assert!(output.stdout.windows(path.len()).any(|bytes| bytes == path));
}

#[test]
fn list_runs_the_environment_and_metadata_gates() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["init", "g", "--branch", "main"])
        .assert()
        .success();
    let root = sandbox.root().join("g");

    sandbox
        .grove_in(&root, &["list"])
        .env("GIT_DIR", "/tmp/redirected.git")
        .assert()
        .code(64)
        .stderr(predicates::str::contains("GIT_DIR"));

    sandbox.git(
        &root,
        &[
            "config",
            "--file",
            root.join(".bare/config").to_str().unwrap(),
            "grove.version",
            "2",
        ],
    );
    sandbox
        .grove_in(&root, &["list"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("newer"));
}
