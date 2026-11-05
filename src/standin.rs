//! `sudo` and friends inside a watched shell.
//!
//! The kernel only lets a normal user attach a seccomp filter to a process marked "no new
//! privileges". Everything the shell starts inherits the mark, and with it set, a setuid program
//! like `sudo` can't become root. That can't be turned off from inside.
//!
//! So each session puts stand-ins first on `PATH`: symlinks to this binary, named after the real
//! programs. A stand-in hands its terminal, arguments and environment to the watcher over the
//! control socket. The watcher has neither the mark nor the filter, so the real program it
//! starts works normally, on the same terminal, in the foreground process group. The stand-in
//! then exits with the real program's exit code.
//!
//! What the real program changes is **not saved**: it runs outside the filter. The first use in
//! a session says so.
//!
//! A stand-in can only name a program from [`NAMES`], and the path comes from the watcher's own
//! `PATH`. Asking from a sandbox is refused: the real program would run outside it. So is asking
//! from another session (inside `tmux`, say): the password prompt would land on the wrong
//! terminal.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{IoSlice, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsFd, AsRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{LazyLock, Mutex, MutexGuard};

use anyhow::{Context, Result, bail};
use nix::sys::signal::{SigHandler, signal};
use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
use nix::unistd::{Pid, getpgid, getsid};

use crate::control::SOCK_VAR;
use crate::launcher::{TERMINAL_SIGNALS, find_program};
use crate::procfs;

/// Programs that need to become root. Other setuid programs (`passwd`, `fusermount`) still
/// don't work in a watched shell.
const NAMES: [&str; 4] = ["sudo", "doas", "su", "pkexec"];

/// What a request starts with, so the watcher can tell it from `list` and `undo`.
pub const REQUEST: &[u8] = b"run\n";

/// The stand-ins of one session.
#[derive(Default)]
pub struct StandIns {
    /// The folder they're in.
    bin: PathBuf,
    /// Where the real programs are.
    real: HashMap<String, PathBuf>,
}

/// Was the first use in this session already pointed out?
static NOTED: AtomicBool = AtomicBool::new(false);

/// Started programs by process id, each with who's waiting for its exit code. The main thread
/// reaps every child ([`crate::launcher`]), so a plain `wait` here would find nothing.
static WAITING: LazyLock<Mutex<HashMap<u32, Sender<i32>>>> = LazyLock::new(Default::default);

fn waiting() -> MutexGuard<'static, HashMap<u32, Sender<i32>>> {
    WAITING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The main thread reaped `pid`.
pub fn reaped(pid: u32, code: i32) {
    if let Some(waiting) = waiting().remove(&pid) {
        let _ = waiting.send(code);
    }
}

