mod cli;
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
    policy::platform::check_platform()?;
    match cli.command {
        cli::Command::Clone { .. } => Err(GroveError::failure("clone is not implemented yet")),
        cli::Command::Init { .. } => Err(GroveError::failure("init is not implemented yet")),
        cli::Command::Add { .. } => Err(GroveError::failure("add is not implemented yet")),
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
