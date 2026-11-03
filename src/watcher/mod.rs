//! The watcher: gets told about paused system calls and decides what happens to them.
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
}

/// What to tell the kernel about a paused call.
pub enum Reply {
    /// Let the call run as if nothing happened.
    Continue,
}

pub trait Watcher {
    /// Wait until the next paused call arrives. `None` means every watched process has
    /// exited, so there is nothing left to watch.
    fn next(&mut self) -> Result<Option<PausedCall>>;

    /// Answer a paused call. The process stays frozen until this is called.
    fn reply(&mut self, call: &PausedCall, reply: Reply) -> Result<()>;
}

/// After this many failures in a row, assume the watcher is broken and stop.
const MAX_FAILURES_IN_A_ROW: u32 = 100;

/// The watcher loop: handle paused calls until nothing is left to watch, or the watcher breaks.
///
/// For now it only reports each call and lets it go ahead. Saving files and writing the
/// journal come in the next steps.
///
/// When this returns, the watcher is dropped. For seccomp, that closes the listener, and from
/// then on every paused call fails with ENOSYS instead of hanging forever.
pub fn run(watcher: &mut dyn Watcher) {
    let mut failures = 0;
    loop {
        let call = match watcher.next() {
            Ok(Some(call)) => call,
            Ok(None) => return,
            // A process killed while paused makes one receive fail. That's normal.
            Err(_) if failures < MAX_FAILURES_IN_A_ROW => {
                failures += 1;
                continue;
            }
            Err(e) => {
                eprintln!("whoops: the watcher stopped: {e:#}");
                return;
            }
        };
        failures = 0;

        eprintln!("whoops: pid {} wants {}", call.pid, call.name);

        // This fails if the process was killed while paused. Nothing to do about that.
        let _ = watcher.reply(&call, Reply::Continue);
    }
}
