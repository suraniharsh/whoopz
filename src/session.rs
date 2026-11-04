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
//!   A process in those groups that was born *before* the command line started can't be part
//!   of it (fish generates completions on its first start, in its own group, for minutes).
//! - its own `[background]` entry otherwise: background jobs (`make &`, a file watcher), and
//!   the older processes above. Those are still saved and can
//!   be undone with `whoops <n>`, but never get mixed into your commands, and plain `whoops`
//!   leaves them alone.
//!
//! A hook can also send "ignore <path>": files whose path starts with it are the shell's own
//! housekeeping (its history file, for example). fish rewrites its history from a background
//! thread at any moment, even in the middle of your command. Changes there are let through
//! too, but only when the shell itself makes them: `rm` on your history file is still saved.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::CString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
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
    /// Paths that didn't exist before this entry: files it created itself. Deleting or moving
    /// them again (temporary files, git's `*.lock`) leaves nothing to undo.
    created: HashSet<PathBuf>,
}

impl Entry {
    fn new(text: String, background: bool) -> Self {
        Self {
            text,
            background,
            written: false,
            seen: HashSet::new(),
            created: HashSet::new(),
        }
    }
}

pub struct Session {
    pub dir: PathBuf,
    stash_dir: Dir,
    journal: File,
    _lock: File,
    marks: Option<Arc<Marks>>,
    next_cmd: u64,
    next_stash: u64,
    entries: HashMap<u64, Entry>,
    /// The command line running now; `None` at the prompt.
    current: Option<u64>,
    /// When the current command line could have started its first process: the previous
    /// "start" or "end" (clock ticks since boot). Processes born before can't belong to it.
    window_start: u64,
    last_boundary: u64,
    /// Background jobs, the entry each one's changes go to, and the start time of the process
    /// they're keyed by (ids are reused once a process is gone).
    background: HashMap<BackgroundKey, (u64, Option<u64>)>,
    /// The entry the call being handled belongs to.
    target: Option<u64>,
    /// The shell process, its program, and path prefixes it keeps its own files under.
    shell_pid: Option<u32>,
    shell_exe: Option<PathBuf>,
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
            window_start: 0,
            last_boundary: procfs::now_ticks(),
            background: HashMap::new(),
            target: None,
            shell_pid: None,
            shell_exe: None,
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

    /// A command line started. `at`: when the hook said so (clock ticks since boot).
    pub fn start_command(&mut self, text: String, at: u64) {
        self.end_command(at);
        let id = self.new_entry(Entry::new(text, false));
        self.current = Some(id);
        // The window opens at the previous boundary (when the prompt came back), not at this
        // "start". Some slack covers a message read a moment late on a busy machine.
        let slack = procfs::ticks_per_second() / 10;
        self.window_start = self.last_boundary.saturating_sub(slack);
        self.last_boundary = at;
    }

