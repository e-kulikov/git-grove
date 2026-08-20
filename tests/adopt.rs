mod harness;

use git_grove::commands::adopt::preflight;
use git_grove::commands::adopt::AdoptArgs;
use git_grove::error::ExitClass;
use git_grove::git::runner::RealGit;
use harness::Sandbox;
use std::path::{Path, PathBuf};

fn flat_repository(sandbox: &Sandbox, name: &str) -> PathBuf {
    let root = sandbox.root().join(name);
    std::fs::create_dir(&root).unwrap();
    sandbox.git(&root, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(root.join("tracked"), b"tracked\n").unwrap();
    sandbox.git(&root, &["add", "tracked"]);
    sandbox.git(&root, &["commit", "--quiet", "-m", "initial"]);
    root
}

#[test]
fn preflight_accepts_a_quiescent_single_worktree_and_defaults_to_its_top_level() {
    let sandbox = Sandbox::new();
    let root = flat_repository(&sandbox, "flat");
    let nested = root.join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(nested.join("untracked"), b"bytes").unwrap();

    let plan = preflight::plan(&RealGit::new(), &AdoptArgs::fresh(None), &nested).unwrap();

    assert_eq!(plan.root.path(), root);
    assert_eq!(plan.decisions.default_branch.decode(), b"main");
    assert_eq!(plan.decisions.payload_path.decode(), b"main");
    assert!(plan.decisions.selected_remote.is_none());
    assert!(plan
        .original
        .payload_manifest
        .iter()
        .any(|entry| entry.path.to_path_buf() == Path::new("nested/untracked")));
    assert!(!root.join(".bare").exists());
    assert!(!std::fs::read_dir(&root).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".grove-adopt-")));
}

#[test]
fn preflight_refuses_git_locks_and_hardlinks_without_mutation() {
    let sandbox = Sandbox::new();
    for case in ["lock", "hardlink"] {
        let root = flat_repository(&sandbox, case);
        match case {
            "lock" => std::fs::write(root.join(".git/index.lock"), b"").unwrap(),
            "hardlink" => {
                std::fs::write(root.join("one"), b"linked").unwrap();
                std::fs::hard_link(root.join("one"), root.join("two")).unwrap();
            }
            _ => unreachable!(),
        }
        let before = std::fs::read(root.join(".git/HEAD")).unwrap();
        let error = preflight::plan(
            &RealGit::new(),
            &AdoptArgs::fresh(Some(root.clone())),
            sandbox.root(),
        )
        .unwrap_err();
        assert_eq!(
            error.class,
            ExitClass::NeedsDecision,
            "case {case}: {error}"
        );
        assert_eq!(std::fs::read(root.join(".git/HEAD")).unwrap(), before);
        assert!(!root.join(".bare").exists());
        assert!(!std::fs::read_dir(&root).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".grove-adopt-")
        }));
    }
}

#[test]
fn installed_git_private_path_split_matches_the_adoption_contract() {
    let sandbox = Sandbox::new();
    let root = flat_repository(&sandbox, "split-source");
    let linked = sandbox.root().join("linked");
    sandbox.git(
        &root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "topic",
            linked.to_str().unwrap(),
        ],
    );
    let admin = sandbox.worktree_admin(&linked);
    for name in [
        "index",
        "HEAD",
        "logs/HEAD",
        "ORIG_HEAD",
        "COMMIT_EDITMSG",
        "FETCH_HEAD",
        "AUTO_MERGE",
        "config.worktree",
        "refs/worktree/x",
        "refs/bisect/x",
        "refs/rewritten/x",
    ] {
        let output = sandbox.git(
            &linked,
            &["rev-parse", "--path-format=absolute", "--git-path", name],
        );
        let actual = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim_end());
        assert!(actual.starts_with(&admin), "{name} mapped to {actual:?}");
    }
    for (name, expected) in [
        ("refs/stash", root.join(".git/refs/stash")),
        ("logs/refs/stash", root.join(".git/logs/refs/stash")),
    ] {
        let output = sandbox.git(
            &linked,
            &["rev-parse", "--path-format=absolute", "--git-path", name],
        );
        let actual = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim_end());
        assert_eq!(actual, expected, "{name}");
    }
}

