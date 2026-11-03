//! `whoops shell`: start your shell with the filter attached, and run the watcher next to it.
//!
//! ```text
//! whoops (this process, never filtered)
//!   │ 1. create the session folder, the hook pipe and the messenger socket
//!   │ 2. fork. In the child, before it becomes the shell:
//!   │      a. put the hook pipe at fd 197    → the shell hook writes "start"/"end" there
//!   │      b. attach the filter               → from now on the child is watched
//!   │      c. send the listener to us         → over a socket pair
//!   │      d. exec the shell                  → the filter stays on, and every program
//!   │                                           the shell starts inherits it
//!   │ 3. receive the listener and start three threads:
//!   │      watcher  answers paused calls
//!   │      marks    keeps the hook pipe drained
//!   │      control  answers `whoops` / `whoops list`
//!   │ 4. reap children until the shell exits, then exit with its code
//! ```

use std::fs::File;
use std::io::{self, IoSlice, IoSliceMut};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use nix::fcntl::OFlag;
use nix::sys::signal::{SigHandler, Signal, signal};
use nix::sys::socket::{
    AddressFamily, ControlMessage, ControlMessageOwned, MsgFlags, SockFlag, SockType, recvmsg,
    sendmsg, socketpair,
};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::Pid;

use crate::control::SOCK_VAR;
use crate::hooks::{self, MARK_FD};
use crate::session::{self, Marks, Session};
use crate::watcher::seccomp::SeccompWatcher;
use crate::{engine, filter};

/// Set inside a watched shell, so `whoops shell` can't be started inside itself.
const ACTIVE_VAR: &str = "WHOOPZ_ACTIVE";

/// Set to 1 to print every paused call and the answer it got, or to a path to write it there.
const DEBUG_VAR: &str = "WHOOPZ_DEBUG";

/// Shells `whoops shell` recognises when it's typed in one.
const KNOWN_SHELLS: &[&str] = &[
    "bash", "zsh", "fish", "nu", "elvish", "tcsh", "csh", "dash", "ksh", "mksh", "yash", "sh",
];

/// Which shell to start: `--shell` if given, else the shell `whoops shell` was typed in, else
/// `$SHELL`. Your login shell (`$SHELL`) isn't always the one you use: a terminal can be set to
/// start fish while `$SHELL` still says bash.
fn choose_shell(flag: Option<&str>) -> Result<PathBuf> {
    if let Some(name) = flag {
        return find_program(name).with_context(|| format!("couldn't find the shell {name}"));
    }
    let parent = std::fs::read_link(format!("/proc/{}/exe", std::os::unix::process::parent_id()));
    if let Ok(exe) = parent
        && exe
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| KNOWN_SHELLS.contains(&n))
    {
        return Ok(exe);
    }
    Ok(std::env::var_os("SHELL")
        .filter(|s| !s.is_empty())
        .map_or_else(|| PathBuf::from("/bin/sh"), PathBuf::from))
}

