mod harness;

use harness::Sandbox;
use rustix::process::{kill_process, Pid, Signal};
use std::os::unix::fs::PermissionsExt;
#[cfg(feature = "failpoints")]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

fn flat_repository(sandbox: &Sandbox, name: &str) -> PathBuf {
    let root = sandbox.root().join(name);
    std::fs::create_dir(&root).unwrap();
    sandbox.git(&root, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(root.join("tracked"), b"tracked\n").unwrap();
    sandbox.git(&root, &["add", "tracked"]);
    sandbox.git(&root, &["commit", "--quiet", "-m", "initial"]);
    root
}

fn signal_git_wrapper(sandbox: &Sandbox) -> (PathBuf, PathBuf, PathBuf) {
    let bin = sandbox.root().join("signal-bin");
    std::fs::create_dir(&bin).unwrap();
    let ready = sandbox.root().join("signal-ready");
    let caught = sandbox.root().join("signal-caught");
    let script = bin.join("git");
    std::fs::write(
        &script,
        b"#!/bin/sh\ncase \"$*\" in\n  *\"$HANG_PATTERN\"*)\n    : > \"$SIGNAL_READY\"\n    trap ': > \"$SIGNAL_CAUGHT\"; exit 0' HUP INT TERM\n    while :; do :; done\n    ;;\nesac\nexec \"$REAL_GIT\" \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, ready, caught)
}

fn barrier_git_wrapper(sandbox: &Sandbox, name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let bin = sandbox.root().join(format!("barrier-bin-{name}"));
    std::fs::create_dir(&bin).unwrap();
    let ready = sandbox.root().join(format!("barrier-ready-{name}"));
    let release = sandbox.root().join(format!("barrier-release-{name}"));
    let script = bin.join("git");
    std::fs::write(
        &script,
        b"#!/bin/sh\ncase \"$*\" in\n  *\"$BARRIER_PATTERN\"*)\n    : > \"$BARRIER_READY\"\n    while test ! -e \"$BARRIER_RELEASE\"; do sleep 0.01; done\n    ;;\nesac\nexec \"$REAL_GIT\" \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, ready, release)
}

fn wrapper_path(bin: PathBuf) -> (std::ffi::OsString, PathBuf) {
    let inherited = std::env::var_os("PATH").unwrap();
    let real_git = std::env::split_paths(&inherited)
        .map(|directory| directory.join("git"))
        .find(|path| path.is_file())
        .expect("git must be present on PATH");
    let mut path = bin.into_os_string();
    path.push(":");
    path.push(inherited);
    (path, real_git)
}

fn wait_for(path: &Path) {
    for _ in 0..500 {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("{} was not created", path.display());
}

#[test]
fn handled_signals_reach_the_direct_git_child_and_leave_recoverable_state() {
    for (name, signal, exit) in [
        ("hup", Signal::HUP, 129),
        ("int", Signal::INT, 130),
        ("term", Signal::TERM, 143),
    ] {
        let sandbox = Sandbox::new();
        let root = flat_repository(&sandbox, name);
        let (bin, ready, caught) = signal_git_wrapper(&sandbox);
        let inherited_path = std::env::var_os("PATH").unwrap();
        let mut path = bin.into_os_string();
        path.push(":");
        path.push(&inherited_path);
        let real_git = std::env::split_paths(&inherited_path)
            .map(|directory| directory.join("git"))
            .find(|path| path.is_file())
            .expect("git must be present on PATH");
        let child = sandbox
            .grove_process(sandbox.root(), &["adopt", root.to_str().unwrap()])
            .env("PATH", path)
            .env("REAL_GIT", real_git)
            .env("SIGNAL_READY", &ready)
            .env("SIGNAL_CAUGHT", &caught)
            .env("HANG_PATTERN", "worktree add --no-checkout")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        wait_for(&ready);
        kill_process(Pid::from_child(&child), signal).unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(exit), "signal {name}");
        assert!(caught.exists(), "signal {name} was not forwarded");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("adopt --continue"), "{stderr}");
        sandbox
            .grove_in(
                sandbox.root(),
                &["adopt", "--continue", root.to_str().unwrap()],
            )
            .assert()
            .success();
    }
}

#[test]
fn a_signal_before_the_initial_transaction_leaves_the_repository_untouched() {
    let sandbox = Sandbox::new();
    let root = flat_repository(&sandbox, "pre-mutation-signal");
    let (bin, ready, caught) = signal_git_wrapper(&sandbox);
    let inherited_path = std::env::var_os("PATH").unwrap();
    let mut path = bin.into_os_string();
    path.push(":");
    path.push(&inherited_path);
    let real_git = std::env::split_paths(&inherited_path)
        .map(|directory| directory.join("git"))
        .find(|path| path.is_file())
        .unwrap();
    let child = sandbox
        .grove_process(sandbox.root(), &["adopt", root.to_str().unwrap()])
        .env("PATH", path)
        .env("REAL_GIT", real_git)
        .env("SIGNAL_READY", &ready)
        .env("SIGNAL_CAUGHT", &caught)
        .env("HANG_PATTERN", "rev-parse --shared-index-path")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(&ready);
    kill_process(Pid::from_child(&child), Signal::TERM).unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(143));
    assert!(caught.exists());
    assert!(root.join(".git").is_dir());
    assert!(!std::fs::read_dir(&root).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".grove-adopt-")));
}

