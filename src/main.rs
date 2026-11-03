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
        /// Which shell to start (a name like `fish`, or a path). Default: the shell you typed
        /// this in, or else $SHELL
        #[arg(long)]
        shell: Option<String>,

        /// If Whoopz can't start, say why and start the shell without it, instead of exiting.
        /// Meant for terminal configs, so a problem never leaves you without a terminal
        #[arg(long)]
        fallback: bool,

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
        Some(Command::Shell {
            shell,
            fallback,
            args,
        }) => match launcher::run_shell(shell.as_deref(), &args) {
            Ok(code) => code,
            Err(e) if fallback => {
                eprintln!("whoops: {e:#}");
                eprintln!("whoops: starting the shell without Whoopz");
                return Err(launcher::exec_unwatched(shell.as_deref(), &args));
            }
            Err(e) => return Err(e),
        },
        Some(Command::List) => control::request("list")?,
        None => match cli.number {
            Some(n) => control::request(&format!("undo {n}"))?,
            None => control::request("undo")?,
        },
    };
    std::process::exit(code);
}
