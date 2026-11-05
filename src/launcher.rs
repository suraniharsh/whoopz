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
use std::io::{self, IoSliceMut};
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
    AddressFamily, ControlMessageOwned, MsgFlags, SockFlag, SockType, recvmsg, socketpair,
};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::Pid;

use crate::control::SOCK_VAR;
use crate::hooks::{self, MARK_FD};
use crate::session::{self, Marks, Session};
use crate::watcher::seccomp::SeccompWatcher;
use crate::{engine, filter, procfs, standin, storage};

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
pub fn find_program(name: &str) -> Option<PathBuf> {
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
    check_system()?;
    let shell_path = choose_shell(shell_flag)?;
    let shell = shell_path.display().to_string();

    // 1. The session, the hook pipe and the messenger socket.
    let data = session::data_dir()?;
    session::clean_old(&data);
    let mut session = Session::create(&data)?;

    let (mark_read, mark_write) = nix::unistd::pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK)?;
    let mark_poll = mark_read.try_clone()?;
    let marks = Arc::new(Marks::new(File::from(mark_read)));
    session.set_marks(marks.clone());

    let sock_path = session.socket_path()?;
    let control = UnixListener::bind(&sock_path)
        .with_context(|| format!("couldn't create the socket {}", sock_path.display()))?;

    let mut cmd = Command::new(&shell_path);
    let stand_ins = standin::install(&session.dir, &mut cmd);
    cmd.env("SHELL", &shell_path)
        .env(ACTIVE_VAR, "1")
        .env(SOCK_VAR, &sock_path)
        .env("WHOOPZ_MARK_FD", MARK_FD.to_string());

    if args.is_empty() {
        let kind = hooks::detect(&shell_path);
        for prefix in hooks::housekeeping(&kind) {
            session.add_housekeeping(&prefix);
        }
        if !hooks::install(&kind, &session.dir, &mut cmd)? {
            eprintln!(
                "whoops: no command hook for {shell}, so the whole session counts as one command"
            );
            session.start_command(format!("{shell} session"), procfs::now_ticks());
        }
    } else {
        // `whoops shell -- -c "…"`: no prompt, no hook. The whole run is one command.
        cmd.args(args);
        session.start_command(args.join(" "), procfs::now_ticks());
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
    let program = filter::program();
    // SAFETY: the closure runs in the child right after fork. That is only risky when the
    // parent has other threads (one might hold a lock the child then waits on forever). We
    // haven't started any yet; they come after the spawn.
    unsafe {
        cmd.pre_exec(move || {
            place_mark_fd(mark_fd)?;
            attach_filter(&program, theirs_fd)
        });
    }
    let child = cmd.spawn().map_err(|e| explain_spawn_error(e, &shell))?;
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
        std::thread::spawn(move || engine::drain_marks(mark_poll, marks, session));
    }
    {
        let session = session.clone();
        std::thread::spawn(move || crate::control::serve(control, session, stand_ins));
    }
    let limit = storage::limit();
    std::thread::spawn(move || storage::keep_trimmed(&data, limit));

    // 4. Wait for the shell, reaping any orphans handed to us on the way.
    let code = wait_for(shell_pid);
    let _ = std::fs::remove_file(&sock_path);
    Ok(code)
}

/// Replace this process with the shell, unwatched. Only returns if that fails.
pub fn exec_unwatched(shell_flag: Option<&str>, args: &[String]) -> anyhow::Error {
    let shell = match choose_shell(shell_flag) {
        Ok(shell) => shell,
        Err(e) => return e,
    };
    let err = Command::new(&shell).args(args).exec();
    anyhow::Error::new(err).context(format!("could not start {}", shell.display()))
}

/// Stop early, with a clear reason, on systems where Whoopz can't work.
fn check_system() -> Result<()> {
    // Letting a paused call go ahead (SECCOMP_USER_NOTIF_FLAG_CONTINUE) arrived in Linux 5.5.
    let release = nix::sys::utsname::uname()?
        .release()
        .to_string_lossy()
        .into_owned();
    let mut parts = release.split(|c: char| !c.is_ascii_digit());
    let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    if (major, minor) < (5, 5) {
        bail!("Whoopz needs Linux 5.5 or newer; this is {release}");
    }

    // Whoopz reads paths out of the programs it watches. With ptrace_scope 2 or 3, Linux
    // allows that only to root (2) or nobody (3), so nothing could be saved.
    if let Ok(scope) = std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope")
        && let Ok(scope) = scope.trim().parse::<u32>()
        && scope >= 2
    {
        bail!(
            "kernel.yama.ptrace_scope is {scope}, which stops Whoopz from seeing which files \
             programs change. It works with 0 or 1 (the usual default)."
        );
    }
    Ok(())
}

