//! End-to-end tests: run real commands in a watched shell, then `whoops`, and check the files.
//!
//! Most tests run `whoops shell -- -c <script>`. There is no prompt in that mode, so the script
//! sends the hook messages itself: `cmd <text>` before each command and `done_` after it. The
//! `interactive_*` tests type into a real bash or zsh through a fake terminal instead, to check
//! the hooks the launcher installs.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_whoops");

/// Hook messages, the way the bash hook sends them.
const PRELUDE: &str = r#"
cmd() { printf 'start %s\0' "$1" >&"$WHOOPZ_MARK_FD"; }
done_() { printf 'end\0' >&"$WHOOPZ_MARK_FD"; }
done_
"#;

/// A temporary home: files to play with in `work/`, and the session data next to it.
struct Sandbox {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    work: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        Self::with_data_in(None)
    }

    /// `data`: put the stash somewhere else (to test copying across drives).
    fn with_data_in(data: Option<&Path>) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let work = root.join("work");
        fs::create_dir(&work).unwrap();
        let sandbox = Self {
            _tmp: tmp,
            root,
            work,
        };
        if let Some(data) = data {
            fs::create_dir_all(data).unwrap();
            fs::write(
                sandbox.root.join("data-dir"),
                data.as_os_str().as_encoded_bytes(),
            )
            .unwrap();
        }
        sandbox
    }

    fn data_dir(&self) -> PathBuf {
        match fs::read_to_string(self.root.join("data-dir")) {
            Ok(dir) => PathBuf::from(dir),
            Err(_) => self.root.join("data"),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.work.join(name)
    }

    fn write(&self, name: &str, text: &str) {
        let path = self.path(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.path(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn exists(&self, name: &str) -> bool {
        self.path(name).symlink_metadata().is_ok()
    }

    /// Everything under `work/`, sorted.
    fn listing(&self) -> Vec<String> {
        fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                out.push(path.strip_prefix(base).unwrap().display().to_string());
                if path.is_dir() && !path.is_symlink() {
                    walk(&path, base, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.work, &self.work, &mut out);
        out.sort();
        out
    }

    /// Everything every session wrote to its journal.
    fn journals(&self) -> String {
        let sessions = self.data_dir().join("whoopz/sessions");
        let mut text = String::new();
        for entry in fs::read_dir(sessions).into_iter().flatten().flatten() {
            text += &fs::read_to_string(entry.path().join("journal.jsonl")).unwrap_or_default();
        }
        text
    }

    fn command(&self, shell: &str) -> Command {
        let mut cmd = Command::new(BIN);
        let bin_dir = Path::new(BIN).parent().unwrap();
        let path = format!(
            "{}:{}",
            bin_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        cmd.current_dir(&self.work)
            .env("SHELL", shell)
            .env("HOME", &self.root)
            .env("XDG_DATA_HOME", self.data_dir())
            .env("XDG_RUNTIME_DIR", &self.root)
            .env("PATH", path)
            .env_remove("WHOOPZ_ACTIVE")
            .env_remove("WHOOPZ_SOCK")
            .env_remove("ZDOTDIR");
        cmd
    }

    /// Run a bash script inside `whoops shell`.
    fn run(&self, script: &str) -> Run {
        let out = self
            .command("/bin/bash")
            .args(["shell", "--", "-c", &format!("{PRELUDE}{script}")])
            .output()
            .expect("could not run whoops");
        Run(out)
    }
}

struct Run(Output);

impl Run {
    fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.0.stdout).into_owned()
    }
    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.0.stderr).into_owned()
    }
    fn ok(self) -> Self {
        assert!(
            self.0.status.success(),
            "exit {:?}\nstdout:\n{}\nstderr:\n{}",
            self.0.status,
            self.stdout(),
            self.stderr()
        );
        self
    }
}

#[test]
fn rm_then_undo_brings_the_file_back() {
    let s = Sandbox::new();
    s.write("notes.txt", "hello");

    let run = s
        .run("cmd 'rm notes.txt'; rm notes.txt; done_; whoops")
        .ok();

    assert_eq!(s.read("notes.txt"), "hello");
    assert!(
        run.stdout().contains("restored notes.txt"),
        "{}",
        run.stdout()
    );
}

#[test]
fn rm_rf_then_undo_brings_the_whole_tree_back() {
    let s = Sandbox::new();
    s.write("proj/src/main.rs", "fn main() {}");
    s.write("proj/README", "readme");
    s.write("proj/src/deep/er/x", "x");
    std::os::unix::fs::symlink("README", s.path("proj/link")).unwrap();
    let before = s.listing();

    s.run("cmd 'rm -rf proj'; rm -rf proj; done_; test ! -e proj && whoops")
        .ok();

    assert_eq!(s.listing(), before);
    assert_eq!(s.read("proj/src/main.rs"), "fn main() {}");
    assert_eq!(
        fs::read_link(s.path("proj/link")).unwrap(),
        Path::new("README")
    );
}

#[test]
fn mv_onto_an_existing_file_then_undo_brings_both_back() {
    let s = Sandbox::new();
    s.write("a.txt", "a");
    s.write("b.txt", "b");

    s.run("cmd 'mv a.txt b.txt'; mv a.txt b.txt; done_; whoops")
        .ok();

    assert_eq!(s.read("a.txt"), "a");
    assert_eq!(s.read("b.txt"), "b");
}

#[test]
fn overwrite_then_undo_brings_the_old_contents_back() {
    let s = Sandbox::new();
    s.write("notes.txt", "old");

    s.run("cmd 'echo new > notes.txt'; echo new > notes.txt; done_; whoops")
        .ok();

    assert_eq!(s.read("notes.txt"), "old");
}

#[test]
fn sed_in_place_then_undo_leaves_no_trace() {
    let s = Sandbox::new();
    s.write("conf", "x=1\n");
    let before = s.listing();

    s.run("cmd sed; sed -i s/1/2/ conf; done_; grep -q x=2 conf && whoops")
        .ok();

    assert_eq!(s.read("conf"), "x=1\n");
    assert_eq!(
        s.listing(),
        before,
        "sed's temporary file should be gone too"
    );
}

#[test]
fn cp_to_a_new_file_then_undo_removes_it() {
    let s = Sandbox::new();
    s.write("a", "data");

    s.run("cmd cp; cp a b; done_; test -e b && whoops").ok();

    assert!(!s.exists("b"));
    assert_eq!(s.read("a"), "data");
}

#[test]
fn mkdir_and_new_files_then_undo_removes_them() {
    let s = Sandbox::new();

    s.run("cmd mk; mkdir -p x/y && touch x/y/z && ln -s z x/y/l; done_; whoops")
        .ok();

    assert!(s.listing().is_empty(), "{:?}", s.listing());
}

#[test]
fn truncate_then_undo_brings_the_contents_back() {
    let s = Sandbox::new();
    s.write("log", "lots of lines\n");

    s.run("cmd t; truncate -s 0 log; done_; test ! -s log && whoops")
        .ok();

    assert_eq!(s.read("log"), "lots of lines\n");
}

#[test]
fn undo_goes_one_command_at_a_time_and_by_number() {
    // The script as a whole is command 1, so `rm a` is command 2.
    let s = Sandbox::new();
    s.write("a", "a");
    s.write("b", "b");
    s.write("c", "c");

    let run = s
        .run(
            "cmd 'rm a'; rm a; done_
             cmd 'rm b'; rm b; done_
             cmd 'rm c'; rm c; done_
             whoops 2 && test -e a && test ! -e b && test ! -e c
             whoops && test -e c && test ! -e b
             whoops && test -e b
             whoops list
             whoops; echo \"exit=$?\"",
        )
        .ok();

    let out = run.stdout();
    assert!(out.contains("(undone)"), "{out}");
    assert!(out.contains("Nothing to undo."), "{out}");
    assert!(out.contains("exit=1"), "{out}");
    assert_eq!(s.read("a") + &s.read("b") + &s.read("c"), "abc");
}

#[test]
fn undo_never_overwrites_something_new() {
    let s = Sandbox::new();
    s.write("f", "original");

    let run = s
        .run("cmd 'rm f'; rm f; done_; echo replacement > f; whoops; echo \"exit=$?\"")
        .ok();

    assert_eq!(s.read("f"), "replacement\n");
    assert!(run.stdout().contains("skipped"), "{}", run.stdout());
    assert!(run.stdout().contains("exit=1"), "{}", run.stdout());
}

#[test]
fn changes_between_commands_are_not_saved() {
    let s = Sandbox::new();
    s.write("f", "x");

    let run = s.run("rm f; whoops list").ok();

    assert!(
        run.stdout().contains("Nothing to undo yet."),
        "{}",
        run.stdout()
    );
}

#[test]
fn reads_are_not_recorded() {
    let s = Sandbox::new();
    s.write("f", "hello");

    let run = s.run("cmd cat; cat f >&2; done_; whoops list").ok();

    assert!(
        run.stdout().contains("Nothing to undo yet."),
        "{}",
        run.stdout()
    );
}

#[test]
fn files_on_another_drive_are_copied() {
    // The stash goes under target/ (on the project's drive), the files under /tmp. If those
    // are the same drive here, this test has nothing to show and passes trivially.
    let data =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("xdev-{}", std::process::id()));
    let s = Sandbox::with_data_in(Some(&data));
    s.write("notes.txt", "hello");
    s.write("other", "old");

    s.run(
        "cmd 'rm notes.txt'; rm notes.txt; done_
         cmd over; echo new > other; done_
         whoops && whoops",
    )
    .ok();

    assert_eq!(s.read("notes.txt"), "hello");
    assert_eq!(s.read("other"), "old");
    let _ = fs::remove_dir_all(&data);
}

