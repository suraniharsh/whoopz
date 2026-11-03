//! What the watcher does with each paused call: save what's about to be lost, then let the
//! change happen.
//!
//! Two phases, because between them the watcher checks that the paused process is still the
//! same one (a pid can be reused once a process dies):
//!
//! 1. [`prepare`] reads the call's paths out of the paused process's memory.
//! 2. [`act`] saves and journals, and answers the call.
//!
//! **Removes and renames the watcher does itself** and hands the result back. That way it
//! knows whether they worked, and it acts on exactly the path it saved, so the program can't
//! change the path in between. **Opens, truncates and creates** go ahead in the kernel
//! (`Continue`), because the program needs its own file descriptor back.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use crate::calls::{FileCall, OpenFlags};
use crate::journal::{Change, Saved};
use crate::procfs::{self, Location};
use crate::session::{MAX_COPY, Session};
use crate::sys;
use crate::watcher::Reply;

/// A [`FileCall`] with its paths read and resolved.
pub enum Prepared {
    Remove {
        at: Location,
        flags: i32,
    },
    Rename {
        from: Location,
        to: Location,
        flags: u32,
    },
    Open {
        at: Location,
        flags: i32,
    },
    Truncate {
        at: Location,
        len: i64,
    },
    Ftruncate {
        file: File,
        path: PathBuf,
        len: i64,
    },
    Create {
        at: Location,
        is_dir: bool,
    },
}

impl Prepared {
    /// The paths this call is about.
    pub fn paths(&self) -> Vec<&Path> {
        match self {
            Prepared::Remove { at, .. }
            | Prepared::Open { at, .. }
            | Prepared::Truncate { at, .. }
            | Prepared::Create { at, .. } => vec![&at.path],
            Prepared::Rename { from, to, .. } => vec![&from.path, &to.path],
            Prepared::Ftruncate { path, .. } => vec![path],
        }
    }

    /// The paths this call is about, for debug output.
    pub fn describe(&self) -> String {
        match self {
            Prepared::Remove { at, .. }
            | Prepared::Open { at, .. }
            | Prepared::Truncate { at, .. }
            | Prepared::Create { at, .. } => at.path.display().to_string(),
            Prepared::Rename { from, to, .. } => {
                format!("{} -> {}", from.path.display(), to.path.display())
            }
            Prepared::Ftruncate { path, .. } => path.display().to_string(),
        }
    }
}

/// Read the paths a paused call points to.
pub fn prepare(pid: u32, call: FileCall) -> io::Result<Prepared> {
    let at = |dir: i32, addr: u64| -> io::Result<Location> {
        procfs::resolve(pid, dir, &procfs::read_string(pid, addr)?)
    };

    Ok(match call {
        FileCall::Remove { dir, path, flags } => Prepared::Remove {
            at: at(dir, path)?,
            flags,
        },
        FileCall::Rename {
            old_dir,
            old,
            new_dir,
            new,
            flags,
        } => Prepared::Rename {
            from: at(old_dir, old)?,
            to: at(new_dir, new)?,
            flags,
        },
        FileCall::Open { dir, path, flags } => {
            let flags = match flags {
                OpenFlags::Value(flags) => flags,
                OpenFlags::At(how) => {
                    let bytes = procfs::read_bytes(pid, how, 8)?;
                    u64::from_ne_bytes(bytes.try_into().expect("read 8 bytes")) as i32
                }
            };
            Prepared::Open {
                at: at(dir, path)?,
                flags,
            }
        }
        FileCall::Truncate { path, len } => Prepared::Truncate {
            at: at(libc::AT_FDCWD, path)?,
            len,
        },
        FileCall::Ftruncate { fd, len } => Prepared::Ftruncate {
            file: procfs::open_fd(pid, fd)?,
            path: procfs::fd_path(pid, fd)?,
            len,
        },
        FileCall::Create { dir, path, is_dir } => Prepared::Create {
            at: at(dir, path)?,
            is_dir,
        },
    })
}

