# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-11-04

### Added

- `whoops shell` starts your shell with Whoopz watching it: bash, zsh, fish, nushell or elvish.
  It starts the shell you typed it in, or the one you pick with `--shell`.
- `whoops` undoes the last command that changed files: deletes (including `rm -rf`), moves and
  renames, overwrites (`>`, `cp` onto a file, `sed -i`), truncates, and new files and folders.
  Each line you type or paste is one command.
- `whoops list` shows what each command changed, and `whoops <n>` undoes a specific one.
- Undo never overwrites anything: if something new is in the way, that step is skipped and
  reported.
- Deleted files are kept with a hard link: instant and free, whatever their size. Files on
  another drive than `~/.local/share` are copied, up to 512 MB.
- Background jobs get their own `[background]` entry, never mixed into the command you're
  running. Plain `whoops` leaves them alone.
- A shell saving its own history is never counted as part of your command.
- One static file for any Linux 5.5 or newer, x86_64 or ARM64, with nothing else to install.
  32-bit programs are watched too. (The ARM64 build hasn't been run on ARM hardware yet.)
- `whoops shell --fallback`: if Whoopz can't start, it says why and starts the shell without
  it, so a terminal config can't leave you without a terminal.
- Clear errors on systems where Whoopz can't work: an old kernel, seccomp blocked by a
  sandbox, or `ptrace_scope` 2 or 3.
- `WHOOPZ_DEBUG=1` (or a file path) shows every call Whoopz pauses.

[Unreleased]: https://github.com/suraniharsh/whoopz/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/suraniharsh/whoopz/releases/tag/v0.1.0
