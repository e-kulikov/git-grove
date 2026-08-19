#![allow(dead_code)]

use std::ffi::OsString;
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
        let out = cmd.output().expect("git must be installed");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        out
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
        let output = self.git(path, &["rev-parse", "--git-dir"]);
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
