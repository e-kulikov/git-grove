mod cli;
mod commands;
pub mod error;
#[allow(dead_code)]
mod fsx;
#[allow(dead_code)]
mod git;
#[allow(dead_code)]
mod grove;
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
        cli::Command::Clone { .. } => Err(GroveError::failure("clone is not implemented yet")),
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
            let _mode = args.resolve()?;
            Err(GroveError::failure("add is not implemented yet"))
        }
        cli::Command::List { .. } => Err(GroveError::failure("list is not implemented yet")),
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
