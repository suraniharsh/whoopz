//! `whoops list` and `whoops [n]`: read the journal and put things back.
//!
//! Undo goes through a command's changes **newest first**. That order matters for
//! `rm -rf project/`: `rm` deletes the files first and the folders last, so undo re-creates
//! the folders first and then puts the files back into them.
//!
//! Every step checks the real state of the disk before changing anything. If something is in
//! the way (a new file where the old one goes back), that step is skipped and reported, never
//! overwritten.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::journal::{self, Change, FileId, Line, Saved};
use crate::session::{MAX_COPY, Session};
use crate::sys;

/// What to print, and the exit code for `whoops`.
pub struct Response {
    pub text: String,
    pub code: i32,
}

struct Command {
    id: u64,
    text: String,
    background: bool,
    changes: Vec<Change>,
    undone: bool,
}

/// How many lines of "restored …" to print before summarising.
const SHOW: usize = 20;

fn commands(s: &Session) -> io::Result<Vec<Command>> {
    let mut cmds: Vec<Command> = Vec::new();
    for line in journal::read(&s.journal_path())? {
        match line {
            Line::Command {
                id,
                text,
                background,
                ..
            } => cmds.push(Command {
                id,
                text,
                background,
                changes: Vec::new(),
                undone: false,
            }),
            Line::Change { cmd, change } => {
                if let Some(c) = cmds.iter_mut().rev().find(|c| c.id == cmd) {
                    c.changes.push(change);
                }
            }
            Line::Undone { cmd } => {
                if let Some(c) = cmds.iter_mut().find(|c| c.id == cmd) {
                    c.undone = true;
                }
            }
        }
    }
    Ok(cmds)
}

pub fn list(s: &Session) -> Response {
    let cmds = match commands(s) {
        Ok(cmds) => cmds,
        Err(e) => return failed(format!("couldn't read the journal: {e}")),
    };
    if cmds.is_empty() {
        return Response {
            text: "Nothing to undo yet.\n".into(),
            code: 0,
        };
    }
    let mut text = String::new();
    for c in &cmds {
        let mark = if c.undone { "  (undone)" } else { "" };
        let label = if c.background {
            format!("[background] {}", c.text)
        } else {
            c.text.clone()
        };
        text += &format!(
            "{:>4}  {:<40}  {}{mark}\n",
            c.id,
            shorten(&label, 40),
            summary(&c.changes)
        );
    }
    Response { text, code: 0 }
}

pub fn undo(s: &mut Session, which: Option<u64>) -> Response {
    let cmds = match commands(s) {
        Ok(cmds) => cmds,
        Err(e) => return failed(format!("couldn't read the journal: {e}")),
    };
    let target = match which {
        Some(n) => match cmds.iter().find(|c| c.id == n) {
            Some(c) if c.undone => return failed(format!("Command {n} was already undone.")),
            Some(c) => c,
            None => {
                return failed(format!(
                    "There's no command {n} with changes. See `whoops list`."
                ));
            }
        },
        // Plain `whoops` undoes your last command, never a background job.
        None => match cmds.iter().rev().find(|c| !c.undone && !c.background) {
            Some(c) => c,
            None => return failed("Nothing to undo.".into()),
        },
    };

    let mut done = Vec::new();
    let mut skipped = Vec::new();
    for change in target.changes.iter().rev() {
        match undo_change(s, change) {
            Ok(Some(outcome)) => done.push(outcome),
            Ok(None) => {} // nothing to do: that change never went through
            Err(msg) => skipped.push(msg),
        }
    }
    if let Err(e) = s.record_undone(target.id) {
        skipped.push(format!("couldn't note the undo in the journal: {e}"));
    }

    let mut text = format!("Undid {}: {}\n", target.id, target.text);
    text += &report(done, &skipped);
    Response {
        text,
        code: if skipped.is_empty() { 0 } else { 1 },
    }
}

/// What one undo step did, for the report.
enum Done {
    /// It had been deleted; it's back.
    Restored(PathBuf),
    /// The command created it; it's gone again (kept in the stash).
    Removed(PathBuf),
    /// The command changed or replaced it; the old version is back.
    PutBack(PathBuf),
    RemovedDir(PathBuf),
    /// Moved back from `to` to `from`.
    Moved {
        from: PathBuf,
        to: PathBuf,
    },
    Swapped(PathBuf, PathBuf),
}

impl Done {
    fn path(&self) -> &Path {
        match self {
            Done::Restored(p) | Done::Removed(p) | Done::PutBack(p) | Done::RemovedDir(p) => p,
            Done::Moved { from, .. } => from,
            Done::Swapped(a, _) => a,
        }
    }

    fn line(&self) -> String {
        match self {
            Done::Restored(p) => format!("restored {}", p.display()),
            Done::Removed(p) => format!("removed {}", p.display()),
            Done::PutBack(p) => format!("put back the old version of {}", p.display()),
            Done::RemovedDir(p) => format!("removed folder {}", p.display()),
            Done::Moved { from, to } => {
                format!("moved {} back to {}", to.display(), from.display())
            }
            Done::Swapped(a, b) => format!("swapped {} and {} back", a.display(), b.display()),
        }
    }
}

