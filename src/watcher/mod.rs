//! The watcher: gets told about paused system calls and answers them.
//!
//! Only seccomp is implemented. The rest of Whoopz talks to the [`Watcher`] trait and never to
//! seccomp directly, so a ptrace-based watcher (Plan B) can be added later as a second
//! implementation without changing anything else.

pub mod seccomp;

use anyhow::Result;

/// One system call the kernel has paused, waiting for our answer.
#[derive(Debug)]
pub struct PausedCall {
    /// Ties our reply to this call. Only the watcher that produced it knows what it means.
    pub id: u64,
    /// The process that made the call.
    pub pid: u32,
    /// The call's name, like `unlinkat`.
    pub name: String,
    /// The six raw arguments. A path is the *address* of its text in that process's memory.
    pub args: [u64; 6],
}

/// What to tell the kernel about a paused call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reply {
    /// Let the kernel run the call, as the program, as if nothing happened.
    Continue,
}

pub trait Watcher: Send {
    /// Wait until the next paused call arrives. `None` means every watched process has
    /// exited, so there is nothing left to watch.
    fn next(&mut self) -> Result<Option<PausedCall>>;

    /// Is the process still paused on this call? A pid can be reused once a process dies, so
    /// after reading a process's memory we check it was really the process we meant.
    fn is_valid(&self, call: &PausedCall) -> bool;

    /// Answer a paused call. The process stays frozen until this is called.
    fn reply(&mut self, call: &PausedCall, reply: Reply) -> Result<()>;
}