#[test]
fn a_signal_during_initial_journal_replacement_leaves_recoverable_evidence() {
    let sandbox = Sandbox::new();
    let root = flat_repository(&sandbox, "journal-signal");
    std::fs::write(root.join("large-untracked"), vec![b'x'; 2 * 1024 * 1024]).unwrap();
    let child = sandbox
        .grove_process(sandbox.root(), &["adopt", root.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut observed = false;
    for _ in 0..5000 {
        observed = std::fs::read_dir(&root).unwrap().any(|entry| {
            let path = entry.unwrap().path();
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".grove-adopt-")
                && path.join("journal.json.new").exists()
        });
        if observed {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(observed, "journal.json.new was never observable");
    kill_process(Pid::from_child(&child), Signal::INT).unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(130));
    sandbox
        .grove_in(
            sandbox.root(),
            &["adopt", "--continue", root.to_str().unwrap()],
        )
        .assert()
        .success();
    assert_eq!(
        std::fs::metadata(root.join("main/large-untracked"))
            .unwrap()
            .len(),
        2 * 1024 * 1024
    );
}

#[test]
fn concurrent_preflight_writers_are_detected_without_losing_their_bytes() {
    for kind in [
        "payload",
        "symlink",
        "index",
        "head",
        "config",
        "config-worktree",
        "loose-ref",
        "packed-refs",
        "shared-index",
        "bare",
    ] {
        let sandbox = Sandbox::new();
        let root = flat_repository(&sandbox, kind);
        if kind == "symlink" {
            std::os::unix::fs::symlink("tracked", root.join("payload-link")).unwrap();
        }
        if kind == "config-worktree" {
            sandbox.git(&root, &["config", "extensions.worktreeConfig", "true"]);
            sandbox.git(&root, &["config", "--worktree", "owner.before", "true"]);
        }
        if kind == "packed-refs" {
            sandbox.git(&root, &["pack-refs", "--all"]);
        }
        if kind == "shared-index" {
            sandbox.git(&root, &["update-index", "--split-index"]);
        }

        let (bin, ready, release) = barrier_git_wrapper(&sandbox, kind);
        let (path, real_git) = wrapper_path(bin);
        let child = sandbox
            .grove_process(sandbox.root(), &["adopt", root.to_str().unwrap()])
            .env("PATH", path)
            .env("REAL_GIT", real_git)
            .env("BARRIER_READY", &ready)
            .env("BARRIER_RELEASE", &release)
            .env("BARRIER_PATTERN", "rev-parse --shared-index-path")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        wait_for(&ready);

        let changed = match kind {
            "payload" => root.join("tracked"),
            "symlink" => {
                std::fs::remove_file(root.join("payload-link")).unwrap();
                std::os::unix::fs::symlink("owner-target", root.join("payload-link")).unwrap();
                root.join("payload-link")
            }
            "index" => root.join(".git/index"),
            "head" => root.join(".git/HEAD"),
            "config" => root.join(".git/config"),
            "config-worktree" => root.join(".git/config.worktree"),
            "loose-ref" => {
                let path = root.join(".git/refs/heads/owner-race");
                let oid = sandbox.git(&root, &["rev-parse", "HEAD"]).stdout;
                std::fs::write(&path, oid).unwrap();
                path
            }
            "packed-refs" => root.join(".git/packed-refs"),
            "shared-index" => {
                let output = sandbox.git(&root, &["rev-parse", "--shared-index-path"]);
                let path = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
                if path.is_absolute() {
                    path
                } else {
                    root.join(path)
                }
            }
            "bare" => {
                let path = root.join(".bare");
                std::fs::create_dir(&path).unwrap();
                std::fs::write(path.join("owner"), b"owner\n").unwrap();
                path.join("owner")
            }
            _ => unreachable!(),
        };
        if !matches!(kind, "symlink" | "loose-ref" | "bare") {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(&changed)
                .unwrap_or_else(|error| {
                    panic!("{kind}: cannot open {}: {error}", changed.display())
                })
                .write_all(b"\nowner-race\n")
                .unwrap();
        }
        std::fs::write(&release, b"go\n").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            matches!(output.status.code(), Some(1 | 2)),
            "{kind}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(changed.exists() || std::fs::symlink_metadata(&changed).is_ok());
        if kind == "symlink" {
            assert_eq!(
                std::fs::read_link(&changed).unwrap(),
                Path::new("owner-target")
            );
        } else if kind == "loose-ref" {
            assert_eq!(std::fs::read(&changed).unwrap().len(), 41);
        } else {
            assert!(
                std::fs::read(&changed)
                    .unwrap()
                    .windows(b"owner".len())
                    .any(|window| window == b"owner"),
                "{kind}: owner bytes were lost"
            );
        }
        let transactions = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".grove-adopt-")
            })
            .count();
        assert!(transactions <= 1, "{kind}: {transactions} transactions");
        if transactions == 1 {
            assert!(String::from_utf8_lossy(&output.stderr).contains("adopt --continue"));
        }
    }
}

