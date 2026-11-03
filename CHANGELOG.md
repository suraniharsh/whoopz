# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `whoops shell` starts your shell (bash or zsh) with Whoopz watching it.
- `whoops` undoes the last command that changed files: deletes (including `rm -rf`), moves
  and renames, overwrites (`>`, `cp` onto a file, `sed -i`), truncates, and new files and
  folders.
- `whoops list` shows what each command changed, and `whoops <n>` undoes a specific one.
- Undo never overwrites anything: if something new is in the way, that step is skipped and
  reported.
- fish support. `whoops shell` now starts the shell you typed it in (or pick one with
  `--shell fish`), so it works when your terminal runs fish but your login shell is bash.
- A shell saving its own history (fish does this from a background thread, at any moment)
  is no longer counted as part of your command.
- Whoopz is now one static file that runs on any Linux 5.5+ distribution (x86_64 and ARM64),
  with nothing to install: it no longer needs libseccomp. It also watches 32-bit programs.
- Background jobs get their own `[background]` entry in `whoops list` instead of being mixed
  into whatever command is running. Plain `whoops` leaves them alone; `whoops <n>` undoes one.
  Their changes after the prompt is back are now saved too.
- Clear errors on systems where Whoopz can't work (old kernel, seccomp blocked by a sandbox,
  `ptrace_scope` 2 or 3).
- `WHOOPZ_DEBUG=1` (or a file path) shows every paused call and where its process sits in the
  process tree.

### Fixed

- Pasting several lines into bash made them one command, so one `whoops` undid all of them.
  Each pasted line is now its own command.
