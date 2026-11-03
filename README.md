# Whoopz

**Always-on undo for the Linux terminal.**

Ran `rm`, `mv`, `>`, `sed -i` or `cp` by mistake? Type `whoops` and your files come back.

> **Status: early.** Undo works for bash and zsh, but it is new. Try it in a throwaway folder
> before you trust it with files you care about.

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

## Install

You need Linux 5.5 or newer, Rust, and `libseccomp` (`libseccomp` on Arch, `libseccomp-dev` on
Debian/Ubuntu).

```sh
git clone https://github.com/suraniharsh/whoopz
cd whoopz
cargo install --path . --locked              # puts `whoops` in ~/.cargo/bin
# or: cargo install --path . --locked --root ~/.local   (puts it in ~/.local/bin)
```

## Use

```sh
whoops shell          # start your shell with Whoopz watching it
```

Inside that shell, work as usual. When something goes wrong:

```sh
whoops                # undo the last command that changed files
whoops list           # see what each command changed
whoops 3              # undo command 3 from the list
exit                  # leave the watched shell
```

```text
$ rm -rf proj
$ whoops list
   2  rm -rf proj                               4 deleted
$ whoops
Undid 2: rm -rf proj
  restored proj
  restored proj/src
  restored proj/src/main.rs
  restored proj/README
```

To start every terminal this way, add this to the end of `~/.bashrc` (or `~/.zshrc`):

```sh
[ -z "$WHOOPZ_ACTIVE" ] && command -v whoops >/dev/null && whoops shell && exit
```

Set `WHOOPZ_DEBUG=1` before `whoops shell` to see every call Whoopz pauses.

## What it can't undo

- Anything that leaves your machine: `git push`, email, API calls
- Databases changing their own files
- `sudo` commands: `sudo` gives an error inside a Whoopz shell
- Files over 512 MB on a different drive from `~/.local/share` (a hard link can't cross
  drives, and a copy that big is skipped, with a warning)
- Changes a background job makes after the prompt is back
- Commands from an earlier `whoops shell` session: undo works within the session it happened in
- A shell started inside the watched shell (`bash`, `zsh`): everything in it counts as one command

Whoopz is a **safety net, not a sandbox**. It is not a security boundary. See
[SECURITY.md](SECURITY.md).

## Where things are kept

Each session has a folder in `~/.local/share/whoopz/sessions/` with its journal and the saved
files. A deleted file is kept with a hard link, so keeping it needs no copy; its disk space
is only freed when the session is cleaned up. Sessions are removed a week after they end.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) and [the vision](docs/vision.md).

## License

[MIT](LICENSE)
