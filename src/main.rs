pub mod error;

use clap::Parser;
use error::{ExitClass, GroveError};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "git-grove",
    version,
    about = "Manage repositories as a bare clone surrounded by git worktrees"
)]
struct Cli {}

fn main() -> ExitCode {
    let _cli = Cli::parse();
    match run() {
        Ok(()) => ExitCode::from(ExitClass::Ok.code()),
        Err(err) => {
            eprintln!("git-grove: {err}");
            ExitCode::from(err.class.code())
        }
    }
}

fn run() -> Result<(), GroveError> {
    Ok(())
}
