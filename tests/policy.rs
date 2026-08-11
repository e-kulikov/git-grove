mod harness;
use harness::Sandbox;

#[test]
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
    assert!(!sandbox.root().join("g").exists());
}

#[test]
fn override_warns_and_strips_redirecting_environment_from_clone() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("override");
    let decoy = sandbox.root().join("decoy.config");
    std::fs::write(&decoy, b"[core]\n\tbare = false\n").unwrap();

    sandbox
        .grove(&[
            "--ignore-unsupported",
            "clone",
            origin.to_str().unwrap(),
            "g",
        ])
        .env("GIT_CONFIG", &decoy)
        .assert()
        .success()
        .stderr(predicates::str::contains("warning"))
        .stderr(predicates::str::contains("GIT_CONFIG"));

    assert!(sandbox.root().join("g/.bare").is_dir());
    assert_eq!(
        std::fs::read_to_string(&decoy).unwrap(),
        "[core]\n\tbare = false\n"
    );
}

#[test]
fn refuses_an_abbreviated_layout_breaking_clone_option() {
    let sandbox = Sandbox::new();
    let origin = sandbox.bare_origin("o");
    sandbox
        .grove(&["clone", origin.to_str().unwrap(), "g", "--", "--mirr"])
        .assert()
        .code(64)
        .stderr(predicates::str::contains("mirror"));
}
