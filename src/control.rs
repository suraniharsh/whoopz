//! How `whoops` (the messenger) talks to the watcher.
//!
//! The messenger runs *inside* the watched shell, so if it moved files back itself, its own
//! renames would be paused and journaled as a new command. Instead it sends a request over a
//! Unix socket, and the watcher, which is not filtered, does the work.
//!
//! The messenger sends two lines: the request (`list`, `undo` or `undo 3`), then its current
//! folder, so paths can be shown relative to it. The watcher answers with the exit code on
//! the first line, then the text to print.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};

use crate::session::{Session, lock};
use crate::undo::{self, Response};

/// Set inside a watched shell: where the watcher's socket is.
pub const SOCK_VAR: &str = "WHOOPZ_SOCK";

/// Watcher side: answer requests until the process exits.
pub fn serve(listener: UnixListener, session: Arc<Mutex<Session>>) {
    for conn in listener.incoming().flatten() {
        let _ = answer(conn, &session);
    }
}

fn answer(conn: UnixStream, session: &Mutex<Session>) -> std::io::Result<()> {
    let mut reader = BufReader::new(&conn);
    let mut request = String::new();
    reader.read_line(&mut request)?;
    let mut cwd = String::new();
    reader.read_line(&mut cwd)?;

    let response = {
        let mut s = lock(session);
        s.apply_marks();
        let mut words = request.split_whitespace();
        match (words.next(), words.next().map(str::parse::<u64>)) {
            (Some("list"), None) => undo::list(&s),
            (Some("undo"), None) => undo::undo(&mut s, None),
            (Some("undo"), Some(Ok(n))) => undo::undo(&mut s, Some(n)),
            _ => Response {
                text: format!("unknown request: {request}"),
                code: 2,
            },
        }
    };
    let text = shorten_paths(&response.text, cwd.trim_end_matches('\n'));
    (&conn).write_all(format!("{}\n{text}", response.code).as_bytes())
}

/// `/home/me/project/src/a.rs` → `src/a.rs` when the messenger is in `/home/me/project`, or
/// `~/project/src/a.rs` when it's somewhere else.
fn shorten_paths(text: &str, cwd: &str) -> String {
    let mut text = text.to_string();
    if cwd.len() > 1 {
        text = text.replace(&format!("{cwd}/"), "");
    }
    if let Some(home) = std::env::var_os("HOME").and_then(|h| h.into_string().ok())
        && home.len() > 1
    {
        text = text.replace(&format!("{home}/"), "~/");
    }
    text
}

/// Messenger side: send a request, print the answer, return the exit code.
pub fn request(req: &str) -> Result<i32> {
    let Some(sock) = std::env::var_os(SOCK_VAR) else {
        bail!("not inside a whoops shell. Start one with `whoops shell`.");
    };
    let mut conn = UnixStream::connect(&sock).context("couldn't reach the watcher")?;
    let cwd = std::env::current_dir().unwrap_or_default();
    writeln!(conn, "{req}\n{}", cwd.display())?;
    conn.shutdown(Shutdown::Write)?;

    let mut reply = String::new();
    conn.read_to_string(&mut reply)?;
    let (code, text) = reply.split_once('\n').unwrap_or((&reply, ""));
    print!("{text}");
    Ok(code.trim().parse().unwrap_or(1))
}
