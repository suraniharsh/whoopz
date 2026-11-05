# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `whoops status` shows how much disk the saved files take, and which sessions use the most.
- A total size limit for saved files: 5 GB, or set `WHOOPZ_STASH_LIMIT` (`2G`, `500M`). The oldest
  go first. Anything saved in the last hour stays, so a big `rm -rf` can still be undone.

### Changed

- Saved files are removed after a week even if their terminal is still open.
- If a saved copy is gone, undo says so and leaves the current file alone.

### Fixed

- A background job whose first process had already ended (`bash -c 'server &'`) showed up in
  `whoops list` with no name. It's now named after the process that made the change.
- Deleting a socket or named pipe printed "couldn't save" over whatever was on the screen.
  These hold no data, so they're now ignored: Whoopz saves files, folders and symlinks.
- A killed session left its socket behind in `$XDG_RUNTIME_DIR/whoopz/`. The next session now
  removes it.

## [0.1.2] - 2026-11-04

### Added

- Undo `git reset --hard`, `git checkout .`, `git restore .` and `git clean -fd`: your files
  come back and `git status` looks like before.

### Changed

- Undo reports each file once, in plain words ("put back the old version of app.rs"), and
  only counts git's own files in `.git/`. Temporary files a command made and removed again
  (git's `*.lock`, `sed -i`'s temporary file) no longer show up, and no longer cause false
  "skipped" lines.

## [0.1.1] - 2026-11-04

### Security

- Whoopz no longer performs deletes and renames for a program. It did so with your full
  rights and from its own view of the filesystem: for a program in a sandbox or container,
  that could delete a different file than the one the program meant, and it ignored
  restrictions on the program (AppArmor, SELinux, Landlock). Every change now goes ahead in
  the kernel as the program; programs in a sandbox or container aren't saved.
- The undo socket only answers the user who owns it, and the `WHOOPZ_DEBUG` log file is
  readable only by you.

### Changed

- Undo checks whether a delete or rename really happened, and skips it quietly if not.

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

[Unreleased]: https://github.com/suraniharsh/whoopz/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/suraniharsh/whoopz/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/suraniharsh/whoopz/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/suraniharsh/whoopz/releases/tag/v0.1.0
