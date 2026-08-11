mod harness;

use harness::Sandbox;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

#[test]
fn builds_a_working_grove_from_a_local_origin() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("o");

    sandbox
        .grove(&["clone", origin.to_str().unwrap(), "g"])
        .assert()
        .success();
    let root = sandbox.root().join("g");

    assert!(root.join(".bare").is_dir());
    assert_eq!(
        std::fs::read(root.join(".git")).unwrap(),
        b"gitdir: ./.bare\n"
    );
    assert!(root.join("AGENTS.md").is_file());
    assert_eq!(
        std::fs::read_link(root.join("CLAUDE.md")).unwrap(),
        Path::new("AGENTS.md")
    );
    assert!(root.join("main").is_dir(), "first worktree missing");

    let refspec = sandbox.git(
        &root,
        &[
            "config",
            "--file",
            root.join(".bare/config").to_str().unwrap(),
            "--get",
            "remote.origin.fetch",
        ],
    );
    assert_eq!(refspec.stdout, b"+refs/heads/*:refs/remotes/origin/*\n");

    let upstream = sandbox.git(
        &root.join("main"),
        &["rev-parse", "--abbrev-ref", "@{upstream}"],
    );
    assert_eq!(upstream.stdout, b"origin/main\n");

    for (key, expected) in [
        ("grove.version", "1"),
        ("grove.defaultBranch", "main"),
        ("grove.remote", "origin"),
        ("grove.publishState", "published"),
        ("worktree.guessRemote", "true"),
    ] {
        let value = sandbox.git(
            &root,
            &[
                "config",
                "--file",
                root.join(".bare/config").to_str().unwrap(),
                "--get",
                key,
            ],
        );
        assert_eq!(String::from_utf8(value.stdout).unwrap().trim(), expected);
    }
}

#[test]
fn derives_the_directory_from_the_url() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("derived");
    sandbox
        .grove(&["clone", origin.to_str().unwrap()])
        .assert()
        .success();
    assert!(sandbox.root().join("derived").join(".bare").is_dir());
}

#[test]
fn honours_a_renamed_remote() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("o");
    sandbox
        .grove(&[
            "clone",
            origin.to_str().unwrap(),
            "g",
            "--",
            "--origin",
            "upstream",
        ])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    let refspec = sandbox.git(
        &root,
        &[
            "config",
            "--file",
            root.join(".bare/config").to_str().unwrap(),
            "--get",
            "remote.upstream.fetch",
        ],
    );
    assert_eq!(refspec.stdout, b"+refs/heads/*:refs/remotes/upstream/*\n");
    assert!(std::fs::read_to_string(root.join("AGENTS.md"))
        .unwrap()
        .contains("upstream/"));
}

#[test]
fn narrows_the_refspec_for_a_narrowed_clone() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("o");
    sandbox
        .grove(&[
            "clone",
            origin.to_str().unwrap(),
            "g",
            "--",
            "--single-branch",
        ])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    let refspec = sandbox.git(
        &root,
        &[
            "config",
            "--file",
            root.join(".bare/config").to_str().unwrap(),
            "--get-all",
            "remote.origin.fetch",
        ],
    );
    assert_eq!(
        refspec.stdout,
        b"+refs/heads/main:refs/remotes/origin/main\n"
    );
}

#[test]
fn refuses_occupied_and_partial_roots_without_overwriting() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("o");

    for name in ["occupied", "partial"] {
        let root = sandbox.root().join(name);
        std::fs::create_dir(&root).unwrap();
        let sentinel = if name == "partial" {
            root.join(".bare")
        } else {
            root.join("mine")
        };
        std::fs::write(&sentinel, b"mine").unwrap();

        sandbox
            .grove(&["clone", origin.to_str().unwrap(), name])
            .assert()
            .code(2);
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"mine");
    }
}

#[test]
fn supports_absolute_nested_targets_and_an_explicit_first_branch() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("o");
    let seed = sandbox.root().join("o-seed");
    sandbox.git(&seed, &["switch", "-c", "feature/nested"]);
    std::fs::write(seed.join("feature.txt"), b"feature\n").unwrap();
    sandbox.git(&seed, &["add", "feature.txt"]);
    sandbox.git(&seed, &["commit", "--quiet", "-m", "feature"]);
    sandbox.git(&seed, &["push", "--quiet", "origin", "feature/nested"]);

    let target = sandbox.root().join("deep/grove");
    sandbox
        .grove(&[
            "clone",
            origin.to_str().unwrap(),
            target.to_str().unwrap(),
            "--branch",
            "feature/nested",
        ])
        .assert()
        .success();

    assert!(target.join("feature/nested/feature.txt").is_file());
    let configured_default = sandbox.git(
        &target,
        &[
            "config",
            "--file",
            target.join(".bare/config").to_str().unwrap(),
            "--get",
            "grove.defaultBranch",
        ],
    );
    assert_eq!(configured_default.stdout, b"feature/nested\n");
}

