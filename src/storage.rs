//! How much disk the saved files take, and keeping that under a limit.
//!
//! Every running session trims all sessions when it starts and every few minutes after that:
//! - Saved files older than [`KEEP_FOR`] go, even if their terminal is still open.
//! - Over the limit, the oldest go first. Anything saved in the last [`KEEP_RECENT`] stays, even
//!   over the limit: `rm -rf` on a folder bigger than the limit must still be undoable.
//!
//! A hard-linked file that still has another name costs nothing, because dropping it would free
//! nothing. Only files the stash alone holds on to count.
//!
//! Sessions don't coordinate. Removing a file another session already removed is harmless, and
//! undo checks that a saved copy is still there before it changes anything.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::control::SOCK_VAR;
use crate::session::{KEEP_FOR, has_ended};

/// Set this to change the limit, for example to `2G` or `500M`.
pub const LIMIT_VAR: &str = "WHOOPZ_STASH_LIMIT";

const DEFAULT_LIMIT: u64 = 5 << 30;

/// Files saved this recently are never dropped to stay under the limit.
const KEEP_RECENT: Duration = Duration::from_secs(60 * 60);

const TRIM_EVERY: Duration = Duration::from_secs(10 * 60);

/// How many sessions `whoops status` lists.
const SHOW: usize = 5;

struct SavedFile {
    path: PathBuf,
    /// What dropping it would free.
    bytes: u64,
    /// When it was saved, in seconds. Saving changes a file's ctime, and nothing else does
    /// once only the stash holds it.
    saved_at: i64,
}

/// The limit to use. A value that makes no sense must not stop a shell from starting.
pub fn limit() -> u64 {
    let Some(text) = std::env::var_os(LIMIT_VAR) else {
        return DEFAULT_LIMIT;
    };
    parse_size(&text.to_string_lossy()).unwrap_or_else(|| {
        eprintln!(
            "whoops: {LIMIT_VAR} should look like 2G or 500M, so using {}",
            size(DEFAULT_LIMIT)
        );
        DEFAULT_LIMIT
    })
}

/// `500M`, `2G`, `1.5GB`, or plain bytes.
fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim().to_ascii_uppercase();
    let number = text.strip_suffix('B').unwrap_or(&text);
    let (number, unit) = match number.chars().last()? {
        'K' => (&number[..number.len() - 1], 1u64 << 10),
        'M' => (&number[..number.len() - 1], 1 << 20),
        'G' => (&number[..number.len() - 1], 1 << 30),
        'T' => (&number[..number.len() - 1], 1 << 40),
        _ => (number, 1),
    };
    let value: f64 = number.parse().ok()?;
    (value.is_finite() && value >= 0.0).then_some((value * unit as f64) as u64)
}

/// Sizes the way `du -h` counts them.
fn size(bytes: u64) -> String {
    let mut value = bytes as f64;
    let mut units = ["B", "KB", "MB", "GB", "TB"].iter().peekable();
    let mut unit = units.next().expect("the list isn't empty");
    while value >= 1024.0 && units.peek().is_some() {
        value /= 1024.0;
        unit = units.next().expect("just checked");
    }
    if *unit == "B" || value >= 10.0 {
        format!("{value:.0} {unit}")
    } else {
        format!("{value:.1} {unit}")
    }
}

fn plural(n: i64, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

fn ago(secs: i64) -> String {
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{} ago", plural(secs / 60, "minute"))
    } else if secs < 86400 {
        format!("{} ago", plural(secs / 3600, "hour"))
    } else {
        format!("{} ago", plural(secs / 86400, "day"))
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |t| t.as_secs() as i64)
}

