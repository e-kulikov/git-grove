pub mod env;
pub mod platform;

use crate::error::{GroveError, Result};
use crate::git::runner::{GitRunner, Invocation};
use std::io::{self, IsTerminal, Write};

pub trait Interaction {
    fn stdin_is_terminal(&self) -> bool;
    fn write_stderr(&mut self, text: &str) -> io::Result<()>;
    fn read_line(&mut self, line: &mut String) -> io::Result<usize>;
}

pub struct SystemInteraction;

impl Interaction for SystemInteraction {
    fn stdin_is_terminal(&self) -> bool {
        io::stdin().is_terminal()
    }

    fn write_stderr(&mut self, text: &str) -> io::Result<()> {
        let mut stderr = io::stderr().lock();
        stderr.write_all(text.as_bytes())?;
        stderr.flush()
    }

    fn read_line(&mut self, line: &mut String) -> io::Result<usize> {
        io::stdin().read_line(line)
    }
}

/// Apply the checks shared by lifecycle commands before their first mutation.
pub fn gate(
    runner: &dyn GitRunner,
    findings: &[env::Finding],
    ignore_unsupported: bool,
    interaction: &mut dyn Interaction,
) -> Result<()> {
    platform::check_platform()?;

    if !findings.is_empty() {
        let mut report =
            String::from("these environment variables are incompatible with the grove layout:\n");
        for finding in findings {
            report.push_str(&format!(
                "  {}={}\n    {}\n",
                finding.name, finding.value, finding.reason
            ));
        }
        let report = report.trim_end();

        if !ignore_unsupported {
            return Err(GroveError::usage(report).with_detail(
                "they are ignored for git commands git-grove runs; pass --ignore-unsupported to continue",
            ));
        }

        interaction
            .write_stderr(&format!("git-grove: warning: {report}\n"))
            .and_then(|_| {
                interaction.write_stderr(
                    "git-grove: continuing with them removed from git's environment\n",
                )
            })
            .map_err(|error| GroveError::failure(format!("cannot write warning: {error}")))?;

        if interaction.stdin_is_terminal() {
            interaction
                .write_stderr("Continue? [y/N] ")
                .map_err(|error| GroveError::failure(format!("cannot write prompt: {error}")))?;
            let mut answer = String::new();
            let bytes = interaction
                .read_line(&mut answer)
                .map_err(|error| GroveError::failure(format!("cannot read response: {error}")))?;
            if bytes == 0 {
                return Err(GroveError::usage("cancelled"));
            }
            let answer = answer.strip_suffix('\n').unwrap_or(&answer);
            let answer = answer.strip_suffix('\r').unwrap_or(answer);
            if !matches!(answer, "y" | "yes") {
                return Err(GroveError::usage("cancelled"));
            }
        }
    }

    let output = runner.run_ok(Invocation::new().args(["--version"]))?;
    let version = platform::GitVersion::parse(&output.stdout)?;
    if !version.at_least(platform::MINIMUM_GIT.0, platform::MINIMUM_GIT.1) {
        return Err(GroveError::usage(format!(
            "git {}.{} or newer is required",
            platform::MINIMUM_GIT.0,
            platform::MINIMUM_GIT.1
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ExitClass;
    use crate::git::runner::{GitOutput, RecordingFake};
    use std::io;

    #[derive(Default)]
    struct TestInteraction {
        terminal: bool,
        answer: String,
        output: String,
    }

    impl Interaction for TestInteraction {
        fn stdin_is_terminal(&self) -> bool {
            self.terminal
        }

        fn write_stderr(&mut self, text: &str) -> io::Result<()> {
            self.output.push_str(text);
            Ok(())
        }

        fn read_line(&mut self, line: &mut String) -> io::Result<usize> {
            line.push_str(&self.answer);
            Ok(self.answer.len())
        }
    }

    fn finding() -> env::Finding {
        env::Finding {
            name: "GIT_DIR".to_string(),
            value: "/elsewhere/.git".to_string(),
            reason: "names a different repository than the one this command builds",
        }
    }

    fn supported_git() -> GitOutput {
        GitOutput {
            status: 0,
            stdout: b"git version 2.47.3\n".to_vec(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn refuses_an_unsafe_environment_before_querying_git() {
        let runner = RecordingFake::new();
        let mut interaction = TestInteraction::default();

        let error = gate(&runner, &[finding()], false, &mut interaction).unwrap_err();

        assert_eq!(error.class, ExitClass::Usage);
        assert!(error.message.contains("GIT_DIR=/elsewhere/.git"));
        assert!(error.to_string().contains("--ignore-unsupported"));
        assert!(runner.calls().is_empty(), "git was queried after refusal");
    }

    #[test]
    fn non_terminal_override_warns_and_checks_the_git_version() {
        let runner = RecordingFake::new();
        runner.push_response(supported_git());
        let mut interaction = TestInteraction::default();

        gate(&runner, &[finding()], true, &mut interaction).unwrap();

        assert!(interaction.output.contains("warning"));
        assert!(interaction.output.contains("GIT_DIR=/elsewhere/.git"));
        assert!(interaction
            .output
            .contains("removed from git's environment"));
        assert_eq!(runner.calls()[0].argv_for_test(), ["--version"]);
    }

    #[test]
    fn terminal_override_requires_exact_lowercase_y_or_yes() {
        for refused in ["Y\n", "YES\n", " yes \n", "\n", ""] {
            let runner = RecordingFake::new();
            let mut interaction = TestInteraction {
                terminal: true,
                answer: refused.to_string(),
                output: String::new(),
            };

            let error = gate(&runner, &[finding()], true, &mut interaction).unwrap_err();

            assert_eq!(error.class, ExitClass::Usage, "accepted {refused:?}");
            assert!(runner.calls().is_empty(), "git was queried after decline");
        }

        for accepted in ["y\n", "yes\r\n"] {
            let runner = RecordingFake::new();
            runner.push_response(supported_git());
            let mut interaction = TestInteraction {
                terminal: true,
                answer: accepted.to_string(),
                output: String::new(),
            };

            gate(&runner, &[finding()], true, &mut interaction).unwrap();
            assert_eq!(runner.calls().len(), 1, "refused {accepted:?}");
        }
    }

    #[test]
    fn refuses_git_older_than_the_measured_minimum() {
        let runner = RecordingFake::new();
        runner.push_response(GitOutput {
            status: 0,
            stdout: b"git version 2.46.9\n".to_vec(),
            stderr: Vec::new(),
        });
        let mut interaction = TestInteraction::default();

        let error = gate(&runner, &[], false, &mut interaction).unwrap_err();

        assert_eq!(error.class, ExitClass::Usage);
        assert!(error.message.contains("git 2.47 or newer is required"));
        assert_eq!(runner.calls()[0].argv_for_test(), ["--version"]);
    }
}
