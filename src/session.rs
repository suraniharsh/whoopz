//! A session: one `whoops shell`, from start to exit, and everything it keeps on disk.
//!
//! ```text
//! ~/.local/share/whoopz/sessions/<time>-<pid>/
//!     journal.jsonl    what changed, per command
//!     stash/           saved files, named 1, 2, 3, …
//!     lock             held while the session runs, so cleanup never removes a live one
//!     bashrc, zdotdir/ the shell hooks
//! ```
//!
//! The session also tracks which command is running. The shell hook sends "start <text>"
//! before each command line and "end" when the prompt comes back. Changes made between
//! commands (by prompt tools, or the shell saving its history) are let through without saving.

use std::collections::HashSet;
use std::ffi::CString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::journal::{self, Change, Line};
use crate::sys::{self, Dir};

/// Files bigger than this are not copied (hard links have no limit).
pub const MAX_COPY: u64 = 512 * 1024 * 1024;

/// Finished sessions are removed after this long.
const KEEP_FOR: Duration = Duration::from_secs(7 * 24 * 60 * 60);

enum State {
    /// Between commands: nothing is saved.
    Idle,
    Running {
        id: u64,
        text: String,
        /// The command line goes in the journal only once the command changes something.
        written: bool,
        /// Paths whose original state this command already saved. A program that rewrites
        /// the same file a hundred times only needs the first version kept.
        seen: HashSet<PathBuf>,
    },
}

pub struct Session {
    pub dir: PathBuf,
    stash_dir: Dir,
    journal: File,
    _lock: File,
    marks: Option<Marks>,
    next_cmd: u64,
    next_stash: u64,
    state: State,
}

/// `$XDG_DATA_HOME/whoopz`, or `~/.local/share/whoopz`.
pub fn data_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let home = std::env::var_os("HOME").context("HOME is not set")?;
            PathBuf::from(home).join(".local/share")
        }
    };
    Ok(base.join("whoopz"))
}

/// Folders only the user can open: the stash holds copies of their files.
fn private_dir(path: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)
}

impl Session {
    pub fn create(data: &Path) -> Result<Session> {
        let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let dir = data
            .join("sessions")
            .join(format!("{secs}-{}", std::process::id()));
        private_dir(&dir.join("stash"))
            .with_context(|| format!("could not create {}", dir.display()))?;

        let lock = File::create(dir.join("lock"))?;
        lock.try_lock().context("could not lock the session")?;
        let journal = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("journal.jsonl"))?;
        let stash_dir = Dir::Fd(sys::open_dir(&dir.join("stash"))?);

