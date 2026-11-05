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
            "{}:{}:{}",
            self.root.join("fake").display(),
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

/// A program in a sandbox sees a different filesystem: inside it, a path can name another
/// file. Whoopz must never act on the file *it* sees under that path. (A version that did the
/// delete itself deleted the host's file here.)
#[test]
fn a_sandboxed_program_never_makes_whoopz_touch_the_wrong_file() {
    let sandboxes = Command::new("unshare")
        .args(["-rm", "true"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !sandboxes {
        eprintln!("unprivileged sandboxes aren't allowed here; skipping");
        return;
    }
    let s = Sandbox::new();
    s.write("host/f", "the host's file");
    s.write("inner/f", "the file the sandbox sees");
    let (host, inner) = (s.path("host"), s.path("inner"));

    // Inside the sandbox, `inner` is mounted over `host`: deleting host/f there deletes inner/f.
    s.run(&format!(
        "cmd sandboxed; unshare -rm sh -c 'mount --bind \"$1\" \"$2\" && rm \"$2/f\"' sh {} {}; done_",
        inner.display(),
        host.display()
    ))
    .ok();

    assert_eq!(
        s.read("host/f"),
        "the host's file",
        "the host's file must be untouched"
    );
    assert!(
        !s.exists("inner/f"),
        "the sandbox's own delete should still happen"
    );
}

/// A delete the program wasn't allowed to do leaves nothing to undo, and says so calmly.
#[test]
fn a_delete_that_failed_leaves_nothing_to_undo() {
    use std::os::unix::fs::PermissionsExt;
    let s = Sandbox::new();
    s.write("ro/f", "keep");
    fs::set_permissions(s.path("ro"), fs::Permissions::from_mode(0o555)).unwrap();

    let run = s
        .run("cmd 'rm ro/f'; rm -f ro/f 2>/dev/null; done_; whoops; echo \"exit=$?\"")
        .ok();

    fs::set_permissions(s.path("ro"), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(s.read("ro/f"), "keep");
    let out = run.stdout();
    assert!(out.contains("nothing to put back"), "{out}");
    assert!(!out.contains("skipped"), "{out}");
    assert!(out.contains("exit=0"), "{out}");
}

/// The classic: a day of uncommitted work, then `git reset --hard`. Undo brings back the
/// edits and the staged new file, `git status` is as before, and the report is short: git's
/// temporary `*.lock` files don't show up, and its own files are only counted.
#[test]
fn git_reset_hard_is_undone_cleanly() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("git isn't installed; skipping");
        return;
    }
    let s = Sandbox::new();
    let git = "git -c user.email=t@t -c user.name=t -c commit.gpgSign=false";
    s.write("app.rs", "v1\n");
    s.write("src/lib.rs", "lib v1\n");
    let run = s
        .run(&format!(
            "{git} init -q && {git} add . && {git} commit -qm one
             echo 'a day of work' > app.rs; echo 'lib v2' > src/lib.rs
             echo 'never committed' > draft.rs; {git} add draft.rs
             before=$({git} status --short)
             cmd 'git reset --hard'; {git} reset -q --hard; done_
             whoops; echo \"exit=$?\"
             test \"$({git} status --short)\" = \"$before\" && echo SAME-STATUS"
        ))
        .ok();

    let out = run.stdout();
    assert_eq!(s.read("app.rs"), "a day of work\n", "{out}");
    assert_eq!(s.read("src/lib.rs"), "lib v2\n", "{out}");
    assert_eq!(s.read("draft.rs"), "never committed\n", "{out}");
    assert!(
        out.contains("SAME-STATUS"),
        "git status should be as before:\n{out}"
    );
    assert!(out.contains("put back the old version of app.rs"), "{out}");
    assert!(out.contains("of git's own file"), "{out}");
    assert!(
        !out.contains(".lock"),
        "lock files shouldn't be listed:\n{out}"
    );
    assert!(!out.contains("skipped"), "{out}");
    assert!(out.contains("exit=0"), "{out}");
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

/// Overwriting a file is undone. In most shells the shell writes that file itself, not a
/// separate program, so only the hook can tell which command it belongs to.
fn overwrite_is_undone(shell: &str, overwrite_notes: &str) {
    let s = Sandbox::new();
    s.write("notes", "old\n");

    let mut term = Terminal::start(&s, shell);
    term.run(overwrite_notes, 2);
    term.run("whoops", 3);
    let out = term.finish();
    assert_eq!(s.read("notes"), "old\n", "{out}");
}

#[test]
fn interactive_fish_redirect_is_undone() {
    if let Some(fish) = installed("fish") {
        overwrite_is_undone(&fish, "echo new > notes");
    }
}

#[test]
fn interactive_nu_undo_is_per_command_line() {
    // `rm` is built into nushell: the shell itself deletes the file.
    if let Some(nu) = installed("nu") {
        interactive_undo_is_per_command_line(&nu);
    }
}

#[test]
fn interactive_nu_save_is_undone() {
    if let Some(nu) = installed("nu") {
        overwrite_is_undone(&nu, r#""new\n" | save -f notes"#);
    }
}

#[test]
fn nu_history_is_not_recorded() {
    if let Some(nu) = installed("nu") {
        history_is_not_recorded(&nu, "ls | length", "history.txt");
    }
}

#[test]
fn interactive_elvish_undo_is_per_command_line() {
    if let Some(elvish) = installed("elvish") {
        interactive_undo_is_per_command_line(&elvish);
    }
}

#[test]
fn interactive_elvish_redirect_is_undone() {
    if let Some(elvish) = installed("elvish") {
        overwrite_is_undone(&elvish, "echo new > notes");
    }
}

#[test]
fn elvish_history_is_not_recorded() {
    // elvish's history is written by its own background daemon, a separate process.
    if let Some(elvish) = installed("elvish") {
        history_is_not_recorded(&elvish, "true", "db.bolt");
    }
}

/// elvish can only be given one rc file, so Whoopz's loads yours. What yours defines must
/// still be there.
#[test]
fn elvish_keeps_your_rc() {
    let Some(elvish) = installed("elvish") else {
        return;
    };
    let s = Sandbox::new();
    let rc = s.root.join(".config/elvish/rc.elv");
    fs::create_dir_all(rc.parent().unwrap()).unwrap();
    fs::write(&rc, "var my-var = 42\nfn my-fn { echo from-my-rc }\n").unwrap();

    let mut term = Terminal::start(&s, &elvish);
    term.run("my-fn; echo var-is-$my-var", 2);
    let out = term.finish();
    assert!(out.contains("from-my-rc"), "{out}");
    assert!(out.contains("var-is-42"), "{out}");
}

/// elvish has no process groups at all: a process started in the background by one command
/// must still not be counted as part of a later one.
#[test]
fn elvish_background_processes_get_their_own_entry() {
    let Some(elvish) = installed("elvish") else {
        return;
    };
    let s = Sandbox::new();
    s.write("a", "a");

    let mut term = Terminal::start(&s, &elvish);
    term.run("sh -c '(sleep 1; echo bg > bgfile) &'", 2);
    term.run("rm a", 3);
    term.run("sleep 2", 4);
    term.run("whoops", 5);
    let out = term.finish();
    assert!(s.exists("a"), "`rm a` should be undone:\n{out}");
    assert!(s.exists("bgfile"), "the background file must stay:\n{out}");
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
    // Hooks that write to /dev/fd/<n> must not show up either (that's a pipe, not a file).
    for pseudo in ["/dev/fd/", "/proc/"] {
        assert!(
            !journals.contains(pseudo),
            "{pseudo} in the journal:\n{journals}"
        );
    }
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

/// `bash -c 'server &'` exits straight away and leaves the job running. The process the entry
/// would be named after is gone, so the name comes from the process that made the change.
#[test]
fn a_background_job_whose_first_process_ended_still_has_a_name() {
    let s = Sandbox::new();
    s.run(
        r#"
cmd 'start the job'
setsid sh -c '(sleep 0.3; touch late) &'
done_
sleep 1
"#,
    )
    .ok();

    assert!(s.exists("late"));
    let journals = s.journals();
    let entry = journals
        .lines()
        .find(|line| line.contains(r#""kind":"command""#) && line.contains(r#""background":true"#))
        .unwrap_or_else(|| panic!("no background entry:\n{journals}"));
    assert!(entry.contains("touch late"), "{entry}");
}

/// The sockets in the runtime folder, by name.
fn sockets(s: &Sandbox) -> Vec<String> {
    let dir = s.root.join("whoopz");
    let mut names: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Start a session that stays open, and return it with the name of its socket.
fn open_session(s: &Sandbox) -> (std::process::Child, String) {
    let before = sockets(s);
    let child = s
        .command("/bin/bash")
        .args(["shell", "--", "-c", "sleep 5"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let name = new_socket(s, &before);
    (child, name)
}

/// Wait for a socket that isn't in `before`.
fn new_socket(s: &Sandbox, before: &[String]) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(new) = sockets(s).into_iter().find(|name| !before.contains(name)) {
            return new;
        }
        assert!(Instant::now() < deadline, "the session made no socket");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A killed session can't remove its own socket, so the next one does. A session that's still
/// running keeps its socket.
#[test]
fn a_killed_sessions_socket_is_cleaned_up() {
    let s = Sandbox::new();
    let (mut running, running_sock) = open_session(&s);
    let (mut killed, killed_sock) = open_session(&s);
    killed.kill().unwrap();
    killed.wait().unwrap();
    assert!(sockets(&s).contains(&killed_sock));

    s.run("true").ok();

    let left = sockets(&s);
    assert!(!left.contains(&killed_sock), "{left:?}");
    assert!(left.contains(&running_sock), "{left:?}");
    running.kill().unwrap();
    running.wait().unwrap();
}

/// Sockets, named pipes and devices hold no data, so they're neither saved nor warned about.
fn special_files_are_ignored(s: &Sandbox) {
    let made = Command::new("mkfifo")
        .args([s.path("gone"), s.path("over")])
        .status()
        .unwrap();
    assert!(made.success());
    s.write("note", "hello");

    let run = s
        .run(
            "cmd 'rm gone'; rm gone; done_
             cmd 'mv note over'; mv note over; done_
             whoops",
        )
        .ok();

    assert!(!run.stderr().contains("couldn't save"), "{}", run.stderr());
    assert!(!s.journals().contains("/gone"), "{}", s.journals());
    assert_eq!(s.read("note"), "hello");
    assert!(!s.exists("gone") && !s.exists("over"));
}

#[test]
fn special_files_are_ignored_on_the_same_drive() {
    special_files_are_ignored(&Sandbox::new());
}

#[test]
fn special_files_are_ignored_across_drives() {
    // As in `files_on_another_drive_are_copied`: the stash under target/, the files under /tmp.
    let data =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("special-{}", std::process::id()));
    special_files_are_ignored(&Sandbox::with_data_in(Some(&data)));
    let _ = fs::remove_dir_all(&data);
}

/// Run `whoops status` outside any shell.
fn status(s: &Sandbox, limit: Option<&str>) -> String {
    let mut cmd = s.command("/bin/bash");
    if let Some(limit) = limit {
        cmd.env("WHOOPZ_STASH_LIMIT", limit);
    }
    let out = cmd.arg("status").output().unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn status_shows_what_is_kept() {
    let s = Sandbox::new();
    assert!(status(&s, None).contains("Saved files: 0 B of 5.0 GB"));

    s.write("big", &"x".repeat(300_000));
    s.run("cmd 'rm big'; rm big; done_").ok();

    let text = status(&s, Some("1G"));
    assert!(text.contains(" KB of 1.0 GB"), "{text}");
    assert!(text.contains("Sessions:    1 (0 running)"), "{text}");
    assert!(text.contains("ended"), "{text}");
}

#[test]
fn status_inside_a_shell_marks_that_shell() {
    let s = Sandbox::new();
    s.write("big", &"x".repeat(300_000));

    let run = s.run("cmd 'rm big'; rm big; done_; whoops status").ok();

    assert!(run.stdout().contains("(this shell)"), "{}", run.stdout());
    assert!(run.stdout().contains("(1 running)"), "{}", run.stdout());
}

#[test]
fn a_limit_that_makes_no_sense_never_stops_the_shell() {
    let s = Sandbox::new();
    let out = s
        .command("/bin/bash")
        .env("WHOOPZ_STASH_LIMIT", "lots")
        .args(["shell", "--", "-c", "echo started"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("started"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("WHOOPZ_STASH_LIMIT"));
}

/// Between commands, so the watcher lets it through unsaved.
const DROP_SAVED_FILES: &str = r#"rm -f "$XDG_DATA_HOME"/whoopz/sessions/*/stash/*"#;

/// When a saved copy was dropped, undo says so and leaves the current files alone. (Taking the
/// current version away first would lose the only copy left.)
#[test]
fn undo_keeps_the_current_file_when_the_saved_copy_was_dropped() {
    let s = Sandbox::new();
    s.write("deleted", "old");
    s.write("overwritten", "old");
    s.write("replaced", "old");

    let run = s.run(&format!(
        "cmd change
         rm deleted
         echo new > overwritten
         sed -i s/old/new/ replaced
         done_
         {DROP_SAVED_FILES}
         whoops"
    ));

    assert!(!s.exists("deleted"));
    assert_eq!(s.read("overwritten"), "new\n");
    assert_eq!(s.read("replaced"), "new");
    let text = run.stdout();
    assert_eq!(text.matches("its saved copy is gone").count(), 3, "{text}");
}

/// Install a fake `sudo` that writes what it sees to `report` in the sandbox's root, and exits 7.
/// `slow <seconds>` makes it wait instead, after writing its process id to `pid`.
fn fake_sudo(s: &Sandbox) {
    use std::os::unix::fs::PermissionsExt;
    let script = format!(
        r#"#!/bin/sh
if [ "$1" = slow ]; then
    echo $$ > {root}/pid
    sleep "$2"
    exit 0
fi
{{
    echo "args=[$1][$2]"
    echo "cwd=$(pwd)"
    echo "path=$PATH"
    echo "whoopz vars: $(env | grep -c WHOOPZ)"
    read -r _ _ _ _ group _ _ foreground _ < /proc/$$/stat
    echo "group=$group foreground=$foreground"
    grep -E '^(NoNewPrivs|Seccomp|SigIgn):' /proc/self/status
    ls -l /proc/$$/fd
    echo "stdin=$(cat)"
}} >> {root}/report
exit 7
"#,
        root = s.root.display()
    );
    let dir = s.root.join("fake");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("sudo"), script).unwrap();
    fs::set_permissions(dir.join("sudo"), fs::Permissions::from_mode(0o755)).unwrap();
}

fn sudo_report(s: &Sandbox) -> String {
    fs::read_to_string(s.root.join("report")).unwrap_or_default()
}

/// The kernel won't let a process with "no new privileges" become root, and everything in a
/// watched shell has it. So the real program must be started from outside the shell.
#[test]
fn sudo_runs_outside_the_watched_shell() {
    let s = Sandbox::new();
    fake_sudo(&s);
    s.write("sub/file", "x");

    let run = s
        .run("cd sub; echo hello | sudo 'a b' c; echo code=${PIPESTATUS[1]}")
        .ok();

    let report = sudo_report(&s);
    assert!(run.stdout().contains("code=7"), "{}", run.stdout());
    assert!(report.contains("args=[a b][c]"), "{report}");
    assert!(report.contains("/work/sub\n"), "{report}");
    assert!(report.contains("stdin=hello"), "{report}");
    assert!(report.contains("whoopz vars: 0"), "{report}");
    assert!(
        !report.contains("sessions/"),
        "stand-ins left on PATH: {report}"
    );
    for line in ["NoNewPrivs:\t0", "Seccomp:\t0"] {
        assert!(report.contains(line), "{line}\n{report}");
    }
    // Nothing of the watcher's leaks into the program, least of all the seccomp listener.
    for leak in ["anon_inode", "socket:", "journal", "stash"] {
        assert!(!report.contains(leak), "{leak}\n{report}");
    }
    // The watcher ignores these, and ignored signals survive exec.
    let ignored = report
        .lines()
        .find_map(|line| line.strip_prefix("SigIgn:\t"))
        .and_then(|mask| u64::from_str_radix(mask, 16).ok())
        .unwrap();
    for sig in [
        libc::SIGINT,
        libc::SIGQUIT,
        libc::SIGTSTP,
        libc::SIGTTIN,
        libc::SIGTTOU,
    ] {
        assert_eq!(ignored & (1 << (sig - 1)), 0, "signal {sig}");
    }
}

/// `sudo` needs the terminal, to ask for a password. A background process that tries gets
/// stopped, so it has to be in the terminal's foreground group.
fn sudo_gets_the_terminal(shell: &str) {
    let s = Sandbox::new();
    fake_sudo(&s);
    let mut term = Terminal::start(&s, shell);
    term.run("sudo first </dev/null", 2);
    term.run("sudo second </dev/null", 3);
    let out = term.finish();

    let report = sudo_report(&s);
    for line in report.lines().filter(|l| l.starts_with("group=")) {
        let (group, foreground) = line.split_once(' ').unwrap();
        assert_eq!(
            group.strip_prefix("group="),
            foreground.strip_prefix("foreground="),
            "{report}"
        );
    }
    assert_eq!(report.matches("group=").count(), 2, "{report}\n{out}");
    assert_eq!(out.matches("runs outside Whoopz").count(), 1, "{out}");
}

#[test]
fn bash_sudo_gets_the_terminal() {
    sudo_gets_the_terminal("/bin/bash");
}

#[test]
fn fish_sudo_gets_the_terminal() {
    if let Some(fish) = installed("fish") {
        sudo_gets_the_terminal(&fish);
    }
}

#[test]
fn ctrl_c_stops_sudo() {
    let s = Sandbox::new();
    fake_sudo(&s);
    let mut term = Terminal::start(&s, "/bin/bash");
    writeln!(term.input, "sudo slow 30").unwrap();

    let pid_file = s.root.join("pid");
    let start = Instant::now();
    while fs::read_to_string(&pid_file).unwrap_or_default().is_empty() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "sudo never started"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let pid = fs::read_to_string(&pid_file).unwrap();
    term.input.write_all(b"\x03").unwrap();
    term.run("true", 2);

    let proc_dir = PathBuf::from(format!("/proc/{}", pid.trim()));
    let start = Instant::now();
    while proc_dir.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "sudo kept running"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    term.finish();
}

/// A long `sudo` must not block `whoops`, which uses the same socket.
#[test]
fn whoops_still_answers_while_sudo_runs() {
    let s = Sandbox::new();
    fake_sudo(&s);
    let run = s
        .run("sudo slow 3 & sleep 0.3; timeout 1 whoops list; echo code=$?; wait")
        .ok();
    assert!(run.stdout().contains("code=0"), "{}", run.stdout());
}

#[test]
fn sudo_is_refused_on_another_terminal() {
    let s = Sandbox::new();
    fake_sudo(&s);
    let run = s.run("setsid -w sudo a b; echo code=$?").ok();
    assert!(run.stdout().contains("code=1"), "{}", run.stdout());
    assert!(run.stderr().contains("wrong one"), "{}", run.stderr());
    assert_eq!(sudo_report(&s), "");
}

/// The real program would run outside the sandbox, so asking from inside one must fail.
#[test]
fn sudo_is_refused_from_a_sandbox() {
    let sandboxes = Command::new("unshare")
        .args(["-rm", "true"])
        .status()
        .is_ok_and(|status| status.success());
    if !sandboxes {
        return;
    }
    let s = Sandbox::new();
    fake_sudo(&s);
    let run = s.run("unshare -rm sudo a b; echo code=$?").ok();
    assert!(run.stdout().contains("code=1"), "{}", run.stdout());
    assert!(run.stderr().contains("sandbox"), "{}", run.stderr());
    assert_eq!(sudo_report(&s), "");
}
