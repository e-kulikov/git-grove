//! `publish --create` end to end, against a faked `gh`/`glab` on `PATH`.
//!
//! No test here ever runs a real `gh`/`glab`, and no test creates a real
//! repository on any hosting provider — every fake is a small shell script
//! dispatching on argv, built directly from the field names the plan's
//! *Measurement provenance* section documents. Where a `repo view` result
//! must point at something `git ls-remote`/`push` can actually reach, the
//! fake's canned URL names a real *local* bare repository this test created
//! with `git init --bare`.

mod harness;

use harness::Sandbox;
use std::path::{Path, PathBuf};

/// A grove created by `init`, with one commit on its default branch —
/// exactly `tests/publish.rs`'s own fixture, duplicated here rather than
/// shared across integration-test binaries (each compiles as its own crate).
fn grove_with_a_commit(sandbox: &Sandbox, name: &str, branch: &str) -> PathBuf {
    sandbox
        .grove(&["init", name, "--branch", branch])
        .assert()
        .success();
    let root = sandbox.root().join(name);
    let worktree = root.join(branch);
    std::fs::write(worktree.join("GROVE.md"), format!("grove {name}\n")).unwrap();
    sandbox.git(&worktree, &["add", "GROVE.md"]);
    sandbox.git(&worktree, &["commit", "--quiet", "-m", "grove seed"]);
    root
}

fn bare_of(root: &Path) -> PathBuf {
    root.join(".bare")
}

fn config_of(sandbox: &Sandbox, root: &Path, key: &str) -> Option<String> {
    sandbox.repo_config(&bare_of(root), key)
}

fn set_config(sandbox: &Sandbox, root: &Path, key: &str, value: &str) {
    sandbox.set_repo_config(&bare_of(root), key, value);
}

/// Quote `path` as a single-quoted `sh` word.
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// The logging preamble every fake provider script starts with: the full
/// argv, the `PWD`-relative working directory, and the presence/value of
/// every environment variable Decision 5 pins or forbids, appended to `log`.
///
/// `GH_TOKEN`/`GITLAB_TOKEN` are logged as `set`/`unset` only, never their
/// raw value: if a developer or CI happened to have a real token in the
/// ambient environment, this must never end up written to a log file (or a
/// test's own assertion-failure output) verbatim, regardless of whether
/// today's `Sandbox::env_clear()` discipline already keeps it from reaching
/// this script in practice.
fn logging_preamble(log: &Path) -> String {
    format!(
        r#"#!/bin/sh
{{
  printf '%s\n' "$*"
  printf 'PWD=%s\n' "$(pwd)"
  printf 'GH_HOST=%s\n' "${{GH_HOST-<unset>}}"
  printf 'GITLAB_HOST=%s\n' "${{GITLAB_HOST-<unset>}}"
  printf 'GH_REPO=%s\n' "${{GH_REPO-<unset>}}"
  if [ -n "${{GH_TOKEN+x}}" ]; then printf 'GH_TOKEN=set\n'; else printf 'GH_TOKEN=unset\n'; fi
  if [ -n "${{GITLAB_TOKEN+x}}" ]; then printf 'GITLAB_TOKEN=set\n'; else printf 'GITLAB_TOKEN=unset\n'; fi
  printf -- '---\n'
}} >> {log}
"#,
        log = shell_quote(log)
    )
}

/// Every logged call's argv line (the `printf '%s\n' "$*"` line from
/// [`logging_preamble`]), in call order.
fn logged_calls(log: &Path) -> Vec<String> {
    if !log.exists() {
        return Vec::new();
    }
    std::fs::read_to_string(log)
        .unwrap()
        .split("---\n")
        .filter(|block| !block.trim().is_empty())
        .map(|block| block.lines().next().unwrap_or("").to_string())
        .collect()
}

fn logged_blocks(log: &Path) -> Vec<String> {
    if !log.exists() {
        return Vec::new();
    }
    std::fs::read_to_string(log)
        .unwrap()
        .split("---\n")
        .filter(|block| !block.trim().is_empty())
        .map(|block| block.to_string())
        .collect()
}

