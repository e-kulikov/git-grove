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

fn read_toml(path: &Path) -> toml_edit::DocumentMut {
    std::fs::read_to_string(path).unwrap().parse().unwrap()
}

/// The command every configured hook must invoke, whichever file it lands in.
fn installed_command(worktree: &Path, agent: &str) -> String {
    match agent {
        "codex" => read_toml(&worktree.join(".codex/config.toml"))["hooks"]["PreToolUse"][0]
            ["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .to_string(),
        _ => read(&worktree.join(".claude/settings.local.json"))["hooks"]["PreToolUse"][0]["hooks"]
            [0]["command"]
            .as_str()
            .unwrap()
            .to_string(),
    }
}

fn policy(sandbox: &Sandbox, root: &Path) -> Vec<String> {
    let config = root.join(".bare").join("config");
    let output = sandbox.git_output(
        root,
        &[
            "config",
            "--file",
            config.to_str().unwrap(),
            "--get-all",
            "grove.hookAgent",
        ],
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn happy_path_for_every_agent() {
    for (agent, relative) in [
        ("claude", ".claude/settings.local.json"),
        ("codex", ".codex/config.toml"),
        ("copilot", ".claude/settings.local.json"),
    ] {
        let sandbox = Sandbox::new();
        let (_root, worktree) = grove(&sandbox);
        sandbox
            .grove_in(&worktree, &["setup", "--agent", agent])
            .assert()
            .success();
        assert!(worktree.join(relative).is_file());
        assert!(installed_command(&worktree, agent).contains("hook-guard"));
    }
}

/// The Codex config is one in-worktree TOML file with the group inline —
/// the exact shape measured working against installed Codex — and there is
/// no separate hooks file any more.
#[test]
fn codex_writes_one_inline_toml_config_and_no_hooks_json() {
    let sandbox = Sandbox::new();
    let (_root, worktree) = grove(&sandbox);
    sandbox
        .grove_in(&worktree, &["setup", "--agent", "codex"])
        .assert()
        .success();

    assert!(!worktree.join(".codex/hooks.json").exists());
    let document = read_toml(&worktree.join(".codex/config.toml"));
    assert_eq!(document["features"]["hooks"].as_bool(), Some(true));
    let entry = &document["hooks"]["PreToolUse"][0];
    assert_eq!(entry["_id"].as_str(), Some("git-grove.protect-metadata.v1"));
    assert_eq!(
        entry["matcher"].as_str(),
        Some("Bash|Edit|Write|apply_patch")
    );
    let handler = &entry["hooks"][0];
    assert_eq!(handler["type"].as_str(), Some("command"));
    assert!(handler["command"]
        .as_str()
        .unwrap()
        .contains("hook-guard --protocol codex PreToolUse"));
    assert_eq!(handler["timeout"].as_integer(), Some(15));
}

/// Codex's own `config.toml` legitimately carries settings that have
/// nothing to do with this tool. Merging must leave every one of them —
/// comments included — exactly as it found them.
#[test]
fn codex_setup_preserves_unrelated_toml_settings_and_comments() {
    let sandbox = Sandbox::new();
    let (_root, worktree) = grove(&sandbox);
    std::fs::create_dir(worktree.join(".codex")).unwrap();
    let target = worktree.join(".codex/config.toml");
    std::fs::write(
        &target,
        b"# my own note\nmodel = \"gpt-5\"\n\n[features]\nweb_search = true\n",
    )
    .unwrap();

    sandbox
        .grove_in(&worktree, &["setup", "--agent", "codex"])
        .assert()
        .success();

    let text = std::fs::read_to_string(&target).unwrap();
    assert!(text.contains("# my own note"), "got {text}");
    assert!(text.contains("model = \"gpt-5\""), "got {text}");
    let document = read_toml(&target);
    assert_eq!(document["features"]["web_search"].as_bool(), Some(true));
    assert_eq!(document["features"]["hooks"].as_bool(), Some(true));
}

#[test]
fn refuses_malformed_existing_toml_without_writing() {
    let sandbox = Sandbox::new();
    let (_root, worktree) = grove(&sandbox);
    std::fs::create_dir(worktree.join(".codex")).unwrap();
    let target = worktree.join(".codex/config.toml");
    std::fs::write(&target, b"this is not = = toml\n").unwrap();

    sandbox
        .grove_in(&worktree, &["setup", "--agent", "codex"])
        .assert()
        .code(2);

    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"this is not = = toml\n",
        "a refusal writes no bytes"
    );
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
fn a_codex_rerun_is_byte_stable_too() {
    let sandbox = Sandbox::new();
    let (_root, worktree) = grove(&sandbox);
    let target = worktree.join(".codex/config.toml");

    sandbox
        .grove_in(&worktree, &["setup", "--agent", "codex"])
        .assert()
        .success();
    let once = std::fs::read(&target).unwrap();
    sandbox
        .grove_in(&worktree, &["setup", "--agent", "codex"])
        .assert()
        .success();
    assert_eq!(std::fs::read(&target).unwrap(), once);
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
    assert!(topic.join(".codex/config.toml").is_file());
    assert!(!main.join(".codex/config.toml").exists());

    let exclude_path = root.join(".bare").join("info").join("exclude");
    let exclude = std::fs::read_to_string(&exclude_path).unwrap();
    assert!(exclude.contains("/.claude/settings.local.json"));
    assert!(exclude.contains("/.codex/config.toml"));

    let status_main = sandbox.git(&main, &["status", "--porcelain"]);
    assert!(status_main.stdout.is_empty());
    let status_topic = sandbox.git(&topic, &["status", "--porcelain"]);
    assert!(status_topic.stdout.is_empty());
}

// --- worktree targeting -------------------------------------------------

/// The grove root is not a working tree at all (`rev-parse
/// --show-toplevel` exits 128 in a bare repository), so this is the
/// fall-back path, not a refusal: `setup` targets the worktree holding the
/// grove's own recorded default branch and says which file it wrote.
#[test]
fn at_the_grove_root_the_default_branch_worktree_is_configured() {
    let sandbox = Sandbox::new();
    let (root, main) = grove(&sandbox);
    sandbox
        .grove_in(&root, &["add", "topic"])
        .assert()
        .success();

    sandbox
        .grove_in(&root, &["setup", "--agent", "claude"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            main.join(".claude/settings.local.json").to_str().unwrap(),
        ));

    assert!(main.join(".claude/settings.local.json").is_file());
    assert!(!root.join("topic/.claude").exists());
}

/// The fall-back is "the cwd resolves to no worktree of this grove", not
/// "the cwd is the grove root": an ordinary intermediate directory walks up
/// to the same bare repository.
#[test]
fn an_intermediate_directory_also_falls_back_to_the_default_branch_worktree() {
    let sandbox = Sandbox::new();
    let (root, main) = grove(&sandbox);
    let intermediate = root.join("scratch");
    std::fs::create_dir(&intermediate).unwrap();

    sandbox
        .grove_in(&intermediate, &["setup", "--agent", "claude"])
        .assert()
        .success();
    assert!(main.join(".claude/settings.local.json").is_file());
}

/// A grove initialized with a non-`main` default branch falls back to that
/// branch's worktree: the rule is the grove's own recorded default, never
/// the literal name `main`.
#[test]
fn the_fallback_follows_the_recorded_default_branch_not_the_name_main() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["init", "grove", "--branch", "trunk"])
        .assert()
        .success();
    let root = sandbox.root().join("grove");

    sandbox
        .grove_in(&root, &["setup", "--agent", "claude"])
        .assert()
        .success();
    assert!(root.join("trunk/.claude/settings.local.json").is_file());
}

#[test]
fn a_cwd_inside_a_worktree_configures_that_worktree() {
    let sandbox = Sandbox::new();
    let (root, main) = grove(&sandbox);
    sandbox
        .grove_in(&root, &["add", "feature/auth"])
        .assert()
        .success();
    let nested = root.join("feature/auth");
    std::fs::create_dir(nested.join("src")).unwrap();

    sandbox
        .grove_in(&nested.join("src"), &["setup", "--agent", "claude"])
        .assert()
        .success();
    assert!(nested.join(".claude/settings.local.json").is_file());
    assert!(!main.join(".claude").exists());
}

#[test]
fn an_explicit_worktree_overrides_the_cwd_in_either_direction() {
    let sandbox = Sandbox::new();
    let (root, main) = grove(&sandbox);
    sandbox
        .grove_in(&root, &["add", "feature/auth"])
        .assert()
        .success();
    let feature = root.join("feature/auth");

    // From the grove root, overriding the default-branch fall-back.
    sandbox
        .grove_in(
            &root,
            &["setup", "--agent", "claude", "--worktree", "feature/auth"],
        )
        .assert()
        .success();
    assert!(feature.join(".claude/settings.local.json").is_file());
    assert!(!main.join(".claude").exists());

    // From inside one worktree, naming another.
    sandbox
        .grove_in(
            &feature,
            &["setup", "--agent", "codex", "--worktree", "main"],
        )
        .assert()
        .success();
    assert!(main.join(".codex/config.toml").is_file());
    assert!(!feature.join(".codex").exists());
}

#[test]
fn an_unknown_worktree_name_is_a_usage_error_and_writes_nothing() {
    let sandbox = Sandbox::new();
    let (root, main) = grove(&sandbox);
    sandbox
        .grove_in(&root, &["setup", "--agent", "claude", "--worktree", "nope"])
        .assert()
        .code(64)
        .stderr(predicates::str::contains("not a worktree of this grove"));
    assert!(!main.join(".claude").exists());
}

#[test]
fn an_absolute_worktree_argument_is_refused() {
    let sandbox = Sandbox::new();
    let (root, _main) = grove(&sandbox);
    sandbox
        .grove_in(&root, &["setup", "--agent", "claude", "--worktree", "/etc"])
        .assert()
        .code(64);
}

/// A grove whose default branch is checked out nowhere is repository state
/// the user has to decide about (exit 2), not a malformed command line.
#[test]
fn no_default_branch_worktree_needs_a_decision_and_names_the_fix() {
    let sandbox = Sandbox::new();
    let (root, main) = grove(&sandbox);
    sandbox
        .grove_in(&root, &["add", "topic"])
        .assert()
        .success();
    sandbox.git(&root, &["worktree", "remove", main.to_str().unwrap()]);

    sandbox
        .grove_in(&root, &["setup", "--agent", "claude"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("not checked out in any worktree"))
        .stderr(predicates::str::contains("git grove add main"));
}

// --- project policy and `add` auto-provisioning -------------------------

#[test]
fn setup_records_the_agent_in_the_grove_policy_idempotently() {
    let sandbox = Sandbox::new();
    let (root, worktree) = grove(&sandbox);
    assert!(policy(&sandbox, &root).is_empty());

    sandbox
        .grove_in(&worktree, &["setup", "--agent", "claude"])
        .assert()
        .success()
        .stdout(predicates::str::contains("grove.hookAgent"));
    assert_eq!(policy(&sandbox, &root), vec!["claude".to_string()]);

    sandbox
        .grove_in(&worktree, &["setup", "--agent", "claude"])
        .assert()
        .success();
    assert_eq!(
        policy(&sandbox, &root),
        vec!["claude".to_string()],
        "a rerun must not grow the record"
    );

    sandbox
        .grove_in(&worktree, &["setup", "--agent", "codex"])
        .assert()
        .success();
    assert_eq!(
        policy(&sandbox, &root),
        vec!["claude".to_string(), "codex".to_string()]
    );
}

#[test]
fn add_provisions_every_recorded_agent_into_a_new_worktree() {
    let sandbox = Sandbox::new();
    let (root, main) = grove(&sandbox);
    sandbox
        .grove_in(&main, &["setup", "--agent", "claude"])
        .assert()
        .success();
    sandbox
        .grove_in(&main, &["setup", "--agent", "codex"])
        .assert()
        .success();

    sandbox
        .grove_in(&root, &["add", "topic"])
        .assert()
        .success()
        .stdout(predicates::str::contains("configured the claude hook"))
        .stdout(predicates::str::contains("configured the codex hook"));

    let topic = root.join("topic");
    assert!(topic.join(".claude/settings.local.json").is_file());
    assert!(topic.join(".codex/config.toml").is_file());
    assert!(installed_command(&topic, "claude").contains("hook-guard"));
    assert!(installed_command(&topic, "codex").contains("hook-guard"));

    let status = sandbox.git(&topic, &["status", "--porcelain"]);
    assert!(
        status.stdout.is_empty(),
        "provisioned files must be excluded, got {:?}",
        status.stdout
    );
}

#[test]
fn add_provisions_a_detached_worktree_too() {
    let sandbox = Sandbox::new();
    let (root, main) = grove(&sandbox);
    std::fs::write(main.join("file.txt"), b"x").unwrap();
    sandbox.git(&main, &["add", "file.txt"]);
    sandbox.git(&main, &["commit", "-m", "seed"]);
    sandbox
        .grove_in(&main, &["setup", "--agent", "codex"])
        .assert()
        .success();

    sandbox
        .grove_in(&root, &["add", "--detach", "main", "detached"])
        .assert()
        .success();
    assert!(root.join("detached/.codex/config.toml").is_file());
}

/// With no recorded policy — the case for everyone who has never run
/// `setup` — `add` behaves exactly as it always did and writes nothing.
#[test]
fn add_without_a_policy_configures_nothing() {
    let sandbox = Sandbox::new();
    let (root, _main) = grove(&sandbox);
    sandbox
        .grove_in(&root, &["add", "topic"])
        .assert()
        .success()
        .stdout(predicates::str::contains("configured the").not());
    let topic = root.join("topic");
    assert!(!topic.join(".claude").exists());
    assert!(!topic.join(".codex").exists());
}

/// A provisioning failure warns loudly and still leaves the worktree in
/// place with a zero exit: a worktree nobody can protect is better than no
/// worktree at all, provided the user is told.
#[test]
fn a_failed_provision_warns_loudly_without_failing_add() {
    let sandbox = Sandbox::new();
    let (root, main) = grove(&sandbox);
    sandbox
        .grove_in(&main, &["setup", "--agent", "claude"])
        .assert()
        .success();

    // A tracked collision on the branch the new worktree checks out: the
    // same refusal `setup` itself makes, reached through `add`.
    std::fs::create_dir_all(main.join(".claude")).unwrap();
    std::fs::write(main.join(".claude/settings.local.json"), b"{}").unwrap();
    sandbox.git(&main, &["add", "-f", ".claude/settings.local.json"]);
    sandbox.git(&main, &["commit", "-m", "track the hook config"]);

    sandbox
        .grove_in(&root, &["add", "topic", "--start", "main"])
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "could not configure the claude hook",
        ))
        .stderr(predicates::str::contains("git grove setup --agent claude"));

    assert!(root.join("topic").is_dir(), "the worktree still exists");
}

