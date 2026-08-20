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