// ---- fresh `--create`, the happy path ----------------------------------

#[test]
fn fresh_create_succeeds_when_the_provider_reports_an_empty_target() {
    let sandbox = Sandbox::new();
    let target = sandbox.empty_origin("hosted");
    let root = grove_with_a_commit(&sandbox, "g", "main");
    let log = sandbox.fake_provider_dir().join("log.txt");

    let body = format!(
        r#"case "$1 $2" in
  "--version"*) printf 'gh version 2.97.0 (2026-07-31)\n'; exit 0 ;;
  "auth status"*) exit 0 ;;
  "repo create"*) exit 0 ;;
  "repo view"*) printf '{{"url":"%s","sshUrl":"%s","isEmpty":true,"nameWithOwner":"acme/widgets"}}\n' {url} {url}; exit 0 ;;
  "config get"*) printf 'https\n'; exit 0 ;;
  *) exit 1 ;;
esac
"#,
        url = shell_quote(&target)
    );
    sandbox.fake_provider("gh", &format!("{}{body}", logging_preamble(&log)));

    sandbox
        .grove_in(
            &root,
            &["publish", "--create", "acme/widgets", "--host", "github"],
        )
        .assert()
        .success();

    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("published")
    );
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishRemote").as_deref(),
        Some("origin")
    );
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishUrl").as_deref(),
        Some(target.to_str().unwrap())
    );
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishProvider").as_deref(),
        Some("github")
    );
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishOwner").as_deref(),
        Some("acme")
    );
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishName").as_deref(),
        Some("widgets")
    );
    assert_eq!(
        sandbox.remote_refs(&target),
        vec![(
            "refs/heads/main".to_string(),
            sandbox.oid(&root.join("main"), "HEAD")
        )]
    );

    let calls = logged_calls(&log);
    assert!(calls.iter().any(|call| call.starts_with("--version")));
    assert!(calls.iter().any(|call| call.starts_with("auth status")));
    assert!(calls
        .iter()
        .any(|call| call.starts_with("repo create acme/widgets")));
    assert!(calls.iter().any(|call| call.starts_with("repo view")));
}

// ---- authentication and version failures, before anything is written ---

#[test]
fn an_unauthenticated_provider_refuses_with_no_config_residue() {
    let sandbox = Sandbox::new();
    let root = grove_with_a_commit(&sandbox, "g", "main");
    let log = sandbox.fake_provider_dir().join("log.txt");

    let body = r#"case "$1 $2" in
  "--version"*) printf 'gh version 2.97.0 (2026-07-31)\n'; exit 0 ;;
  "auth status"*) printf 'not logged in\n' 1>&2; exit 1 ;;
  *) exit 1 ;;
esac
"#;
    sandbox.fake_provider("gh", &format!("{}{body}", logging_preamble(&log)));

    sandbox
        .grove_in(
            &root,
            &["publish", "--create", "acme/widgets", "--host", "github"],
        )
        .assert()
        .code(2);

    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("unpublished"),
        "no receipt was ever written; this is the plain state `init` leaves"
    );
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishProvider").as_deref(),
        None
    );
    let calls = logged_calls(&log);
    assert!(!calls.iter().any(|call| call.starts_with("repo create")));
    assert!(!calls.iter().any(|call| call.starts_with("repo view")));
}

#[test]
fn a_provider_version_below_the_floor_refuses_at_exit_64_before_any_grove_mutation() {
    let sandbox = Sandbox::new();
    let root = grove_with_a_commit(&sandbox, "g", "main");
    let log = sandbox.fake_provider_dir().join("log.txt");

    let body = r#"case "$1 $2" in
  "--version"*) printf 'gh version 2.90.0 (2026-01-01)\n'; exit 0 ;;
  *) exit 1 ;;