/// The lines under "Undid …": one per file, in plain words.
///
/// A command that deleted a file and wrote a new one in its place (git does that) shows as
/// "put back the old version", not as "removed" plus "restored". git's own files in `.git/`
/// are counted, not listed: they're put back so `git status` matches, but they aren't yours.
fn report(done: Vec<Done>, skipped: &[String]) -> String {
    let mut merged: Vec<Done> = Vec::new();
    for outcome in done {
        let pair = merged.iter().position(|m| {
            m.path() == outcome.path()
                && matches!(
                    (m, &outcome),
                    (Done::Removed(_), Done::Restored(_)) | (Done::Restored(_), Done::Removed(_))
                )
        });
        match pair {
            Some(i) => merged[i] = Done::PutBack(outcome.path().to_path_buf()),
            None => merged.push(outcome),
        }
    }

    let in_git = |d: &Done| d.path().components().any(|c| c.as_os_str() == ".git");
    let git_files = merged.iter().filter(|d| in_git(d)).count();
    let lines: Vec<String> = merged
        .iter()
        .filter(|d| !in_git(d))
        .map(Done::line)
        .collect();

    let mut text = String::new();
    if lines.is_empty() && git_files == 0 && skipped.is_empty() {
        text += "  nothing to put back: its changes didn't go through\n";
    }
    for line in lines.iter().take(SHOW) {
        text += &format!("  {line}\n");
    }
    if lines.len() > SHOW {
        text += &format!("  … and {} more\n", lines.len() - SHOW);
    }
    if git_files > 0 {
        let s = if git_files == 1 { "" } else { "s" };
        text += &format!("  … and {git_files} of git's own file{s} in .git/\n");
    }
    for msg in skipped {
        text += &format!("  skipped: {msg}\n");
    }
    text
}

