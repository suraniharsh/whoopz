//! `whoops shell`: start your shell with the filter attached, and run the watcher next to it.
//!
//! ```text
//! whoops (this process, never filtered)
//!   │ 1. make a socket pair: a private line between us and the child
//!   │ 2. fork. In the child, before it becomes the shell:
//!   │      a. attach the filter          → from now on the child is watched
//!   │      b. send the listener to us    → over the socket pair
//!   │      c. exec the shell             → the filter stays on, and every program
//!   │                                      the shell starts inherits it
//!   │ 3. receive the listener and start the watcher on its own thread
//!   │ 4. wait for the shell to exit, and exit with its code
//! ```

use std::io::{self, IoSlice, IoSliceMut};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::Command;

use anyhow::{Context, Result, bail};
use nix::sys::signal::{SigHandler, Signal, signal};
use nix::sys::socket::{
    AddressFamily, ControlMessage, ControlMessageOwned, MsgFlags, SockFlag, SockType, recvmsg,
    sendmsg, socketpair,
};

use crate::filter;
use crate::watcher::{self, seccomp::SeccompWatcher};

/// Set inside a watched shell, so `whoops shell` can't be started inside itself.
const ACTIVE_VAR: &str = "WHOOPZ_ACTIVE";

/// Start the shell under the watcher and return its exit code once it ends.
pub fn run_shell(args: &[String]) -> Result<i32> {
    if std::env::var_os(ACTIVE_VAR).is_some() {
        bail!("already inside a whoops shell");
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());

    // 1. The socket pair. CLOEXEC: neither end leaks into the shell once it starts.
    let (ours, theirs) = socketpair(
        AddressFamily::Unix,
        SockType::Stream,
        None,
        SockFlag::SOCK_CLOEXEC,
    )?;
    let theirs_fd = theirs.as_raw_fd();

    // 2. Fork and exec. `pre_exec` runs in the child, after the fork and before the exec.
    let mut cmd = Command::new(&shell);
    cmd.args(args).env(ACTIVE_VAR, "1");
    // SAFETY: the closure runs in the child right after fork. That is only risky when the
    // parent has other threads (one might hold a lock the child then waits on forever). We
    // haven't started any yet; the watcher thread comes after the spawn.
    unsafe {
        cmd.pre_exec(move || attach_filter(theirs_fd).map_err(io::Error::other));
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("could not start {shell}"))?;

    // Close our copy of the child's end. If the child dies before sending the listener, its
    // copy closes too, and `receive_listener` gets "end of file" instead of waiting forever.
    drop(theirs);

    // 3. Receive the listener and start watching.
    let listener = receive_listener(&ours)?;
    ignore_terminal_signals();
    let mut seccomp = SeccompWatcher::new(listener);
    std::thread::spawn(move || watcher::run(&mut seccomp));

    // 4. Wait for the shell. Exiting the process also stops the watcher thread.
    let status = child.wait()?;
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
}

/// Runs in the child: attach the filter, then hand the listener to the parent.
fn attach_filter(sock: RawFd) -> Result<()> {
    let ctx = filter::build()?;

    // Loading also sets `no_new_privs`, which Linux requires before a normal user may attach a
    // filter. It means setuid programs like `sudo` can't gain privileges inside the shell.
    ctx.load()?;

    // From here on, this process is watched. It must not make a paused call before the parent
    // has the listener: no one would be there to answer, and both sides would wait forever.
    // Sending on a socket is not a paused call, so this is safe.
    let listener = ctx.get_notify_fd()?;
    send_listener(sock, listener)?;

    // The listener was opened with CLOEXEC, so the exec into the shell closes our copy.
    Ok(())
}

/// Send a file descriptor over a Unix socket. The kernel copies it into the other process.
fn send_listener(sock: RawFd, fd: RawFd) -> Result<()> {
    let fds = [fd];
    let cmsgs = [ControlMessage::ScmRights(&fds)];
    // The fd rides along as "ancillary data", which needs at least one real byte to travel with.
    let iov = [IoSlice::new(b"!")];
    sendmsg::<()>(sock, &iov, &cmsgs, MsgFlags::empty(), None)
        .context("could not send the listener to the watcher")?;
    Ok(())
}

/// Receive the file descriptor the child sent with [`send_listener`].
fn receive_listener(sock: &OwnedFd) -> Result<OwnedFd> {
    let mut byte = [0u8; 1];
    let mut iov = [IoSliceMut::new(&mut byte)];
    let mut space = nix::cmsg_space!(RawFd);
    let msg = recvmsg::<()>(
        sock.as_raw_fd(),
        &mut iov,
        Some(&mut space[..]),
        MsgFlags::MSG_CMSG_CLOEXEC,
    )
    .context("could not receive the listener from the shell")?;

    for cmsg in msg.cmsgs()? {
        if let ControlMessageOwned::ScmRights(fds) = cmsg
            && let Some(&fd) = fds.first()
        {
            // SAFETY: the kernel just created this fd for us, and nothing else owns it.
            return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
        }
    }
    bail!("the shell exited before Whoopz could start watching it")
}

/// Keep Ctrl-C, Ctrl-\ and Ctrl-Z from stopping the watcher. They are meant for the shell.
///
/// This runs only after the shell has started: ignored signals survive exec, so doing it any
/// earlier would make Ctrl-C stop working for every program in the shell. SIGTTOU and SIGTTIN
/// are ignored so the watcher can still print to the terminal while the shell owns it.
fn ignore_terminal_signals() {
    for sig in [
        Signal::SIGINT,
        Signal::SIGQUIT,
        Signal::SIGTSTP,
        Signal::SIGTTOU,
        Signal::SIGTTIN,
    ] {
        // SAFETY: SigIgn installs no handler code, so there is nothing that could run unsafely.
        unsafe {
            let _ = signal(sig, SigHandler::SigIgn);
        }
    }
}
