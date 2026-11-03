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
    let cname = sys::cstring(name)?;
    if name.first() == Some(&b'/') {
        return Ok(Location {
            dir: Dir::Absolute,
            name: cname,
            path: clean(PathBuf::from(OsStr::from_bytes(name))),
        });
    }

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
