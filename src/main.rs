use clap::Parser;

#[derive(Parser)]
#[command(
    name = "git-grove",
    version,
    about = "Manage repositories as a bare clone surrounded by git worktrees"
)]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
}
