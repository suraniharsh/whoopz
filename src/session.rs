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
//!
//! Every paused call is assigned to an **entry** before anything is saved:
//! - the command line running now, if the call comes from the shell itself or from the
//!   terminal's **foreground** process group (the command you're waiting on), or
//! - its own `[background]` entry, if it comes from a background job (`make &`, a file
//!   watcher, fish generating completions on its first start). Those are still saved and can
//!   be undone with `whoops <n>`, but never get mixed into your commands, and plain `whoops`
//!   leaves them alone.
//!
//! A hook can also send "ignore <path>": files whose path starts with it are the shell's own
//! housekeeping (its history file, for example). fish rewrites its history from a background
//! thread at any moment, even in the middle of your command. Changes there are let through
//! too, but only when the shell itself makes them: `rm` on your history file is still saved.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::journal::{self, Change, Line};
use crate::procfs;
use crate::sys::{self, Dir};

/// Files bigger than this are not copied (hard links have no limit).
pub const MAX_COPY: u64 = 512 * 1024 * 1024;

/// Finished sessions are removed after this long.
const KEEP_FOR: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// One undo entry: a command line, or a background job.
struct Entry {
    text: String,
    background: bool,
    /// The entry goes in the journal only once it changes something.
    written: bool,
    /// Paths whose original state this entry already saved. A program that rewrites the same
    /// file a hundred times only needs the first version kept.
    seen: HashSet<PathBuf>,
}

impl Entry {
    fn new(text: String, background: bool) -> Self {
        Self {
            text,
            background,
            written: false,
            seen: HashSet::new(),
        }
    }
}

pub struct Session {
    pub dir: PathBuf,
    stash_dir: Dir,
    journal: File,
    _lock: File,
    marks: Option<Marks>,
    next_cmd: u64,
    next_stash: u64,
    entries: HashMap<u64, Entry>,
    /// The command line running now; `None` at the prompt.
    current: Option<u64>,
    /// Background process groups, the entry each one's changes go to, and the start time of
    /// the group's leader (a process group id is reused once the group is gone).
    background: HashMap<i32, (u64, Option<u64>)>,
    /// The entry the call being handled belongs to.
    target: Option<u64>,
    /// The shell process, and path prefixes it keeps its own files under.
    shell_pid: Option<u32>,
    housekeeping: Vec<Vec<u8>>,
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
            entries: HashMap::new(),
            current: None,
            background: HashMap::new(),
            target: None,
            shell_pid: None,
            housekeeping: Vec::new(),
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
        self.end_command();
        let id = self.new_entry(Entry::new(text, false));
        self.current = Some(id);
    }

    pub fn end_command(&mut self) {
        // A finished command gets no more changes: anything it left running in the background
        // goes to its own entry.
        if let Some(id) = self.current.take() {
            self.entries.remove(&id);
        }
    }

    fn new_entry(&mut self, entry: Entry) -> u64 {
        let id = self.next_cmd;
        self.next_cmd += 1;
        self.entries.insert(id, entry);
        id
    }

    /// The running command's text, if a hook said one is running.
    pub fn current_text(&self) -> Option<&str> {
        self.current
            .and_then(|id| self.entries.get(&id))
            .map(|e| e.text.as_str())
    }

    /// Decide which entry a call from `pid` belongs to; `None` means don't save it. The answer
    /// is used by [`Session::first_time`] and [`Session::record`] until the next call.
    pub fn choose_target(&mut self, pid: u32) -> Option<u64> {
        self.target = self.target_for(pid);
        self.target
    }

    fn target_for(&mut self, pid: u32) -> Option<u64> {
        let Some(shell) = self.shell_pid else {
            return self.current;
        };
        if is_thread_of(shell, pid) {
            return self.current;
        }
        let (Ok(proc_info), Ok(shell_info)) = (procfs::info(pid), procfs::info(shell)) else {
            return self.current;
        };
        // The shell's own group (prompt tools, fish functions, shells without job control),
        // and the foreground group (the command you're waiting on): the current command.
        if proc_info.pgid == shell_info.pgid || proc_info.pgid == proc_info.tpgid {
            return self.current;
        }
        Some(self.background_entry(proc_info.pgid, pid))
    }

    /// The entry for a background process group, made the first time the group changes
    /// something.
    fn background_entry(&mut self, pgid: i32, pid: u32) -> u64 {
        let leader_start = procfs::info(pgid as u32).ok().map(|i| i.start_time);
        if let Some(&(id, start)) = self.background.get(&pgid)
            && (leader_start.is_none() || leader_start == start)
        {
            return id;
        }
        let mut text = procfs::cmdline(pgid as u32);
        if text.is_empty() {
            text = procfs::cmdline(pid);
        }
        let id = self.new_entry(Entry::new(text, true));
        self.background.insert(pgid, (id, leader_start));
        id
    }

    /// True the first time the target entry touches `path`.
    pub fn first_time(&mut self, path: &Path) -> bool {
        let target = self.target.and_then(|id| self.entries.get_mut(&id));
        target.is_some_and(|e| e.seen.insert(path.to_path_buf()))
    }

    pub fn set_shell_pid(&mut self, pid: u32) {
        self.shell_pid = Some(pid);
    }

    /// Is this the shell itself (any of its threads) changing one of its own files?
    pub fn is_housekeeping(&self, pid: u32, paths: &[&Path]) -> bool {
        use std::os::unix::ffi::OsStrExt;
        let Some(shell) = self.shell_pid else {
            return false;
        };
        is_thread_of(shell, pid)
            && !paths.is_empty()
            && paths.iter().all(|path| {
                let bytes = path.as_os_str().as_bytes();
                self.housekeeping
                    .iter()
                    .any(|prefix| bytes.starts_with(prefix))
            })
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
                Mark::Ignore(prefix) => self.housekeeping.push(prefix),
            }
        }
    }

    // --- the journal ---

    /// Record a change made by the target entry.
    pub fn record(&mut self, change: Change) -> io::Result<()> {
        let Some(cmd) = self.target else {
            return Ok(());
        };
        let Some(entry) = self.entries.get_mut(&cmd) else {
            return Ok(());
        };
        if !entry.written {
            let time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let line = Line::Command {
                id: cmd,
                text: entry.text.clone(),
                time,
                background: entry.background,
            };
            journal::append(&mut self.journal, &line)?;
            entry.written = true;
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

/// Is `pid` the process `shell` or one of its threads? A thread shows up as
/// `/proc/<shell>/task/<thread id>`.
fn is_thread_of(shell: u32, pid: u32) -> bool {
    pid == shell || Path::new(&format!("/proc/{shell}/task/{pid}")).exists()
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
    /// The shell keeps its own files under this path.
    Ignore(Vec<u8>),
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
            if let Some(prefix) = msg[..end].strip_prefix(b"ignore ") {
                if !prefix.is_empty() {
                    marks.push(Mark::Ignore(prefix.to_vec()));
                }
                continue;
            }
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
