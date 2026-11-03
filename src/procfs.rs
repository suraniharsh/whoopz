//! Reading from a paused process through `/proc/<pid>/`.
//!
//! A paused call's arguments are raw numbers. A path argument is the *address* of the text
//! inside the other process's memory, so we read it from `/proc/<pid>/mem`. A relative path is
//! relative to the process's current folder (`/proc/<pid>/cwd`) or to a folder it has open
//! (`/proc/<pid>/fd/<n>`).
//!
//! Linux only lets us read another process's memory if it is one of our descendants (see
//! `kernel.yama.ptrace_scope`). Every watched process is, because whoops starts the shell, and
//! the launcher makes whoops a "subreaper" so orphans are handed back to it, not to init.

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

use crate::sys::{self, Dir};

/// The longest path Linux accepts, including the closing zero byte.
const PATH_MAX: usize = libc::PATH_MAX as usize;

/// Read a zero-terminated string at `addr` in process `pid`'s memory.
pub fn read_string(pid: u32, addr: u64) -> io::Result<Vec<u8>> {
    let mem = File::open(format!("/proc/{pid}/mem"))?;
    let mut out = Vec::new();
    let mut buf = [0u8; 512];
    while out.len() < PATH_MAX {
        // A read that runs into unmapped memory stops early; that's fine as long as the zero
        // byte came first.
        let n = mem.read_at(&mut buf, addr + out.len() as u64)?;
        if n == 0 {
            return Err(io::Error::from_raw_os_error(libc::EFAULT));
        }
        if let Some(end) = buf[..n].iter().position(|&b| b == 0) {
            out.extend_from_slice(&buf[..end]);
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n]);
    }
    Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG))
}

/// Read `len` raw bytes at `addr` in process `pid`'s memory.
pub fn read_bytes(pid: u32, addr: u64, len: usize) -> io::Result<Vec<u8>> {
    let mem = File::open(format!("/proc/{pid}/mem"))?;
    let mut buf = vec![0u8; len];
    mem.read_exact_at(&mut buf, addr)?;
    Ok(buf)
}

/// A path given by a paused process, ready to act on.
pub struct Location {
    /// Our own handle on the folder the name is relative to.
    pub dir: Dir,
    /// The name exactly as the program gave it.
    pub name: CString,
    /// The full path, for the journal and for undo.
    pub path: PathBuf,
}

/// Work out where `name` (relative to the process's folder `dirfd`) really is.
pub fn resolve(pid: u32, dirfd: i32, name: &[u8]) -> io::Result<Location> {
    if name.first() == Some(&b'/') {
        // Some absolute paths mean something different in each process: `/dev/fd/3` is *its*
        // descriptor 3. Look at them from the paused process's side, not ours.
        let name = as_seen_by(pid, name);
        return Ok(Location {
            dir: Dir::Absolute,
            name: sys::cstring(&name)?,
            path: clean(PathBuf::from(OsStr::from_bytes(&name))),
        });
    }
    let cname = sys::cstring(name)?;

    let link = if dirfd == libc::AT_FDCWD {
        format!("/proc/{pid}/cwd")
    } else {
        format!("/proc/{pid}/fd/{dirfd}")
    };
    // `/proc/<pid>/cwd` is a "magic link": reading it gives the folder's path, and opening it
    // gives the folder itself, even if it was renamed meanwhile.
    let base = std::fs::read_link(&link)?;
    let dir = Dir::Fd(sys::open_dir(link.as_ref())?);
    Ok(Location {
        dir,
        name: cname,
        path: clean(base.join(OsStr::from_bytes(name))),
    })
}

/// Rewrite paths that point into the calling process itself (`/dev/fd/3`, `/dev/stdout`,
/// `/proc/self/…`) to the same place under `/proc/<pid>/`.
fn as_seen_by(pid: u32, name: &[u8]) -> Vec<u8> {
    let own = |rest: &[u8]| [format!("/proc/{pid}").as_bytes(), rest].concat();
    for (prefix, replacement) in [
        (&b"/dev/fd/"[..], &b"/fd/"[..]),
        (b"/proc/self/", b"/"),
        (b"/proc/thread-self/", b"/"),
    ] {
        if let Some(rest) = name.strip_prefix(prefix) {
            return own(&[replacement, rest].concat());
        }
    }
    for (path, fd) in [
        (&b"/dev/stdin"[..], "0"),
        (b"/dev/stdout", "1"),
        (b"/dev/stderr", "2"),
    ] {
        if name == path {
            return own(format!("/fd/{fd}").as_bytes());
        }
    }
    name.to_vec()
}