#[test]
fn preflight_selects_the_only_remote_and_refuses_ambiguous_metadata() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("source");
    let root = sandbox.root().join("remote-flat");
    sandbox.git(
        sandbox.root(),
        &[
            "clone",
            "--quiet",
            origin.to_str().unwrap(),
            root.to_str().unwrap(),
        ],
    );
    let plan = preflight::plan(
        &RealGit::new(),
        &AdoptArgs::fresh(Some(root.clone())),
        sandbox.root(),
    )
    .unwrap();
    assert_eq!(
        plan.decisions.selected_remote.as_ref().unwrap().decode(),
        b"origin"
    );
    drop(plan);

    sandbox.git(
        &root,
        &["remote", "add", "backup", origin.to_str().unwrap()],
    );
    let mut args = AdoptArgs::fresh(Some(root));
    args.default_branch = Some("main".into());
    let error = preflight::plan(&RealGit::new(), &args, sandbox.root()).unwrap_err();
    assert_eq!(error.class, ExitClass::NeedsDecision);
    assert!(error.to_string().contains("multiple remotes"));
}

#[test]
fn abort_removes_only_a_strict_untouched_torn_bootstrap() {
    let sandbox = Sandbox::new();
    let root = flat_repository(&sandbox, "torn-bootstrap");
    let transaction = root.join(".grove-adopt-deadbeef");
    std::fs::create_dir(&transaction).unwrap();
    let mut permissions = std::fs::metadata(&transaction).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o700);
    std::fs::set_permissions(&transaction, permissions).unwrap();
    std::fs::write(transaction.join("journal.json.new"), b"{").unwrap();

    sandbox
        .grove_in(
            sandbox.root(),
            &["adopt", "--continue", root.to_str().unwrap()],
        )
        .assert()
        .code(2);
    assert!(transaction.exists());

    sandbox
        .grove_in(
            sandbox.root(),
            &["adopt", "--abort", root.to_str().unwrap()],
        )
        .assert()
        .success();
    assert!(!transaction.exists());
    assert!(root.join(".git").is_dir());
}

#[test]
fn fresh_adopt_preserves_the_payload_and_builds_a_grove() {
    let sandbox = Sandbox::new();
    let root = flat_repository(&sandbox, "fresh");
    std::fs::write(root.join("delete-me"), b"delete\n").unwrap();
    std::fs::write(root.join("rename-me"), b"rename\n").unwrap();
    std::fs::write(root.join("executable"), b"#!/bin/sh\n").unwrap();
    std::fs::write(root.join(".gitignore"), b"ignored\n").unwrap();
    std::os::unix::fs::symlink("tracked", root.join("tracked-link")).unwrap();
    sandbox.git(
        &root,
        &[
            "add",
            "delete-me",
            "rename-me",
            "executable",
            ".gitignore",
            "tracked-link",
        ],
    );
    sandbox.git(&root, &["commit", "--quiet", "-m", "fixture"]);
    std::fs::write(root.join("tracked"), b"modified\n").unwrap();
    std::fs::write(root.join("untracked"), b"untracked\n").unwrap();
    std::fs::write(root.join("ignored"), b"ignored\n").unwrap();
    std::fs::write(root.join("staged"), b"staged\n").unwrap();
    sandbox.git(&root, &["add", "staged"]);
    sandbox.git(&root, &["rm", "--quiet", "delete-me"]);
    sandbox.git(&root, &["mv", "rename-me", "renamed"]);
    let mut permissions = std::fs::metadata(root.join("executable"))
        .unwrap()
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(root.join("executable"), permissions).unwrap();
    sandbox.git(&root, &["update-index", "--split-index"]);
    let head = sandbox.git(&root, &["rev-parse", "HEAD"]).stdout;
    for name in ["ORIG_HEAD", "AUTO_MERGE"] {
        std::fs::write(root.join(".git").join(name), &head).unwrap();
    }
    std::fs::write(root.join(".git/COMMIT_EDITMSG"), b"message\n").unwrap();
    std::fs::write(root.join(".git/FETCH_HEAD"), b"").unwrap();
    let before_status = sandbox
        .git(
            &root,
            &[
                "status",
                "--porcelain=v2",
                "-z",
                "--branch",
                "--untracked-files=all",
                "--ignored=matching",
            ],
        )
        .stdout;
    let before_stage = sandbox.git(&root, &["ls-files", "--stage", "-z"]).stdout;
    let before_verbose = sandbox.git(&root, &["ls-files", "-v", "-z"]).stdout;

    sandbox.grove_in(&root, &["adopt"]).assert().success();

    assert_eq!(
        std::fs::read(root.join(".git")).unwrap(),
        b"gitdir: ./.bare\n"
    );
    assert!(root.join(".bare").is_dir());
    assert_eq!(
        std::fs::read(root.join("main/tracked")).unwrap(),
        b"modified\n"
    );
    assert_eq!(
        std::fs::read(root.join("main/untracked")).unwrap(),
        b"untracked\n"
    );
    assert_eq!(
        sandbox
            .git(
                &root.join("main"),
                &[
                    "status",
                    "--porcelain=v2",
                    "-z",
                    "--branch",
                    "--untracked-files=all",
                    "--ignored=matching",
                ]
            )
            .stdout,
        before_status
    );
    assert_eq!(
        sandbox
            .git(&root.join("main"), &["ls-files", "--stage", "-z"])
            .stdout,
        before_stage
    );
    assert_eq!(
        sandbox
            .git(&root.join("main"), &["ls-files", "-v", "-z"])
            .stdout,
        before_verbose
    );
    for name in ["ORIG_HEAD", "AUTO_MERGE", "COMMIT_EDITMSG", "FETCH_HEAD"] {
        let path = String::from_utf8(
            sandbox
                .git(
                    &root.join("main"),
                    &["rev-parse", "--path-format=absolute", "--git-path", name],
                )
                .stdout,
        )
        .unwrap();
        assert!(Path::new(path.trim()).starts_with(root.join(".bare/worktrees")));
        assert!(!root.join(".bare").join(name).exists());
    }
    assert_eq!(
        sandbox.repo_config(&root, "grove.defaultBranch").as_deref(),
        Some("main")
    );
    assert!(!std::fs::read_dir(&root).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".grove-adopt-")));
}

