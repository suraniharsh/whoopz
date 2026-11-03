//! Small safe wrappers around the libc file calls Whoopz needs.
//!
//! The `…at` calls take a folder plus a name relative to it. That lets the watcher act on
//! *exactly* the folder a paused program meant, even when the program's path is relative.
//!
//! Every function returns `io::Result`. When a call fails, `raw_os_error()` gives the errno,
//! which the watcher can hand back to the paused program as if its own call had failed.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use std::os::unix::ffi::OsStrExt;

/// The folder a name is relative to.
pub enum Dir {
    /// Our own handle on a folder (opened with `O_PATH`, so it can't be read, only used).
    Fd(OwnedFd),
    /// No folder needed: the name is an absolute path.
    Absolute,
}

impl Dir {
    fn raw(&self) -> RawFd {
        match self {
            Dir::Fd(fd) => fd.as_raw_fd(),
            Dir::Absolute => libc::AT_FDCWD,
        }
    }
}

/// File metadata, as returned by `fstatat`.
pub type Stat = libc::stat;

pub fn is_dir(st: &Stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFDIR
}

pub fn is_regular(st: &Stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFREG
}

pub fn is_symlink(st: &Stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFLNK
}

/// Permission bits only (rwx, setuid, setgid, sticky).
pub fn mode_bits(st: &Stat) -> u32 {
    st.st_mode & 0o7777
}

pub fn cstring(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

pub fn path_cstring(path: &Path) -> io::Result<CString> {
    cstring(path.as_os_str().as_bytes())
}

fn check(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

/// Open a folder as an `O_PATH` handle, for use as the `dir` of the other calls.
pub fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    let path = path_cstring(path)?;
    let flags = libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC;
    // SAFETY: `path` is a valid C string; the returned fd is new and owned by us.
    let fd = check(unsafe { libc::open(path.as_ptr(), flags) })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Open `name` in `dir` for reading, without following a final symlink.
pub fn open_read_nofollow(dir: &Dir, name: &CStr) -> io::Result<File> {
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: valid C string; the returned fd is new and owned by us.
    let fd = check(unsafe { libc::openat(dir.raw(), name.as_ptr(), flags) })?;
    Ok(File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
}

/// Open `name` in `dir` for reading, following symlinks like a normal `open` does.
pub fn open_read(dir: &Dir, name: &CStr) -> io::Result<File> {
    let flags = libc::O_RDONLY | libc::O_CLOEXEC;
    // SAFETY: valid C string; the returned fd is new and owned by us.
    let fd = check(unsafe { libc::openat(dir.raw(), name.as_ptr(), flags) })?;
    Ok(File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
}

fn statat(dir: &Dir, name: &CStr, flags: libc::c_int) -> io::Result<Stat> {
    let mut st = std::mem::MaybeUninit::<Stat>::uninit();
    // SAFETY: valid C string; `st` is written by the kernel on success.
    check(unsafe { libc::fstatat(dir.raw(), name.as_ptr(), st.as_mut_ptr(), flags) })?;
    Ok(unsafe { st.assume_init() })
}

/// Metadata of `name` itself, not following a final symlink (like `lstat`).
pub fn lstat_at(dir: &Dir, name: &CStr) -> io::Result<Stat> {
    statat(dir, name, libc::AT_SYMLINK_NOFOLLOW)
}

/// Metadata of what `name` points to, following symlinks (like `stat`).
pub fn stat_at(dir: &Dir, name: &CStr) -> io::Result<Stat> {
    statat(dir, name, 0)
}

/// Metadata of an open file.
pub fn fstat(file: &File) -> io::Result<Stat> {
    let mut st = std::mem::MaybeUninit::<Stat>::uninit();
    // SAFETY: `file` is an open fd; `st` is written by the kernel on success.
    check(unsafe { libc::fstat(file.as_raw_fd(), st.as_mut_ptr()) })?;
    Ok(unsafe { st.assume_init() })
}

/// Give an existing file a second name. Both names then share the same data.
pub fn link_at(old_dir: &Dir, old: &CStr, new_dir: &Dir, new: &CStr) -> io::Result<()> {
    // SAFETY: valid C strings.
    check(unsafe { libc::linkat(old_dir.raw(), old.as_ptr(), new_dir.raw(), new.as_ptr(), 0) })?;
    Ok(())
}

/// Remove a name (`flags = AT_REMOVEDIR` for an empty folder).
pub fn unlink_at(dir: &Dir, name: &CStr, flags: libc::c_int) -> io::Result<()> {
    // SAFETY: valid C string.
    check(unsafe { libc::unlinkat(dir.raw(), name.as_ptr(), flags) })?;
    Ok(())
}

/// Rename, with the `renameat2` flags (`RENAME_NOREPLACE`, `RENAME_EXCHANGE`).
///
/// Called as a raw system call: older C libraries (musl, glibc before 2.28) have no
/// `renameat2()` function, though every kernel Whoopz runs on has the call.
pub fn rename_at(
    old_dir: &Dir,
    old: &CStr,
    new_dir: &Dir,
    new: &CStr,
    flags: libc::c_uint,
) -> io::Result<()> {
    // SAFETY: valid C strings.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            old_dir.raw(),
            old.as_ptr(),
            new_dir.raw(),
            new.as_ptr(),
            flags,
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Where a symlink points.
pub fn read_link_at(dir: &Dir, name: &CStr) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: valid C string; `buf` is large enough for any link target.
    let len =
        unsafe { libc::readlinkat(dir.raw(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    if len < 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(len as usize);
    Ok(buf)
}

/// Rename between two absolute paths without replacing anything already at `to`.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    rename_at(
        &Dir::Absolute,
        &path_cstring(from)?,
        &Dir::Absolute,
        &path_cstring(to)?,
        libc::RENAME_NOREPLACE,
    )
}

/// Swap two absolute paths.
pub fn exchange(a: &Path, b: &Path) -> io::Result<()> {
    rename_at(
        &Dir::Absolute,
        &path_cstring(a)?,
        &Dir::Absolute,
        &path_cstring(b)?,
        libc::RENAME_EXCHANGE,
    )
}

/// Metadata of an absolute path, not following a final symlink. `None` if nothing is there.
pub fn lstat_path(path: &Path) -> io::Result<Option<Stat>> {
    match lstat_at(&Dir::Absolute, &path_cstring(path)?) {
        Ok(st) => Ok(Some(st)),
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(None),
        Err(e) => Err(e),
    }
}
