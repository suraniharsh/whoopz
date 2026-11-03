//! The watcher loop, and a helper thread that keeps the shell hook's pipe drained.

use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex};

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

use crate::session::{Session, lock};
use crate::watcher::{PausedCall, Reply, Watcher};
use crate::{calls, handle};

/// After this many failures in a row, assume the watcher is broken and stop.
const MAX_FAILURES_IN_A_ROW: u32 = 100;

/// Handle paused calls until nothing is left to watch, or the watcher breaks.
///
/// When this returns, the watcher is dropped. For seccomp that closes the listener, and from
/// then on every paused call fails with ENOSYS instead of hanging forever.
pub fn run(mut watcher: Box<dyn Watcher>, session: Arc<Mutex<Session>>, mut debug: Debug) {
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

        let reply = {
            let mut s = lock(&session);
            // Learn which command is running before deciding anything about this call.
            s.apply_marks();
            // Described before deciding: the process is still paused, so /proc still shows it.
            let described = debug.enabled().then(|| describe(&call, &s));
            let reply = decide(watcher.as_ref(), &call, &mut s);
            if let Some(line) = described {
                debug.line(&format!("{line}\treply={reply:?}"));
            }
            reply
        };
        if let Some(reply) = reply {
            // Fails only if the process was killed while paused. Nothing to do about that.
            let _ = watcher.reply(&call, reply);
        }
    }
}

/// Where `WHOOPZ_DEBUG` output goes: nowhere, the terminal (`1`), or a file (a path).
pub enum Debug {
    Off,
    Stderr,
    File(std::fs::File),
}

impl Debug {
    pub fn from_env(value: Option<std::ffi::OsString>) -> Debug {
        match value {
            Some(v) if v == "1" => Debug::Stderr,
            Some(v) if v.as_encoded_bytes().starts_with(b"/") => std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&v)
                .map_or(Debug::Stderr, Debug::File),
            _ => Debug::Off,
        }
    }

    pub fn enabled(&self) -> bool {
        !matches!(self, Debug::Off)
    }

    pub fn line(&mut self, text: &str) {
        match self {
            Debug::Off => {}
            Debug::Stderr => eprintln!("whoops: {text}"),
            Debug::File(f) => {
                use std::io::Write;
                let _ = writeln!(f, "{text}");
            }
        }
    }
}

/// One tab-separated debug line about a paused call: which process, where it sits in the
/// process tree, what it's about to change, and what the shell hook says is running.
fn describe(call: &PausedCall, s: &Session) -> String {
    let proc_info = crate::procfs::info(call.pid);
    let (comm, ppid, pgid, sid, tpgid) = match &proc_info {
        Ok(i) => (i.comm.as_str(), i.ppid, i.pgid, i.sid, i.tpgid),
        Err(_) => ("?", 0, 0, 0, 0),
    };
    let what = calls::decode(&call.name, &call.args)
        .and_then(|c| handle::prepare(call.pid, c).ok())
        .map(|p| p.describe())
        .unwrap_or_default();
    let leader = if pgid > 0 {
        crate::procfs::cmdline(pgid as u32)
    } else {
        String::new()
    };
    format!(
        "call={}\tpid={}\tcomm={comm}\tppid={ppid}\tpgid={pgid}\tsid={sid}\ttpgid={tpgid}\tpath={what}\tleader={leader}\thook={}",
        call.name,
        call.pid,
        s.current_text().unwrap_or("-"),
    )
}

/// `None` means the process is gone, so there's no one to answer.
fn decide(watcher: &dyn Watcher, call: &PausedCall, s: &mut Session) -> Option<Reply> {
    // Between commands nothing is saved.
    if !s.is_running() {
        return Some(Reply::Continue);
    }
    let Some(file_call) = calls::decode(&call.name, &call.args) else {
        return Some(Reply::Continue);
    };

    let prepared = match handle::prepare(call.pid, file_call) {
        Ok(prepared) => prepared,
        Err(e) => {
            // EFAULT: the program passed a bad pointer, and the kernel will tell it so.
            if e.raw_os_error() != Some(libc::EFAULT) && watcher.is_valid(call) {
                eprintln!(
                    "whoops: couldn't see what `{}` was about to change, so it isn't saved: {e}",
                    call.name
                );
            }
            return Some(Reply::Continue);
        }
    };

    // We just read from `/proc/<pid>/`. Make sure that pid still belongs to the paused process.
    if !watcher.is_valid(call) {
        return None;
    }
    Some(handle::act(prepared, s))
}

/// Keep reading the hook's messages even when no file-changing call comes along to trigger
/// it. Otherwise, after enough plain commands like `ls`, the pipe would fill up and the shell
/// would freeze while writing to it.
///
/// `pipe` is a second handle on the same pipe, used only to wait on. Reading happens under the
/// session lock, in [`Session::apply_marks`], so messages are applied in order no matter which
/// thread gets to them first.
pub fn drain_marks(pipe: OwnedFd, session: Arc<Mutex<Session>>) {
    loop {
        let mut fds = [PollFd::new(pipe.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::NONE) {
            Err(Errno::EINTR) => continue,
            Err(_) => return,
            Ok(_) => {}
        }
        let events = fds[0].revents().unwrap_or(PollFlags::empty());
        if events.contains(PollFlags::POLLIN) {
            lock(&session).apply_marks();
        } else if events.contains(PollFlags::POLLHUP) {
            return; // the shell and everything it started have exited
        }
    }
}
