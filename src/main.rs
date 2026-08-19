mod cli;
mod commands;
pub mod error;
#[allow(dead_code)]
mod fsx;
#[allow(dead_code)]
mod git;
#[allow(dead_code)]
mod grove;
mod output;
#[allow(dead_code)]
mod policy;

use clap::{CommandFactory, Parser};
use error::{ExitClass, GroveError, Result};
use std::process::ExitCode;

fn main() -> ExitCode {
    let argv = match cli::normalize(std::env::args_os().collect()) {
        Ok(argv) => argv,
        Err(err) => return render(Err(err)),
    };
    let cli = match cli::Cli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(err) => {
            if !err.use_stderr() {
                let _ = err.print();
                return ExitCode::from(ExitClass::Ok.code());
            }
            return render(Err(GroveError::usage(err.to_string().trim_end())));
        }
    };
    render(run(cli))
}

fn render(result: Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::from(ExitClass::Ok.code()),
        Err(err) => {
            eprintln!("git-grove: {err}");
            ExitCode::from(err.class.code())
        }
    }
}

fn run(cli: cli::Cli) -> Result<()> {
    let cli::Cli {
        ignore_unsupported,
        command,
    } = cli;
    match command {
        cli::Command::Clone {
            url,
            dir,
            branch,
            git_options,
        } => {
            let verdict = policy::clone_options::classify(&git_options)?;
            let runner = git::runner::RealGit::new();
            let findings = policy::env::scan_os(std::env::vars_os());
            let mut interaction = policy::SystemInteraction;
            policy::gate(&runner, &findings, ignore_unsupported, &mut interaction)?;
            let cwd = std::env::current_dir().map_err(|error| {
                GroveError::failure(format!("cannot read the current directory: {error}"))
            })?;
            commands::clone::run(&runner, &url, dir, branch, verdict, &cwd).map(|_| ())
        }
        cli::Command::Init { dir, branch } => {
            let runner = git::runner::RealGit::new();
            let findings = policy::env::scan_os(std::env::vars_os());
            let mut interaction = policy::SystemInteraction;
            policy::gate(&runner, &findings, ignore_unsupported, &mut interaction)?;
            let cwd = std::env::current_dir().map_err(|error| {
                GroveError::failure(format!("cannot read the current directory: {error}"))
            })?;
            commands::init::run(&runner, dir, branch, &cwd).map(|_| ())
        }
        cli::Command::Add(args) => {
            let mode = args.resolve()?;
            let runner = git::runner::RealGit::new();
            let findings = policy::env::scan_os(std::env::vars_os());
            let mut interaction = policy::SystemInteraction;
            policy::gate(&runner, &findings, ignore_unsupported, &mut interaction)?;
            let cwd = std::env::current_dir().map_err(|error| {
                GroveError::failure(format!("cannot read the current directory: {error}"))
            })?;
            let grove = grove::discover::Grove::discover(&cwd)?;
            let metadata = grove::metadata::read(&runner, &grove)?;
            grove::metadata::ensure_supported(&metadata)?;
            commands::add::run(&runner, &grove, mode).map(|_| ())
        }
        cli::Command::List { porcelain } => {
            let runner = git::runner::RealGit::new();
            let findings = policy::env::scan_os(std::env::vars_os());
            let mut interaction = policy::SystemInteraction;
            policy::gate(&runner, &findings, ignore_unsupported, &mut interaction)?;
            let cwd = std::env::current_dir().map_err(|error| {
                GroveError::failure(format!("cannot read the current directory: {error}"))
            })?;
            let grove = grove::discover::Grove::discover(&cwd)?;
            let metadata = grove::metadata::read(&runner, &grove)?;
            grove::metadata::ensure_supported(&metadata)?;
            match commands::list::run(&runner, &grove, porcelain)? {
                ExitClass::Ok => Ok(()),
                ExitClass::NeedsDecision => Err(GroveError::needs_decision(
                    "some registered worktrees need attention",
                )),
                ExitClass::Failure | ExitClass::Usage => {
                    unreachable!("list returns only success or needs-decision classes")
                }
            }
        }
        cli::Command::Sync => {
            let runner = git::runner::RealGit::new();
            let findings = policy::env::scan_os(std::env::vars_os());
            let mut interaction = policy::SystemInteraction;
            policy::gate(&runner, &findings, ignore_unsupported, &mut interaction)?;
            let cwd = std::env::current_dir().map_err(|error| {
                GroveError::failure(format!("cannot read the current directory: {error}"))
            })?;
            let grove = grove::discover::Grove::discover(&cwd)?;
            let metadata = grove::metadata::read(&runner, &grove)?;
            grove::metadata::ensure_supported(&metadata)?;
            let report = commands::sync::run(&runner, &grove)?;
            output::write_rows(&mut std::io::stdout().lock(), &report.rows, false)?;
            match report.class {
                ExitClass::Ok => Ok(()),
                ExitClass::NeedsDecision => {
                    let error = GroveError::needs_decision(
                        "some worktrees remain behind or need attention",
                    );
                    if report.diagnostics.is_empty() {
                        Err(error)
                    } else {
                        Err(error.with_detail(report.diagnostics.join("; ")))
                    }
                }
                ExitClass::Failure | ExitClass::Usage => {
                    unreachable!("sync returns only success or needs-decision classes")
                }
            }
        }
        cli::Command::Completion { shell } => {
            let mut command = cli::Cli::command();
            clap_complete::generate(
                clap_complete::Shell::from(shell),
                &mut command,
                "git-grove",
                &mut std::io::stdout(),
            );
            Ok(())
        }
    }
}