/// `Ok(None)`: nothing to do, because the change never went through.
fn undo_change(s: &mut Session, change: &Change) -> Result<Option<Done>, String> {
    match change {
        Change::Deleted { path, saved, was } => {
            if still_there(path, *was)? {
                return Ok(None);
            }
            restore(s, path, saved)?;
            Ok(Some(Done::Restored(path.clone())))
        }

        Change::Replaced { path, saved, was } => {
            if still_there(path, *was)? {
                return Ok(None);
            }
            still_kept(s, path, saved)?;
            // Take the command's version away (into the stash), then put the old one back.
            if lstat(path)?.is_some() {
                move_to_stash(s, path)?;
            }
            restore(s, path, saved)?;
            Ok(Some(Done::PutBack(path.clone())))
        }

        Change::Renamed {
            from,
            to,
            replaced,
            was,
        } => {
            if still_there(from, *was)? {
                return Ok(None);
            }
            if lstat(to)?.is_none() {
                return Err(format!("{} isn't there anymore", to.display()));
            }
            if lstat(from)?.is_some() {
                return Err(format!("something is already at {}", from.display()));
            }
            sys::rename_noreplace(to, from)
                .map_err(|e| format!("couldn't move {} back: {e}", to.display()))?;
            if let Some(replaced) = replaced {
                restore(s, to, replaced).map_err(|e| {
                    format!("moved {} back, but not what it replaced: {e}", to.display())
                })?;
            }
            Ok(Some(Done::Moved {
                from: from.clone(),
                to: to.clone(),
            }))
        }

        Change::Exchanged { a, b } => {
            sys::exchange(a, b).map_err(|e| {
                format!(
                    "couldn't swap {} and {} back: {e}",
                    a.display(),
                    b.display()
                )
            })?;
            Ok(Some(Done::Swapped(a.clone(), b.clone())))
        }

        Change::Overwritten { path, saved } => {
            let Saved::Stash { file } = saved else {
                return Err(format!("{} wasn't saved", path.display()));
            };
            still_kept(s, path, saved)?;
            write_back(&s.stash_path(file), path)
                .map_err(|e| format!("couldn't put back {}: {e}", path.display()))?;
            Ok(Some(Done::PutBack(path.clone())))
        }

        Change::Created { path } => match lstat(path)? {
            None => Ok(None),
            Some(st) if sys::is_dir(&st) => {
                Err(format!("{} is a folder now; left it", path.display()))
            }
            Some(_) => {
                move_to_stash(s, path)?;
                Ok(Some(Done::Removed(path.clone())))
            }
        },

        Change::CreatedDir { path } => match fs::remove_dir(path) {
            Ok(()) => Ok(Some(Done::RemovedDir(path.clone()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("left folder {}: {e}", path.display())),
        },
    }
}

/// Is the very same file (`was`) still at `path`? Then the delete or rename never went
/// through: the program wasn't allowed to, or it failed.
fn still_there(path: &Path, was: Option<FileId>) -> Result<bool, String> {
    let Some(was) = was else { return Ok(false) };
    Ok(lstat(path)?.is_some_and(|st| st.st_dev == was.dev && st.st_ino == was.ino))
}

/// Old saved files get dropped (see [`crate::storage`]). Check before changing anything, or
/// undo would take away the current version and have nothing to put in its place.
fn still_kept(s: &Session, path: &Path, saved: &Saved) -> Result<(), String> {
    if let Saved::Stash { file } = saved
        && lstat(&s.stash_path(file))?.is_none()
    {
        return Err(format!(
            "{} can't come back: its saved copy is gone (saved files are dropped after a week, \
             or to stay under the size limit)",
            path.display()
        ));
    }
    Ok(())
}

/// Put a removed or replaced thing back at `path`. Never overwrites anything.
fn restore(s: &Session, path: &Path, saved: &Saved) -> Result<(), String> {
    still_kept(s, path, saved)?;
    if lstat(path)?.is_some() {
        return Err(format!("something is already at {}", path.display()));
    }
    if let Some(parent) = path.parent()
        && !parent.is_dir()
    {
        return Err(format!(
            "can't restore {}: its folder is gone",
            path.display()
        ));
    }

    let result = match saved {
        Saved::Stash { file } => {
            let stashed = s.stash_path(file);
            // Same drive: just move the stash's name back. Otherwise, copy.
            match sys::rename_noreplace(&stashed, path) {
                Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                    copy_new(&stashed, path).and_then(|()| fs::remove_file(&stashed))
                }
                other => other,
            }
        }
        Saved::Symlink { target } => std::os::unix::fs::symlink(target, path),
        Saved::Dir { mode } => fs::create_dir(path)
            .and_then(|()| fs::set_permissions(path, fs::Permissions::from_mode(*mode))),
        Saved::Nothing { reason } => {
            return Err(format!("{} wasn't saved ({reason})", path.display()));
        }
    };
    result.map_err(|e| format!("couldn't restore {}: {e}", path.display()))
}

/// Copy the saved contents back into the file at `path`, then drop the saved copy.
fn write_back(stashed: &Path, path: &Path) -> io::Result<()> {
    if path.exists() {
        // Write into the existing file: it keeps its permissions, and a symlink stays a symlink.
        let mut out = OpenOptions::new().write(true).truncate(true).open(path)?;
        io::copy(&mut File::open(stashed)?, &mut out)?;
    } else {
        copy_new(stashed, path)?;
    }
    fs::remove_file(stashed)
}

/// Undoing a create: move the new file into the stash rather than deleting it, so a mistaken
/// undo isn't a new way to lose data.
fn move_to_stash(s: &mut Session, path: &Path) -> Result<(), String> {
    let (name, _) = s.new_stash_name();
    let dest = s.stash_path(&name);
    match sys::rename_noreplace(path, &dest) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            let st = lstat(path)?.ok_or_else(|| format!("{} vanished", path.display()))?;
            if sys::is_symlink(&st) {
                // Nothing to keep: a symlink is only a name.
                return fs::remove_file(path).map_err(|e| e.to_string());
            }
            if st.st_size as u64 > MAX_COPY {
                return Err(format!(
                    "{} is too big to keep a copy of; left it",
                    path.display()
                ));
            }
            copy_new(path, &dest)
                .and_then(|()| fs::remove_file(path))
                .map_err(|e| format!("couldn't remove {}: {e}", path.display()))
        }
        Err(e) => Err(format!("couldn't remove {}: {e}", path.display())),
    }
}

/// Copy a file to a path where nothing exists yet, keeping permissions and modification time.
fn copy_new(from: &Path, to: &Path) -> io::Result<()> {
    let src = File::open(from)?;
    let meta = src.metadata()?;
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(to)?;
    io::copy(&mut &src, &mut out)?;
    out.set_permissions(meta.permissions())?;
    out.set_modified(meta.modified()?)?;
    Ok(())
}

fn lstat(path: &Path) -> Result<Option<sys::Stat>, String> {
    sys::lstat_path(path).map_err(|e| format!("couldn't check {}: {e}", path.display()))
}

fn summary(changes: &[Change]) -> String {
    let (mut deleted, mut moved, mut overwritten, mut created) = (0, 0, 0, 0);
    for c in changes {
        match c {
            Change::Deleted { .. } => deleted += 1,
            Change::Renamed { .. } | Change::Exchanged { .. } => moved += 1,
            Change::Overwritten { .. } | Change::Replaced { .. } => overwritten += 1,
            Change::Created { .. } | Change::CreatedDir { .. } => created += 1,
        }
    }
    [
        (deleted, "deleted"),
        (moved, "moved"),
        (overwritten, "overwritten"),
        (created, "created"),
    ]
    .iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, what)| format!("{n} {what}"))
    .collect::<Vec<_>>()
    .join(", ")
}

fn shorten(text: &str, max: usize) -> String {
    let one_line = text.replace('\n', " ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        one_line.chars().take(max - 1).collect::<String>() + "…"
    }
}

fn failed(text: String) -> Response {
    Response {
        text: text + "\n",
        code: 1,
    }
}
