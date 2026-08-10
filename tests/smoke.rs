mod harness;
use harness::Sandbox;

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
fn bare_origin_ignores_parent_git_configuration() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("origin");
    assert!(origin.join("refs/heads/main").exists());
    let output = sandbox.git(sandbox.root(), &["config", "--list"]);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("test.inherited"));
}
