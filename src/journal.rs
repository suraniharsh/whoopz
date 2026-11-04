//! The journal: one JSON line per event, appended as things happen.
//!
//! ```text
//! {"kind":"command","id":3,"text":"rm -rf build","time":1762190000}
//! {"kind":"change","cmd":3,"change":{"op":"deleted","path":"/home/me/build/a.o","saved":{"as":"stash","file":"17"}}}
//! {"kind":"undone","cmd":3}
//! ```
//!
//! Undo reads a command's changes and applies their opposites, newest first.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Line {
    /// A command started (or a background job made its first change). Only written once it
    /// changes something.
    Command {
        id: u64,
        text: String,
        time: u64,
        #[serde(default)]
        background: bool,
    },
    /// Something a command changed.
    Change { cmd: u64, change: Change },
    /// A command was undone.
    Undone { cmd: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Change {
    /// A file, symlink or empty folder was removed.
    Deleted {
        #[serde(with = "path_text")]
        path: PathBuf,
        saved: Saved,
        /// Which file it was. If that same file is still at `path` at undo time, the delete
        /// never went through (the program wasn't allowed, say), and there's nothing to undo.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        was: Option<FileId>,
    },
    /// `from` was renamed to `to`. If something was already at `to`, it was replaced.
    Renamed {
        #[serde(with = "path_text")]
        from: PathBuf,
        #[serde(with = "path_text")]
        to: PathBuf,
        replaced: Option<Saved>,
        /// Which file was at `from`. Still there at undo time means the rename never happened.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        was: Option<FileId>,
    },
    /// A file the command made itself (a temporary file) was moved over `path`, replacing what
    /// was there: `git` writing `index.lock` then renaming it to `index`, `sed -i`, editors
    /// saving safely. `saved` is the old version.
    Replaced {
        #[serde(with = "path_text")]
        path: PathBuf,
        saved: Saved,
        /// The old file. Still there at undo time means the replace never happened.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        was: Option<FileId>,
    },
    /// `a` and `b` swapped places (`renameat2` with `RENAME_EXCHANGE`).
    Exchanged {
        #[serde(with = "path_text")]
        a: PathBuf,
        #[serde(with = "path_text")]
        b: PathBuf,
    },
    /// A file's contents were emptied or shortened. `saved` holds the old contents.
    Overwritten {
        #[serde(with = "path_text")]
        path: PathBuf,
        saved: Saved,
    },
    /// A new file, symlink or hard link appeared.
    Created {
        #[serde(with = "path_text")]
        path: PathBuf,
    },
    /// A new folder appeared.
    CreatedDir {
        #[serde(with = "path_text")]
        path: PathBuf,
    },
}

/// A file's identity: the drive it's on and its number there (inode). Unlike its name, it
/// doesn't change when the file is renamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

/// What we kept of something before it was removed or overwritten.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "as", rename_all = "snake_case")]
pub enum Saved {
    /// A file in the session's stash folder: a hard link (no copy) or, across drives, a copy.
    Stash { file: String },
    /// A symlink: all there is to keep is where it pointed.
    Symlink {
        #[serde(with = "path_text")]
        target: PathBuf,
    },
    /// An empty folder: all there is to keep is its permissions.
    Dir { mode: u32 },
    /// Couldn't keep it, and why.
    Nothing { reason: String },
}

/// Append one line to the journal. One `write` call per line, so a crash of the watcher
/// right after it still leaves the line in the file.
pub fn append(journal: &mut File, line: &Line) -> io::Result<()> {
    let mut text = serde_json::to_string(line).map_err(io::Error::other)?;
    text.push('\n');
    journal.write_all(text.as_bytes())
}

/// Read every line of a journal. Lines that don't parse (for example, half-written at a crash)
/// are skipped.
pub fn read(path: &Path) -> io::Result<Vec<Line>> {
    let file = File::open(path)?;
    let mut lines = Vec::new();
    for text in BufReader::new(file).lines() {
        if let Ok(line) = serde_json::from_str(&text?) {
            lines.push(line);
        }
    }
    Ok(lines)
}

/// Linux paths are bytes and don't have to be valid UTF-8, but JSON strings do. Valid text is
/// stored as it is; any other byte is written as `%XX`, and `%` itself as `%25`.
mod path_text {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(path: &Path, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&encode(path.as_os_str().as_bytes()))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PathBuf, D::Error> {
        let text = String::deserialize(d)?;
        Ok(PathBuf::from(OsStr::from_bytes(&decode(&text))))
    }

    pub fn encode(bytes: &[u8]) -> String {
        let mut out = String::new();
        for chunk in bytes.utf8_chunks() {
            for c in chunk.valid().chars() {
                if c == '%' {
                    out.push_str("%25");
                } else {
                    out.push(c);
                }
            }
            for b in chunk.invalid() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    }

    pub fn decode(text: &str) -> Vec<u8> {
        let bytes = text.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%'
                && let Some(hex) = text.get(i + 1..i + 3)
                && let Ok(b) = u8::from_str_radix(hex, 16)
            {
                out.push(b);
                i += 3;
                continue;
            }
            out.push(bytes[i]);
            i += 1;
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn round_trips_any_bytes() {
            for path in [
                &b"/home/me/notes.txt"[..],
                b"/tmp/100%",
                b"/tmp/caf\xe9",
                b"/x/%41",
            ] {
                assert_eq!(decode(&encode(path)), path);
            }
            assert_eq!(encode(b"/tmp/caf\xe9"), "/tmp/caf%E9");
        }
    }
}