/// Make the stand-ins for the programs this system has, and put them first on the shell's
/// `PATH`. A failure here only means `sudo` keeps its error, so it mustn't stop the shell.
pub fn install(session_dir: &Path, shell: &mut Command) -> StandIns {
    let bin = session_dir.join("bin");
    let Ok(exe) = std::env::current_exe() else {
        return StandIns::default();
    };
    if fs::create_dir(&bin).is_err() {
        return StandIns::default();
    }
    let mut real = HashMap::new();
    for name in NAMES {
        if let Some(path) = find_program(name)
            && std::os::unix::fs::symlink(&exe, bin.join(name)).is_ok()
        {
            real.insert(name.to_string(), path);
        }
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs = std::iter::once(bin.clone()).chain(std::env::split_paths(&path));
    if let Ok(path) = std::env::join_paths(dirs) {
        shell.env("PATH", path);
    }
    StandIns { bin, real }
}

/// Is this process running as a stand-in? Then the name it was started under.
pub fn called_as() -> Option<String> {
    let arg0 = std::env::args_os().next()?;
    let name = Path::new(&arg0).file_name()?.to_str()?;
    NAMES.contains(&name).then(|| name.to_string())
}

/// `name\0cwd\0count\0args…\0env…\0`
fn encode(name: &str, cwd: &Path, args: &[OsString], env: &[OsString]) -> Vec<u8> {
    let count = args.len().to_string();
    let head = [
        name.as_bytes(),
        cwd.as_os_str().as_bytes(),
        count.as_bytes(),
    ];
    let rest = args.iter().chain(env).map(|item| item.as_bytes());
    let mut out = Vec::new();
    for item in head.into_iter().chain(rest) {
        out.extend_from_slice(item);
        out.push(0);
    }
    out
}

struct Request {
    name: String,
    cwd: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
}

fn decode(bytes: &[u8]) -> Option<Request> {
    let mut items = bytes
        .strip_suffix(&[0])?
        .split(|&b| b == 0)
        .map(|item| OsString::from_vec(item.to_vec()));
    let name = items.next()?.into_string().ok()?;
    let cwd = PathBuf::from(items.next()?);
    let count: usize = items.next()?.to_str()?.parse().ok()?;
    let args: Vec<OsString> = items.by_ref().take(count).collect();
    let env = items
        .filter_map(|item| {
            let bytes = item.as_bytes();
            let at = bytes.iter().position(|&b| b == b'=')?;
            Some((
                OsStr::from_bytes(&bytes[..at]).to_owned(),
                OsStr::from_bytes(&bytes[at + 1..]).to_owned(),
            ))
        })
        .collect();
    Some(Request {
        name,
        cwd,
        args,
        env,
    })
}

/// A closed stdin, stdout or stderr can't be sent, so send nothing useful in its place.
fn standard_fds() -> Result<[RawFd; 3]> {
    let mut fds = [0, 1, 2];
    for fd in &mut fds {
        // SAFETY: only asks about the fd.
        if unsafe { libc::fcntl(*fd, libc::F_GETFD) } < 0 {
            // Left open on purpose: it has to outlive the message, and this process exits soon.
            *fd = File::open("/dev/null")?.into_raw_fd();
        }
    }
    Ok(fds)
}

/// Stand-in side: have the watcher run the real program. Returns its exit code.
pub fn request(name: &str) -> Result<i32> {
    let Some(sock) = std::env::var_os(SOCK_VAR) else {
        bail!("this {name} is part of Whoopz and only works inside a whoops shell");
    };
    let mut conn = UnixStream::connect(&sock).context("couldn't reach the watcher")?;

    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let env: Vec<OsString> = std::env::vars_os()
        .map(|(key, value)| {
            let mut pair = key;
            pair.push("=");
            pair.push(value);
            pair
        })
        .collect();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));

    // The fds ride along with the first bytes, as in `launcher::send_listener`.
    sendmsg::<()>(
        conn.as_raw_fd(),
        &[IoSlice::new(REQUEST)],
        &[ControlMessage::ScmRights(&standard_fds()?)],
        MsgFlags::empty(),
        None,
    )
    .context("couldn't reach the watcher")?;
    conn.write_all(&encode(name, &cwd, &args, &env))?;
    conn.shutdown(Shutdown::Write)?;

    let mut reply = String::new();
    conn.read_to_string(&mut reply)?;
    let (code, text) = reply.split_once('\n').unwrap_or((&reply, ""));
    eprint!("{text}");
    Ok(code.trim().parse().unwrap_or(1))
}

/// Watcher side: answer a stand-in. `first` is what already arrived with its fds. Blocks until
/// the real program ends, and returns the reply.
pub fn run(
    conn: &UnixStream,
    first: &[u8],
    stdio: Vec<OwnedFd>,
    peer: i32,
    stand_ins: &StandIns,
) -> String {
    let result =
        read_request(conn, first).and_then(|request| start(request, stdio, peer, stand_ins));
    match result {
        Ok(code) => format!("{code}\n"),
        Err(why) => format!("1\nwhoops: {why}\n"),
    }
}

fn read_request(mut conn: &UnixStream, first: &[u8]) -> Result<Request, String> {
    let mut bytes = first.strip_prefix(REQUEST).unwrap_or(first).to_vec();
    conn.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    decode(&bytes).ok_or_else(|| "a stand-in sent something unreadable".to_string())
}

