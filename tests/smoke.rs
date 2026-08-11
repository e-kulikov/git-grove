mod harness;
use harness::Sandbox;
use predicates::prelude::PredicateBooleanExt;

#[test]
fn reports_its_version() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["--version"])
        .assert()
        .success()
        .stdout(predicates::str::contains("git-grove 0.1.0"));
}

#[test]
fn reports_usage_errors_with_documented_exit_code_and_prefix() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["--definitely-invalid"])
        .assert()
        .code(64)
        .stderr(predicates::str::starts_with("git-grove:"));
}

#[test]
fn refuses_a_mistyped_subcommand_with_usage_code_and_clone_hint() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["clnoe"])
        .assert()
        .code(64)
        .stderr(predicates::str::contains(
            "neither a command nor a repository location",
        ))
        .stderr(predicates::str::contains("git grove clone <url>"));
}

#[test]
fn lists_only_the_supported_lifecycle_aliases_in_help() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["-h"])
        .assert()
        .success()
        .stdout(predicates::str::contains("plant=clone"))
        .stdout(predicates::str::contains("seed=init"))
        .stdout(predicates::str::contains("sprout=add"))
        .stdout(predicates::str::contains("survey=list"))
        .stdout(predicates::str::contains("transplant").not())
        .stdout(predicates::str::contains("tend=").not())
        .stdout(predicates::str::contains("propagate").not());
}

#[test]
fn global_help_limits_the_override_to_unsafe_git_environment() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "unsafe Git environment variables",
        ))
        .stdout(predicates::str::contains("incompatible options").not())
        .stdout(predicates::str::contains("unsupported platform").not())
        .stdout(predicates::str::contains("Git version").not());
}

#[test]
fn emits_runtime_zsh_completion() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["completion", "zsh"])
        .assert()
        .success()
        .stdout(predicates::str::contains("_git-grove"));
}

#[test]
fn defaults_to_list_only_when_inside_a_grove() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&[])
        .assert()
        .code(64)
        .stderr(predicates::str::contains("not inside a grove"));

    sandbox
        .grove(&["init", ".", "--branch", "main"])
        .assert()
        .success();
    sandbox
        .grove(&[])
        .assert()
        .success()
        .stdout(predicates::str::contains("UNBORN"));
}

#[test]
fn applies_implicit_actions_after_the_global_policy_override() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("implicit");
    sandbox
        .grove(&["--ignore-unsupported", origin.to_str().unwrap(), "cloned"])
        .assert()
        .success();
    assert!(sandbox.root().join("cloned/.bare").is_dir());

    sandbox
        .grove_in(
            &sandbox.root().join("cloned/main"),
            &["--ignore-unsupported"],
        )
        .assert()
        .success()
        .stdout(predicates::str::contains("main"));
}

#[test]
fn rejects_invalid_add_shapes_with_usage_code() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["add"])
        .assert()
        .code(64)
        .stderr(predicates::str::contains("requires a branch"));
    sandbox
        .grove(&["add", "--detach", "HEAD", "one", "two"])
        .assert()
        .code(64)
        .stderr(predicates::str::contains("accepts at most one directory"));
}

#[test]
fn add_help_describes_branch_and_detached_forms() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["add", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("add <branch> [dir]"))
        .stdout(predicates::str::contains("add --detach <revision> [dir]"))
        .stdout(predicates::str::contains(
            "at most two positional arguments",
        ));
}

#[test]
fn does_not_treat_an_empty_git_marker_as_an_existing_repository() {
    let sandbox = Sandbox::new();
    let candidate = sandbox.root().join("candidate");
    std::fs::create_dir(&candidate).unwrap();
    std::fs::write(candidate.join(".git"), "").unwrap();
    sandbox
        .grove(&["candidate"])
        .assert()
        .code(64)
        .stderr(predicates::str::contains(
            "neither a command nor a repository location",
        ));
}

#[test]
fn accepts_a_tilde_user_locator_as_an_implicit_clone() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["~other/src/repository"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("git clone --bare failed"));
}

#[test]
fn bare_origin_ignores_parent_git_configuration() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("origin");
    assert!(origin.join("refs/heads/main").exists());
    let output = sandbox.git(sandbox.root(), &["config", "--list"]);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("test.inherited"));
}