/// Turn a failure to attach the filter into something a person can act on.
fn explain_spawn_error(e: io::Error, shell: &str) -> anyhow::Error {
    let why = match e.raw_os_error() {
        Some(libc::ENOSYS) => "this kernel was built without seccomp",
        Some(libc::EINVAL) => {
            "this kernel doesn't support seccomp user notification (needs Linux 5.5 or newer)"
        }
        Some(libc::EACCES) | Some(libc::EPERM) => {
            "something (a container or sandbox) doesn't allow attaching a seccomp filter"
        }
        Some(libc::EBUSY) => "this process is already being watched in a way that conflicts",
        _ => return anyhow::Error::new(e).context(format!("could not start {shell}")),
    };
    anyhow::anyhow!("could not start {shell}: {why} ({e})")
}

/// Reap children until `shell` exits. Returns its exit code (128 + signal if it was killed).
fn wait_for(shell: Pid) -> i32 {
    loop {
        match waitpid(Pid::from_raw(-1), None) {
            Ok(WaitStatus::Exited(pid, code)) if pid == shell => return code,
            Ok(WaitStatus::Signaled(pid, sig, _)) if pid == shell => return 128 + sig as i32,
            Ok(WaitStatus::Exited(pid, code)) => standin::reaped(pid.as_raw() as u32, code),
            Ok(WaitStatus::Signaled(pid, sig, _)) => {
                standin::reaped(pid.as_raw() as u32, 128 + sig as i32);
            }
            Ok(_) => continue, // the shell being stopped/continued
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
fn attach_filter(program: &[libc::sock_filter], sock: RawFd) -> io::Result<()> {
    let listener = filter::attach(program)?;

    // From here on, this process is watched. It must not make a paused call before the parent
    // has the listener: no one would be there to answer, and both sides would wait forever.
    // Sending on a socket is not a paused call, so this is safe.
    send_listener(sock, listener.as_raw_fd())
    // `listener` is closed here; the parent has its own copy now.
}

/// Send a file descriptor over a Unix socket. The kernel copies it into the other process.
///
/// Runs in the child between fork and exec, so it allocates nothing: the message lives on the
/// stack. (Allocating there is only safe while the parent has a single thread.)
fn send_listener(sock: RawFd, fd: RawFd) -> io::Result<()> {
    // The fd rides along as "ancillary data", which needs at least one real byte to travel with.
    let mut byte = *b"!";
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    // Room for one control message carrying one fd, aligned for `cmsghdr`.
    let mut space = [0u64; 4];
    // SAFETY: plain C structures; every pointer points into this stack frame, which outlives
    // the sendmsg call, and the buffer is large enough for CMSG_SPACE(one int).
    unsafe {
        let len = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
        debug_assert!(len <= std::mem::size_of_val(&space));
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = space.as_mut_ptr().cast();
        msg.msg_controllen = len as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>(), fd);
        if libc::sendmsg(sock, &msg, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
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

/// What the watcher ignores. Anything it starts has to set these back.
pub const TERMINAL_SIGNALS: [Signal; 5] = [
    Signal::SIGINT,
    Signal::SIGQUIT,
    Signal::SIGTSTP,
    Signal::SIGTTOU,
    Signal::SIGTTIN,
];

/// Keep Ctrl-C, Ctrl-\ and Ctrl-Z from stopping the watcher. They are meant for the shell.
///
/// This runs only after the shell has started: ignored signals survive exec, so doing it any
/// earlier would make Ctrl-C stop working for every program in the shell. SIGTTOU and SIGTTIN
/// are ignored so the watcher can still print to the terminal while the shell owns it.
fn ignore_terminal_signals() {
    for sig in TERMINAL_SIGNALS {
        // SAFETY: SigIgn installs no handler code, so there is nothing that could run unsafely.
        unsafe {
            let _ = signal(sig, SigHandler::SigIgn);
        }
    }
}
