//! Turning a paused call's name and six raw numbers into something meaningful.
//!
//! Different calls do the same thing with arguments in different places: `unlink(path)`,
//! `unlinkat(dir, path, flags)` and `rmdir(path)` all remove a name. Decoding them into one
//! [`FileCall`] means the rest of Whoopz handles "remove" once, not three times.

/// The flags of an open call.
#[derive(Debug, Clone, Copy)]
pub enum OpenFlags {
    /// The flags are the argument itself.
    Value(i32),
    /// `openat2` passes a pointer to a `struct open_how`; the flags are its first 8 bytes.
    At(u64),
}

/// A file-changing call, with its arguments named. Paths are still addresses in the paused
/// process's memory; [`crate::handle::prepare`] reads them.
#[derive(Debug, Clone, Copy)]
pub enum FileCall {
    /// `unlink`, `unlinkat`, `rmdir`
    Remove { dir: i32, path: u64, flags: i32 },
    /// `rename`, `renameat`, `renameat2`
    Rename {
        old_dir: i32,
        old: u64,
        new_dir: i32,
        new: u64,
        flags: u32,
    },
    /// `open`, `openat`, `openat2`, `creat`
    Open {
        dir: i32,
        path: u64,
        flags: OpenFlags,
    },
    /// `truncate`
    Truncate { path: u64, len: i64 },
    /// `ftruncate`
    Ftruncate { fd: i32, len: i64 },
    /// `mkdir`, `mkdirat`, `link`, `linkat`, `symlink`, `symlinkat`: a new name appears
    Create { dir: i32, path: u64, is_dir: bool },
}

const CWD: i32 = libc::AT_FDCWD;

/// Decode a call by name. `None` for calls Whoopz doesn't handle.
pub fn decode(name: &str, a: &[u64; 6]) -> Option<FileCall> {
    // Folder fds and flags are C `int`s: only the low 32 bits of the register count.
    let int = |i: usize| a[i] as i32;

    Some(match name {
        "unlink" => FileCall::Remove {
            dir: CWD,
            path: a[0],
            flags: 0,
        },
        "rmdir" => FileCall::Remove {
            dir: CWD,
            path: a[0],
            flags: libc::AT_REMOVEDIR,
        },
        "unlinkat" => FileCall::Remove {
            dir: int(0),
            path: a[1],
            flags: int(2),
        },

        "rename" => FileCall::Rename {
            old_dir: CWD,
            old: a[0],
            new_dir: CWD,
            new: a[1],
            flags: 0,
        },
        "renameat" => FileCall::Rename {
            old_dir: int(0),
            old: a[1],
            new_dir: int(2),
            new: a[3],
            flags: 0,
        },
        "renameat2" => FileCall::Rename {
            old_dir: int(0),
            old: a[1],
            new_dir: int(2),
            new: a[3],
            flags: a[4] as u32,
        },

        "open" => FileCall::Open {
            dir: CWD,
            path: a[0],
            flags: OpenFlags::Value(int(1)),
        },
        "openat" => FileCall::Open {
            dir: int(0),
            path: a[1],
            flags: OpenFlags::Value(int(2)),
        },
        "openat2" => FileCall::Open {
            dir: int(0),
            path: a[1],
            flags: OpenFlags::At(a[2]),
        },
        // creat(path, mode) is exactly open(path, O_CREAT | O_WRONLY | O_TRUNC, mode)
        "creat" => FileCall::Open {
            dir: CWD,
            path: a[0],
            flags: OpenFlags::Value(libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC),
        },

        "truncate" => FileCall::Truncate {
            path: a[0],
            len: a[1] as i64,
        },
        "ftruncate" => FileCall::Ftruncate {
            fd: int(0),
            len: a[1] as i64,
        },

        "mkdir" => FileCall::Create {
            dir: CWD,
            path: a[0],
            is_dir: true,
        },
        "mkdirat" => FileCall::Create {
            dir: int(0),
            path: a[1],
            is_dir: true,
        },
        // link(old, new), symlink(target, new): the new name is argument 1
        "link" | "symlink" => FileCall::Create {
            dir: CWD,
            path: a[1],
            is_dir: false,
        },
        // linkat(old_dir, old, new_dir, new, flags)
        "linkat" => FileCall::Create {
            dir: int(2),
            path: a[3],
            is_dir: false,
        },
        // symlinkat(target, new_dir, new)
        "symlinkat" => FileCall::Create {
            dir: int(1),
            path: a[2],
            is_dir: false,
        },

        _ => return None,
    })
}