fn start(
    request: Request,
    stdio: Vec<OwnedFd>,
    peer: i32,
    stand_ins: &StandIns,
) -> Result<i32, String> {
    let name = &request.name;
    let real = stand_ins
        .real
        .get(name)
        .ok_or_else(|| format!("{name} isn't something Whoopz can start"))?;
    if !procfs::sees_same_files(peer as u32) {
        return Err(format!(
            "{name} can't run from inside a sandbox: it would run outside it"
        ));
    }
    let peer = Pid::from_raw(peer);
    let session = getsid(Some(peer)).map_err(|e| e.to_string())?;
    if getsid(None).map_err(|e| e.to_string())? != session {
        return Err(format!(
            "{name} can't run here: this isn't the terminal Whoopz started in, so its password \
             prompt would go to the wrong one. Use a terminal outside Whoopz."
        ));
    }
    let group = getpgid(Some(peer)).map_err(|e| e.to_string())?.as_raw();

    let [stdin, stdout, stderr] = <[OwnedFd; 3]>::try_from(stdio)
        .map_err(|_| "a stand-in sent the wrong number of fds".to_string())?;
    note_first_use(name, &stderr);

    let path: Option<OsString> =
        request
            .env
            .iter()
            .find(|(key, _)| key == "PATH")
            .and_then(|(_, value)| {
                let dirs = std::env::split_paths(value).filter(|dir| *dir != stand_ins.bin);
                std::env::join_paths(dirs).ok()
            });
    let env = request
        .env
        .iter()
        .filter(|(key, _)| key != "PATH" && !key.as_bytes().starts_with(b"WHOOPZ_"));

    let mut cmd = Command::new(real);
    cmd.arg0(name)
        .args(&request.args)
        .current_dir(&request.cwd)
        .env_clear()
        .envs(env.map(|(key, value)| (key, value)))
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    if let Some(path) = path {
        cmd.env("PATH", path);
    }
    // SAFETY: the closure runs between fork and exec, with other threads alive, so it may only
    // make plain system calls. These two allocate nothing and take no locks.
    unsafe {
        cmd.pre_exec(move || {
            // Join the stand-in's process group: if that's the terminal's foreground group,
            // the real program can read the password, and Ctrl-C reaches it. A failure leaves
            // it in a background group, where reading stops it, as it would any job.
            libc::setpgid(0, group);
            // The watcher ignores these, and ignored signals survive exec.
            for sig in TERMINAL_SIGNALS {
                let _ = signal(sig, SigHandler::SigDfl);
            }
            Ok(())
        });
    }

    let (sender, receiver) = mpsc::channel();
    {
        // Held across the spawn, so the main thread can't reap the child before it's listed.
        let mut waiting = waiting();
        let child = cmd
            .spawn()
            .map_err(|e| format!("couldn't start {}: {e}", real.display()))?;
        waiting.insert(child.id(), sender);
    }
    Ok(receiver.recv().unwrap_or(1))
}

/// Say once per session that this program isn't watched. Only on a terminal: a script's error
/// output is not the place, and that output may be read by something else.
fn note_first_use(name: &str, stderr: &OwnedFd) {
    let on_terminal = nix::unistd::isatty(stderr.as_fd()).unwrap_or(false);
    if on_terminal && !NOTED.swap(true, Ordering::Relaxed) {
        let Ok(copy) = stderr.try_clone() else { return };
        let _ = writeln!(
            File::from(copy),
            "whoops: {name} runs outside Whoopz, so what it changes can't be undone"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_survives_the_trip() {
        let args = [OsString::from("rm"), OsString::from("a b"), OsString::new()];
        let env = [OsString::from("A=1=2"), OsString::from("EMPTY=")];
        let got = decode(&encode("sudo", Path::new("/tmp/x y"), &args, &env)).unwrap();

        assert_eq!(got.name, "sudo");
        assert_eq!(got.cwd, Path::new("/tmp/x y"));
        assert_eq!(got.args, args);
        assert_eq!(
            got.env,
            [
                (OsString::from("A"), OsString::from("1=2")),
                (OsString::from("EMPTY"), OsString::new()),
            ]
        );
    }

    #[test]
    fn a_broken_request_is_refused() {
        assert!(decode(b"").is_none());
        assert!(decode(b"sudo\0/\0lots\0").is_none());
    }
}