fn session_dirs(data: &Path) -> Vec<PathBuf> {
    fs::read_dir(data.join("sessions"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .collect()
}

fn saved_files(session: &Path) -> Vec<SavedFile> {
    fs::read_dir(session.join("stash"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            Some(SavedFile {
                path: entry.path(),
                bytes: if meta.nlink() > 1 {
                    0
                } else {
                    meta.blocks() * 512
                },
                saved_at: meta.ctime(),
            })
        })
        .collect()
}

/// Which files to remove, given all of them.
fn to_drop(mut files: Vec<SavedFile>, limit: u64, now: i64) -> Vec<PathBuf> {
    files.sort_by_key(|f| f.saved_at);
    let mut total: u64 = files.iter().map(|f| f.bytes).sum();
    let mut drop = Vec::new();
    for file in files {
        let age = now - file.saved_at;
        let expired = age > KEEP_FOR.as_secs() as i64;
        let frees_space = total > limit && file.bytes > 0 && age >= KEEP_RECENT.as_secs() as i64;
        if expired || frees_space {
            total -= file.bytes;
            drop.push(file.path);
        }
    }
    drop
}

fn trim(data: &Path, limit: u64, now: i64) {
    let files = session_dirs(data)
        .iter()
        .flat_map(|dir| saved_files(dir))
        .collect();
    for path in to_drop(files, limit, now) {
        let _ = fs::remove_file(path);
    }
}

/// Trim now and every few minutes, for as long as the session runs.
pub fn keep_trimmed(data: &Path, limit: u64) {
    loop {
        trim(data, limit, now());
        std::thread::sleep(TRIM_EVERY);
    }
}

/// The text of `whoops status`.
pub fn status(data: &Path, limit: u64) -> String {
    let this_shell: Option<OsString> = std::env::var_os(SOCK_VAR)
        .and_then(|sock| Path::new(&sock).file_stem().map(ToOwned::to_owned));
    report(data, limit, this_shell, now())
}

fn report(data: &Path, limit: u64, this_shell: Option<OsString>, now: i64) -> String {
    struct Row {
        bytes: u64,
        running: bool,
        started: Option<i64>,
        this_shell: bool,
    }
    let mut rows = Vec::new();
    let mut oldest: Option<i64> = None;
    for dir in session_dirs(data) {
        let files = saved_files(&dir);
        if let Some(first) = files.iter().map(|f| f.saved_at).min() {
            oldest = Some(oldest.map_or(first, |o| o.min(first)));
        }
        let name = dir.file_name().unwrap_or_default();
        rows.push(Row {
            bytes: files.iter().map(|f| f.bytes).sum(),
            running: !has_ended(&dir),
            started: name.to_str().and_then(started_at),
            this_shell: this_shell.as_deref() == Some(name),
        });
    }

    let total: u64 = rows.iter().map(|r| r.bytes).sum();
    let running = rows.iter().filter(|r| r.running).count();
    let mut text = format!("Saved files: {} of {}\n", size(total), size(limit));
    text += &format!("Sessions:    {} ({running} running)\n", rows.len());
    if let Some(oldest) = oldest {
        text += &format!("Oldest:      {}\n", ago(now - oldest));
    }
    if total > limit {
        text += "\nOver the limit: files saved in the last hour are kept anyway.\n";
    }

    rows.retain(|r| r.bytes > 0);
    rows.sort_by_key(|r| std::cmp::Reverse(r.bytes));
    if !rows.is_empty() {
        text += "\nBiggest sessions:\n";
    }
    for r in rows.iter().take(SHOW) {
        let state = if r.running { "running" } else { "ended" };
        let started = r
            .started
            .map(|t| format!("  started {}", ago(now - t)))
            .unwrap_or_default();
        let mark = if r.this_shell { "  (this shell)" } else { "" };
        text += &format!("{:>8}  {state:<7}{started}{mark}\n", size(r.bytes));
    }
    text
}

/// Session folders are named `<start time>-<pid>`.
fn started_at(name: &str) -> Option<i64> {
    name.split_once('-')?.0.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3600;
    const NOW: i64 = 1_000_000;

    fn file(name: &str, bytes: u64, age: i64) -> SavedFile {
        SavedFile {
            path: name.into(),
            bytes,
            saved_at: NOW - age,
        }
    }

    fn drops(files: Vec<SavedFile>, limit: u64) -> Vec<String> {
        to_drop(files, limit, NOW)
            .iter()
            .map(|p| p.display().to_string())
            .collect()
    }

    #[test]
    fn under_the_limit_nothing_goes() {
        let files = vec![file("a", 50, 5 * HOUR), file("b", 50, 2 * HOUR)];
        assert!(drops(files, 100).is_empty());
    }

    #[test]
    fn over_the_limit_the_oldest_go_first_and_only_as_many_as_needed() {
        let files = vec![
            file("new", 50, 2 * HOUR),
            file("oldest", 50, 9 * HOUR),
            file("old", 50, 5 * HOUR),
        ];
        assert_eq!(drops(files, 100), ["oldest"]);
    }

    #[test]
    fn something_saved_just_now_stays_even_over_the_limit() {
        let files = vec![file("old", 50, 2 * HOUR), file("huge", 9000, 60)];
        assert_eq!(drops(files, 100), ["old"]);
    }

    #[test]
    fn a_week_old_file_goes_even_under_the_limit() {
        let files = vec![file("ancient", 50, 8 * 24 * HOUR), file("new", 50, HOUR)];
        assert_eq!(drops(files, 1000), ["ancient"]);
    }

    #[test]
    fn a_file_that_frees_nothing_stays() {
        let files = vec![file("shared", 0, 5 * HOUR), file("real", 200, 2 * HOUR)];
        assert_eq!(drops(files, 100), ["real"]);
    }

    #[test]
    fn trim_removes_the_files_from_every_session() {
        let tmp = tempfile::tempdir().unwrap();
        let stash = tmp.path().join("sessions/1-1/stash");
        fs::create_dir_all(&stash).unwrap();
        fs::write(stash.join("1"), vec![1; 100_000]).unwrap();

        trim(tmp.path(), 1 << 30, now());
        assert!(stash.join("1").exists(), "under the limit");
        trim(tmp.path(), 0, now());
        assert!(stash.join("1").exists(), "saved just now");
        trim(tmp.path(), 0, now() + 2 * HOUR);
        assert!(!stash.join("1").exists());
    }

    #[test]
    fn sizes_are_read_the_way_people_write_them() {
        assert_eq!(parse_size("2G"), Some(2 << 30));
        assert_eq!(parse_size("500m"), Some(500 << 20));
        assert_eq!(parse_size("1.5GB"), Some(3 << 29));
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("0"), Some(0));
        for bad in ["", "G", "lots", "-1G", "NaN"] {
            assert_eq!(parse_size(bad), None, "{bad}");
        }
    }

    #[test]
    fn sizes_are_shown_the_way_du_shows_them() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1536), "1.5 KB");
        assert_eq!(size(292 << 20), "292 MB");
        assert_eq!(size(5 << 30), "5.0 GB");
    }

    #[test]
    fn ages_read_naturally() {
        assert_eq!(ago(5), "just now");
        assert_eq!(ago(60), "1 minute ago");
        assert_eq!(ago(2 * HOUR), "2 hours ago");
        assert_eq!(ago(50 * HOUR), "2 days ago");
    }
}
