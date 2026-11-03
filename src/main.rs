//! Whoopz: always-on undo for the Linux terminal. The command you type is `whoops`.

mod filter;
mod launcher;
mod watcher;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "whoops",
    version,
    about = "Always-on undo for the Linux terminal"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start your shell with Whoopz watching it
    Shell {
        /// Arguments for the shell, after `--` (for example: whoops shell -- -c "rm notes.txt")
        #[arg(last = true)]
        args: Vec<String>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Some(Command::Shell { args }) => {
            let code = launcher::run_shell(&args)?;
            std::process::exit(code);
        }
        None => {
            eprintln!("whoops: undo isn't built yet. Start a watched shell with `whoops shell`.");
            std::process::exit(2);
        }
    }
}
