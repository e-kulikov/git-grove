mod harness;
use harness::Sandbox;

#[test]
#[ignore = "Task 15 wires clone dispatch through the policy gate"]
fn refuses_a_redirecting_environment_variable() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("o");
    sandbox
        .grove(&["clone", origin.to_str().unwrap(), "g"])
        .env("GIT_CONFIG", "/tmp/decoy.config")
        .assert()
        .code(64)
        .stderr(predicates::str::contains("GIT_CONFIG"))
        .stderr(predicates::str::contains("--ignore-unsupported"));
}

#[test]
#[ignore = "Task 15 wires clone dispatch through forwarded-option classification"]
fn refuses_an_abbreviated_layout_breaking_clone_option() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("o");
    sandbox
        .grove(&["clone", origin.to_str().unwrap(), "g", "--", "--mirr"])
        .assert()
        .code(64)
        .stderr(predicates::str::contains("mirror"));
}