#[cfg(not(feature = "failpoints"))]
#[test]
fn featureless_binary_ignores_the_failpoint_environment() {
    let sandbox = Sandbox::new();
    let root = flat_repository(&sandbox, "featureless");
    sandbox
        .grove_in(sandbox.root(), &["adopt", root.to_str().unwrap()])
        .env("GIT_GROVE_FAILPOINT", "error:1")
        .assert()
        .success();
}

#[cfg(feature = "failpoints")]
fn injected(sandbox: &Sandbox, root: &Path, kind: &str, checkpoint: u64) -> std::process::Output {
    sandbox
        .grove_process(sandbox.root(), &["adopt", root.to_str().unwrap()])
        .env("GIT_GROVE_FAILPOINT", format!("{kind}:{checkpoint}"))
        .output()
        .unwrap()
}

#[cfg(feature = "failpoints")]
fn assert_final(root: &Path) {
    assert!(root.join(".bare").is_dir());
    assert_eq!(
        std::fs::read(root.join(".git")).unwrap(),
        b"gitdir: ./.bare\n"
    );
    assert_eq!(
        std::fs::read(root.join("main/tracked")).unwrap(),
        b"tracked\n"
    );
}

#[cfg(feature = "failpoints")]
fn assert_original(root: &Path) {
    assert!(root.join(".git").is_dir());
    assert!(!root.join(".bare").exists());
    assert_eq!(std::fs::read(root.join("tracked")).unwrap(), b"tracked\n");
}

#[cfg(feature = "failpoints")]
fn transaction_path(root: &Path) -> PathBuf {
    std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".grove-adopt-")
        })
        .expect("transaction directory")
}

