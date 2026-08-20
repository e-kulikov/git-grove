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
