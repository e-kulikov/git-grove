#![allow(dead_code)]

use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

/// Hermetic sandbox: a temporary HOME plus a temporary working area.
/// The environment is applied per child process, never to this process.
pub struct Sandbox {
    home: TempDir,
    work: TempDir,
    path: OsString,
}

#[cfg(unix)]
#[derive(Debug, Eq, PartialEq)]
pub struct TreeEntry {
    path: Vec<u8>,
    kind: u8,
    mode: u32,
    dev: u64,
    ino: u64,
    nlink: u64,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
    content: Vec<u8>,
}

/// A byte-preserving recursive snapshot used to prove preflight refusals do
/// not mutate the repository. Access times are intentionally excluded.
#[cfg(unix)]
pub fn tree_snapshot(root: &Path) -> Vec<TreeEntry> {
    fn visit(root: &Path, relative: &Path, entries: &mut Vec<TreeEntry>) {
        let absolute = root.join(relative);
        let metadata = std::fs::symlink_metadata(&absolute).unwrap();
        let file_type = metadata.file_type();
        let content = if file_type.is_file() {
            std::fs::read(&absolute).unwrap()
        } else if file_type.is_symlink() {
            std::fs::read_link(&absolute)
                .unwrap()
                .as_os_str()
                .as_bytes()
                .to_vec()
        } else {
            Vec::new()
        };
        entries.push(TreeEntry {
            path: relative.as_os_str().as_bytes().to_vec(),
            kind: if file_type.is_dir() {
                1
            } else if file_type.is_file() {
                2
            } else if file_type.is_symlink() {
                3
            } else {
                4
            },
            mode: metadata.mode(),
            dev: metadata.dev(),
            ino: metadata.ino(),
            nlink: metadata.nlink(),
            size: metadata.size(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
            content,
        });
        if file_type.is_dir() {
            let mut children = std::fs::read_dir(&absolute)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>();
            children.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
            for child in children {
                visit(root, &relative.join(child), entries);
            }
        }
    }

    let mut entries = Vec::new();
    visit(root, Path::new(""), &mut entries);
    entries
}

impl Sandbox {
    pub fn new() -> Self {
        Sandbox {
            home: TempDir::new().unwrap(),
            work: TempDir::new().unwrap(),
            path: std::env::var_os("PATH").expect("PATH must be set"),
        }
    }

    pub fn root(&self) -> &Path {
        self.work.path()
    }

    fn apply_env(&self, cmd: &mut Command) {
        cmd.env_clear()
            .env("PATH", &self.path)
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env("TZ", "UTC");
    }

    pub fn git(&self, cwd: &Path, args: &[&str]) -> Output {
        let out = self.git_output(cwd, args);
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    pub fn git_output(&self, cwd: &Path, args: &[&str]) -> Output {
        let mut cmd = Command::new("git");
        cmd.current_dir(cwd)
            .args([
                "-c",
                "init.defaultBranch=main",
                "-c",
                "core.hooksPath=",
                "-c",
                "commit.gpgSign=false",
            ])
            .args(args);
        self.apply_env(&mut cmd);
        cmd.output().expect("git must be installed")
    }

    #[cfg(unix)]
    pub fn git_os(&self, cwd: &Path, args: &[OsString]) -> Output {
        let mut cmd = Command::new("git");
        cmd.current_dir(cwd)
            .args([
                OsString::from("-c"),
                OsString::from("init.defaultBranch=main"),
                OsString::from("-c"),
                OsString::from("core.hooksPath="),
                OsString::from("-c"),
                OsString::from("commit.gpgSign=false"),
            ])
            .args(args);
        self.apply_env(&mut cmd);
        let out = cmd.output().expect("git must be installed");
        assert!(
            out.status.success(),
            "git with OS arguments failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    pub fn grove_in(&self, cwd: &Path, args: &[&str]) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::cargo_bin("git-grove").unwrap();
        cmd.current_dir(cwd).args(args);
        cmd.env_clear()
            .env("PATH", &self.path)
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env("TZ", "UTC");
        cmd
    }

    pub fn grove(&self, args: &[&str]) -> assert_cmd::Command {
        self.grove_in(self.root(), args)
    }

    pub fn grove_process(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("git-grove"));
        command.current_dir(cwd).args(args);
        self.apply_env(&mut command);
        command
    }

    #[cfg(unix)]
    pub fn grove_os(&self, args: &[OsString]) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::cargo_bin("git-grove").unwrap();
        cmd.current_dir(self.root()).args(args);
        cmd.env_clear()
            .env("PATH", &self.path)
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env("TZ", "UTC");
        cmd
    }

    /// A bare repository with one commit on branch `main`, usable as a clone source.
    pub fn bare_origin(&self, name: &str) -> PathBuf {
        let origin = self.root().join(format!("{name}.git"));
        self.git(
            self.root(),
            &[
                "init",
                "--quiet",
                "--bare",
                "--initial-branch=main",
                origin.to_str().unwrap(),
            ],
        );
        let seed = self.root().join(format!("{name}-seed"));
        self.git(
            self.root(),
            &[
                "clone",
                "--quiet",
                origin.to_str().unwrap(),
                seed.to_str().unwrap(),
            ],
        );
        std::fs::write(seed.join("README.md"), "seed\n").unwrap();
        self.git(&seed, &["add", "README.md"]);
        self.git(&seed, &["commit", "--quiet", "-m", "seed"]);
        self.git(&seed, &["push", "--quiet", "origin", "main"]);
        origin
    }

    /// A bare repository with **no** commit, usable as a publication target.
    pub fn empty_origin(&self, name: &str) -> PathBuf {
        self.empty_origin_with_head(name, "main")
    }

    /// A bare repository with no commit whose unborn `HEAD` names `branch`.
    pub fn empty_origin_with_head(&self, name: &str, branch: &str) -> PathBuf {
        let origin = self.root().join(format!("{name}.git"));
        self.git(
            self.root(),
            &[
                "init",
                "--quiet",
                "--bare",
                &format!("--initial-branch={branch}"),
                origin.to_str().unwrap(),
            ],
        );
        origin
    }

    /// Every ref in `repo`, as `(refname, oid)`, in `for-each-ref` order.
    pub fn remote_refs(&self, repo: &Path) -> Vec<(String, String)> {
        let output = self.git(repo, &["for-each-ref", "--format=%(refname) %(objectname)"]);
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| {
                let (name, oid) = line.split_once(' ').expect("for-each-ref format");
                (name.to_string(), oid.to_string())
            })
            .collect()
    }

    /// The ref `HEAD` *points at* in `repo`, or `None` when `HEAD` is not a
    /// symbolic ref at all.
    ///
    /// This reads the symref target, not a resolved commit: an empty bare
    /// repository whose unborn `HEAD` names a branch that does not exist yet —
    /// and a repository whose `HEAD` was left dangling by a push into another
    /// branch — both still report that target here and exit `0`. Tests that
    /// need to know whether the hosting side actually *resolves* `HEAD` must
    /// ask over the wire with `ls-remote --symref`, which is what `publish`
    /// itself does.
    pub fn remote_head_symref(&self, repo: &Path) -> Option<String> {
        let mut cmd = Command::new("git");
        cmd.current_dir(repo)
            .args(["symbolic-ref", "--quiet", "HEAD"]);
        self.apply_env(&mut cmd);
        let out = cmd.output().expect("git must be installed");
        out.status.success().then(|| {
            String::from_utf8(out.stdout)
                .unwrap()
                .trim_end()
                .to_string()
        })
    }

    /// Read one configuration value from `repo`, or `None` when it is unset.
    pub fn repo_config(&self, repo: &Path, key: &str) -> Option<String> {
        let mut cmd = Command::new("git");
        cmd.current_dir(repo).args(["config", "--get-all", key]);
        self.apply_env(&mut cmd);
        let out = cmd.output().expect("git must be installed");
        out.status.success().then(|| {
            String::from_utf8(out.stdout)
                .unwrap()
                .trim_end()
                .to_string()
        })
    }

    /// Set one configuration value in `repo`.
    pub fn set_repo_config(&self, repo: &Path, key: &str, value: &str) {
        self.git(repo, &["config", key, value]);
    }

    /// Remove a configuration key from `repo`, tolerating its absence.
    pub fn unset_repo_config(&self, repo: &Path, key: &str) {
        let mut cmd = Command::new("git");
        cmd.current_dir(repo).args(["config", "--unset-all", key]);
        self.apply_env(&mut cmd);
        let out = cmd.output().expect("git must be installed");
        assert!(
            out.status.success() || out.status.code() == Some(5),
            "git config --unset-all {key} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Clone `origin` into a fresh peer directory under the sandbox root.
    pub fn peer_clone(&self, origin: &Path, name: &str) -> PathBuf {
        let peer = self.root().join(name);
        self.git(
            self.root(),
            &[
                "clone",
                "--quiet",
                origin.to_str().unwrap(),
                peer.to_str().unwrap(),
            ],
        );
        peer
    }

    /// Write `contents` to `relative` inside `repo`, then add, commit, and
    /// push the change to `remote`/`branch`.
    pub fn commit_and_push(
        &self,
        repo: &Path,
        relative: &str,
        contents: &[u8],
        remote: &str,
        branch: &str,
    ) {
        std::fs::write(repo.join(relative), contents).unwrap();
        self.git(repo, &["add", relative]);
        self.git(
            repo,
            &["commit", "--quiet", "-m", &format!("advance {relative}")],
        );
        self.git(repo, &["push", "--quiet", remote, branch]);
    }

    /// The admin (git-dir) directory backing the worktree at `path`.
    pub fn worktree_admin(&self, path: &Path) -> PathBuf {
        let output = self.git(path, &["rev-parse", "--absolute-git-dir"]);
        PathBuf::from(String::from_utf8(output.stdout).unwrap().trim_end())
    }

    /// The resolved commit OID of `revision` inside `repo`.
    pub fn oid(&self, repo: &Path, revision: &str) -> String {
        let output = self.git(repo, &["rev-parse", revision]);
        String::from_utf8(output.stdout)
            .unwrap()
            .trim_end()
            .to_string()
    }
}