    pub fn end_command(&mut self, at: u64) {
        // A finished command gets no more changes: anything it left running in the background
        // goes to its own entry.
        if let Some(id) = self.current.take() {
            self.entries.remove(&id);
            self.last_boundary = at;
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
        // A background job: its own group, not the one the terminal is waiting on.
        if proc_info.pgid != shell_info.pgid && proc_info.pgid != proc_info.tpgid {
            let key = BackgroundKey::Group(proc_info.pgid);
            return Some(self.background_entry(key, proc_info.pgid as u32));
        }
        // The shell's own group (prompt tools, fish functions, shells without job control) or
        // the foreground group (the command you're waiting on).
        self.current?;
        if proc_info.start_time >= self.window_start {
            return self.current;
        }
        // Born before this command line started, so not part of it.
        let launched = procfs::child_of_shell(pid, shell);
        Some(self.background_entry(BackgroundKey::Process(launched), launched))
    }

    /// The entry for a background job, made the first time it changes something. `named_by`
    /// is the process whose command line names the entry.
    fn background_entry(&mut self, key: BackgroundKey, named_by: u32) -> u64 {
        let started = procfs::info(named_by).ok().map(|i| i.start_time);
        if let Some(&(id, start)) = self.background.get(&key)
            && (started.is_none() || started == start)
        {
            return id;
        }
        let id = self.new_entry(Entry::new(procfs::cmdline(named_by), true));
        self.background.insert(key, (id, started));
        id
    }

    /// True the first time the target entry touches `path`.
    pub fn first_time(&mut self, path: &Path) -> bool {
        let target = self.target.and_then(|id| self.entries.get_mut(&id));
        target.is_some_and(|e| e.seen.insert(path.to_path_buf()))
    }

    /// Note that the target entry created `path` itself (nothing was there before).
    pub fn mark_created(&mut self, path: &Path) {
        if let Some(e) = self.target.and_then(|id| self.entries.get_mut(&id)) {
            e.created.insert(path.to_path_buf());
        }
    }

    /// Did the target entry create `path` itself?
    pub fn was_created(&self, path: &Path) -> bool {
        self.target
            .and_then(|id| self.entries.get(&id))
            .is_some_and(|e| e.created.contains(path))
    }

    /// Paths the shell keeps its own files under, known before it starts.
    pub fn add_housekeeping(&mut self, prefix: &Path) {
        use std::os::unix::ffi::OsStrExt;
        self.housekeeping
            .push(prefix.as_os_str().as_bytes().to_vec());
    }

    pub fn set_shell_pid(&mut self, pid: u32) {
        self.shell_pid = Some(pid);
        self.shell_exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
    }

    /// Is this the shell itself (any of its threads, or a helper running the same program, like
    /// elvish's history daemon) changing one of its own files?
    pub fn is_housekeeping(&self, pid: u32, paths: &[&Path]) -> bool {
        use std::os::unix::ffi::OsStrExt;
        let Some(shell) = self.shell_pid else {
            return false;
        };
        let same_program = || {
            self.shell_exe.is_some()
                && std::fs::read_link(format!("/proc/{pid}/exe")).ok() == self.shell_exe
        };
        (is_thread_of(shell, pid) || same_program())
            && !paths.is_empty()
            && paths.iter().all(|path| {
                let bytes = path.as_os_str().as_bytes();
                self.housekeeping
                    .iter()
                    .any(|prefix| bytes.starts_with(prefix))
            })
    }

    pub fn set_marks(&mut self, marks: Arc<Marks>) {
        self.marks = Some(marks);
    }

    /// Apply every "start"/"end" message the shell hook has sent so far.
    ///
    /// Called before handling each paused call. The hook writes "start" before the command
    /// runs, so by the time one of its calls is paused, the message is already in the pipe.
    pub fn apply_marks(&mut self) {
        let Some(marks) = self.marks.clone() else {
            return;
        };
        for (mark, at) in marks.take() {
            match mark {
                Mark::Start(text) => self.start_command(text, at),
                Mark::End => self.end_command(at),
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

/// What a background entry is keyed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum BackgroundKey {
    /// A background job in its own process group.
    Group(i32),
    /// An older process in the shell's own group, by the process the shell launched.
    Process(u32),
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
///
/// Each message is stamped with the time it was read. A dedicated thread reads as soon as
/// anything arrives, without waiting for the session lock, so the stamp is close to when the
/// hook wrote it. The session uses those times to tell a command's own processes from older
/// ones.
pub struct Marks {
    inner: Mutex<MarksInner>,
}

struct MarksInner {
    pipe: File,
    partial: Vec<u8>,
    ready: VecDeque<(Mark, u64)>,
}

impl Marks {
    /// `pipe` must be non-blocking, so reading stops when it's empty instead of waiting.
    pub fn new(pipe: File) -> Self {
        Self {
            inner: Mutex::new(MarksInner {
                pipe,
                partial: Vec::new(),
                ready: VecDeque::new(),
            }),
        }
    }

    /// Read whatever the hook has written so far, and stamp it.
    pub fn pull(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let mut buf = [0u8; 4096];
        loop {
            match inner.pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => inner.partial.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break, // WouldBlock: nothing more for now
            }
        }

        let now = procfs::now_ticks();
        while let Some(end) = inner.partial.iter().position(|&b| b == 0) {
            let msg: Vec<u8> = inner.partial.drain(..=end).collect();
            if let Some(mark) = parse(&msg[..end]) {
                inner.ready.push_back((mark, now));
            }
        }
    }

    /// Everything read so far (reading first, so nothing already written is missed).
    fn take(&self) -> Vec<(Mark, u64)> {
        self.pull();
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.ready.drain(..).collect()
    }
}

fn parse(msg: &[u8]) -> Option<Mark> {
    if let Some(prefix) = msg.strip_prefix(b"ignore ") {
        return (!prefix.is_empty()).then(|| Mark::Ignore(prefix.to_vec()));
    }
    let msg = String::from_utf8_lossy(msg);
    if msg == "end" {
        Some(Mark::End)
    } else {
        // Anything else isn't from our hook; ignore it.
        msg.strip_prefix("start ")
            .map(|text| Mark::Start(text.trim().to_string()))
    }
}

/// Lock the shared session. If another thread panicked while holding it, carry on anyway:
/// the journal on disk is still correct, and stopping would freeze every paused program.
pub fn lock(session: &std::sync::Mutex<Session>) -> std::sync::MutexGuard<'_, Session> {
    session
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