#[test]
fn exit_code_is_passed_through() {
    let s = Sandbox::new();
    let out = s
        .command("/bin/bash")
        .args(["shell", "--", "-c", "exit 3"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
}

#[test]
fn refuses_to_start_inside_itself() {
    let s = Sandbox::new();
    let out = s
        .command("/bin/bash")
        .args(["shell", "--", "-c", "true"])
        .env("WHOOPZ_ACTIVE", "1")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("already inside"));
}

#[test]
fn fallback_starts_the_shell_unwatched_when_whoopz_cant_start() {
    let s = Sandbox::new();
    // Starting inside a whoops shell is refused, which is an easy way to make startup fail.
    let out = s
        .command("/bin/bash")
        .args(["shell", "--fallback", "--", "-c", "echo plain shell"])
        .env("WHOOPZ_ACTIVE", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "plain shell\n");
    assert!(String::from_utf8_lossy(&out.stderr).contains("without Whoopz"));
}

#[test]
fn undo_outside_a_whoops_shell_explains_itself() {
    let s = Sandbox::new();
    let out = s.command("/bin/bash").output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not inside a whoops shell"));
}

// --- interactive shells, through a fake terminal ---

/// A shell running in a pseudo-terminal, typed into like a person would.
struct Terminal {
    input: fs::File,
    output: Arc<Mutex<String>>,
    child: std::process::Child,
}

impl Terminal {
    fn start(s: &Sandbox, shell: &str) -> Self {
        let pty = nix::pty::openpty(None, None).unwrap();
        let slave = fs::File::from(pty.slave);
        let mut cmd = s.command(shell);
        cmd.arg("shell")
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // Like a real terminal: the shell gets its own session, with the fake terminal as its
        // controlling terminal. Without one, job control fails, and fish waits forever.
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap();

        let master = fs::File::from(pty.master);
        let output = Arc::new(Mutex::new(String::new()));
        {
            let output = output.clone();
            let mut reader = master.try_clone().unwrap();
            let mut answer = master.try_clone().unwrap();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let chunk = &buf[..n];
                    // Answer like a real terminal: fish 4 waits for "what terminal are you?"
                    // before its first prompt, and some line editors ask where the cursor is.
                    if contains(chunk, b"\x1b[0c") || contains(chunk, b"\x1b[c") {
                        let _ = answer.write_all(b"\x1b[?62;22c");
                    }
                    if contains(chunk, b"\x1b[6n") {
                        let _ = answer.write_all(b"\x1b[1;1R");
                    }
                    output
                        .lock()
                        .unwrap()
                        .push_str(&String::from_utf8_lossy(chunk));
                }
            });
        }
        let mut term = Self {
            input: master,
            output,
            child,
        };
        term.run("true", 1);
        term
    }

    /// Type a command line, then wait until it has finished. Each line ends by printing a
    /// marker the shell glues together (`DONE` + n), so the typed text, which the terminal
    /// echoes back, can't be mistaken for it. `printf` works the same in every shell.
    fn run(&mut self, line: &str, n: u32) {
        let marker = format!("DONE{n}");
        // `cmd &; next` is a syntax error; `&` already separates commands.
        let sep = if line.trim_end().ends_with('&') {
            " "
        } else {
            "; "
        };
        writeln!(self.input, "{line}{sep}printf '%s%s\\n' DONE {n}").unwrap();
        let start = Instant::now();
        while !self.output.lock().unwrap().contains(&marker) {
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "timed out waiting for `{line}`. Output so far:\n{}",
                self.output.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // Let the prompt come back (and the hook send "end") before typing on.
        std::thread::sleep(Duration::from_millis(200));
    }

    /// Paste several lines at once, the way a terminal does it (bracketed paste). Bash then
    /// runs them back to back without showing a prompt in between.
    fn paste(&mut self, lines: &[&str], n: u32) {
        let marker = format!("DONE{n}");
        let text = lines.join("\n");
        write!(
            self.input,
            "\x1b[200~{text}\nprintf '%s%s\\n' DONE {n}\x1b[201~\n"
        )
        .unwrap();
        let start = Instant::now();
        while !self.output.lock().unwrap().contains(&marker) {
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "timed out waiting for the paste. Output so far:\n{}",
                self.output.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    fn finish(mut self) -> String {
        writeln!(self.input, "exit").unwrap();
        let _ = self.child.wait();
        self.output.lock().unwrap().clone()
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The path of a shell, or `None` if it isn't installed (the test is then skipped).
fn installed(name: &str) -> Option<String> {
    let found = ["/usr/bin", "/bin"]
        .iter()
        .map(|dir| format!("{dir}/{name}"))
        .find(|p| Path::new(p).exists());
    if found.is_none() {
        eprintln!("{name} isn't installed; skipping");
    }
    found
}

fn interactive_undo_is_per_command_line(shell: &str) {
    let s = Sandbox::new();
    s.write("a", "a");
    s.write("b", "b");

    let mut term = Terminal::start(&s, shell);
    term.run("rm a", 2);
    term.run("rm b", 3);
    term.run("whoops", 4); // undoes `rm b` only
    let only_b = s.exists("b") && !s.exists("a");
    term.run("whoops", 5); // then `rm a`
    let out = term.finish();
    assert!(only_b, "the first undo should bring back only b:\n{out}");
    assert!(s.exists("a"), "the second undo should bring back a:\n{out}");
    assert!(out.contains("Undid"), "{out}");
}

#[test]
fn interactive_bash_undo_is_per_command_line() {
    interactive_undo_is_per_command_line("/bin/bash");
}

#[test]
fn interactive_bash_pasted_lines_are_separate_commands() {
    let s = Sandbox::new();
    s.write("a", "a");
    s.write("b", "b");

    let mut term = Terminal::start(&s, "/bin/bash");
    term.run("bind 'set enable-bracketed-paste on'", 2);
    term.paste(&["rm a", "rm b"], 3);
    term.run("whoops", 4); // undoes `rm b` only
    let only_b = s.exists("b") && !s.exists("a");
    term.run("whoops", 5);
    let out = term.finish();
    assert!(only_b, "the first undo should bring back only b:\n{out}");
    assert!(s.exists("a"), "the second undo should bring back a:\n{out}");
}

#[test]
fn interactive_bash_one_line_with_semicolons_is_one_command() {
    let s = Sandbox::new();
    s.write("a", "a");
    s.write("b", "b");

    let mut term = Terminal::start(&s, "/bin/bash");
    term.run("rm a; rm b", 2);
    term.run("whoops", 3);
    let out = term.finish();
    assert!(s.exists("a") && s.exists("b"), "{out}");
}

#[test]
fn interactive_zsh_undo_is_per_command_line() {
    if let Some(zsh) = installed("zsh") {
        interactive_undo_is_per_command_line(&zsh);
    }
}

#[test]
fn interactive_fish_undo_is_per_command_line() {
    if let Some(fish) = installed("fish") {
        interactive_undo_is_per_command_line(&fish);
    }
}

#[test]
fn interactive_fish_redirect_is_undone() {
    // fish opens `> file` itself, in the shell process, so only the hook can tell which
    // command it belongs to.
    let Some(fish) = installed("fish") else {
        return;
    };
    let s = Sandbox::new();
    s.write("notes", "old\n");

    let mut term = Terminal::start(&s, &fish);
    term.run("echo new > notes", 2);
    term.run("whoops", 3);
    let out = term.finish();
    assert_eq!(s.read("notes"), "old\n", "{out}");
}

/// A background job's changes get their own entry. They never get mixed into the command
/// that happens to be running, and plain `whoops` leaves them alone.
fn background_jobs_get_their_own_entry(shell: &str) {
    let s = Sandbox::new();
    s.write("a", "a");

    let mut term = Terminal::start(&s, shell);
    term.run("sh -c 'sleep 1; echo bg > bgfile' &", 2);
    term.run("rm a", 3);
    term.run("sleep 2", 4); // the background job writes its file during this command
    term.run("whoops", 5); // must undo `rm a`
    term.run("whoops list", 6);
    let out = term.finish();
    assert!(s.exists("a"), "`rm a` should be undone:\n{out}");
    assert!(
        s.exists("bgfile"),
        "the background job's file must stay:\n{out}"
    );
    assert!(out.contains("[background]"), "{out}");
}

/// What fish does on its first start: a process in the shell's own group (no job of its own),
/// started before a command, writing files while that command runs. It isn't part of it.
#[test]
fn older_processes_in_the_shells_group_are_not_part_of_a_command() {
    let s = Sandbox::new();
    s.write("a", "a");

    let mut term = Terminal::start(&s, "/bin/bash");
    // With job control off (`set +m`), a `&` job stays in the shell's own group.
    term.run("set +m; sh -c 'sleep 1; echo old > oldfile' & set -m", 2);
    term.run("rm a", 3);
    term.run("sleep 2", 4); // the old process writes its file during this command
    term.run("whoops", 5);
    term.run("whoops list", 6);
    let out = term.finish();
    assert!(s.exists("a"), "`rm a` should be undone:\n{out}");
    assert!(
        s.exists("oldfile"),
        "the older process's file must stay:\n{out}"
    );
}

#[test]
fn bash_background_jobs_get_their_own_entry() {
    background_jobs_get_their_own_entry("/bin/bash");
}

#[test]
fn fish_background_jobs_get_their_own_entry() {
    if let Some(fish) = installed("fish") {
        background_jobs_get_their_own_entry(&fish);
    }
}

/// The shell saving its own history isn't part of your command.
fn history_is_not_recorded(shell: &str, save_history: &str, history_file: &str) {
    let s = Sandbox::new();
    let mut term = Terminal::start(&s, shell);
    term.run("true", 2);
    term.run(save_history, 3);
    let out = term.finish();
    let journals = s.journals();
    assert!(
        !journals.contains(history_file),
        "the history file shouldn't be in the journal:\n{journals}\n{out}"
    );
}

#[test]
fn bash_history_is_not_recorded() {
    history_is_not_recorded("/bin/bash", "history -w", ".bash_history");
}

#[test]
fn fish_history_is_not_recorded() {
    if let Some(fish) = installed("fish") {
        history_is_not_recorded(&fish, "builtin history save", "fish_history");
    }
}

#[test]
fn shell_flag_picks_the_shell() {
    let s = Sandbox::new();
    let out = s
        .command("/bin/false")
        .args(["shell", "--shell", "bash", "--", "-c", "echo $SHELL"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .ends_with("/bash")
    );
}
