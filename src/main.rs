//! Whoopz: always-on undo for the Linux terminal. The command you type is `whoops`.
//!
//! How the pieces fit:
//!
//! ```text
//! launcher   starts your shell with the filter attached            (whoops shell)
//! filter     which calls the kernel pauses
//! watcher    receives paused calls and answers them (seccomp; ptrace could be added)
//! engine     the loop: for each paused call, decide what to do
//! calls      decodes a call's raw numbers         procfs  reads paths from the paused process
//! handle     saves what's about to be lost         sys     safe wrappers around file calls
//! session    the journal, the stash, which command is running
//! hooks      tells the watcher where commands start and end (bash, zsh)
//! control    how `whoops` inside the shell asks the watcher for an undo
//! undo       puts things back                                      (whoops, whoops list)
//! ```

mod calls;
mod control;
mod engine;
mod filter;
mod handle;
mod hooks;
mod journal;
mod launcher;
mod procfs;
mod session;
mod sys;
mod undo;
mod watcher;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "whoops",
    version,
    about = "Always-on undo for the Linux terminal",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Undo this command (a number from `whoops list`) instead of the last one
    number: Option<u64>,
}

#[derive(Subcommand)]
enum Command {
    /// Start your shell with Whoopz watching it
    Shell {
        /// Arguments for the shell, after `--` (for example: whoops shell -- -c "rm notes.txt")
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Show the commands in this shell that changed files
    List,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let code = match cli.command {
        Some(Command::Shell { args }) => launcher::run_shell(&args)?,
        Some(Command::List) => control::request("list")?,
        None => match cli.number {
            Some(n) => control::request(&format!("undo {n}"))?,
            None => control::request("undo")?,
        },
    };
    std::process::exit(code);
}
