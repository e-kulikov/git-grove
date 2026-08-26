use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn release_version_requires_a_strict_matching_tag() {
    let script = repo_root().join("scripts/validate-release-version.sh");
    let manifest = repo_root().join("Cargo.toml");

    let valid = Command::new(&script)
        .args(["v0.5.0", manifest.to_str().unwrap()])
        .output()
        .expect("run version validator");
    assert!(
        valid.status.success(),
        "{}",
        String::from_utf8_lossy(&valid.stderr)
    );
    assert_eq!(valid.stdout, b"0.5.0\n");

    for tag in ["0.5.0", "v01.5.0", "v0.5", "v0.5.0-rc.1", "v0.6.0"] {
        let invalid = Command::new(&script)
            .args([tag, manifest.to_str().unwrap()])
            .output()
            .expect("run version validator");
        assert!(!invalid.status.success(), "accepted invalid tag {tag}");
    }
}

#[test]
fn release_docs_describe_real_metadata_and_the_narrow_environment_consent() {
    for relative in ["README.md", "man/git-grove.1"] {
        let document = fs::read_to_string(repo_root().join(relative)).unwrap();
        let rendered_words = document.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            !document.contains(".grove.toml"),
            "{relative} advertises a metadata file that does not exist"
        );
        for required in [
            ".bare/config",
            "grove.version",
            "grove.defaultBranch",
            "grove.remote",
            "grove.publishState",
            "grove.publishRemote",
            "grove.publishUrl",
        ] {
            assert!(
                document.contains(required),
                "{relative} omits metadata contract {required}"
            );
        }
        assert!(
            rendered_words.contains(
                "does not bypass the platform check, minimum Git version, or refused clone options"
            ),
            "{relative} does not bound --ignore-unsupported precisely"
        );
        assert!(
            !document.contains("Report unsupported platform, Git version"),
            "{relative} claims the environment consent overrides hard gates"
        );
    }
}

#[test]
fn release_docs_describe_sync_and_its_safety_contract() {
    for relative in ["README.md", "man/git-grove.1"] {
        let document = fs::read_to_string(repo_root().join(relative)).unwrap();
        for required in [
            "sync",
            "tend",
            "--ff-only",
            "--no-autostash",
            "--no-overwrite-ignore",
            "git-town",
        ] {
            assert!(
                document.contains(required),
                "{relative} omits sync contract term {required}"
            );
        }
        let rendered_words = document.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            rendered_words.contains("fetch --all"),
            "{relative} must explicitly rule out fetch --all"
        );
        assert!(
            rendered_words.contains("worktree that is clean and behind"),
            "{relative} must describe that only a clean, behind worktree is updated"
        );
        assert!(
            document.contains("path")
                && (document.contains("order") || document.contains("sequential")),
            "{relative} must describe stable sequential path ordering"
        );
        assert!(
            document.contains("status `1`") || document.contains(".B 1"),
            "{relative} must describe exit 1 for a fetch failure"
        );
        assert!(
            document.contains("status `2`") || document.contains(".B 2"),
            "{relative} must describe exit 2 for an unresolved or blocked state"
        );
        assert!(
            !rendered_words.contains("--no-overwrite-ignore @{upstream}"),
            "{relative} must not claim the merge targets the symbolic @{{upstream}} revision"
        );
    }
}

