mod harness;

use git_grove::transaction::journal::{RawBytes, ValidatedBytePath};
use harness::Sandbox;
use predicates::str::contains;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

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