esac
"#;
    sandbox.fake_provider("gh", &format!("{}{body}", logging_preamble(&log)));

    sandbox
        .grove_in(
            &root,
            &["publish", "--create", "acme/widgets", "--host", "github"],
        )
        .assert()
        .code(64);

    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("unpublished")
    );
}

// ---- malformed `owner/name`, decided before the fake is ever invoked ----

#[test]
fn a_malformed_create_target_is_a_usage_error_that_never_invokes_the_provider() {
    let sandbox = Sandbox::new();
    let root = grove_with_a_commit(&sandbox, "g", "main");
    let log = sandbox.fake_provider_dir().join("log.txt");
    sandbox.fake_provider("gh", &format!("{}exit 1\n", logging_preamble(&log)));

    for bad in ["widgets", "acme/widgets/extra", "/widgets", "acme/"] {
        sandbox
            .grove_in(&root, &["publish", "--create", bad, "--host", "github"])
            .assert()
            .code(64);
    }

    assert!(logged_calls(&log).is_empty(), "the fake was never invoked");
}

// ---- the creating-state matrix ------------------------------------------

/// Seed a grove already in `creating` state with a complete, matching
/// four-key receipt — the same technique `tests/publish.rs` uses to seed a
/// pre-existing classic receipt.
fn seed_creating_receipt(sandbox: &Sandbox, root: &Path, provider: &str, owner: &str, name: &str) {
    set_config(sandbox, root, "grove.publishState", "creating");
    set_config(sandbox, root, "grove.publishProvider", provider);
    set_config(sandbox, root, "grove.publishOwner", owner);
    set_config(sandbox, root, "grove.publishName", name);
    set_config(sandbox, root, "grove.publishRemote", "origin");
}

#[test]
fn a_bare_publish_against_a_creating_grove_refuses_and_never_touches_the_provider() {
    let sandbox = Sandbox::new();
    let origin = sandbox.empty_origin("origin");
    let root = grove_with_a_commit(&sandbox, "g", "main");
    seed_creating_receipt(&sandbox, &root, "github", "acme", "widgets");

    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("acme/widgets"));
}

#[test]
fn a_bare_publish_against_an_incomplete_creating_receipt_self_heals_and_publishes() {
    let sandbox = Sandbox::new();
    let origin = sandbox.empty_origin("origin");
    let root = grove_with_a_commit(&sandbox, "g", "main");
    // Incomplete: only two of the four keys, simulating a crash mid-write.
    set_config(&sandbox, &root, "grove.publishState", "creating");
    set_config(&sandbox, &root, "grove.publishProvider", "github");
    set_config(&sandbox, &root, "grove.publishOwner", "acme");

    sandbox
        .grove_in(&root, &["publish", origin.to_str().unwrap()])
        .assert()
        .success();

    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("published")
    );
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishProvider").as_deref(),
        None,
        "the incomplete creating keys must be cleared, not carried forward"
    );
}

#[test]
fn continuation_never_calls_create_when_repo_view_already_confirms_the_target() {
    let sandbox = Sandbox::new();
    let target = sandbox.empty_origin("hosted");
    let root = grove_with_a_commit(&sandbox, "g", "main");
    seed_creating_receipt(&sandbox, &root, "github", "acme", "widgets");
    let log = sandbox.fake_provider_dir().join("log.txt");

    let body = format!(
        r#"case "$1 $2" in
  "--version"*) printf 'gh version 2.97.0 (2026-07-31)\n'; exit 0 ;;
  "auth status"*) exit 0 ;;
  "repo view"*) printf '{{"url":"%s","sshUrl":"%s","isEmpty":true,"nameWithOwner":"acme/widgets"}}\n' {url} {url}; exit 0 ;;
  "config get"*) printf 'https\n'; exit 0 ;;
  "repo create"*) printf 'must never be called\n' 1>&2; exit 1 ;;
  *) exit 1 ;;