#[cfg(feature = "failpoints")]
#[test]
fn corrupt_and_disagreeing_journals_are_preserved_for_inspection() {
    for kind in ["zero", "truncated", "trailing", "schema", "disagreement"] {
        let sandbox = Sandbox::new();
        let root = flat_repository(&sandbox, kind);
        injected(&sandbox, &root, "error", 5);
        let transaction = transaction_path(&root);
        let current = transaction.join("journal.json");
        let original = std::fs::read(&current).unwrap();
        let (evidence, expected) = match kind {
            "zero" => (current.clone(), Vec::new()),
            "truncated" => (current.clone(), original[..original.len() / 2].to_vec()),
            "trailing" => {
                let mut bytes = original.clone();
                bytes.extend_from_slice(b" owner-trailing");
                (current.clone(), bytes)
            }
            "schema" => {
                let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
                value["schema"] = serde_json::Value::from(99);
                (current.clone(), serde_json::to_vec(&value).unwrap())
            }
            "disagreement" => (transaction.join("journal.json.new"), original.clone()),
            _ => unreachable!(),
        };
        std::fs::write(&evidence, &expected).unwrap();
        sandbox
            .grove_in(
                sandbox.root(),
                &["adopt", "--continue", root.to_str().unwrap()],
            )
            .assert()
            .code(2);
        assert_eq!(std::fs::read(&evidence).unwrap(), expected, "{kind}");
        assert!(transaction.exists(), "{kind}");
    }
}

#[cfg(feature = "failpoints")]
#[test]
fn every_checkpoint_has_state_derived_continue_and_abort_outcomes() {
    use git_grove::transaction::recovery::{inspect_region, RecoveryRegion};

    let count_sandbox = Sandbox::new();
    let count_root = flat_repository(&count_sandbox, "count");
    let count = count_sandbox
        .grove_in(
            count_sandbox.root(),
            &["adopt", count_root.to_str().unwrap()],
        )
        .env("GIT_GROVE_FAILPOINT", "count")
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    let count = String::from_utf8(count)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("git-grove: failpoint checkpoints: "))
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!(count > 0);

    for kind in ["error", "kill"] {
        let sandbox = Sandbox::new();
        let mut regions = Vec::new();
        for checkpoint in 1..=count {
            let continue_root = flat_repository(&sandbox, &format!("{kind}-c-{checkpoint}"));
            let output = injected(&sandbox, &continue_root, kind, checkpoint);
            if kind == "kill" {
                assert_eq!(output.status.signal(), Some(9), "checkpoint {checkpoint}");
            }
            let region = inspect_region(&continue_root).unwrap();
            regions.push(region);
            if kind == "error" {
                let expected = if region == RecoveryRegion::None { 1 } else { 2 };
                assert_eq!(
                    output.status.code(),
                    Some(expected),
                    "checkpoint {checkpoint}"
                );
                let stderr = String::from_utf8_lossy(&output.stderr);
                if region == RecoveryRegion::None {
                    assert!(!stderr.contains("adopt --continue"), "{stderr}");
                } else {
                    assert!(stderr.contains("adopt --continue"), "{stderr}");
                    assert!(stderr.contains("adopt --abort"), "{stderr}");
                }
            }
            match region {
                RecoveryRegion::Forward | RecoveryRegion::Committed => {
                    sandbox
                        .grove_in(
                            sandbox.root(),
                            &["adopt", "--continue", continue_root.to_str().unwrap()],
                        )
                        .assert()
                        .success();
                    assert_final(&continue_root);
                }
                RecoveryRegion::None => {
                    assert_final(&continue_root);
                    sandbox
                        .grove_in(
                            sandbox.root(),
                            &["adopt", "--continue", continue_root.to_str().unwrap()],
                        )
                        .assert()
                        .code(1);
                }
            }

            let abort_root = flat_repository(&sandbox, &format!("{kind}-a-{checkpoint}"));
            let output = injected(&sandbox, &abort_root, kind, checkpoint);
            if kind == "kill" {
                assert_eq!(output.status.signal(), Some(9), "checkpoint {checkpoint}");
            }
            let abort_region = inspect_region(&abort_root).unwrap();
            assert_eq!(abort_region, region, "checkpoint {checkpoint}");
            match abort_region {
                RecoveryRegion::Forward => {
                    sandbox
                        .grove_in(
                            sandbox.root(),
                            &["adopt", "--abort", abort_root.to_str().unwrap()],
                        )
                        .assert()
                        .success();
                    assert_original(&abort_root);
                }
                RecoveryRegion::Committed => {
                    sandbox
                        .grove_in(
                            sandbox.root(),
                            &["adopt", "--abort", abort_root.to_str().unwrap()],
                        )
                        .assert()
                        .code(2);
                    sandbox
                        .grove_in(
                            sandbox.root(),
                            &["adopt", "--continue", abort_root.to_str().unwrap()],
                        )
                        .assert()
                        .success();
                    assert_final(&abort_root);
                }
                RecoveryRegion::None => {
                    assert_final(&abort_root);
                    sandbox
                        .grove_in(
                            sandbox.root(),
                            &["adopt", "--abort", abort_root.to_str().unwrap()],
                        )
                        .assert()
                        .code(1);
                }
            }
        }
        assert!(regions.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(regions.contains(&RecoveryRegion::Forward));
        assert!(regions.contains(&RecoveryRegion::Committed));
        assert!(regions.contains(&RecoveryRegion::None));
        assert_eq!(
            regions.windows(2).filter(|pair| pair[0] != pair[1]).count(),
            2
        );
    }
}

