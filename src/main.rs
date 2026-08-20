use clap::{CommandFactory, Parser};
use git_grove::error::{ExitClass, GroveError, Result};
use git_grove::{cli, commands, fsx, git, grove, output, policy};
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
            ExitCode::from(err.code())
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
        cli::Command::Adopt {
            path,
            remote,
            default_branch,
            continue_adoption,
            abort,
        } => {
            let runner = git::runner::RealGit::new();
            let findings = policy::env::scan_os(std::env::vars_os());
            let mut interaction = policy::SystemInteraction;
            policy::gate(&runner, &findings, ignore_unsupported, &mut interaction)?;
            let cwd = std::env::current_dir().map_err(|error| {
                GroveError::failure(format!("cannot read the current directory: {error}"))
            })?;
            let action = if continue_adoption {
                commands::adopt::AdoptAction::Continue
            } else if abort {
                commands::adopt::AdoptAction::Abort
            } else {
                commands::adopt::AdoptAction::Fresh
            };
            commands::adopt::run(
                &runner,
                &commands::adopt::AdoptArgs {
                    path,
                    remote,
                    default_branch,
                    action,
                },
                &cwd,
            )
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
            let _lock = fsx::lock::GroveLock::acquire_path(
                &grove.bare_dir(),
                fsx::lock::LockMode::Exclusive,
                "git grove add",
            )?;
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
            let _lock = fsx::lock::GroveLock::acquire_path(
                &grove.bare_dir(),
                fsx::lock::LockMode::Shared,
                "git grove list",
            )?;
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
            let _lock = fsx::lock::GroveLock::acquire_path(
                &grove.bare_dir(),
                fsx::lock::LockMode::Exclusive,
                "git grove sync",
            )?;
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
        cli::Command::Publish {
            url,
            remote,
            all_branches,
        } => {
            let runner = git::runner::RealGit::new();
            let findings = policy::env::scan_os(std::env::vars_os());
            let mut interaction = policy::SystemInteraction;
            policy::gate(&runner, &findings, ignore_unsupported, &mut interaction)?;
            let cwd = std::env::current_dir().map_err(|error| {
                GroveError::failure(format!("cannot read the current directory: {error}"))
            })?;
            let grove = grove::discover::Grove::discover(&cwd)?;
            let _lock = fsx::lock::GroveLock::acquire_path(
                &grove.bare_dir(),
                fsx::lock::LockMode::Exclusive,
                "git grove publish",
            )?;
            let metadata = grove::metadata::read(&runner, &grove)?;
            grove::metadata::ensure_supported(&metadata)?;
            let request = commands::publish::Request {
                url,
                remote,
                all_branches,
            };
            let report = commands::publish::run(&runner, &grove, &metadata, &request)?;
            output::write_lines(&mut std::io::stdout().lock(), &report.lines)?;
            match report.class {
                ExitClass::Ok => Ok(()),
                ExitClass::NeedsDecision => {
                    let error =
                        GroveError::needs_decision("this publication needs a decision to finish");
                    if report.diagnostics.is_empty() {
                        Err(error)
                    } else {
                        Err(error.with_detail(report.diagnostics.join("; ")))
                    }
                }
                ExitClass::Failure | ExitClass::Usage => {
                    unreachable!("publish returns only success or needs-decision classes")
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
