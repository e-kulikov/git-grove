use crate::error::{GroveError, Result};
use crate::policy::env;
use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone, Default)]
pub struct Invocation {
    git_dir: Option<PathBuf>,
    work_tree: Option<PathBuf>,
    cwd: Option<PathBuf>,
    args: Vec<OsString>,
}

impl Invocation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn git_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.git_dir = Some(path.into());
        self
    }

    pub fn work_tree(mut self, path: impl Into<PathBuf>) -> Self {
        self.work_tree = Some(path.into());
        self
    }

    pub fn cwd(mut self, path: impl Into<PathBuf>) -> Self {
        self.cwd = Some(path.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().to_os_string()));
        self
    }

    fn argv(&self) -> Vec<OsString> {
        let mut argv = Vec::new();
        if let Some(dir) = &self.git_dir {
            let mut flag = OsString::from("--git-dir=");
            flag.push(dir);
            argv.push(flag);
        }
        if let Some(tree) = &self.work_tree {
            let mut flag = OsString::from("--work-tree=");
            flag.push(tree);
            argv.push(flag);
        }
        argv.extend(self.args.iter().cloned());
        argv
    }

    /// The full argument vector, preserving operating-system string bytes.
    pub fn argv_os(&self) -> Vec<OsString> {
        self.argv()
    }

    /// Test helper: the argument vector as lossy UTF-8.
    pub fn argv_for_test(&self) -> Vec<String> {
        self.argv_os()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct GitOutput {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl GitOutput {
    pub fn ok(&self) -> bool {
        self.status == 0
    }

    pub fn stdout_trimmed(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }
}

pub trait GitRunner {
    fn run(&self, invocation: Invocation) -> Result<GitOutput>;

    /// Run and fail with a Failure-class error when git exits non-zero.
    fn run_ok(&self, invocation: Invocation) -> Result<GitOutput> {
        let described = invocation.argv_for_test().join(" ");
        let out = self.run(invocation)?;
        if !out.ok() {
            return Err(GroveError::failure(format!("git {described} failed"))
                .with_detail(String::from_utf8_lossy(&out.stderr).trim().to_string()));
        }
        Ok(out)
    }
}

pub struct RealGit;

impl RealGit {
    pub fn new() -> Self {
        Self
    }
}

impl GitRunner for RealGit {
    fn run(&self, invocation: Invocation) -> Result<GitOutput> {
        let mut cmd = Command::new("git");
        cmd.args(invocation.argv());
        if let Some(cwd) = &invocation.cwd {
            cmd.current_dir(cwd);
        }
        env::sanitize(&mut cmd);
        let out = cmd
            .output()
            .map_err(|error| GroveError::failure(format!("cannot run git: {error}")))?;
        Ok(GitOutput {
            status: out.status.code().unwrap_or(-1),
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }
}

#[derive(Default)]
pub struct RecordingFake {
    calls: RefCell<Vec<Invocation>>,
    responses: RefCell<Vec<GitOutput>>,
}

impl RecordingFake {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_response(&self, output: GitOutput) {
        self.responses.borrow_mut().push(output);
    }

    pub fn calls(&self) -> Vec<Invocation> {
        self.calls.borrow().clone()
    }
}

impl GitRunner for RecordingFake {
    fn run(&self, invocation: Invocation) -> Result<GitOutput> {
        self.calls.borrow_mut().push(invocation);
        let mut responses = self.responses.borrow_mut();
        Ok(if responses.is_empty() {
            GitOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            }
        } else {
            responses.remove(0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ExitClass;
    #[cfg(unix)]
    use std::ffi::OsString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn builds_the_argument_vector_with_pins_first() {
        let inv = Invocation::new()
            .git_dir("/g/.bare")
            .args(["worktree", "list"]);
        assert_eq!(
            inv.argv_for_test(),
            vec!["--git-dir=/g/.bare", "worktree", "list"]
        );
    }

    #[test]
    fn includes_work_tree_when_set() {
        let inv = Invocation::new()
            .git_dir("/g/.bare/worktrees/main")
            .work_tree("/g/main")
            .args(["status"]);
        assert_eq!(
            inv.argv_for_test(),
            vec![
                "--git-dir=/g/.bare/worktrees/main",
                "--work-tree=/g/main",
                "status"
            ]
        );
    }

    #[test]
    fn fake_records_calls_and_returns_queued_responses() {
        let fake = RecordingFake::new();
        fake.push_response(GitOutput {
            status: 0,
            stdout: b"ok".to_vec(),
            stderr: Vec::new(),
        });
        let out = fake.run(Invocation::new().args(["version"])).unwrap();
        assert_eq!(out.stdout, b"ok");
        assert_eq!(fake.calls()[0].argv_for_test(), vec!["version"]);
    }

    #[test]
    fn run_ok_returns_failure_with_git_stderr() {
        let fake = RecordingFake::new();
        fake.push_response(GitOutput {
            status: 129,
            stdout: Vec::new(),
            stderr: b"unknown option\n".to_vec(),
        });

        let error = fake.run_ok(Invocation::new().args(["status"])).unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
        assert_eq!(error.message, "git status failed");
        assert_eq!(error.detail.as_deref(), Some("unknown option"));
    }

    #[cfg(unix)]
    #[test]
    fn fake_preserves_non_unicode_argument_bytes() {
        let fake = RecordingFake::new();
        let argument = OsString::from_vec(b"branch-\xff".to_vec());

        fake.run(Invocation::new().args([argument])).unwrap();

        assert_eq!(
            fake.calls()[0].argv_os(),
            vec![OsString::from_vec(b"branch-\xff".to_vec())]
        );
    }
}