        Ok(Session {
            dir,
            stash_dir,
            journal,
            _lock: lock,
            marks: None,
            next_cmd: 1,
            next_stash: 1,
            state: State::Idle,
        })
    }

    /// Where `whoops` (the messenger) asks for an undo.
    ///
    /// Socket paths are limited to 108 bytes, so it goes in `$XDG_RUNTIME_DIR` (short, and
    /// private to the user) when there is one. A socket with no file at all (an "abstract"
    /// socket) would avoid the limit, but anyone on the machine could connect to it.
    pub fn socket_path(&self) -> io::Result<PathBuf> {
        let id = self.dir.file_name().unwrap_or_default();
        match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(run) if !run.is_empty() => {
                let dir = PathBuf::from(run).join("whoopz");
                private_dir(&dir)?;
                Ok(dir.join(id).with_extension("sock"))
            }
            _ => Ok(self.dir.join("sock")),
        }
    }

    pub fn journal_path(&self) -> PathBuf {
        self.dir.join("journal.jsonl")
    }

    // --- which command is running ---

    pub fn start_command(&mut self, text: String) {
        let id = self.next_cmd;
        self.next_cmd += 1;
        self.state = State::Running {
            id,
            text,
            written: false,
            seen: HashSet::new(),
        };
    }

    pub fn end_command(&mut self) {
        self.state = State::Idle;
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, State::Running { .. })
    }

    /// True the first time this command touches `path`.
    pub fn first_time(&mut self, path: &Path) -> bool {
        match &mut self.state {
            State::Running { seen, .. } => seen.insert(path.to_path_buf()),
            State::Idle => false,
        }
    }

    pub fn set_marks(&mut self, marks: Marks) {
        self.marks = Some(marks);
    }

    /// Apply every "start"/"end" message the shell hook has sent so far.
    ///
    /// Called before handling each paused call. The hook writes "start" before the command
    /// runs, so by the time one of its calls is paused, the message is already in the pipe.
    pub fn apply_marks(&mut self) {
        let Some(marks) = &mut self.marks else { return };
        for mark in marks.drain() {
            match mark {
                Mark::Start(text) => self.start_command(text),
                Mark::End => self.end_command(),
            }
        }
    }

    // --- the journal ---

    /// Record a change made by the running command.
    pub fn record(&mut self, change: Change) -> io::Result<()> {
        let State::Running {
            id, text, written, ..
        } = &mut self.state
        else {
            return Ok(());
        };
        let cmd = *id;
        if !*written {
            let time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let line = Line::Command {
                id: cmd,
                text: text.clone(),
                time,
            };
            journal::append(&mut self.journal, &line)?;
            *written = true;
        }
        journal::append(&mut self.journal, &Line::Change { cmd, change })
    }

    pub fn record_undone(&mut self, cmd: u64) -> io::Result<()> {
        journal::append(&mut self.journal, &Line::Undone { cmd })
    }

    // --- the stash ---

    pub fn stash_dir(&self) -> &Dir {
        &self.stash_dir
    }

    /// A fresh name in the stash, as a string (for the journal) and a C string (for the calls).
    pub fn new_stash_name(&mut self) -> (String, CString) {
        let name = self.next_stash.to_string();
        self.next_stash += 1;
        let cname = CString::new(name.clone()).expect("digits contain no zero byte");
        (name, cname)
    }

    pub fn stash_path(&self, name: &str) -> PathBuf {
        self.dir.join("stash").join(name)
    }

    /// Remove a stash file that turned out not to be needed.
    pub fn discard_stash(&self, name: &str) {
        let _ = fs::remove_file(self.stash_path(name));
    }
}

/// Remove sessions that ended more than [`KEEP_FOR`] ago.
pub fn clean_old(data: &Path) {
    let Ok(entries) = fs::read_dir(data.join("sessions")) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        // A session whose lock we can take has ended (or crashed).
        let ended = File::open(dir.join("lock")).is_ok_and(|lock| lock.try_lock().is_ok());
        let last_used = fs::metadata(dir.join("journal.jsonl"))
            .or_else(|_| fs::metadata(&dir))
            .and_then(|m| m.modified());
        let old = last_used.is_ok_and(|t| t.elapsed().is_ok_and(|age| age > KEEP_FOR));
        if ended && old {
            let _ = fs::remove_dir_all(&dir);
        }
    }
}

/// A message from the shell hook.
#[derive(Debug, PartialEq)]
pub enum Mark {
    Start(String),
    End,
}

/// The reading end of the pipe the shell hook writes to. Messages end with a zero byte:
/// `start rm -rf build\0`, `end\0`.
pub struct Marks {
    pipe: File,
    partial: Vec<u8>,
}

impl Marks {
    /// `pipe` must be non-blocking, so draining stops when it's empty instead of waiting.
    pub fn new(pipe: File) -> Self {
        Self {
            pipe,
            partial: Vec::new(),
        }
    }

    fn drain(&mut self) -> Vec<Mark> {
        let mut buf = [0u8; 4096];
        loop {
            match self.pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => self.partial.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break, // WouldBlock: nothing more for now
            }
        }

        let mut marks = Vec::new();
        while let Some(end) = self.partial.iter().position(|&b| b == 0) {
            let msg: Vec<u8> = self.partial.drain(..=end).collect();
            let msg = String::from_utf8_lossy(&msg[..end]);
            if msg == "end" {
                marks.push(Mark::End);
            } else if let Some(text) = msg.strip_prefix("start ") {
                marks.push(Mark::Start(text.trim().to_string()));
            }
            // Anything else isn't from our hook; ignore it.
        }
        marks
    }
}

/// Lock the shared session. If another thread panicked while holding it, carry on anyway:
/// the journal on disk is still correct, and stopping would freeze every paused program.
pub fn lock(session: &std::sync::Mutex<Session>) -> std::sync::MutexGuard<'_, Session> {
    session
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
