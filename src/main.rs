pub mod error;
#[allow(dead_code)]
mod git;
#[allow(dead_code)]
mod policy;

use clap::Parser;
use error::{ExitClass, GroveError, Result};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "git-grove",
    version,
    about = "Manage repositories as a bare clone surrounded by git worktrees"
)]
struct Cli {}

fn main() -> ExitCode {
    let _cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            if !err.use_stderr() {
                err.exit();
            }
            return render(Err(GroveError::usage(err.to_string().trim_end())));
        }
    };
    render(run())
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

fn run() -> Result<()> {
    Ok(())
}
