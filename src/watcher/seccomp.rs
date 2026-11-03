//! A [`Watcher`] built on seccomp user notification.
//!
//! The kernel sends a notice down the listener for each paused call, and we answer on the same
//! listener, with three `ioctl` calls (see `man 2 seccomp_unotify`):
//!
//! - `NOTIF_RECV`: wait for the next notice (call number, architecture, arguments, pid)
//! - `NOTIF_SEND`: answer it (let it run, or hand back a result we produced ourselves)
//! - `NOTIF_ID_VALID`: is that process still paused on that call?

use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};

use anyhow::Result;
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

use super::{PausedCall, Reply, Watcher};
use crate::filter;

/// `NOTIF_ID_VALID` as Linux 5.0 to 5.16 defined it. 5.17 fixed its direction bits (the
/// number `libc` has), but still accepts this original one, so it works on every kernel.
const NOTIF_ID_VALID_ANY_KERNEL: libc::Ioctl = 0x8008_2102u32 as libc::Ioctl;

pub struct SeccompWatcher {
    /// The listener the kernel sends notices on. Closing it releases every paused process.
    listener: OwnedFd,
}

impl SeccompWatcher {
    pub fn new(listener: OwnedFd) -> Self {
        Self { listener }
    }

    fn ioctl<T>(&self, request: libc::Ioctl, arg: &mut T) -> io::Result<()> {
        loop {
            // SAFETY: `arg` is the struct this request expects, valid for the whole call.
            let ret = unsafe { libc::ioctl(self.listener.as_raw_fd(), request, arg as *mut T) };
            if ret == 0 {
                return Ok(());
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINTR) {
                return Err(err);
            }
        }
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

        // The kernel insists the struct is all zeroes going in.
        // SAFETY: seccomp_notif is plain integers; all zeroes is a valid value.
        let mut req: libc::seccomp_notif = unsafe { std::mem::zeroed() };
        self.ioctl(libc::SECCOMP_IOCTL_NOTIF_RECV, &mut req)?;

        let name = filter::name_of(req.data.arch, req.data.nr)
            .map_or_else(|| format!("syscall {}", req.data.nr), str::to_string);
        Ok(Some(PausedCall {
            id: req.id,
            pid: req.pid,
            name,
            args: req.data.args,
        }))
    }

    fn is_valid(&self, call: &PausedCall) -> bool {
        let mut id = call.id;
        self.ioctl(NOTIF_ID_VALID_ANY_KERNEL, &mut id).is_ok()
    }

    fn reply(&mut self, call: &PausedCall, reply: Reply) -> Result<()> {
        let (val, error, flags) = match reply {
            // Let the kernel run the real call.
            Reply::Continue => (0, 0, libc::SECCOMP_USER_NOTIF_FLAG_CONTINUE as u32),
            Reply::Return(value) => (value, 0, 0),
            // Errors go back as negative numbers, like a raw system call returns them.
            Reply::Fail(errno) => (0, -errno, 0),
        };
        let mut resp = libc::seccomp_notif_resp {
            id: call.id,
            val,
            error,
            flags,
        };
        self.ioctl(libc::SECCOMP_IOCTL_NOTIF_SEND, &mut resp)?;
        Ok(())
    }
}
