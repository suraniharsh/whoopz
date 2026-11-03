//! A [`Watcher`] built on seccomp user notification.
//!
//! The kernel sends a notice down the listener for each paused call, and we answer on the same
//! listener. See `man 2 seccomp_unotify`.

use std::os::fd::{AsFd, AsRawFd, OwnedFd};

use anyhow::Result;
use libseccomp::{ScmpNotifReq, ScmpNotifResp, ScmpNotifRespFlags};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

use super::{PausedCall, Reply, Watcher};

pub struct SeccompWatcher {
    /// The listener the kernel sends notices on. Closing it releases every paused process.
    listener: OwnedFd,
}

impl SeccompWatcher {
    pub fn new(listener: OwnedFd) -> Self {
        Self { listener }
    }
}

impl Watcher for SeccompWatcher {
    fn next(&mut self) -> Result<Option<PausedCall>> {
        // Sleep until a notice arrives, or the listener "hangs up". The kernel hangs it up once
        // every process using the filter has exited. Receiving without this check would fail
        // over and over after the shell exits, which looks like a crash but isn't one.
        let mut fds = [PollFd::new(self.listener.as_fd(), PollFlags::POLLIN)];
        loop {
            match poll(&mut fds, PollTimeout::NONE) {
                Err(Errno::EINTR) => continue,
                result => {
                    result?;
                    break;
                }
            }
        }
        let events = fds[0].revents().unwrap_or(PollFlags::empty());
        if events.contains(PollFlags::POLLHUP) && !events.contains(PollFlags::POLLIN) {
            return Ok(None);
        }

        let req = ScmpNotifReq::receive(self.listener.as_raw_fd())?;

        // The notice carries a number; the name depends on the architecture (x86_64 or 32-bit).
        let name = req
            .data
            .syscall
            .get_name_by_arch(req.data.arch)
            .unwrap_or_else(|_| format!("syscall {}", req.data.syscall));

        Ok(Some(PausedCall {
            id: req.id,
            pid: req.pid,
            name,
        }))
    }

    fn reply(&mut self, call: &PausedCall, reply: Reply) -> Result<()> {
        let resp = match reply {
            Reply::Continue => ScmpNotifResp::new_continue(call.id, ScmpNotifRespFlags::empty()),
        };
        resp.respond(self.listener.as_raw_fd())?;
        Ok(())
    }
}
