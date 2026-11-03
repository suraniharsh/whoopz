# Whoopz

**Always-on undo for the Linux terminal.**

Ran `rm`, `mv`, `>`, `sed -i` or `cp` by mistake? Type `whoops` and your files come back.

> **Status: early development. Nothing works yet.** Do not trust it with files you care about.

## The idea

Tools like [`try`](https://github.com/binpash/try) and [`maybe`](https://github.com/p-e-w/maybe)
only help if you knew a command was risky *before* you ran it. Safe `rm` replacements like
[`rip`](https://github.com/MilesCranmer/rip2) and [`trash-cli`](https://github.com/andreafrancia/trash-cli)
only cover `rm`.

Whoopz is always on, covers every program, and works *after* the mistake.

## How it works

Your shell runs under a small watcher. Using Linux's
[seccomp user notification](https://man7.org/linux/man-pages/man2/seccomp_unotify.2.html), the
kernel pauses only the system calls that change files (delete, rename, overwrite, truncate) and
asks the watcher first. The watcher saves what is about to be lost, writes a line in a journal,
and lets the call go ahead. Reads are never paused, so the terminal stays fast.

Deleted files are kept with a hard link, so even a 10 GB file is saved instantly, with no copy.

`whoops` reads the journal backwards and puts everything back.

## Planned commands

```sh
whoops          # undo the last command
whoops list     # show recent commands and what they changed
whoops 3        # undo command #3
```

## What it can't undo

- Anything that leaves your machine: `git push`, email, API calls
- Databases changing their own files
- `sudo` commands (they error inside a Whoopz shell)
- Files on another drive than the stash (they need a real copy)

Whoopz is a **safety net, not a sandbox**. It is not a security boundary. See
[SECURITY.md](SECURITY.md).

## Requirements

- Linux 5.5 or newer
- bash or zsh
- `libseccomp`

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) and [the vision](docs/vision.md).

## License

[MIT](LICENSE)
