mod harness;
use harness::Sandbox;

#[test]
fn creates_the_layout_and_the_first_worktree() {
    let sandbox = Sandbox::new();
    sandbox
        .grove(&["init", "fresh", "--branch", "main"])
        .assert()
        .success();

    let root = sandbox.root().join("fresh");
    assert!(root.join(".bare").is_dir(), "bare repository missing");
    assert_eq!(
        std::fs::read_to_string(root.join(".git")).unwrap(),
        "gitdir: ./.bare\n"
    );
    assert!(root.join("AGENTS.md").is_file());
    assert_eq!(
        std::fs::read_link(root.join("CLAUDE.md"))
            .unwrap()
            .to_str()
            .unwrap(),
        "AGENTS.md"
    );
    assert!(root.join("main").is_dir(), "first worktree missing");

    let head = sandbox.git(&root.join("main"), &["symbolic-ref", "--short", "HEAD"]);
    assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "main");

    let state = sandbox.git(
        &root,
        &[
            "config",
            "--file",
            root.join(".bare/config").to_str().unwrap(),
            "--get",
            "grove.publishState",
        ],
    );
    assert_eq!(String::from_utf8_lossy(&state.stdout).trim(), "unpublished");
}

#[test]
fn refuses_a_directory_that_holds_files() {
    let sandbox = Sandbox::new();
    let occupied = sandbox.root().join("occupied");
    std::fs::create_dir(&occupied).unwrap();
    std::fs::write(occupied.join("notes.txt"), b"x").unwrap();

    sandbox
        .grove(&["init", "occupied"])
        .assert()
        .code(64)
        .stderr(predicates::str::contains("adopt"));
}

#[test]
fn refuses_unsafe_environment_before_creating_the_root() {
    let sandbox = Sandbox::new();

    sandbox
        .grove(&["init", "fresh"])
        .env("GIT_DIR", "/tmp/redirected.git")
        .assert()
        .code(64)
        .stderr(predicates::str::contains("GIT_DIR"))
        .stderr(predicates::str::contains("--ignore-unsupported"));

    assert!(!sandbox.root().join("fresh").exists());
}

#[test]
fn non_terminal_override_warns_and_sanitizes_git() {
    let sandbox = Sandbox::new();

    sandbox
        .grove(&["--ignore-unsupported", "init", "fresh", "--branch", "main"])
        .env("GIT_DIR", "/tmp/redirected.git")
        .assert()
        .success()
        .stderr(predicates::str::contains("warning"))
        .stderr(predicates::str::contains("removed from git's environment"));

    assert!(sandbox.root().join("fresh/.bare").is_dir());
}

#[test]
fn refuses_an_old_git_before_creating_the_root() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new();
    let bin = sandbox.root().join("old-bin");
    std::fs::create_dir(&bin).unwrap();
    let git = bin.join("git");
    std::fs::write(&git, "#!/bin/sh\nprintf 'git version 2.46.9\\n'\n").unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();

    sandbox
        .grove(&["init", "fresh"])
        .env("PATH", &bin)
        .assert()
        .code(64)
        .stderr(predicates::str::contains("git 2.47 or newer is required"));

    assert!(!sandbox.root().join("fresh").exists());
}

#[test]
fn uses_the_default_branch_selected_by_git_when_branch_is_absent() {
    let sandbox = Sandbox::new();
    let git_config = sandbox.root().join("gitconfig");
    std::fs::write(&git_config, "[init]\n\tdefaultBranch = trunk\n").unwrap();

    sandbox
        .grove(&["init", "fresh"])
        .env("GIT_CONFIG_GLOBAL", &git_config)
        .assert()
        .success();

    assert!(sandbox.root().join("fresh/trunk").is_dir());
    let head = sandbox.git(
        &sandbox.root().join("fresh/trunk"),
        &["symbolic-ref", "--short", "HEAD"],
    );
    assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "trunk");
}

#[cfg(unix)]
#[test]
fn preserves_non_utf8_branch_bytes() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let sandbox = Sandbox::new();
    let branch = OsString::from_vec(b"topic-\xff".to_vec());
    sandbox
        .grove(&["init", "fresh", "--branch"])
        .arg(&branch)
        .assert()
        .success();

    let worktree = sandbox.root().join("fresh").join(&branch);
    assert!(worktree.is_dir());
    let head = std::process::Command::new("git")
        .current_dir(&worktree)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .unwrap();
    assert!(head.status.success());
    assert_eq!(head.stdout, b"topic-\xff\n");
}

#[cfg(unix)]
#[test]
fn unrelated_non_utf8_environment_does_not_bypass_error_rendering() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let sandbox = Sandbox::new();
    sandbox
        .grove(&["init", "fresh", "--branch", "main"])
        .env(
            OsString::from_vec(b"UNRELATED_\xff".to_vec()),
            OsString::from_vec(b"value-\xfe".to_vec()),
        )
        .assert()
        .success();
}