#[test]
fn branch_matrix_keeps_a_non_default_payload_and_creates_the_default_checkout() {
    let sandbox = Sandbox::new();
    let root = flat_repository(&sandbox, "matrix");
    sandbox.git(&root, &["branch", "topic"]);
    sandbox.git(&root, &["switch", "--quiet", "topic"]);

    sandbox
        .grove_in(
            sandbox.root(),
            &["adopt", "--default-branch", "main", root.to_str().unwrap()],
        )
        .assert()
        .success();

    assert_eq!(
        String::from_utf8(
            sandbox
                .git(&root.join("topic"), &["branch", "--show-current"])
                .stdout
        )
        .unwrap()
        .trim(),
        "topic"
    );
    assert_eq!(
        String::from_utf8(
            sandbox
                .git(&root.join("main"), &["branch", "--show-current"])
                .stdout
        )
        .unwrap()
        .trim(),
        "main"
    );
}

#[test]
fn branch_matrix_preserves_a_detached_payload_and_materializes_the_default() {
    let sandbox = Sandbox::new();
    let root = flat_repository(&sandbox, "detached");
    let oid = String::from_utf8(sandbox.git(&root, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    sandbox.git(&root, &["switch", "--quiet", "--detach"]);

    sandbox
        .grove_in(
            sandbox.root(),
            &["adopt", "--default-branch", "main", root.to_str().unwrap()],
        )
        .assert()
        .success();

    let payload = root.join(format!("detached-{}", &oid[..12]));
    assert_eq!(
        String::from_utf8(sandbox.git(&payload, &["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim(),
        oid
    );
    assert_eq!(
        String::from_utf8(
            sandbox
                .git(&payload, &["rev-parse", "--abbrev-ref", "HEAD"])
                .stdout
        )
        .unwrap()
        .trim(),
        "HEAD"
    );
    assert_eq!(
        String::from_utf8(
            sandbox
                .git(&root.join("main"), &["branch", "--show-current"])
                .stdout
        )
        .unwrap()
        .trim(),
        "main"
    );
}

#[test]
fn branch_matrix_tracks_a_remote_only_default_without_fetching() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("adopt-origin");
    let root = sandbox.root().join("remote-only");
    sandbox.git(
        sandbox.root(),
        &[
            "clone",
            "--quiet",
            origin.to_str().unwrap(),
            root.to_str().unwrap(),
        ],
    );
    sandbox.git(&root, &["switch", "--quiet", "-c", "topic"]);
    sandbox.git(&root, &["branch", "-D", "main"]);

    sandbox
        .grove_in(sandbox.root(), &["adopt", root.to_str().unwrap()])
        .assert()
        .success();

    assert_eq!(
        String::from_utf8(
            sandbox
                .git(&root.join("main"), &["branch", "--show-current"])
                .stdout
        )
        .unwrap()
        .trim(),
        "main"
    );
    assert_eq!(
        sandbox
            .repo_config(&root.join("main"), "branch.main.remote")
            .as_deref(),
        Some("origin")
    );
    assert_eq!(
        sandbox.repo_config(&root, "grove.remote").as_deref(),
        Some("origin")
    );
}

#[test]
fn branch_matrix_keeps_worktree_config_private_and_bare_layout_separate() {
    let sandbox = Sandbox::new();
    let root = flat_repository(&sandbox, "worktree-config");
    sandbox.git(&root, &["config", "extensions.worktreeConfig", "true"]);
    sandbox.git(&root, &["config", "--worktree", "custom.payload", "kept"]);

    sandbox
        .grove_in(sandbox.root(), &["adopt", root.to_str().unwrap()])
        .assert()
        .success();

    assert_eq!(
        sandbox
            .repo_config(&root.join("main"), "custom.payload")
            .as_deref(),
        Some("kept")
    );
    assert_eq!(
        sandbox.repo_config(&root, "core.bare").as_deref(),
        Some("true")
    );
    assert!(!root.join("main/.git").is_dir());
}

#[cfg(feature = "failpoints")]
#[test]
fn continue_replays_durable_phase_boundaries_and_torn_replacements() {
    let sandbox = Sandbox::new();
    for checkpoint in [1, 2, 5, 6, 10, 25, 50] {
        let root = flat_repository(&sandbox, &format!("continue-{checkpoint}"));
        std::fs::write(root.join("tracked"), format!("checkpoint {checkpoint}\n")).unwrap();
        sandbox
            .grove_in(sandbox.root(), &["adopt", root.to_str().unwrap()])
            .env("GIT_GROVE_FAILPOINT", format!("error:{checkpoint}"))
            .assert()
            .failure();
        assert!(std::fs::read_dir(&root).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".grove-adopt-")));

        sandbox
            .grove_in(
                sandbox.root(),
                &["adopt", "--continue", root.to_str().unwrap()],
            )
            .assert()
            .success();
        assert_eq!(
            std::fs::read(root.join("main/tracked")).unwrap(),
            format!("checkpoint {checkpoint}\n").as_bytes()
        );
        assert!(!std::fs::read_dir(&root).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".grove-adopt-")));
    }
}