/// Save, journal, and decide the answer to the paused call.
pub fn act(call: Prepared, s: &mut Session) -> Reply {
    match call {
        Prepared::Remove { at, flags } => remove(s, at, flags),
        Prepared::Rename { from, to, flags } => rename(s, from, to, flags),
        Prepared::Open { at, flags } => {
            open(s, &at, flags);
            Reply::Continue
        }
        Prepared::Truncate { at, len } => {
            if let Ok(st) = sys::stat_at(&at.dir, &at.name)
                && let Ok(file) = sys::open_read(&at.dir, &at.name)
            {
                shrink(s, &at.path, &file, &st, len);
            }
            Reply::Continue
        }
        Prepared::Ftruncate { file, path, len } => {
            if let Ok(st) = sys::fstat(&file) {
                shrink(s, &path, &file, &st, len);
            }
            Reply::Continue
        }
        Prepared::Create { at, is_dir } => {
            // Only something that isn't there yet is "created".
            if sys::lstat_at(&at.dir, &at.name).is_err() && s.first_time(&at.path) {
                let change = if is_dir {
                    Change::CreatedDir { path: at.path }
                } else {
                    Change::Created { path: at.path }
                };
                journal(s, change);
            }
            Reply::Continue
        }
    }
}

/// `rm`: keep the file under a second name in the stash, then remove the original name.
fn remove(s: &mut Session, at: Location, flags: i32) -> Reply {
    let saved = if flags & libc::AT_REMOVEDIR != 0 {
        // Only empty folders can be removed, so all there is to keep is the permissions.
        match sys::lstat_at(&at.dir, &at.name) {
            Ok(st) if sys::is_dir(&st) => Some(Saved::Dir {
                mode: sys::mode_bits(&st),
            }),
            _ => None,
        }
    } else {
        save_existing(s, &at)
    };

    match sys::unlink_at(&at.dir, &at.name, flags) {
        Ok(()) => {
            if let Some(saved) = saved {
                journal(
                    s,
                    Change::Deleted {
                        path: at.path,
                        saved,
                    },
                );
            }
            Reply::Return(0)
        }
        Err(e) => {
            forget(s, saved);
            Reply::Fail(errno(&e))
        }
    }
}

/// `mv`: write down both names. If something is about to be replaced, keep it first.
fn rename(s: &mut Session, from: Location, to: Location, flags: u32) -> Reply {
    let exchange = flags & libc::RENAME_EXCHANGE != 0;
    let noreplace = flags & libc::RENAME_NOREPLACE != 0;

    // Renaming a file onto another name for the same file changes nothing.
    let same = match (
        sys::lstat_at(&from.dir, &from.name),
        sys::lstat_at(&to.dir, &to.name),
    ) {
        (Ok(a), Ok(b)) => a.st_dev == b.st_dev && a.st_ino == b.st_ino,
        _ => false,
    };

    let replaced = if exchange || noreplace || same {
        None
    } else {
        save_existing(s, &to)
    };

    match sys::rename_at(&from.dir, &from.name, &to.dir, &to.name, flags) {
        Ok(()) => {
            if !same {
                let change = if exchange {
                    Change::Exchanged {
                        a: from.path,
                        b: to.path,
                    }
                } else {
                    Change::Renamed {
                        from: from.path,
                        to: to.path,
                        replaced,
                    }
                };
                journal(s, change);
            }
            Reply::Return(0)
        }
        Err(e) => {
            forget(s, replaced);
            Reply::Fail(errno(&e))
        }
    }
}

/// `> file`, `cp` onto a file: keep a copy of the old contents. A new file is just noted.
fn open(s: &mut Session, at: &Location, flags: i32) {
    let creates = flags & libc::O_CREAT != 0;
    let truncates = flags & libc::O_TRUNC != 0;

    match sys::stat_at(&at.dir, &at.name) {
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
            if creates && s.first_time(&at.path) {
                journal(
                    s,
                    Change::Created {
                        path: at.path.clone(),
                    },
                );
            }
        }
        Ok(st) if truncates => {
            if let Ok(file) = sys::open_read(&at.dir, &at.name) {
                shrink(s, &at.path, &file, &st, 0);
            }
        }
        _ => {}
    }
}