fn failing_git_shim(sandbox: &Sandbox, foreign: Option<&std::path::Path>) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let bin = sandbox.root().join("failing-bin");
    std::fs::create_dir(&bin).unwrap();
    let git = bin.join("git");
    let foreign_write = foreign
        .map(|path| format!("printf foreign > '{}'\n", path.display()))
        .unwrap_or_default();
    std::fs::write(
        &git,
        format!(
            "#!/bin/sh\ncase \"$*\" in\n  *'symbolic-ref --short HEAD'*)\n    {foreign_write}    printf 'injected failure\\n' >&2\n    exit 9\n    ;;\nesac\nexec /usr/bin/git \"$@\"\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

#[test]
fn failure_retains_partial_state_with_the_exact_recovery_path() {
    let sandbox = Sandbox::new();
    let root = sandbox.root().join("fresh");
    let bin = failing_git_shim(&sandbox, None);
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").expect("PATH must be set")
    );

    sandbox
        .grove(&["init", "fresh"])
        .env("PATH", path)
        .assert()
        .code(1)
        .stderr(predicates::str::contains("injected failure"))
        .stderr(predicates::str::contains(format!(
            "partial initialization retained at {}",
            root.display()
        )));

    assert!(root.join(".bare").is_dir());
    assert!(root.join(".git").is_file());
}

#[test]
fn failure_preserves_a_foreign_entry_created_concurrently() {
    let sandbox = Sandbox::new();
    let foreign = sandbox.root().join("fresh/foreign.txt");
    let bin = failing_git_shim(&sandbox, Some(&foreign));
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").expect("PATH must be set")
    );

    sandbox
        .grove(&["init", "fresh"])
        .env("PATH", path)
        .assert()
        .code(1);

    assert_eq!(std::fs::read(&foreign).unwrap(), b"foreign");
    assert!(sandbox.root().join("fresh/.bare").is_dir());
    assert!(sandbox.root().join("fresh/.git").is_file());
}

#[test]
fn cleanup_does_not_claim_a_foreign_entry_created_while_git_runs() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new();
    let foreign = sandbox.root().join("fresh/.bare/foreign.txt");
    let bin = sandbox.root().join("concurrent-bin");
    std::fs::create_dir(&bin).unwrap();
    let git = bin.join("git");
    std::fs::write(
        &git,
        "#!/bin/sh\ncase \"$1\" in\n  init) /usr/bin/git \"$@\" || exit $?; printf foreign > \"$GROVE_FOREIGN\"; exit 0;;\nesac\ncase \"$*\" in\n  *'symbolic-ref --short HEAD'*) exit 9;;\nesac\nexec /usr/bin/git \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").expect("PATH must be set")
    );

    sandbox
        .grove(&["init", "fresh"])
        .env("PATH", path)
        .env("GROVE_FOREIGN", &foreign)
        .assert()
        .code(1);

    assert_eq!(std::fs::read(&foreign).unwrap(), b"foreign");
}

#[test]
fn worktree_failure_retains_nested_parent_directories_for_recovery() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new();
    let bin = sandbox.root().join("worktree-failing-bin");
    std::fs::create_dir(&bin).unwrap();
    let git = bin.join("git");
    std::fs::write(
        &git,
        "#!/bin/sh\ncase \"$*\" in\n  *'worktree add'*) printf 'worktree failure\\n' >&2; exit 9;;\nesac\nexec /usr/bin/git \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").expect("PATH must be set")
    );

    sandbox
        .grove(&["init", "fresh", "--branch", "release/one"])
        .env("PATH", path)
        .assert()
        .code(1)
        .stderr(predicates::str::contains("worktree failure"))
        .stderr(predicates::str::contains(format!(
            "partial initialization retained at {}",
            sandbox.root().join("fresh").display()
        )));

    assert!(sandbox.root().join("fresh/release").is_dir());
}

#[test]
fn failure_retains_a_nested_root_for_recovery() {
    let sandbox = Sandbox::new();
    let bin = failing_git_shim(&sandbox, None);
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").expect("PATH must be set")
    );

    sandbox
        .grove(&["init", "nested/fresh"])
        .env("PATH", path)
        .assert()
        .code(1)
        .stderr(predicates::str::contains(format!(
            "partial initialization retained at {}",
            sandbox.root().join("nested/fresh").display()
        )));

    assert!(sandbox.root().join("nested/fresh/.bare").is_dir());
}

#[test]
fn a_failing_git_init_retains_its_partial_repository_for_recovery() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new();
    let root = sandbox.root().join("fresh");
    let bin = sandbox.root().join("init-failing-bin");
    std::fs::create_dir(&bin).unwrap();
    let git = bin.join("git");
    std::fs::write(
        &git,
        "#!/bin/sh\ncase \"$1\" in\n  init) /usr/bin/git \"$@\" || exit $?; printf 'init failure\\n' >&2; exit 9;;\nesac\nexec /usr/bin/git \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").expect("PATH must be set")
    );

    sandbox
        .grove(&["init", "fresh"])
        .env("PATH", path)
        .assert()
        .code(1)
        .stderr(predicates::str::contains("init failure"))
        .stderr(predicates::str::contains(format!(
            "partial initialization retained at {}",
            root.display()
        )));

    assert!(root.join(".bare").is_dir());
}