#[test]
fn configures_upstreams_for_every_matching_local_branch() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("o");
    let seed = sandbox.root().join("o-seed");
    for branch in ["alpha", "release/one"] {
        sandbox.git(&seed, &["switch", "-c", branch, "main"]);
        sandbox.git(&seed, &["push", "--quiet", "origin", branch]);
    }

    sandbox
        .grove(&["clone", origin.to_str().unwrap(), "g"])
        .assert()
        .success();
    let root = sandbox.root().join("g");
    for branch in ["main", "alpha", "release/one"] {
        let remote = sandbox.git(
            &root,
            &[
                "config",
                "--file",
                root.join(".bare/config").to_str().unwrap(),
                "--get",
                &format!("branch.{branch}.remote"),
            ],
        );
        let merge = sandbox.git(
            &root,
            &[
                "config",
                "--file",
                root.join(".bare/config").to_str().unwrap(),
                "--get",
                &format!("branch.{branch}.merge"),
            ],
        );
        assert_eq!(remote.stdout, b"origin\n");
        assert_eq!(merge.stdout, format!("refs/heads/{branch}\n").as_bytes());
    }
}

#[test]
fn retains_a_failed_clone_for_identity_aware_recovery() {
    let sandbox = Sandbox::new();
    let missing = sandbox.root().join("missing.git");

    sandbox
        .grove(&["clone", missing.to_str().unwrap(), "failed"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("partial clone retained"));

    assert!(sandbox.root().join("failed/.bare").is_dir());
}

#[test]
fn refuses_broken_symlink_roots_and_bare_entries() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("o");
    std::os::unix::fs::symlink("missing", sandbox.root().join("root-link")).unwrap();
    sandbox
        .grove(&["clone", origin.to_str().unwrap(), "root-link"])
        .assert()
        .code(2);
    assert_eq!(
        std::fs::read_link(sandbox.root().join("root-link")).unwrap(),
        Path::new("missing")
    );

    let root = sandbox.root().join("bare-link");
    std::fs::create_dir(&root).unwrap();
    std::os::unix::fs::symlink("missing", root.join(".bare")).unwrap();
    sandbox
        .grove(&["clone", origin.to_str().unwrap(), "bare-link"])
        .assert()
        .code(2);
    assert_eq!(
        std::fs::read_link(root.join(".bare")).unwrap(),
        Path::new("missing")
    );
}

#[test]
fn rejects_usage_before_creating_the_target() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("o");

    sandbox
        .grove(&[
            "clone",
            origin.to_str().unwrap(),
            "never-created",
            "--branch",
            ".bare",
        ])
        .assert()
        .code(64);
    assert!(!sandbox.root().join("never-created").exists());

    sandbox
        .grove_os(&[
            OsString::from("clone"),
            OsString::new(),
            OsString::from("empty-url-target"),
        ])
        .assert()
        .code(64)
        .stderr(predicates::str::contains("repository URL is empty"));
    assert!(!sandbox.root().join("empty-url-target").exists());
}

#[test]
fn dissociated_reference_leaves_no_alternates() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("o");
    sandbox
        .grove(&[
            "clone",
            origin.to_str().unwrap(),
            "g",
            "--",
            "--reference",
            origin.to_str().unwrap(),
            "--dissociate",
        ])
        .assert()
        .success();
    assert!(!sandbox
        .root()
        .join("g/.bare/objects/info/alternates")
        .exists());
}

#[test]
fn preserves_non_utf8_url_target_branch_and_remote_bytes() {
    let sandbox = Sandbox::new();
    let original = sandbox.bare_origin("raw");
    let raw_origin = sandbox
        .root()
        .join(OsString::from_vec(b"origin-\xff.git".to_vec()));
    std::fs::rename(&original, &raw_origin).unwrap();
    let raw_branch = OsString::from_vec(b"branch-\xfe".to_vec());
    let seed = sandbox.root().join("raw-seed");
    sandbox.git_os(
        &seed,
        &[
            OsString::from("switch"),
            OsString::from("-c"),
            raw_branch.clone(),
            OsString::from("main"),
        ],
    );
    sandbox.git_os(
        &seed,
        &[
            OsString::from("push"),
            raw_origin.as_os_str().to_os_string(),
            raw_branch.clone(),
        ],
    );
    let raw_target = OsString::from_vec(b"grove-\xfd".to_vec());
    let raw_remote = OsString::from_vec(b"remote-\xfc".to_vec());

    sandbox
        .grove_os(&[
            OsString::from("clone"),
            raw_origin.as_os_str().to_os_string(),
            raw_target.clone(),
            OsString::from("--branch"),
            raw_branch.clone(),
            OsString::from("--"),
            OsString::from("--origin"),
            raw_remote.clone(),
        ])
        .assert()
        .success();

    let root = sandbox.root().join(&raw_target);
    assert!(root.join(&raw_branch).is_dir());
    let config = std::fs::read(root.join(".bare/config")).unwrap();
    assert!(config
        .windows(raw_remote.as_bytes().len())
        .any(|window| window == raw_remote.as_bytes()));
    assert!(config
        .windows(raw_branch.as_bytes().len())
        .any(|window| window == raw_branch.as_bytes()));
}
