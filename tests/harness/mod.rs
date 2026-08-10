#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

/// Hermetic sandbox: a temporary HOME plus a temporary working area.
/// The environment is applied per child process, never to this process.
pub struct Sandbox {
    home: TempDir,
    work: TempDir,
}

impl Sandbox {
    pub fn new() -> Self {
        Sandbox {
            home: TempDir::new().unwrap(),
            work: TempDir::new().unwrap(),
        }
    }

    pub fn root(&self) -> &Path {
        self.work.path()
    }

    fn apply_env(&self, cmd: &mut Command) {
        cmd.env("HOME", self.home.path())
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

    pub fn grove_in(&self, cwd: &Path, args: &[&str]) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::cargo_bin("git-grove").unwrap();
        cmd.current_dir(cwd).args(args);
        cmd.env("HOME", self.home.path())
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
}