#[test]
fn release_docs_describe_publish_and_its_transaction_contract() {
    for relative in ["README.md", "man/git-grove.1"] {
        let document = fs::read_to_string(repo_root().join(relative)).unwrap();
        let rendered_words = document.split_whitespace().collect::<Vec<_>>().join(" ");
        for required in [
            "publish",
            "propagate",
            "unpublished",
            "publishing",
            "published",
            "grove.publishRemote",
            "grove.publishUrl",
            "--remote",
            "--all-branches",
        ] {
            assert!(
                document.contains(required),
                "{relative} omits publish contract term {required}"
            );
        }
        assert!(
            rendered_words.contains(
                "before the first step that mutates the remote or the local remote configuration"
            ),
            "{relative} must state the receipt write point as the mutation-only rule"
        );
        assert!(
            !rendered_words.contains("before any remote-affecting step"),
            "{relative} must not paraphrase the receipt rule back to the wider wording"
        );
        assert!(
            rendered_words.contains("left untouched") && rendered_words.contains("corrected URL"),
            "{relative} must state the user-visible consequence: a grove refused while \
             inspecting the target is left untouched and can simply be republished"
        );
        assert!(
            rendered_words.contains("one atomic push"),
            "{relative} must describe --all-branches as a single atomic push"
        );
        assert!(
            rendered_words.contains("does not advertise atomic push"),
            "{relative} must describe the refusal when the target cannot do atomic push"
        );
        assert!(
            rendered_words.contains("never force-pushes"),
            "{relative} must state that publish never force-pushes"
        );
        assert!(
            rendered_words.contains("diverged") || rendered_words.contains("divergent"),
            "{relative} must describe the divergent-target refusal"
        );
        assert!(
            rendered_words.contains("unrelated"),
            "{relative} must describe the unrelated-target refusal"
        );
        assert!(
            rendered_words.contains("stays in the publishing state"),
            "{relative} must describe the unconfirmed hosting-side default branch outcome"
        );
        assert!(
            !rendered_words.contains("publish adopt"),
            "{relative} must not advertise `publish adopt`"
        );
    }
}

#[test]
fn release_docs_describe_adopt_and_its_recovery_contract() {
    for relative in ["README.md", "man/git-grove.1"] {
        let document = fs::read_to_string(repo_root().join(relative)).unwrap();
        for required in [
            "adopt",
            "transplant",
            "--remote",
            "--default-branch",
            "--continue",
            "--abort",
            ".grove-adopt-",
            "SIGINT",
            "SIGTERM",
            "SIGHUP",
        ] {
            assert!(
                document.contains(required),
                "{relative} omits adopt contract term {required}"
            );
        }
        let rendered = document.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            rendered.contains("exactly one worktree"),
            "{relative} must document the single-worktree precondition"
        );
        assert!(
            document.contains("--create") && document.contains("--host"),
            "{relative} must document publish --create's --host requirement"
        );
    }
}

#[test]
fn grove_format_version_is_unchanged_by_the_0_5_0_package_version() {
    let source = fs::read_to_string(repo_root().join("src/grove/metadata.rs")).unwrap();
    assert!(
        source.contains("pub const FORMAT_VERSION: u32 = 1;"),
        "grove layout format must remain version 1 across the 0.5.0 package bump"
    );
}

#[test]
fn release_workflow_proves_static_linkage_from_elf_headers() {
    let workflow = fs::read_to_string(repo_root().join(".github/workflows/release.yml")).unwrap();

    assert!(
        !workflow.contains("grep -F 'statically linked'"),
        "file(1) describes static PIE differently across runner versions"
    );
    assert_eq!(
        workflow.matches("grep -Fq 'INTERP'").count(),
        2,
        "both the built and packaged binaries must reject an ELF interpreter"
    );
    assert_eq!(
        workflow.matches("grep -Fq 'NEEDED'").count(),
        2,
        "both the built and packaged binaries must reject dynamic dependencies"
    );
    assert!(workflow.contains("flat-repository adopt smoke"));
    assert!(workflow.contains("GIT_GROVE_FAILPOINT=error:1"));
    assert!(workflow.contains("status --porcelain=v2"));
    assert!(workflow.contains("ls-files --stage"));
}

