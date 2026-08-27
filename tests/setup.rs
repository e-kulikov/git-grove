mod harness;
use harness::Sandbox;
use predicates::prelude::PredicateBooleanExt;
use std::path::{Path, PathBuf};

fn grove(sandbox: &Sandbox) -> (PathBuf, PathBuf) {
    sandbox
        .grove(&["init", "grove", "--branch", "main"])
        .assert()
        .success();
    let root = sandbox.root().join("grove");
    (root.clone(), root.join("main"))
}

fn read(path: &Path) -> serde_json::Value {
    let bytes = std::fs::read(path).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn happy_path_for_every_agent() {
    for (agent, relative) in [
        ("claude", ".claude/settings.local.json"),
        ("codex", ".codex/hooks.json"),
        ("copilot", ".claude/settings.local.json"),
    ] {
        let sandbox = Sandbox::new();
        let (_root, worktree) = grove(&sandbox);
        sandbox
            .grove_in(&worktree, &["setup", "--agent", agent])
            .assert()
            .success();
        let value = read(&worktree.join(relative));
        assert!(value["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("hook-guard"));
    }
}

#[test]
fn writes_are_invisible_to_git_status() {
    let sandbox = Sandbox::new();
    let (_root, worktree) = grove(&sandbox);
    sandbox
        .grove_in(&worktree, &["setup", "--agent", "claude"])
        .assert()
        .success();
    sandbox
        .grove_in(&worktree, &["setup", "--agent", "codex"])
        .assert()
        .success();
    let status = sandbox.git(&worktree, &["status", "--porcelain"]);
    assert!(
        status.stdout.is_empty(),
        "expected clean status, got {:?}",
        status.stdout
    );
}

#[test]
fn rerun_is_byte_stable_and_claude_then_copilot_converge() {
    let sandbox = Sandbox::new();
    let (_root, worktree) = grove(&sandbox);
    let target = worktree.join(".claude/settings.local.json");

    sandbox
        .grove_in(&worktree, &["setup", "--agent", "claude"])
        .assert()
        .success();
    let after_claude = std::fs::read(&target).unwrap();

    sandbox
        .grove_in(&worktree, &["setup", "--agent", "copilot"])
        .assert()
        .success();
    let after_copilot = std::fs::read(&target).unwrap();
    assert_eq!(after_claude, after_copilot);

    sandbox
        .grove_in(&worktree, &["setup", "--agent", "claude"])
        .assert()
        .success();
    assert_eq!(std::fs::read(&target).unwrap(), after_copilot);
}

#[test]
fn two_worktrees_receive_separate_configs_and_share_one_exclude_entry() {
    let sandbox = Sandbox::new();
    let (root, main) = grove(&sandbox);
    sandbox
        .grove_in(&root, &["add", "topic"])
        .assert()
        .success();
    let topic = root.join("topic");

    sandbox
        .grove_in(&main, &["setup", "--agent", "claude"])
        .assert()
        .success();
    sandbox
        .grove_in(&topic, &["setup", "--agent", "codex"])
        .assert()
        .success();

    assert!(main.join(".claude/settings.local.json").is_file());
    assert!(!topic.join(".claude/settings.local.json").exists());
    assert!(topic.join(".codex/hooks.json").is_file());
    assert!(!main.join(".codex/hooks.json").exists());

    let exclude_path = root.join(".bare").join("info").join("exclude");
    let exclude = std::fs::read_to_string(&exclude_path).unwrap();
    assert!(exclude.contains("/.claude/settings.local.json"));
    assert!(exclude.contains("/.codex/hooks.json"));

    let status_main = sandbox.git(&main, &["status", "--porcelain"]);
    assert!(status_main.stdout.is_empty());
    let status_topic = sandbox.git(&topic, &["status", "--porcelain"]);
    assert!(status_topic.stdout.is_empty());
}

#[test]
fn refuses_an_exact_tracked_collision_without_writing() {
    let sandbox = Sandbox::new();
    let (_root, worktree) = grove(&sandbox);
    std::fs::create_dir(worktree.join(".claude")).unwrap();
    std::fs::write(
        worktree.join(".claude/settings.local.json"),
        b"{\"tracked\":true}",
    )
    .unwrap();
    sandbox.git(&worktree, &["add", "-f", ".claude/settings.local.json"]);

    sandbox
        .grove_in(&worktree, &["setup", "--agent", "claude"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("already tracked"));

    assert_eq!(
        std::fs::read(worktree.join(".claude/settings.local.json")).unwrap(),
        b"{\"tracked\":true}"
    );
}

#[test]
fn a_neighboring_tracked_file_is_unaffected() {
    let sandbox = Sandbox::new();
    let (_root, worktree) = grove(&sandbox);
    std::fs::create_dir(worktree.join(".claude")).unwrap();
    std::fs::write(worktree.join(".claude/other.json"), b"{}").unwrap();
    sandbox.git(&worktree, &["add", "-f", ".claude/other.json"]);

    sandbox
        .grove_in(&worktree, &["setup", "--agent", "claude"])
        .assert()
        .success();
    assert!(worktree.join(".claude/settings.local.json").is_file());
}

#[test]
fn refuses_malformed_existing_json_without_writing() {
    let sandbox = Sandbox::new();
    let (_root, worktree) = grove(&sandbox);
    std::fs::create_dir(worktree.join(".claude")).unwrap();
    std::fs::write(worktree.join(".claude/settings.local.json"), b"not json").unwrap();

    sandbox
        .grove_in(&worktree, &["setup", "--agent", "claude"])
        .assert()
        .code(2);

    assert_eq!(
        std::fs::read(worktree.join(".claude/settings.local.json")).unwrap(),
        b"not json"
    );
}

#[test]
fn preserves_unrelated_settings_and_events() {
    let sandbox = Sandbox::new();
    let (_root, worktree) = grove(&sandbox);
    std::fs::create_dir(worktree.join(".claude")).unwrap();
    std::fs::write(
        worktree.join(".claude/settings.local.json"),
        br#"{"someOtherSetting": true, "hooks": {"SessionStart": [{"unrelated": true}]}}"#,
    )
    .unwrap();

    sandbox
        .grove_in(&worktree, &["setup", "--agent", "claude"])
        .assert()
        .success();

    let value = read(&worktree.join(".claude/settings.local.json"));
    assert_eq!(value["someOtherSetting"], true);
    assert_eq!(value["hooks"]["SessionStart"][0]["unrelated"], true);
}

#[cfg(unix)]
#[test]
fn refuses_a_symlinked_parent_directory_without_writing() {
    let sandbox = Sandbox::new();
    let (root, worktree) = grove(&sandbox);
    let elsewhere = root.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, worktree.join(".claude")).unwrap();

    sandbox
        .grove_in(&worktree, &["setup", "--agent", "claude"])
        .assert()
        .code(2);

    assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());
}

#[test]
fn refuses_at_the_grove_root() {
    let sandbox = Sandbox::new();
    let (root, _worktree) = grove(&sandbox);
    sandbox
        .grove_in(&root, &["setup", "--agent", "claude"])
        .assert()
        .code(64);
}

#[test]
fn refuses_outside_any_grove() {
    let sandbox = Sandbox::new();
    sandbox
        .grove_in(sandbox.root(), &["setup", "--agent", "claude"])
        .assert()
        .code(64);
}

#[test]
fn help_and_completions_list_all_three_agent_values_and_hide_hook_guard() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["setup", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("claude"))
        .stdout(predicates::str::contains("codex"))
        .stdout(predicates::str::contains("copilot"))
        .stdout(predicates::str::contains("hook-guard").not());
    for shell in ["zsh", "bash", "fish"] {
        sandbox
            .grove(&["completion", shell])
            .assert()
            .success()
            .stdout(predicates::str::contains("setup"))
            .stdout(predicates::str::contains("claude"))
            .stdout(predicates::str::contains("codex"))
            .stdout(predicates::str::contains("copilot"));
    }
}

#[cfg(feature = "failpoints")]
#[test]
fn a_crash_between_the_exclude_and_config_writes_heals_on_rerun() {
    let sandbox = Sandbox::new();
    let (root, worktree) = grove(&sandbox);

    sandbox
        .grove_in(&worktree, &["setup", "--agent", "claude"])
        .env("GIT_GROVE_FAILPOINT", "error:1")
        .assert()
        .failure();

    let exclude_path = root.join(".bare").join("info").join("exclude");
    let exclude = std::fs::read_to_string(&exclude_path).unwrap();
    assert!(exclude.contains("/.claude/settings.local.json"));
    assert!(!worktree.join(".claude/settings.local.json").exists());

    sandbox
        .grove_in(&worktree, &["setup", "--agent", "claude"])
        .assert()
        .success();
    assert!(worktree.join(".claude/settings.local.json").is_file());
    let exclude_after = std::fs::read_to_string(&exclude_path).unwrap();
    assert_eq!(exclude_after, exclude);
}