esac
"#,
        url = shell_quote(&target)
    );
    sandbox.fake_provider("gh", &format!("{}{body}", logging_preamble(&log)));

    sandbox
        .grove_in(
            &root,
            &["publish", "--create", "acme/widgets", "--host", "github"],
        )
        .assert()
        .success();

    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("published")
    );
    let calls = logged_calls(&log);
    assert!(
        !calls.iter().any(|call| call.starts_with("repo create")),
        "create must never be invoked once repo view already confirms the target: {calls:?}"
    );
}

#[test]
fn create_against_an_already_published_creating_grove_resumes_via_the_recorded_url_without_touching_repo_view_or_create(
) {
    let sandbox = Sandbox::new();
    let origin = sandbox.empty_origin("origin");
    let root = grove_with_a_commit(&sandbox, "g", "main");
    seed_creating_receipt(&sandbox, &root, "github", "acme", "widgets");
    set_config(&sandbox, &root, "grove.publishState", "published");
    set_config(
        &sandbox,
        &root,
        "grove.publishUrl",
        origin.to_str().unwrap(),
    );
    sandbox.git(
        &root.join("main"),
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    sandbox.git(&root.join("main"), &["push", "--quiet", "origin", "main"]);
    let log = sandbox.fake_provider_dir().join("log.txt");

    let body = r#"case "$1 $2" in
  "--version"*) printf 'gh version 2.97.0 (2026-07-31)\n'; exit 0 ;;
  "auth status"*) exit 0 ;;
  "repo view"*) printf 'must never be called\n' 1>&2; exit 1 ;;
  "repo create"*) printf 'must never be called\n' 1>&2; exit 1 ;;
  *) exit 1 ;;
esac
"#;
    sandbox.fake_provider("gh", &format!("{}{body}", logging_preamble(&log)));

    sandbox
        .grove_in(
            &root,
            &["publish", "--create", "acme/widgets", "--host", "github"],
        )
        .assert()
        .success();

    let calls = logged_calls(&log);
    assert!(calls.iter().any(|call| call.starts_with("--version")));
    assert!(calls.iter().any(|call| call.starts_with("auth status")));
    assert!(!calls.iter().any(|call| call.starts_with("repo")));
}

// ---- the exact argv/env/cwd lockdown (Decision 5) -----------------------

#[test]
fn glab_repo_create_always_carries_skip_git_init_and_pins_host_env_and_an_outside_cwd() {
    let sandbox = Sandbox::new();
    let target = sandbox.empty_origin("hosted");
    let root = grove_with_a_commit(&sandbox, "g", "main");
    let log = sandbox.fake_provider_dir().join("log.txt");

    let body = format!(
        r#"case "$1 $2" in
  "--version"*) printf 'glab 1.114.0 (4d7c6cda7)\n'; exit 0 ;;
  "auth status"*) exit 0 ;;
  "repo create"*) exit 0 ;;
  "repo view"*) printf '{{"http_url_to_repo":"%s","ssh_url_to_repo":"%s","empty_repo":true,"path_with_namespace":"acme/widgets"}}\n' {url} {url}; exit 0 ;;
  "config get"*) printf 'https\n'; exit 0 ;;
  *) exit 1 ;;
esac
"#,
        url = shell_quote(&target)
    );
    sandbox.fake_provider("glab", &format!("{}{body}", logging_preamble(&log)));

    sandbox
        .grove_in(
            &root,
            &[
                "publish",
                "--create",
                "acme/widgets",
                "--host",
                "gitlab",
                "--public",
            ],
        )
        .assert()
        .success();

    let blocks = logged_blocks(&log);
    let create_block = blocks
        .iter()
        .find(|block| block.starts_with("repo create"))
        .expect("repo create must have been called");
    assert!(
        create_block.contains("--skipGitInit"),
        "glab repo create must always carry --skipGitInit: {create_block}"
    );
    assert!(create_block.contains("--defaultBranch main"));
    assert!(create_block.contains("--public"));
    assert!(!create_block.contains("--private"));
    assert!(create_block.contains("GITLAB_HOST=gitlab.com"));
    assert!(create_block.contains("GH_HOST=<unset>"));
    assert!(create_block.contains("GH_REPO=<unset>"));

    let pwd_line = create_block
        .lines()
        .find(|line| line.starts_with("PWD="))
        .unwrap();
    let cwd = Path::new(pwd_line.trim_start_matches("PWD="));
    assert!(
        !cwd.starts_with(&root),
        "the provider child's cwd must be outside the grove root: {cwd:?}"
    );
    // The scratch directory is removed unconditionally after each call.
    assert!(
        !cwd.exists(),
        "the scratch directory must be removed once the child has exited: {cwd:?}"
    );
}

