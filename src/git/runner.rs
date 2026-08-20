use crate::error::{GroveError, Result};
use crate::policy::env;
use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Default)]
pub struct Invocation {
    git_dir: Option<PathBuf>,
    work_tree: Option<PathBuf>,
    cwd: Option<PathBuf>,
    args: Vec<OsString>,
    environment: Vec<(OsString, OsString)>,
    c_locale: bool,
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

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.environment
            .push((key.as_ref().to_os_string(), value.as_ref().to_os_string()));
        self
    }

    /// Pin this child's locale so its diagnostics are the untranslated C-locale
    /// text. This is a parsing contract for one invocation, not a change to the
    /// environment policy: every other git child keeps the user's locale, so
    /// git's own diagnostics still reach the user in their language.
    ///
    /// Measured on git 2.47.3: `LC_ALL=C` wins over `LANGUAGE`, because gettext
    /// ignores `LANGUAGE` under the C locale. `LANGUAGE=` is belt and braces.
    pub fn c_locale(mut self) -> Self {
        self.c_locale = true;
        self
    }

    /// Whether this invocation carries the locale pin.
    pub fn is_c_locale(&self) -> bool {
        self.c_locale
    }

    pub fn environment_for_test(&self) -> Vec<(OsString, OsString)> {
        let mut environment = self.environment.clone();
        if self.c_locale {
            environment.push((OsString::from("LC_ALL"), OsString::from("C")));
            environment.push((OsString::from("LANGUAGE"), OsString::new()));
        }
        environment
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
    /// Git's diagnostics, raw. Forwarding these to the user, escaped, is always
    /// safe. **Branching** on their content is only sound when the output came
    /// from [`GitRunner::run_classified`], which pins the child's locale: git
    /// translates its diagnostics, so an unpinned match is a match on whatever
    /// language the user happens to run in.
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

    /// Run with the child's locale pinned to `C`, so its stderr is the
    /// untranslated text. This is the only documented way to obtain stderr the
    /// tool may branch on; see [`GitOutput::stderr`].
    ///
    /// Accepted cost: the pinned child's *own* unrelated diagnostics render in
    /// English too. Server-side rejection-hook text is hook output rather than
    /// gettext, so hook messages are unaffected either way.
    fn run_classified(&self, invocation: Invocation) -> Result<GitOutput> {
        self.run(invocation.c_locale())
    }
}

pub struct RealGit {
    program: OsString,
}

impl Default for RealGit {
    fn default() -> Self {
        Self::new()
    }
}

impl RealGit {
    pub fn new() -> Self {
        Self {
            program: OsString::from("git"),
        }
    }

    #[cfg(test)]
    fn with_program(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

impl GitRunner for RealGit {
    fn run(&self, invocation: Invocation) -> Result<GitOutput> {
        let mut cmd = Command::new(&self.program);
        cmd.args(invocation.argv());
        if let Some(cwd) = &invocation.cwd {
            cmd.current_dir(cwd);
        }
        env::sanitize(&mut cmd);
        for (key, value) in invocation.environment_for_test() {
            cmd.env(key, value);
        }
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| GroveError::failure(format!("cannot run git: {error}")))?;
        if let Err(error) = crate::transaction::signal::begin_child(child.id()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        let out = child
            .wait_with_output()
            .map_err(|error| GroveError::failure(format!("cannot wait for git: {error}")))?;
        crate::transaction::signal::finish_child()?;
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
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

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
    fn per_child_environment_preserves_order_and_locale_pin_wins_last() {
        let invocation = Invocation::new()
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("LC_ALL", "caller")
            .c_locale();
        assert_eq!(
            invocation.environment_for_test(),
            vec![
                (OsString::from("GIT_OPTIONAL_LOCKS"), OsString::from("0")),
                (OsString::from("LC_ALL"), OsString::from("caller")),
                (OsString::from("LC_ALL"), OsString::from("C")),
                (OsString::from("LANGUAGE"), OsString::new()),
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

    #[test]
    fn the_locale_pin_is_environment_not_argv() {
        let inv = Invocation::new().c_locale().args(["push"]);

        assert_eq!(inv.argv_for_test(), vec!["push"]);
        assert!(inv.is_c_locale());
    }

    #[test]
    fn a_plain_invocation_is_not_locale_pinned() {
        assert!(!Invocation::new().args(["push"]).is_c_locale());
    }

    #[test]
    fn run_classified_marks_the_invocation_and_otherwise_delegates_to_run() {
        let fake = RecordingFake::new();
        fake.push_response(GitOutput {
            status: 128,
            stdout: Vec::new(),
            stderr: b"fatal: the receiving end does not support --atomic push\n".to_vec(),
        });

        let out = fake
            .run_classified(Invocation::new().git_dir("/g/.bare").args(["push"]))
            .unwrap();

        assert_eq!(out.status, 128);
        assert_eq!(
            out.stderr,
            b"fatal: the receiving end does not support --atomic push\n"
        );
        let call = &fake.calls()[0];
        assert!(call.is_c_locale());
        assert_eq!(call.argv_for_test(), vec!["--git-dir=/g/.bare", "push"]);
    }

    /// Ask git itself to report the child environment it was handed, so the pin
    /// is proved where it is applied rather than inferred from a translation.
    ///
    /// The plan asks for a child spawned under `LANGUAGE=de`. `RealGit` has no
    /// per-child environment API, so the only way to arrange that here would be
    /// mutating this process's environment, which the plan's Global Constraints
    /// forbid. Asserting the pinned values directly is stronger and needs no
    /// German catalog to be installed; the end-to-end `LANGUAGE=de` case lives
    /// in `tests/publish.rs`, where the harness can set the child environment.
    #[test]
    fn real_git_pins_lc_all_and_clears_language_only_when_classified() {
        const ALIAS: &str = r#"alias.groveenv=!printf '%s|%s\n' "$LC_ALL" "$LANGUAGE""#;
        let git = RealGit::new();

        let pinned = git
            .run_classified(Invocation::new().args(["-c", ALIAS, "groveenv"]))
            .unwrap();

        assert!(pinned.ok(), "stderr: {}", pinned.stdout_trimmed());
        assert_eq!(pinned.stdout, b"C|\n");

        let inherited = git
            .run(Invocation::new().args(["-c", ALIAS, "groveenv"]))
            .unwrap();

        assert!(inherited.ok());
        assert_eq!(
            inherited.stdout,
            format!(
                "{}|{}\n",
                std::env::var("LC_ALL").unwrap_or_default(),
                std::env::var("LANGUAGE").unwrap_or_default()
            )
            .into_bytes(),
            "run must not pin the locale"
        );
    }

    #[test]
    fn run_classified_yields_the_c_locale_diagnostic_text() {
        let git = RealGit::new();

        let out = git
            .run_classified(Invocation::new().args([
                "ls-remote",
                "--",
                "/nonexistent/git-grove-probe.git",
            ]))
            .unwrap();

        assert!(!out.ok());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("does not appear to be a git repository"),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_runner_drains_stdout_and_stderr_larger_than_pipe_capacity() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("large-output");
        std::fs::write(
            &script,
            b"#!/bin/sh\ndd if=/dev/zero bs=131072 count=1 2>/dev/null\ndd if=/dev/zero bs=131072 count=1 1>&2 2>/dev/null\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let output = RealGit::with_program(script)
            .run(Invocation::new())
            .unwrap();

        assert_eq!(output.status, 0);
        assert_eq!(output.stdout.len(), 131_072);
        assert_eq!(output.stderr.len(), 131_072);
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