#[cfg(feature = "failpoints")]
#[test]
fn abort_reverses_each_completed_or_physically_completed_phase() {
    let sandbox = Sandbox::new();
    for checkpoint in [1, 5, 10, 15, 20, 25, 30, 35, 40, 45] {
        let root = flat_repository(&sandbox, &format!("abort-{checkpoint}"));
        std::fs::write(root.join("tracked"), format!("abort {checkpoint}\n")).unwrap();
        std::fs::write(root.join("untracked"), b"untracked\n").unwrap();
        let before_status = sandbox
            .git(
                &root,
                &[
                    "status",
                    "--porcelain=v2",
                    "-z",
                    "--branch",
                    "--untracked-files=all",
                    "--ignored=matching",
                ],
            )
            .stdout;

        sandbox
            .grove_in(sandbox.root(), &["adopt", root.to_str().unwrap()])
            .env("GIT_GROVE_FAILPOINT", format!("error:{checkpoint}"))
            .assert()
            .failure();
        sandbox
            .grove_in(
                sandbox.root(),
                &["adopt", "--abort", root.to_str().unwrap()],
            )
            .assert()
            .success();

        assert!(root.join(".git").is_dir(), "checkpoint {checkpoint}");
        assert!(!root.join(".bare").exists(), "checkpoint {checkpoint}");
        assert_eq!(
            std::fs::read(root.join("tracked")).unwrap(),
            format!("abort {checkpoint}\n").as_bytes()
        );
        assert_eq!(
            sandbox
                .git(
                    &root,
                    &[
                        "status",
                        "--porcelain=v2",
                        "-z",
                        "--branch",
                        "--untracked-files=all",
                        "--ignored=matching",
                    ],
                )
                .stdout,
            before_status,
            "checkpoint {checkpoint}"
        );
        assert!(!std::fs::read_dir(&root).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".grove-adopt-")));
    }
}