#[test]
fn release_package_is_deterministic_and_has_the_install_contract() {
    let temp = tempfile::tempdir().unwrap();
    let binary = temp.path().join("git-grove");
    fs::write(
        &binary,
        "#!/usr/bin/env bash\nset -euo pipefail\n[[ $1 == completion ]]\nprintf 'completion:%s\\n' \"$2\"\n",
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();

    let first = temp.path().join("first");
    let second = temp.path().join("second");
    run_packager(&binary, &first);
    run_packager(&binary, &second);

    let archive_name = "git-grove_0.5.0_linux_x86_64.tar.gz";
    assert_eq!(
        fs::read(first.join(archive_name)).unwrap(),
        fs::read(second.join(archive_name)).unwrap(),
        "same inputs must produce byte-identical archives"
    );

    let checksum = Command::new("sha256sum")
        .args(["-c", "SHA256SUMS"])
        .current_dir(&first)
        .output()
        .unwrap();
    assert!(
        checksum.status.success(),
        "{}",
        String::from_utf8_lossy(&checksum.stderr)
    );

    let listing = Command::new("tar")
        .args(["-tzf", archive_name])
        .current_dir(&first)
        .output()
        .unwrap();
    assert!(listing.status.success());
    let prefix = "git-grove_0.5.0_linux_x86_64";
    let expected = [
        format!("{prefix}/"),
        format!("{prefix}/LICENSE"),
        format!("{prefix}/README.md"),
        format!("{prefix}/completions/"),
        format!("{prefix}/completions/_git-grove"),
        format!("{prefix}/completions/git-grove.bash"),
        format!("{prefix}/completions/git-grove.fish"),
        format!("{prefix}/git-grove"),
        format!("{prefix}/man/"),
        format!("{prefix}/man/git-grove.1"),
    ];
    let actual: Vec<_> = String::from_utf8(listing.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(actual, expected);

    assert_archive_metadata(&first.join(archive_name), prefix);

    let modes = Command::new("tar")
        .args(["-tvzf", archive_name])
        .current_dir(&first)
        .output()
        .unwrap();
    let modes = String::from_utf8(modes.stdout).unwrap();
    let binary_line = modes
        .lines()
        .find(|line| line.ends_with(&format!(" {prefix}/git-grove")))
        .expect("binary archive entry");
    assert!(binary_line.starts_with("-rwxr-xr-x"), "{binary_line}");

    let extract = temp.path().join("extract");
    fs::create_dir(&extract).unwrap();
    let status = Command::new("tar")
        .args(["-xzf", first.join(archive_name).to_str().unwrap()])
        .current_dir(&extract)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        fs::read_to_string(extract.join(prefix).join("completions/_git-grove")).unwrap(),
        "completion:zsh\n"
    );
}

fn run_packager(binary: &Path, destination: &Path) {
    let output = Command::new(repo_root().join("scripts/package-release.sh"))
        .env("SOURCE_DATE_EPOCH", "946684800")
        .args([
            "0.5.0",
            binary.to_str().unwrap(),
            destination.to_str().unwrap(),
        ])
        .output()
        .expect("run release packager");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_archive_metadata(archive: &Path, prefix: &str) {
    let output = Command::new("tar")
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .args([
            "--numeric-owner",
            "--full-time",
            "-tvzf",
            archive.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    for line in String::from_utf8(output.stdout).unwrap().lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        assert!(fields.len() >= 6, "malformed tar listing: {line}");
        let mode = fields[0];
        let owner = fields[1];
        let date = fields[3];
        let time = fields[4];
        let name = fields[5];

        assert_eq!(owner, "0/0", "wrong archive owner: {line}");
        assert_eq!(date, "2000-01-01", "wrong archive date: {line}");
        assert_eq!(time, "00:00:00", "wrong archive time: {line}");

        let expected_mode = if name.ends_with('/') {
            "drwxr-xr-x"
        } else if name == format!("{prefix}/git-grove") {
            "-rwxr-xr-x"
        } else {
            "-rw-r--r--"
        };
        assert_eq!(mode, expected_mode, "wrong archive mode: {line}");
    }
}
