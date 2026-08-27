mod harness;
use harness::Sandbox;
use predicates::prelude::PredicateBooleanExt;
use std::path::Path;

fn grove(sandbox: &Sandbox) -> (std::path::PathBuf, std::path::PathBuf) {
    sandbox
        .grove(&["init", "grove", "--branch", "main"])
        .assert()
        .success();
    let root = sandbox.root().join("grove");
    let worktree = root.join("main");
    (root, worktree)
}

fn run_hook_guard(
    sandbox: &Sandbox,
    cwd: &Path,
    protocol: &str,
    payload: &str,
) -> assert_cmd::assert::Assert {
    sandbox
        .grove_in(cwd, &["hook-guard", "--protocol", protocol, "PreToolUse"])
        .write_stdin(payload)
        .assert()
}

#[test]
fn denies_a_structured_write_whose_path_resolves_under_bare() {
    let sandbox = Sandbox::new();
    let (root, worktree) = grove(&sandbox);
    let target = root.join(".bare/git-grove-hook-probe");
    let payload = format!(
        r#"{{"tool_name":"Write","tool_input":{{"file_path":"{}"}}}}"#,
        target.to_str().unwrap()
    );
    run_hook_guard(&sandbox, &worktree, "claude-compatible", &payload)
        .success()
        .stderr("")
        .stdout(predicates::str::contains("\"permissionDecision\":\"deny\""));
}

#[test]
fn denies_an_edit_of_the_exact_root_git_pointer() {
    let sandbox = Sandbox::new();
    let (root, worktree) = grove(&sandbox);
    let payload = format!(
        r#"{{"tool_name":"Edit","tool_input":{{"file_path":"{}"}}}}"#,
        root.join(".git").to_str().unwrap()
    );
    run_hook_guard(&sandbox, &worktree, "claude-compatible", &payload)
        .success()
        .stdout(predicates::str::contains("\"permissionDecision\":\"deny\""));
}

#[test]
fn allows_an_ordinary_write_inside_the_worktree() {
    let sandbox = Sandbox::new();
    let (_root, worktree) = grove(&sandbox);
    let payload = format!(
        r#"{{"tool_name":"Write","tool_input":{{"file_path":"{}"}}}}"#,
        worktree.join("new-file.txt").to_str().unwrap()
    );
    run_hook_guard(&sandbox, &worktree, "claude-compatible", &payload)
        .success()
        .stdout("{}\n");
}

#[test]
fn denies_a_bash_command_reaching_into_bare_via_relative_cwd() {
    let sandbox = Sandbox::new();
    let (_root, worktree) = grove(&sandbox);
    let payload = r#"{"tool_name":"Bash","tool_input":{"command":"cat ../.bare/config"}}"#;
    run_hook_guard(&sandbox, &worktree, "claude-compatible", payload)
        .success()
        .stdout(predicates::str::contains("\"permissionDecision\":\"deny\""));
}

#[test]
fn denies_a_bash_command_reaching_into_bare_through_a_glued_option_value() {
    let sandbox = Sandbox::new();
    let (_root, worktree) = grove(&sandbox);
    let payload =
        r#"{"tool_name":"Bash","tool_input":{"command":"tool --output=../.bare/config"}}"#;
    run_hook_guard(&sandbox, &worktree, "claude-compatible", payload)
        .success()
        .stdout(predicates::str::contains("\"permissionDecision\":\"deny\""));
}

#[test]
fn allows_an_unrelated_bash_command() {
    let sandbox = Sandbox::new();
    let (_root, worktree) = grove(&sandbox);
    let payload = r#"{"tool_name":"Bash","tool_input":{"command":"git status"}}"#;
    run_hook_guard(&sandbox, &worktree, "claude-compatible", payload)
        .success()
        .stdout("{}\n");
}

#[test]
fn codex_denies_apply_patch_moving_into_bare() {
    let sandbox = Sandbox::new();
    let (root, worktree) = grove(&sandbox);
    let patch = format!(
        "*** Begin Patch\n*** Update File: notes.txt\n*** Move to: {}/config\n*** End Patch",
        root.join(".bare").to_str().unwrap()
    );
    let payload = serde_json::json!({
        "tool_name": "apply_patch",
        "tool_input": {"command": patch},
    })
    .to_string();
    run_hook_guard(&sandbox, &worktree, "codex", &payload)
        .success()
        .stdout(predicates::str::contains(
            "\"hookEventName\":\"PreToolUse\"",
        ))
        .stdout(predicates::str::contains("\"permissionDecision\":\"deny\""));
}

#[test]
fn a_malformed_payload_denies_closed_with_exactly_one_json_object_and_no_stderr() {
    let sandbox = Sandbox::new();
    let (_root, worktree) = grove(&sandbox);
    run_hook_guard(&sandbox, &worktree, "claude-compatible", "not json")
        .success()
        .stderr("")
        .stdout(predicates::str::contains("\"permissionDecision\":\"deny\""));
}

#[test]
fn a_non_grove_cwd_denies_closed_with_an_actionable_reason() {
    let sandbox = Sandbox::new();
    let payload = r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#;
    run_hook_guard(&sandbox, sandbox.root(), "codex", payload)
        .success()
        .stdout(predicates::str::contains("not inside a grove"));
}

#[test]
fn hook_guard_is_hidden_from_help_and_normalization_still_knows_it() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("hook-guard").not());
    // `hook-guard` in KNOWN keeps normalize from mistaking it for a clone
    // locator; a missing --protocol is then clap's own usage error, not
    // "neither a command nor a repository location".
    sandbox
        .grove(&["hook-guard"])
        .assert()
        .code(64)
        .stderr(predicates::str::contains("neither a command nor a repository location").not());
}

#[test]
fn hook_guard_leaks_into_generated_completions_a_measured_clap_complete_limitation() {
    // `#[command(hide = true)]` removes hook-guard from --help (asserted
    // above) but not from clap_complete 4.6.9's generated scripts, on any
    // of the three shells: the completion generator must structurally know
    // about every registered subcommand to emit valid dispatch code, and
    // this version does not additionally filter hidden ones out of the
    // offered-word lists. Measured, not a regression to chase — this is the
    // "except where clap structurally must know it" carve-out the plan
    // itself names for exactly this situation (Task 8's setup wiring).
    let sandbox = Sandbox::new();
    for shell in ["zsh", "bash", "fish"] {
        sandbox
            .grove(&["completion", shell])
            .assert()
            .success()
            .stdout(predicates::str::contains("hook-guard"));
    }
}