// ---- create fails; the follow-up `repo view` decides the outcome -------

#[test]
fn create_fails_and_repo_view_confirms_missing_rolls_back_and_surfaces_the_failure() {
    let sandbox = Sandbox::new();
    let root = grove_with_a_commit(&sandbox, "g", "main");
    let log = sandbox.fake_provider_dir().join("log.txt");

    let body = r#"case "$1 $2" in
  "--version"*) printf 'gh version 2.97.0 (2026-07-31)\n'; exit 0 ;;
  "auth status"*) exit 0 ;;
  "repo create"*) printf 'name already exists on this account\n' 1>&2; exit 1 ;;
  "repo view"*) exit 1 ;;
  *) exit 1 ;;
esac
"#;
    sandbox.fake_provider("gh", &format!("{}{body}", logging_preamble(&log)));

    sandbox
        .grove_in(
            &root,
            &["publish", "--create", "acme/widgets", "--host", "github"],
        )
        .assert()
        .code(1)
        .stderr(predicates::str::contains("acme/widgets"));

    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("unpublished"),
        "the four-key receipt must be rolled back, leaving no residue"
    );
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishProvider").as_deref(),
        None
    );
    assert_eq!(
        config_of(&sandbox, &root, "grove.publishRemote").as_deref(),
        None
    );
}

#[test]
fn create_returns_ghs_documented_auth_exit_and_maps_it_to_a_decision() {
    let sandbox = Sandbox::new();
    let root = grove_with_a_commit(&sandbox, "g", "main");
    let log = sandbox.fake_provider_dir().join("log.txt");

    let body = r#"case "$1 $2" in
  "--version"*) printf 'gh version 2.97.0 (2026-07-31)\n'; exit 0 ;;
  "auth status"*) exit 0 ;;
  "repo create"*) printf 'authentication required\n' 1>&2; exit 4 ;;
  "repo view"*) exit 1 ;;
  *) exit 1 ;;
esac
"#;
    sandbox.fake_provider("gh", &format!("{}{body}", logging_preamble(&log)));

    sandbox
        .grove_in(
            &root,
            &["publish", "--create", "acme/widgets", "--host", "github"],
        )
        .assert()
        .code(2);

    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("unpublished")
    );
}

#[test]
fn create_fails_and_repo_view_confirms_an_unrelated_existing_repository() {
    let sandbox = Sandbox::new();
    let root = grove_with_a_commit(&sandbox, "g", "main");
    let log = sandbox.fake_provider_dir().join("log.txt");

    let body = r#"case "$1 $2" in
  "--version"*) printf 'gh version 2.97.0 (2026-07-31)\n'; exit 0 ;;
  "auth status"*) exit 0 ;;
  "repo create"*) printf 'name already exists\n' 1>&2; exit 1 ;;
  "repo view"*) printf '{"url":"https://github.invalid/someone-else/widgets.git","sshUrl":"git@github.invalid:someone-else/widgets.git","isEmpty":true,"nameWithOwner":"someone-else/widgets"}\n'; exit 0 ;;
  *) exit 1 ;;
esac
"#;
    sandbox.fake_provider("gh", &format!("{}{body}", logging_preamble(&log)));

    sandbox
        .grove_in(
            &root,
            &["publish", "--create", "acme/widgets", "--host", "github"],
        )
        .assert()
        .code(2)
        .stderr(predicates::str::contains("unrelated"));

    assert_eq!(
        config_of(&sandbox, &root, "grove.publishState").as_deref(),
        Some("unpublished")
    );
}
