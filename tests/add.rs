mod harness;

use harness::Sandbox;
use std::path::Path;

fn grove_from_origin(sandbox: &Sandbox) -> std::path::PathBuf {
    let origin = sandbox.bare_origin("upstream");
    let seed = sandbox.root().join("upstream-seed");
    sandbox.git(&seed, &["branch", "feature-x"]);
    sandbox.git(&seed, &["push", "--quiet", "origin", "feature-x"]);

    let root = sandbox.root().join("g");
    std::fs::create_dir(&root).unwrap();
    sandbox.git(
        &root,
        &[
            "clone",
            "--quiet",
            "--bare",
            origin.to_str().unwrap(),
            root.join(".bare").to_str().unwrap(),
        ],
    );
    std::fs::write(root.join(".git"), "gitdir: ./.bare\n").unwrap();
    let config = root.join(".bare/config");
    sandbox.git(
        &root,
        &[
            "config",
            "--file",
            config.to_str().unwrap(),
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/origin/*",
        ],
    );
    for (key, value) in [
        ("grove.version", "1"),
        ("grove.defaultBranch", "main"),
        ("grove.remote", "origin"),
        ("grove.publishState", "published"),
    ] {
        sandbox.git(
            &root,
            &["config", "--file", config.to_str().unwrap(), key, value],
        );
    }
    sandbox.git(
        &root,
        &[
            "--git-dir",
            root.join(".bare").to_str().unwrap(),
            "fetch",
            "--quiet",
            "--all",
        ],
    );
    root
}

fn empty_grove(sandbox: &Sandbox, name: &str) -> std::path::PathBuf {
    let root = sandbox.root().join(name);
    std::fs::create_dir(&root).unwrap();
    sandbox.git(
        &root,
        &[
            "init",
            "--quiet",
            "--bare",
            "--initial-branch=main",
            root.join(".bare").to_str().unwrap(),
        ],
    );
    std::fs::write(root.join(".git"), "gitdir: ./.bare\n").unwrap();
    let config = root.join(".bare/config");
    for (key, value) in [
        ("grove.version", "1"),
        ("grove.defaultBranch", "main"),
        ("grove.publishState", "unpublished"),
    ] {
        sandbox.git(
            &root,
            &["config", "--file", config.to_str().unwrap(), key, value],
        );
    }
    root
}

#[test]
fn checks_out_an_existing_local_branch() {
    let sandbox = Sandbox::new();
    let root = grove_from_origin(&sandbox);

    sandbox.grove_in(&root, &["add", "main"]).assert().success();

    let head = sandbox.git(&root.join("main"), &["symbolic-ref", "--short", "HEAD"]);
    assert_eq!(head.stdout, b"main\n");
}

#[test]
fn names_the_branch_explicitly_for_a_nested_path() {
    let sandbox = Sandbox::new();
    let root = grove_from_origin(&sandbox);
    let seed = sandbox.root().join("upstream-seed");
    sandbox.git(&seed, &["branch", "release/1.0"]);
    sandbox.git(&seed, &["push", "--quiet", "origin", "release/1.0"]);
    sandbox.git(
        &root,
        &[
            "--git-dir",
            root.join(".bare").to_str().unwrap(),
            "fetch",
            "--quiet",
            "--all",
        ],
    );

    sandbox
        .grove_in(&root, &["add", "release/1.0"])
        .assert()
        .success();

    let head = sandbox.git(
        &root.join("release/1.0"),
        &["symbolic-ref", "--short", "HEAD"],
    );
    assert_eq!(
        head.stdout, b"release/1.0\n",
        "the branch must not be derived from the path basename"
    );
}

#[test]
fn tracks_a_remote_only_branch() {
    let sandbox = Sandbox::new();
    let root = grove_from_origin(&sandbox);
    sandbox.git(
        &root,
        &[
            "--git-dir",
            root.join(".bare").to_str().unwrap(),
            "branch",
            "-D",
            "feature-x",
        ],
    );

    sandbox
        .grove_in(&root, &["add", "feature-x"])
        .assert()
        .success();

    let upstream = sandbox.git(
        &root.join("feature-x"),
        &["rev-parse", "--abbrev-ref", "@{upstream}"],
    );
    assert_eq!(upstream.stdout, b"origin/feature-x\n");
}

#[test]
fn refuses_a_new_branch_without_a_start_point() {
    let sandbox = Sandbox::new();
    let root = grove_from_origin(&sandbox);

    sandbox
        .grove_in(&root, &["add", "brand-new"])
        .assert()
        .code(64)
        .stderr(predicates::str::contains("--start"));
}

#[test]
fn refuses_an_occupied_path() {
    let sandbox = Sandbox::new();
    let root = grove_from_origin(&sandbox);
    std::fs::create_dir(root.join("main")).unwrap();

    sandbox.grove_in(&root, &["add", "main"]).assert().code(2);
}

#[test]
fn rejects_start_for_local_and_remote_existing_branches() {
    let sandbox = Sandbox::new();
    let root = grove_from_origin(&sandbox);

    sandbox
        .grove_in(&root, &["add", "main", "--start", "HEAD"])
        .assert()
        .code(64)
        .stderr(predicates::str::contains("already exists locally"));

    sandbox.git(
        &root,
        &[
            "--git-dir",
            root.join(".bare").to_str().unwrap(),
            "branch",
            "-D",
            "feature-x",
        ],
    );
    sandbox
        .grove_in(&root, &["add", "feature-x", "--start", "HEAD"])
        .assert()
        .code(64)
        .stderr(predicates::str::contains("already exists on a remote"));
}

#[test]
fn reports_multiple_remote_candidates_in_deterministic_order() {
    let sandbox = Sandbox::new();
    let root = grove_from_origin(&sandbox);
    sandbox.git(
        &root,
        &[
            "--git-dir",
            root.join(".bare").to_str().unwrap(),
            "branch",
            "-D",
            "feature-x",
        ],
    );
    let backup = sandbox.bare_origin("backup");
    let backup_seed = sandbox.root().join("backup-seed");
    sandbox.git(&backup_seed, &["branch", "feature-x"]);
    sandbox.git(&backup_seed, &["push", "--quiet", "origin", "feature-x"]);
    sandbox.git(
        &root,
        &[
            "--git-dir",
            root.join(".bare").to_str().unwrap(),
            "remote",
            "add",
            "backup",
            backup.to_str().unwrap(),
        ],
    );
    sandbox.git(
        &root,
        &[
            "--git-dir",
            root.join(".bare").to_str().unwrap(),
            "fetch",
            "--quiet",
            "backup",
        ],
    );

    sandbox
        .grove_in(&root, &["add", "feature-x"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains(
            "backup/feature-x, origin/feature-x",
        ));
}

#[test]
fn creates_a_new_branch_from_an_explicit_start() {
    let sandbox = Sandbox::new();
    let root = grove_from_origin(&sandbox);
    let expected = sandbox.git(
        &root,
        &[
            "--git-dir",
            root.join(".bare").to_str().unwrap(),
            "rev-parse",
            "main",
        ],
    );

    sandbox
        .grove_in(&root, &["add", "brand-new", "--start", "main"])
        .assert()
        .success();

    let head = sandbox.git(&root.join("brand-new"), &["rev-parse", "HEAD"]);
    assert_eq!(head.stdout, expected.stdout);
    let branch = sandbox.git(
        &root.join("brand-new"),
        &["symbolic-ref", "--short", "HEAD"],
    );
    assert_eq!(branch.stdout, b"brand-new\n");
}

#[test]
fn creates_an_orphan_branch_in_an_unborn_grove() {
    let sandbox = Sandbox::new();
    let root = empty_grove(&sandbox, "empty");

    sandbox
        .grove_in(&root, &["add", "topic"])
        .assert()
        .success();

    let head = sandbox.git(&root.join("topic"), &["symbolic-ref", "--short", "HEAD"]);
    assert_eq!(head.stdout, b"topic\n");
}

#[test]
fn creates_a_detached_worktree_in_the_derived_directory() {
    let sandbox = Sandbox::new();
    let root = grove_from_origin(&sandbox);
    let short = sandbox.git(
        &root,
        &[
            "--git-dir",
            root.join(".bare").to_str().unwrap(),
            "rev-parse",
            "--short",
            "main",
        ],
    );
    let short = String::from_utf8(short.stdout).unwrap();
    let path = root.join(format!("detached-{}", short.trim()));

    sandbox
        .grove_in(&root, &["add", "--detach", "main"])
        .assert()
        .success();

    let symbolic = std::process::Command::new("git")
        .current_dir(&path)
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(symbolic.status.code(), Some(1));
}

#[test]
fn explicit_nested_directory_does_not_change_branch_identity() {
    let sandbox = Sandbox::new();
    let root = grove_from_origin(&sandbox);

    sandbox
        .grove_in(&root, &["add", "main", "nested/by-path"])
        .assert()
        .success();

    let head = sandbox.git(
        &root.join("nested/by-path"),
        &["symbolic-ref", "--short", "HEAD"],
    );
    assert_eq!(head.stdout, b"main\n");
}

#[test]
fn a_branch_checked_out_in_another_worktree_needs_a_human_decision() {
    let sandbox = Sandbox::new();
    let root = grove_from_origin(&sandbox);
    sandbox.grove_in(&root, &["add", "main"]).assert().success();

    sandbox
        .grove_in(&root, &["add", "main", "other-main"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("already checked out"));
    assert!(!root.join("other-main").exists());
}

#[test]
fn rejects_unsafe_worktree_paths_without_touching_foreign_entries() {
    for kind in ["file", "broken-symlink", "symlink-ancestor", "escape"] {
        let sandbox = Sandbox::new();
        let root = grove_from_origin(&sandbox);
        let requested = match kind {
            "file" => {
                std::fs::write(root.join("occupied"), b"foreign").unwrap();
                "occupied"
            }
            "broken-symlink" => {
                std::os::unix::fs::symlink("missing", root.join("occupied")).unwrap();
                "occupied"
            }
            "symlink-ancestor" => {
                std::fs::create_dir(root.join("real")).unwrap();
                std::os::unix::fs::symlink("real", root.join("linked")).unwrap();
                "linked/worktree"
            }
            "escape" => "../outside",
            _ => unreachable!(),
        };

        sandbox
            .grove_in(&root, &["add", "main", requested])
            .assert()
            .failure();

        match kind {
            "file" => assert_eq!(std::fs::read(root.join("occupied")).unwrap(), b"foreign"),
            "broken-symlink" => assert_eq!(
                std::fs::read_link(root.join("occupied")).unwrap(),
                Path::new("missing")
            ),
            "symlink-ancestor" => assert!(root.join("linked").is_symlink()),
            "escape" => assert!(!sandbox.root().join("outside").exists()),
            _ => unreachable!(),
        }
    }
}

#[test]
fn refuses_unsupported_metadata_before_creating_a_worktree() {
    let sandbox = Sandbox::new();
    let root = grove_from_origin(&sandbox);
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
        .grove_in(&root, &["add", "main"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("newer"));
    assert!(!root.join("main").exists());
}

#[test]
fn lifecycle_gate_refuses_unsafe_environment_before_discovery_or_mutation() {
    let sandbox = Sandbox::new();
    let root = grove_from_origin(&sandbox);

    sandbox
        .grove_in(&root, &["add", "main"])
        .env("GIT_DIR", "/tmp/redirected.git")
        .assert()
        .code(64)
        .stderr(predicates::str::contains("GIT_DIR"));
    assert!(!root.join("main").exists());
}

#[test]
fn a_non_absence_git_query_status_is_an_unexpected_failure() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new();
    let root = grove_from_origin(&sandbox);
    let bin = sandbox.root().join("failing-bin");
    std::fs::create_dir(&bin).unwrap();
    let git = bin.join("git");
    std::fs::write(
        &git,
        "#!/bin/sh\ncase \"$*\" in\n  *'show-ref --verify --quiet refs/heads/main'*) printf 'query broke' >&2; exit 7;;\nesac\nexec /usr/bin/git \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());

    sandbox
        .grove_in(&root, &["add", "main"])
        .env("PATH", path)
        .assert()
        .code(1)
        .stderr(predicates::str::contains("exit status 7"))
        .stderr(predicates::str::contains(r"query\x20broke"));
    assert!(!root.join("main").exists());
}

#[cfg(unix)]
#[test]
fn preserves_non_utf8_branch_and_path_bytes() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let sandbox = Sandbox::new();
    let root = grove_from_origin(&sandbox);
    let branch = OsString::from_vec(b"topic-\xff".to_vec());
    let directory = OsString::from_vec(b"worktree-\xfe".to_vec());
    let mut bare_flag = OsString::from("--git-dir=");
    bare_flag.push(root.join(".bare"));
    sandbox.git_os(
        &root,
        &[
            bare_flag,
            OsString::from("branch"),
            branch.clone(),
            OsString::from("main"),
        ],
    );

    sandbox
        .grove_in(&root, &["add"])
        .arg(&branch)
        .arg(&directory)
        .assert()
        .success()
        .stdout(predicates::str::contains(r"worktree-\xFE"));

    let worktree = root.join(&directory);
    let head = sandbox.git_os(
        &worktree,
        &[
            OsString::from("symbolic-ref"),
            OsString::from("--short"),
            OsString::from("HEAD"),
        ],
    );
    assert_eq!(head.stdout, b"topic-\xff\n");
}