#[cfg(feature = "failpoints")]
#[test]
fn recovery_refuses_manual_edits_to_generated_state() {
    let sandbox = Sandbox::new();
    let guide_root = flat_repository(&sandbox, "edited-guide");
    injected(&sandbox, &guide_root, "error", 40);
    std::fs::write(guide_root.join("AGENTS.md"), b"owner edit\n").unwrap();
    sandbox
        .grove_in(
            sandbox.root(),
            &["adopt", "--abort", guide_root.to_str().unwrap()],
        )
        .assert()
        .code(2)
        .stderr(predicates::str::contains(
            "neither its exact before nor after",
        ));
    assert_eq!(
        std::fs::read(guide_root.join("AGENTS.md")).unwrap(),
        b"owner edit\n"
    );

    let default_root = flat_repository(&sandbox, "edited-default");
    sandbox.git(&default_root, &["branch", "topic"]);
    sandbox.git(&default_root, &["switch", "--quiet", "topic"]);
    sandbox
        .grove_in(
            sandbox.root(),
            &[
                "adopt",
                "--default-branch",
                "main",
                default_root.to_str().unwrap(),
            ],
        )
        .env("GIT_GROVE_FAILPOINT", "error:35")
        .assert()
        .code(2);
    std::fs::write(default_root.join("main/owner-file"), b"preserve\n").unwrap();
    sandbox
        .grove_in(
            sandbox.root(),
            &["adopt", "--abort", default_root.to_str().unwrap()],
        )
        .assert()
        .code(2)
        .stderr(predicates::str::contains("modified; refusing rollback"));
    assert_eq!(
        std::fs::read(default_root.join("main/owner-file")).unwrap(),
        b"preserve\n"
    );

    let pointer_root = flat_repository(&sandbox, "edited-pointer");
    injected(&sandbox, &pointer_root, "error", 29);
    std::fs::write(pointer_root.join("main/.git"), b"owner pointer\n").unwrap();
    sandbox
        .grove_in(
            sandbox.root(),
            &["adopt", "--abort", pointer_root.to_str().unwrap()],
        )
        .assert()
        .code(2)
        .stderr(predicates::str::contains("pointer was modified"));
    assert_eq!(
        std::fs::read(pointer_root.join("main/.git")).unwrap(),
        b"owner pointer\n"
    );

    let deleted_root = flat_repository(&sandbox, "deleted-payload");
    injected(&sandbox, &deleted_root, "error", 29);
    std::fs::remove_file(deleted_root.join("main/tracked")).unwrap();
    sandbox
        .grove_in(
            sandbox.root(),
            &["adopt", "--abort", deleted_root.to_str().unwrap()],
        )
        .assert()
        .code(2)
        .stderr(predicates::str::contains("payload entry set changed"));
    assert!(!deleted_root.join("main/tracked").exists());
}
