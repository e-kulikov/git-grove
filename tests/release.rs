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
        .args(["v0.1.0", manifest.to_str().unwrap()])
        .output()
        .expect("run version validator");
    assert!(
        valid.status.success(),
        "{}",
        String::from_utf8_lossy(&valid.stderr)
    );
    assert_eq!(valid.stdout, b"0.1.0\n");

    for tag in ["0.1.0", "v01.1.0", "v0.1", "v0.1.0-rc.1", "v0.2.0"] {
        let invalid = Command::new(&script)
            .args([tag, manifest.to_str().unwrap()])
            .output()
            .expect("run version validator");
        assert!(!invalid.status.success(), "accepted invalid tag {tag}");
    }
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

    let archive_name = "git-grove_0.1.0_linux_x86_64.tar.gz";
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
    let prefix = "git-grove_0.1.0_linux_x86_64";
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
            "0.1.0",
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