/// A policy value from a newer git-grove is reported and skipped, never
/// fatal, and never stops the agents this binary does know.
#[test]
fn an_unrecognized_policy_value_is_reported_and_skipped() {
    let sandbox = Sandbox::new();
    let (root, main) = grove(&sandbox);
    sandbox
        .grove_in(&main, &["setup", "--agent", "codex"])
        .assert()
        .success();
    let config = root.join(".bare").join("config");
    sandbox.git(
        &root,
        &[
            "config",
            "--file",
            config.to_str().unwrap(),
            "--add",
            "grove.hookAgent",
            "some-future-agent",
        ],
    );

    sandbox
        .grove_in(&root, &["add", "topic"])
        .assert()
        .success()
        .stderr(predicates::str::contains("some-future-agent"));
    assert!(root.join("topic/.codex/config.toml").is_file());
}

// --- refusals and safety, unchanged by the revised scope ----------------

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

/// The registration is a durable, grove-wide side effect of a per-worktree
/// command, so it must not outlive a failed write: an agent recorded here
/// would make every later `add` try to provision something the user never
/// successfully set up.
#[test]
fn a_refused_setup_records_no_policy() {
    let sandbox = Sandbox::new();
    let (root, worktree) = grove(&sandbox);
    std::fs::create_dir(worktree.join(".claude")).unwrap();
    std::fs::write(worktree.join(".claude/settings.local.json"), b"not json").unwrap();

    sandbox
        .grove_in(&worktree, &["setup", "--agent", "claude"])
        .assert()
        .code(2);
    assert!(policy(&sandbox, &root).is_empty());
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
        .stdout(predicates::str::contains("--worktree"))
        .stdout(predicates::str::contains("hook-guard").not());
    for shell in ["zsh", "bash", "fish"] {
        sandbox
            .grove(&["completion", shell])
            .assert()
            .success()
            .stdout(predicates::str::contains("setup"))
            .stdout(predicates::str::contains("claude"))
            .stdout(predicates::str::contains("codex"))
            .stdout(predicates::str::contains("copilot"))
            .stdout(predicates::str::contains("worktree"));
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
