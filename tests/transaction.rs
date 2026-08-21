mod harness;

use git_grove::transaction::journal::{RawBytes, ValidatedBytePath};
use harness::Sandbox;
use predicates::str::contains;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

#[cfg(feature = "failpoints")]
fn flat_repository(sandbox: &Sandbox, name: &str) -> std::path::PathBuf {
    let root = sandbox.root().join(name);
    std::fs::create_dir(&root).unwrap();
    sandbox.git(&root, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(root.join("tracked"), b"tracked\n").unwrap();
    sandbox.git(&root, &["add", "tracked"]);
    sandbox.git(&root, &["commit", "--quiet", "-m", "initial"]);
    root
}

#[test]
fn journal_raw_bytes_and_paths_reject_noncanonical_encodings() {
    let raw = RawBytes::from_bytes(b"name-\xff");
    assert_eq!(raw.decode(), b"name-\xff");
    assert!(serde_json::from_str::<RawBytes>(r#"{"encoding":"Hex","value":"FF"}"#).is_err());
    assert!(ValidatedBytePath::new(Path::new("../escape")).is_err());
    assert!(ValidatedBytePath::component(b"a/b").is_err());
}

struct LockHolder {
    child: Child,
}

impl LockHolder {
    fn acquire(root: &Path, shared: bool) -> Self {
        let ready = root.join(if shared {
            "shared-lock-ready"
        } else {
            "exclusive-lock-ready"
        });
        let mut command = Command::new("flock");
        if shared {
            command.arg("--shared");
        } else {
            command.arg("--exclusive");
        }
        let mut child = command
            .arg(".bare")
            .args(["sh", "-c", "touch \"$1\"; cat >/dev/null", "sh"])
            .arg(&ready)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("flock must be installed");
        for _ in 0..100 {
            if ready.exists() {
                return Self { child };
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(child.stdin.take());
        let _ = child.kill();
        let _ = child.wait();
        panic!("lock holder did not become ready");
    }
}

impl Drop for LockHolder {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}

#[test]
fn grove_commands_use_shared_and_exclusive_inode_locks() {
    let sandbox = Sandbox::new();
    sandbox.grove(&["init", "g"]).assert().success();
    let root = sandbox.root().join("g");
    let publish_target = sandbox.empty_origin("publish-target");

    {
        let _shared = LockHolder::acquire(&root, true);
        sandbox.grove_in(&root, &["list"]).assert().success();
        sandbox
            .grove_in(&root, &["add", "topic"])
            .assert()
            .code(2)
            .stderr(contains("another git-grove command"));
        sandbox
            .grove_in(&root, &["sync"])
            .assert()
            .code(2)
            .stderr(contains("another git-grove command"));
        sandbox
            .grove_in(&root, &["publish", publish_target.to_str().unwrap()])
            .assert()
            .code(2)
            .stderr(contains("another git-grove command"));
    }

    let _exclusive = LockHolder::acquire(&root, false);
    sandbox
        .grove_in(&root, &["list"])
        .assert()
        .code(2)
        .stderr(contains("another git-grove command"));
}

#[cfg(feature = "failpoints")]
#[test]
fn recovery_blocks_other_commands_and_preserves_multiple_or_unsafe_candidates() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new();
    let root = flat_repository(&sandbox, "blocked");
    sandbox
        .grove_in(sandbox.root(), &["adopt", root.to_str().unwrap()])
        .env("GIT_GROVE_FAILPOINT", "error:10")
        .assert()
        .failure();
    sandbox
        .grove_in(&root, &["list"])
        .assert()
        .code(2)
        .stdout("")
        .stderr(contains("adopt --continue"));
    sandbox
        .grove_in(&root, &["add", "other"])
        .assert()
        .code(2)
        .stderr(contains("adopt --abort"));
    sandbox
        .grove_in(
            sandbox.root(),
            &["adopt", "--continue", root.to_str().unwrap()],
        )
        .assert()
        .success();

    let multiple = flat_repository(&sandbox, "multiple");
    sandbox
        .grove_in(sandbox.root(), &["adopt", multiple.to_str().unwrap()])
        .env("GIT_GROVE_FAILPOINT", "error:1")
        .assert()
        .failure();
    let foreign = multiple.join(".grove-adopt-ffffffff");
    std::fs::create_dir(&foreign).unwrap();
    std::fs::set_permissions(&foreign, std::fs::Permissions::from_mode(0o700)).unwrap();
    sandbox
        .grove_in(
            sandbox.root(),
            &["adopt", "--continue", multiple.to_str().unwrap()],
        )
        .assert()
        .code(2)
        .stderr(contains("multiple adoption transactions"));
    assert!(foreign.exists());

    let unsafe_root = flat_repository(&sandbox, "unsafe-candidate");
    sandbox
        .grove_in(sandbox.root(), &["adopt", unsafe_root.to_str().unwrap()])
        .env("GIT_GROVE_FAILPOINT", "error:1")
        .assert()
        .failure();
    let transaction = std::fs::read_dir(&unsafe_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".grove-adopt-")
        })
        .unwrap();
    std::fs::set_permissions(&transaction, std::fs::Permissions::from_mode(0o755)).unwrap();
    sandbox
        .grove_in(
            sandbox.root(),
            &["adopt", "--continue", unsafe_root.to_str().unwrap()],
        )
        .assert()
        .code(2)
        .stderr(contains("unsafe type, mode, or owner"));
    assert!(transaction.exists());
}
