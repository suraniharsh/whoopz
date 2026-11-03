#!/usr/bin/env python3
"""Record the README demo: a real `whoops shell`, typed into at human speed.

    cargo build --release --target x86_64-unknown-linux-musl
    python3 scripts/demo/record.py target/x86_64-unknown-linux-musl/release/whoops docs/demo.cast
    agg --font-size 22 --theme monokai --last-frame-duration 4 docs/demo.cast docs/demo.gif

It writes an asciinema recording (asciicast v2). Turn it into a GIF with agg
(https://github.com/asciinema/agg), or play it with `asciinema play docs/demo.cast`.

Everything happens in a throwaway folder: a fake home with a small project in it. Nothing
outside that folder is touched. Only Python's standard library is used.
"""

import fcntl
import json
import os
import pty
import select
import shutil
import struct
import sys
import tempfile
import termios
import time

WIDTH, HEIGHT = 72, 18

# The story: a project, an `rm -rf` you regret, and `whoops`.
# Each step: (what to type, seconds to wait after Enter).
STEPS = [
    ("ls", 1.2),
    ("rm -rf src", 1.6),
    ("ls", 2.0),
    ("whoops", 2.4),
    ("ls src", 3.5),
]

PROJECT = {
    "Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\n',
    "README.md": "# app\n",
    "src/main.rs": "fn main() {\n    app::run();\n}\n",
    "src/lib.rs": "pub mod cli;\npub mod net;\n\npub fn run() {}\n",
    "src/cli.rs": "// a week of work\n",
    "src/net/mod.rs": "// another week\n",
}

BASHRC = r"""
PS1='\[\e[1;34m\]\w\[\e[0m\] \[\e[1;32m\]❯\[\e[0m\] '
PROMPT_COMMAND=()
alias ls='ls --color=auto'
"""


def main():
    whoops, cast_path = os.path.abspath(sys.argv[1]), sys.argv[2]
    home = tempfile.mkdtemp(prefix="whoopz-demo-")
    try:
        record(whoops, home, cast_path)
    finally:
        shutil.rmtree(home, ignore_errors=True)


def record(whoops, home, cast_path):
    project = os.path.join(home, "project")
    for name, text in PROJECT.items():
        path = os.path.join(project, name)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            f.write(text)
    with open(os.path.join(home, ".bashrc"), "w") as f:
        f.write(BASHRC)
    run_dir = tempfile.mkdtemp(prefix="wd-", dir="/tmp")  # socket paths must be short

    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(project)
        env = {
            "HOME": home,
            "PATH": os.path.dirname(whoops) + ":/usr/bin:/bin",
            "TERM": "xterm-256color",
            "SHELL": "/bin/bash",
            "LANG": os.environ.get("LANG", "C.UTF-8"),
            "XDG_DATA_HOME": os.path.join(home, ".local/share"),
            "XDG_RUNTIME_DIR": run_dir,
        }
        os.execve(whoops, [whoops, "shell", "--shell", "/bin/bash"], env)

    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", HEIGHT, WIDTH, 0, 0))
    events, start = [], None

    def pump(seconds):
        end = time.time() + seconds
        while time.time() < end:
            ready, _, _ = select.select([fd], [], [], 0.02)
            if not ready:
                continue
            try:
                data = os.read(fd, 65536)
            except OSError:
                return
            # Output before the first keystroke (the first prompt) is shown at time 0.
            at = 0.0 if start is None else time.time() - start
            events.append([round(at, 3), "o", data.decode("utf-8", "replace")])

    def type_line(text):
        for ch in text:
            os.write(fd, ch.encode())
            pump(0.07)
        pump(0.35)
        os.write(fd, b"\r")

    pump(2.0)  # let the shell start; this part isn't shown as time passing
    start = time.time()
    pump(0.8)
    for text, wait in STEPS:
        type_line(text)
        pump(wait)
    os.write(fd, b"exit\r")
    try:
        os.waitpid(pid, 0)
    except ChildProcessError:
        pass
    shutil.rmtree(run_dir, ignore_errors=True)

    # Drop the final "exit" from the recording.
    last = max(i for i, e in enumerate(events) if "ls src" in e[2] or "main.rs" in e[2])
    events = events[: last + 1]

    header = {"version": 2, "width": WIDTH, "height": HEIGHT, "title": "whoops"}
    os.makedirs(os.path.dirname(os.path.abspath(cast_path)), exist_ok=True)
    with open(cast_path, "w") as f:
        f.write(json.dumps(header) + "\n")
        for event in events:
            f.write(json.dumps(event, ensure_ascii=False) + "\n")
    print(f"wrote {cast_path}: {len(events)} events, {events[-1][0]:.1f} s")


if __name__ == "__main__":
    main()