/// `/proc` and `/sys` are the kernel's views of itself, not files anyone wants back.
pub fn is_pseudo(path: &std::path::Path) -> bool {
    path.starts_with("/proc") || path.starts_with("/sys")
}

/// The path of an open file descriptor in process `pid`.
pub fn fd_path(pid: u32, fd: i32) -> io::Result<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/fd/{fd}"))
}

/// Open, for reading, the same file that process `pid` has open as `fd`.
pub fn open_fd(pid: u32, fd: i32) -> io::Result<File> {
    File::open(format!("/proc/{pid}/fd/{fd}"))
}

/// Drop `.` parts (`/a/./b` → `/a/b`). `..` stays, because a symlinked folder makes it
/// impossible to remove correctly without looking at the disk.
fn clean(path: PathBuf) -> PathBuf {
    path.components().collect()
}

/// What `/proc/<pid>/stat` says about a process's place in the process tree.
#[derive(Debug)]
pub struct ProcInfo {
    /// The program's name (at most 15 characters).
    pub comm: String,
    pub ppid: i32,
    /// Process group. An interactive shell puts each command (a pipeline counts as one) in its
    /// own group, so Ctrl-C reaches only that command.
    pub pgid: i32,
    /// Session: everything started from one terminal login.
    pub sid: i32,
    /// The process group the terminal currently sends keyboard input and Ctrl-C to.
    pub tpgid: i32,
    /// When the process started, in clock ticks since boot.
    pub start_time: u64,
}

pub fn info(pid: u32) -> io::Result<ProcInfo> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    // "1234 (name) S 1 1234 1234 34816 1234 …". The name can contain spaces and brackets,
    // so split at the *last* ')'.
    let bad = || io::Error::from_raw_os_error(libc::EINVAL);
    let open = stat.find('(').ok_or_else(bad)?;
    let close = stat.rfind(')').ok_or_else(bad)?;
    let comm = stat[open + 1..close].to_string();
    let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    let num = |i: usize| -> io::Result<i32> {
        fields.get(i).and_then(|f| f.parse().ok()).ok_or_else(bad)
    };
    // After the name: state ppid pgrp session tty_nr tpgid …
    Ok(ProcInfo {
        comm,
        ppid: num(1)?,
        pgid: num(2)?,
        sid: num(3)?,
        tpgid: num(5)?,
        start_time: fields
            .get(19)
            .and_then(|f| f.parse().ok())
            .ok_or_else(bad)?,
    })
}

/// The command line a process was started with, arguments joined by spaces.
pub fn cmdline(pid: u32) -> String {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    raw.split(|&b| b == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The unit of [`ProcInfo::start_time`] (usually 100 per second).
pub fn ticks_per_second() -> u64 {
    // SAFETY: plain call.
    unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64
}

/// Now, in the same unit as [`ProcInfo::start_time`]: clock ticks since boot.
pub fn now_ticks() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is written by the kernel.
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
    let hz = ticks_per_second();
    ts.tv_sec as u64 * hz + ts.tv_nsec as u64 * hz / 1_000_000_000
}

/// The ancestor of `pid` that the shell started directly (`pid` itself if the shell is its
/// parent). Used to name a stray process after the program the shell launched.
pub fn child_of_shell(pid: u32, shell: u32) -> u32 {
    let mut current = pid;
    for _ in 0..64 {
        match info(current) {
            Ok(i) if i.ppid as u32 == shell || i.ppid <= 1 => return current,
            Ok(i) => current = i.ppid as u32,
            Err(_) => return current,
        }
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_into_the_process_itself_are_seen_from_its_side() {
        let seen = |p: &str| String::from_utf8(as_seen_by(42, p.as_bytes())).unwrap();
        assert_eq!(seen("/dev/fd/197"), "/proc/42/fd/197");
        assert_eq!(seen("/proc/self/fd/1"), "/proc/42/fd/1");
        assert_eq!(seen("/dev/stdout"), "/proc/42/fd/1");
        assert_eq!(seen("/home/me/notes.txt"), "/home/me/notes.txt");
        assert_eq!(seen("/dev/fdx"), "/dev/fdx");
    }
}