/// A path as given, or a name looked up in `$PATH`.
fn find_program(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        return Some(PathBuf::from(name));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Start the shell under the watcher and return its exit code once it ends.
pub fn run_shell(shell_flag: Option<&str>, args: &[String]) -> Result<i32> {
    if std::env::var_os(ACTIVE_VAR).is_some() {
        bail!("already inside a whoops shell");
    }
    let shell_path = choose_shell(shell_flag)?;
    let shell = shell_path.display().to_string();

    // 1. The session, the hook pipe and the messenger socket.
    let data = session::data_dir()?;
    session::clean_old(&data);
    let mut session = Session::create(&data)?;

    let (mark_read, mark_write) = nix::unistd::pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK)?;
    let mark_poll = mark_read.try_clone()?;
    session.set_marks(Marks::new(File::from(mark_read)));

    let sock_path = session.socket_path()?;
    let control = UnixListener::bind(&sock_path)
        .with_context(|| format!("couldn't create the socket {}", sock_path.display()))?;

    let mut cmd = Command::new(&shell_path);
    cmd.env("SHELL", &shell_path)
        .env(ACTIVE_VAR, "1")
        .env(SOCK_VAR, &sock_path)
        .env("WHOOPZ_MARK_FD", MARK_FD.to_string());

    if args.is_empty() {
        let kind = hooks::detect(&shell_path);
        if !hooks::install(&kind, &session.dir, &mut cmd)? {
            eprintln!(
                "whoops: no command hook for {shell}, so the whole session counts as one command"
            );
            session.start_command(format!("{shell} session"));
        }
    } else {
        // `whoops shell -- -c "…"`: no prompt, no hook. The whole run is one command.
        cmd.args(args);
        session.start_command(args.join(" "));
    }

    // Orphaned processes (a program that "daemonizes") get handed to us instead of init. They
    // stay our descendants, which Linux requires before we may read their memory.
    // SAFETY: plain prctl with integer arguments.
    unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };

    // 2. Fork and exec. `pre_exec` runs in the child, after the fork and before the exec.
    let (ours, theirs) = socketpair(
        AddressFamily::Unix,
        SockType::Stream,
        None,
        SockFlag::SOCK_CLOEXEC,
    )?;
    let theirs_fd = theirs.as_raw_fd();
    let mark_fd = mark_write.as_raw_fd();
    // SAFETY: the closure runs in the child right after fork. That is only risky when the
    // parent has other threads (one might hold a lock the child then waits on forever). We
    // haven't started any yet; they come after the spawn.
    unsafe {
        cmd.pre_exec(move || {
            place_mark_fd(mark_fd)?;
            attach_filter(theirs_fd).map_err(io::Error::other)
        });
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("could not start {shell}"))?;
    let shell_pid = Pid::from_raw(child.id() as i32);
    session.set_shell_pid(child.id());

    // Close our copies of the child's ends. If the child dies before sending the listener, its
    // copy closes too, and `receive_listener` gets "end of file" instead of waiting forever.
    drop(theirs);
    drop(mark_write);

    // 3. Receive the listener and start the threads.
    let listener = receive_listener(&ours)?;
    ignore_terminal_signals();

    let session = Arc::new(Mutex::new(session));
    let mut debug = engine::Debug::from_env(std::env::var_os(DEBUG_VAR));
    debug.line(&format!("shell={shell}\tshell_pid={shell_pid}"));
    let watcher = Box::new(SeccompWatcher::new(listener));
    {
        let session = session.clone();
        std::thread::spawn(move || engine::run(watcher, session, debug));
    }
    {
        let session = session.clone();
        std::thread::spawn(move || engine::drain_marks(mark_poll, session));
    }
    {
        let session = session.clone();
        std::thread::spawn(move || crate::control::serve(control, session));
    }

    // 4. Wait for the shell, reaping any orphans handed to us on the way.
    let code = wait_for(shell_pid);
    let _ = std::fs::remove_file(&sock_path);
    Ok(code)
}

/// Reap children until `shell` exits. Returns its exit code (128 + signal if it was killed).
fn wait_for(shell: Pid) -> i32 {
    loop {
        match waitpid(Pid::from_raw(-1), None) {
            Ok(WaitStatus::Exited(pid, code)) if pid == shell => return code,
            Ok(WaitStatus::Signaled(pid, sig, _)) if pid == shell => return 128 + sig as i32,
            Ok(_) => continue, // an orphan, or the shell being stopped/continued
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => return 1, // no children left at all
        }
    }
}

/// Runs in the child: put the hook pipe at [`MARK_FD`], where the hook expects it.
fn place_mark_fd(fd: RawFd) -> io::Result<()> {
    // SAFETY: plain fd calls on fds we own.
    unsafe {
        if fd == MARK_FD {
            // Already in place: only the close-on-exec flag has to go, so it survives exec.
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
        } else if libc::dup2(fd, MARK_FD) < 0 {
            // dup2 gives the copy no close-on-exec flag, so the shell inherits it.
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
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
