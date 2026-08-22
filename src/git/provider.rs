//! Shelling out to `gh`/`glab`. A thin, git-agnostic-in-spirit process
//! invocation layer, exactly parallel to [`crate::git::runner`].
//!
//! This module must not import `commands::publish` or `grove::metadata`.

use crate::error::{GroveError, Result};
use crate::fsx;
use crate::grove::layout;
use crate::policy::env;
use crate::policy::platform::{self, ProviderVersion};
use rustix::fs::{mkdirat, Mode, CWD};
use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    GitHub,
    GitLab,
}

impl Provider {
    pub fn program(self) -> &'static str {
        match self {
            Provider::GitHub => "gh",
            Provider::GitLab => "glab",
        }
    }

    /// The variable and pinned value this provider's child is spawned with,
    /// on the child's own environment only — never `policy::env`'s global
    /// gate, which would make every *other* command refuse for any user who
    /// happens to export either variable for unrelated reasons.
    pub fn host_env(self) -> (&'static str, &'static str) {
        match self {
            Provider::GitHub => ("GH_HOST", "github.com"),
            Provider::GitLab => ("GITLAB_HOST", "gitlab.com"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderOutput {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl ProviderOutput {
    pub fn ok(&self) -> bool {
        self.status == 0
    }
}

pub trait ProviderRunner {
    fn run(&self, provider: Provider, args: &[&OsStr]) -> Result<ProviderOutput>;
}

/// Build the child command for one provider invocation: the program named by
/// `provider`, the given `args`, `cwd` pinned to `scratch` (a directory
/// outside the grove root), `env::sanitize`'s usual removals, the host
/// variable pinned to its fixed value, and `GH_REPO` explicitly removed.
/// `GH_TOKEN`/`GITLAB_TOKEN` are left untouched, so they are inherited.
fn build_command(provider: Provider, args: &[&OsStr], scratch: &Path) -> Command {
    let mut cmd = Command::new(provider.program());
    cmd.args(args);
    cmd.current_dir(scratch);
    env::sanitize(&mut cmd);
    let (key, value) = provider.host_env();
    cmd.env(key, value);
    cmd.env_remove("GH_REPO");
    cmd
}

/// Create a fresh, empty, mode-`0700` scratch directory outside `grove_root`,
/// named with [`fsx::hex_nonce`] under the grove root's *parent* — never
/// `std::env::temp_dir()`, which `TMPDIR` (ambient, unscanned by
/// `policy::env`) could silently redirect under the grove.
fn create_scratch_dir(grove_root: &Path) -> Result<PathBuf> {
    let parent = grove_root.parent().ok_or_else(|| {
        GroveError::failure(
            "the grove root has no parent directory for a provider scratch directory",
        )
    })?;
    let scratch = parent.join(format!(".git-grove-provider-{}", fsx::hex_nonce()?));
    mkdirat(CWD, &scratch, Mode::from_raw_mode(0o700)).map_err(|error| {
        GroveError::failure(format!(
            "cannot create a provider scratch directory: {error}"
        ))
    })?;
    let canonical = scratch.canonicalize().map_err(|error| {
        GroveError::failure(format!(
            "cannot canonicalise the provider scratch directory: {error}"
        ))
    })?;
    layout::assert_outside_grove(grove_root, &canonical)?;
    Ok(canonical)
}

/// Spawns `gh`/`glab` exactly like [`crate::git::runner::RealGit`] spawns
/// `git`: the same `env::sanitize`, the same signal-aware child bookkeeping.
pub struct RealProvider {
    grove_root: PathBuf,
}

impl RealProvider {
    pub fn new(grove_root: impl Into<PathBuf>) -> Self {
        Self {
            grove_root: grove_root.into(),
        }
    }
}

impl ProviderRunner for RealProvider {
    fn run(&self, provider: Provider, args: &[&OsStr]) -> Result<ProviderOutput> {
        let scratch = create_scratch_dir(&self.grove_root)?;
        let result = (|| -> Result<ProviderOutput> {
            let mut cmd = build_command(provider, args, &scratch);
            let mut child = cmd
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|error| {
                    GroveError::failure(format!("cannot run {}: {error}", provider.program()))
                })?;
            if let Err(error) = crate::transaction::signal::begin_child(child.id()) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
            let out = child.wait_with_output().map_err(|error| {
                GroveError::failure(format!("cannot wait for {}: {error}", provider.program()))
            })?;
            crate::transaction::signal::finish_child()?;
            Ok(ProviderOutput {
                status: out.status.code().unwrap_or(-1),
                stdout: out.stdout,
                stderr: out.stderr,
            })
        })();
        let _ = std::fs::remove_dir_all(&scratch);
        result
    }
}

/// Check `<provider> --version` against the declared floor
/// ([`platform::MINIMUM_GH`]/[`platform::MINIMUM_GLAB`]), mirroring
/// `policy::gate`'s existing git-version check: unparsable output is a
/// `Failure`, a version below the floor is a `Usage` error, run before any
/// provider mutation. The provider binary being absent surfaces as whatever
/// `Result` `runner.run` returned for that (a `Failure`, matching
/// `RealGit::run`'s identical "cannot run git" precedent) — this function
/// adds no special case for it.
pub fn check_provider_version(runner: &dyn ProviderRunner, provider: Provider) -> Result<()> {
    let output = runner.run(provider, &[OsStr::new("--version")])?;
    if !output.ok() {
        return Err(
            GroveError::failure(format!("cannot run {} --version", provider.program()))
                .with_detail(String::from_utf8_lossy(&output.stderr).trim().to_string()),
        );
    }
    let version = match provider {
        Provider::GitHub => ProviderVersion::parse_gh(&output.stdout)?,
        Provider::GitLab => ProviderVersion::parse_glab(&output.stdout)?,
    };
    let (major, minor) = match provider {
        Provider::GitHub => platform::MINIMUM_GH,
        Provider::GitLab => platform::MINIMUM_GLAB,
    };
    if !version.at_least(major, minor) {
        return Err(GroveError::usage(format!(
            "{} {major}.{minor} or newer is required",
            provider.program()
        )));
    }
    Ok(())
}

/// A `RecordingFake`-equivalent for [`ProviderRunner`], used by every test
/// above `git::provider` itself so no command's tests ever run a real
/// `gh`/`glab`.
#[derive(Default)]
pub struct RecordingFake {
    calls: RefCell<Vec<(Provider, Vec<OsString>)>>,
    responses: RefCell<Vec<ProviderOutput>>,
}

impl RecordingFake {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_response(&self, output: ProviderOutput) {
        self.responses.borrow_mut().push(output);
    }

    pub fn calls(&self) -> Vec<(Provider, Vec<OsString>)> {
        self.calls.borrow().clone()
    }
}

impl ProviderRunner for RecordingFake {
    fn run(&self, provider: Provider, args: &[&OsStr]) -> Result<ProviderOutput> {
        self.calls.borrow_mut().push((
            provider,
            args.iter().map(|arg| arg.to_os_string()).collect(),
        ));
        let mut responses = self.responses.borrow_mut();
        Ok(if responses.is_empty() {
            ProviderOutput {
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
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn github_pins_gh_host_and_removes_gh_repo() {
        let cmd = build_command(
            Provider::GitHub,
            &[OsStr::new("repo")],
            Path::new("/tmp/scratch"),
        );
        assert_eq!(cmd.get_program(), OsStr::new("gh"));
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs
            .iter()
            .any(|(k, v)| *k == OsStr::new("GH_HOST") && *v == Some(OsStr::new("github.com"))));
        assert!(envs
            .iter()
            .any(|(k, v)| *k == OsStr::new("GH_REPO") && v.is_none()));
        assert!(!envs.iter().any(|(k, _)| *k == OsStr::new("GITLAB_HOST")));
    }

    #[test]
    fn gitlab_pins_gitlab_host_and_leaves_gh_variables_alone() {
        let cmd = build_command(
            Provider::GitLab,
            &[OsStr::new("repo")],
            Path::new("/tmp/scratch"),
        );
        assert_eq!(cmd.get_program(), OsStr::new("glab"));
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs
            .iter()
            .any(|(k, v)| *k == OsStr::new("GITLAB_HOST") && *v == Some(OsStr::new("gitlab.com"))));
        assert!(!envs.iter().any(|(k, _)| *k == OsStr::new("GH_HOST")));
    }

    #[test]
    fn gh_token_and_gitlab_token_are_never_touched() {
        for provider in [Provider::GitHub, Provider::GitLab] {
            let cmd = build_command(provider, &[], Path::new("/tmp/scratch"));
            let envs: Vec<_> = cmd.get_envs().collect();
            assert!(!envs.iter().any(|(k, _)| *k == OsStr::new("GH_TOKEN")));
            assert!(!envs.iter().any(|(k, _)| *k == OsStr::new("GITLAB_TOKEN")));
        }
    }

    #[test]
    fn cwd_is_always_the_scratch_directory() {
        let cmd = build_command(Provider::GitHub, &[], Path::new("/tmp/scratch-xyz"));
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/tmp/scratch-xyz")));
    }

    #[test]
    fn args_pass_through_unmodified() {
        let cmd = build_command(
            Provider::GitHub,
            &[OsStr::new("repo"), OsStr::new("create")],
            Path::new("/tmp/scratch"),
        );
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, vec![OsStr::new("repo"), OsStr::new("create")]);
    }

    #[test]
    fn recording_fake_records_provider_and_argv_and_returns_queued_responses() {
        let fake = RecordingFake::new();
        fake.push_response(ProviderOutput {
            status: 0,
            stdout: b"ok".to_vec(),
            stderr: Vec::new(),
        });

        let out = fake
            .run(Provider::GitHub, &[OsStr::new("repo"), OsStr::new("view")])
            .unwrap();

        assert_eq!(out.stdout, b"ok");
        assert_eq!(
            fake.calls()[0],
            (
                Provider::GitHub,
                vec![OsString::from("repo"), OsString::from("view")]
            )
        );
    }

    #[test]
    fn scratch_directory_is_created_verified_outside_the_grove_and_removed_after() {
        let grove_parent = tempfile::tempdir().unwrap();
        let grove_root = grove_parent.path().join("grove");
        std::fs::create_dir(&grove_root).unwrap();

        let scratch = create_scratch_dir(&grove_root).unwrap();
        assert!(scratch.exists());
        assert!(!scratch.starts_with(grove_root.canonicalize().unwrap()));
        let metadata = std::fs::metadata(&scratch).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);

        std::fs::remove_dir_all(&scratch).unwrap();
    }

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn real_provider_removes_the_scratch_directory_even_when_the_child_fails() {
        let grove_parent = tempfile::tempdir().unwrap();
        let grove_root = grove_parent.path().join("grove");
        std::fs::create_dir(&grove_root).unwrap();
        let provider = RealProvider::new(&grove_root);

        let output = provider
            .run(
                Provider::GitHub,
                &[OsStr::new("--this-flag-does-not-exist")],
            )
            .unwrap();

        // The real `gh` binary need not be installed for this assertion: a
        // spawn failure (ENOENT) is itself proof enough that no scratch
        // directory survives, since cleanup runs on both branches.
        let leftovers: Vec<_> = std::fs::read_dir(&grove_parent)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .as_bytes()
                    .starts_with(b".git-grove-provider-")
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "leftover scratch directories: {leftovers:?}"
        );
        let _ = output;
    }

    #[test]
    fn version_gate_accepts_the_measured_minimum() {
        let fake = RecordingFake::new();
        fake.push_response(ProviderOutput {
            status: 0,
            stdout: b"gh version 2.97.0 (2026-07-31)\n".to_vec(),
            stderr: Vec::new(),
        });

        check_provider_version(&fake, Provider::GitHub).unwrap();
        assert_eq!(
            fake.calls()[0],
            (Provider::GitHub, vec![OsString::from("--version")])
        );
    }

    #[test]
    fn version_gate_refuses_below_the_floor_with_usage() {
        let fake = RecordingFake::new();
        fake.push_response(ProviderOutput {
            status: 0,
            stdout: b"gh version 2.96.0 (2026-06-01)\n".to_vec(),
            stderr: Vec::new(),
        });

        let error = check_provider_version(&fake, Provider::GitHub).unwrap_err();

        assert_eq!(error.class, ExitClass::Usage);
    }

    #[test]
    fn version_gate_accepts_the_measured_glab_minimum() {
        let fake = RecordingFake::new();
        fake.push_response(ProviderOutput {
            status: 0,
            stdout: b"glab 1.114.0 (4d7c6cda7)\n".to_vec(),
            stderr: Vec::new(),
        });

        check_provider_version(&fake, Provider::GitLab).unwrap();
    }

    #[test]
    fn version_gate_refuses_glab_below_the_floor() {
        let fake = RecordingFake::new();
        fake.push_response(ProviderOutput {
            status: 0,
            stdout: b"glab 1.113.0 (deadbeef)\n".to_vec(),
            stderr: Vec::new(),
        });

        let error = check_provider_version(&fake, Provider::GitLab).unwrap_err();

        assert_eq!(error.class, ExitClass::Usage);
    }

    #[test]
    fn version_gate_treats_unparsable_output_as_failure() {
        let fake = RecordingFake::new();
        fake.push_response(ProviderOutput {
            status: 0,
            stdout: b"not a version\n".to_vec(),
            stderr: Vec::new(),
        });

        let error = check_provider_version(&fake, Provider::GitHub).unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
    }
}