/// A file is about to be cut down to `len` bytes. Keep a copy if that loses anything.
fn shrink(s: &mut Session, path: &Path, file: &File, st: &sys::Stat, len: i64) {
    // Only real files hold data worth keeping (`> /dev/null` truncates too).
    if !sys::is_regular(st) || len >= st.st_size || !s.first_time(path) {
        return;
    }
    let saved = copy_to_stash(s, file, st).unwrap_or_else(|reason| {
        warn(path, &reason);
        Saved::Nothing { reason }
    });
    journal(
        s,
        Change::Overwritten {
            path: path.to_path_buf(),
            saved,
        },
    );
}

/// Keep whatever is at `at` before it disappears. `None` if nothing is there.
fn save_existing(s: &mut Session, at: &Location) -> Option<Saved> {
    let st = sys::lstat_at(&at.dir, &at.name).ok()?;

    if sys::is_dir(&st) {
        return Some(Saved::Dir {
            mode: sys::mode_bits(&st),
        });
    }
    if sys::is_symlink(&st) {
        return Some(match sys::read_link_at(&at.dir, &at.name) {
            Ok(target) => Saved::Symlink {
                target: PathBuf::from(OsString::from_vec(target)),
            },
            Err(e) => not_saved(&at.path, format!("couldn't read the symlink: {e}")),
        });
    }

    // The trick that makes this cheap: a second name for the same data. No copying, so even
    // a huge file is saved instantly.
    let (name, cname) = s.new_stash_name();
    let linked = sys::link_at(&at.dir, &at.name, s.stash_dir(), &cname);
    if linked.is_ok() {
        return Some(Saved::Stash { file: name });
    }

    // Hard links only work within one drive. Across drives, fall back to a copy.
    if !sys::is_regular(&st) {
        return Some(not_saved(&at.path, "it's a special file".into()));
    }
    let copied = sys::open_read_nofollow(&at.dir, &at.name)
        .map_err(|e| format!("couldn't open it: {e}"))
        .and_then(|file| copy_to_stash(s, &file, &st));
    Some(copied.unwrap_or_else(|reason| not_saved(&at.path, reason)))
}

/// Copy a file's contents into the stash, keeping its permissions and modification time.
fn copy_to_stash(s: &mut Session, file: &File, st: &sys::Stat) -> Result<Saved, String> {
    let size = st.st_size as u64;
    if size > MAX_COPY {
        return Err(format!(
            "too big to copy ({} MB, the limit is {} MB)",
            size >> 20,
            MAX_COPY >> 20
        ));
    }

    let (name, _) = s.new_stash_name();
    let dest = s.stash_path(&name);
    let result = (|| -> io::Result<()> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&dest)?;
        // `io::copy` uses `copy_file_range` on Linux, so the kernel does the copying.
        io::copy(&mut &*file, &mut out)?;
        out.set_permissions(std::fs::Permissions::from_mode(sys::mode_bits(st)))?;
        let mtime = UNIX_EPOCH + Duration::new(st.st_mtime as u64, st.st_mtime_nsec as u32);
        out.set_modified(mtime)?;
        Ok(())
    })();

    match result {
        Ok(()) => Ok(Saved::Stash { file: name }),
        Err(e) => {
            s.discard_stash(&name);
            Err(format!("couldn't copy it: {e}"))
        }
    }
}

fn not_saved(path: &Path, reason: String) -> Saved {
    warn(path, &reason);
    Saved::Nothing { reason }
}

/// The change didn't happen after all: drop what we saved for it.
fn forget(s: &Session, saved: Option<Saved>) {
    if let Some(Saved::Stash { file }) = saved {
        s.discard_stash(&file);
    }
}

fn journal(s: &mut Session, change: Change) {
    if let Err(e) = s.record(change) {
        eprintln!("whoops: couldn't write the journal: {e}");
    }
}

fn warn(path: &Path, reason: &str) {
    eprintln!("whoops: couldn't save {}: {reason}", path.display());
}

fn errno(e: &io::Error) -> i32 {
    e.raw_os_error().unwrap_or(libc::EIO)
}
