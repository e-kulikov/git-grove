mod harness;
use harness::Sandbox;
use predicates::prelude::PredicateBooleanExt;

/// The exact bytes `git grove --skill` must print, mirrored from the
/// compile-time embedded document so a content regression here fails loudly
/// rather than silently accepting whatever `src/skill.md` currently says.
const SKILL: &str = include_str!("../src/skill.md");

#[test]
fn skill_prints_the_embedded_document_and_touches_neither_policy_nor_git() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["--skill"])
        .env("GIT_DIR", "/elsewhere/.git")
        .env("PATH", "/nonexistent-bin-that-cannot-hold-a-git-executable")
        .assert()
        .success()
        .stdout(SKILL)
        .stderr("");
}

#[test]
fn skill_wins_over_a_parsed_command_and_ignores_other_global_flags() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["--skill", "list"])
        .assert()
        .success()
        .stdout(SKILL)
        .stderr("");
    sandbox
        .grove(&["--skill", "--ignore-unsupported"])
        .assert()
        .success()
        .stdout(SKILL)
        .stderr("");
}

#[test]
fn help_wins_over_skill() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["--skill", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Usage: git-grove"))
        .stdout(predicates::str::contains(SKILL).not());
}

#[test]
fn every_visible_subcommand_is_documented_in_the_skill() {
    use clap::CommandFactory;
    let command = git_grove::cli::Cli::command();
    for subcommand in command.get_subcommands() {
        if subcommand.is_hide_set() {
            continue;
        }
        assert!(
            SKILL.contains(subcommand.get_name()),
            "skill.md does not mention `{}`",
            subcommand.get_name()
        );
    }
}

#[test]
fn the_skill_never_smuggles_the_orchestration_only_convention() {
    assert!(!SKILL.contains("Work inside a worktree, never at the grove root"));
}

#[test]
fn the_skill_carries_no_per_grove_facts_or_version_stamp() {
    assert!(!SKILL.contains(env!("CARGO_PKG_VERSION")));
    assert!(!SKILL.to_lowercase().contains("narrow"));
}
