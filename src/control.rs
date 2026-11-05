//! How `whoops` (the messenger) talks to the watcher.
//!
//! The messenger runs *inside* the watched shell, so if it moved files back itself, its own
//! renames would be paused and journaled as a new command. Instead it sends a request over a
//! Unix socket, and the watcher, which is not filtered, does the work.
//!
//! The messenger sends two lines: the request (`list`, `undo` or `undo 3`), then its current
//! folder, so paths can be shown relative to it. The watcher answers with the exit code on
//! the first line, then the text to print.
//!
//! A `sudo` stand-in sends its terminal along as well: see [`crate::standin`].

use std::io::{BufRead, BufReader, Cursor, IoSliceMut, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use nix::sys::socket::{ControlMessageOwned, MsgFlags, getsockopt, recvmsg, sockopt};

use crate::session::{Session, lock};
use crate::standin::{self, StandIns};
use crate::undo::{self, Response};

/// Set inside a watched shell: where the watcher's socket is.
pub const SOCK_VAR: &str = "WHOOPZ_SOCK";

/// Watcher side: answer requests until the process exits.
pub fn serve(listener: UnixListener, session: Arc<Mutex<Session>>, stand_ins: StandIns) {
    let stand_ins = Arc::new(stand_ins);
    for conn in listener.incoming().flatten() {
        // Each on its own thread: a stand-in's request lasts as long as `sudo` runs.
        let (session, stand_ins) = (session.clone(), stand_ins.clone());
        std::thread::spawn(move || answer(conn, &session, &stand_ins));
    }
}

/// Read the first bytes of a request, and any fds that came with them. A plain read would lose
/// the fds, so this comes first for every request.
fn receive(conn: &UnixStream, buf: &mut [u8]) -> std::io::Result<(usize, Vec<OwnedFd>)> {
    let mut iov = [IoSliceMut::new(buf)];
    let mut space = nix::cmsg_space!([RawFd; 3]);
    let msg = recvmsg::<()>(
        conn.as_raw_fd(),
        &mut iov,
        Some(&mut space),
        MsgFlags::MSG_CMSG_CLOEXEC,
    )?;
    let mut fds = Vec::new();
    for cmsg in msg.cmsgs()? {
        if let ControlMessageOwned::ScmRights(received) = cmsg {
            // SAFETY: the kernel just created these fds for us, and nothing else owns them.
            fds.extend(
                received
                    .iter()
                    .map(|&fd| unsafe { OwnedFd::from_raw_fd(fd) }),
            );
        }
    }
    Ok((msg.bytes, fds))
}

fn answer(conn: UnixStream, session: &Mutex<Session>, stand_ins: &StandIns) -> std::io::Result<()> {
    // The socket sits in a folder only you can open. Check anyway that the caller is you.
    let peer = getsockopt(&conn, sockopt::PeerCredentials).map_err(std::io::Error::from)?;
    if peer.uid() != nix::unistd::getuid().as_raw() {
        return Ok(());
    }

    let mut first = [0; 4096];
    let (len, fds) = receive(&conn, &mut first)?;
    let first = &first[..len];
    if first.starts_with(standin::REQUEST) {
        let reply = standin::run(&conn, first, fds, peer.pid(), stand_ins);
        return (&conn).write_all(reply.as_bytes());
    }

    let mut reader = BufReader::new(Cursor::new(first).chain(&conn));
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
