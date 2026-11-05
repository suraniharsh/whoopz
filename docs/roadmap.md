# Roadmap

What's planned for Whoopz, by area. There are no dates. What Whoopz will *never* do is in
[the vision](vision.md).

✅ done · 🚧 in progress · ⬜ planned

Want something that isn't here? Open an issue first.

## Shells

Each shell needs a small hook so Whoopz knows where commands start and end.
[How to add yours](hooks.md).

- ✅ bash
- ✅ zsh
- ✅ fish
- ✅ nushell
- ✅ elvish
- ⬜ xonsh
- ⬜ tcsh
- ⬜ Any other shell, without a hook: a safe fallback that groups what it can by process group

## Undo

- ✅ Undo the last command, or any command by number (`whoops`, `whoops list`, `whoops <n>`)
- ✅ Deletes, `rm -rf`, moves, overwrites (`>`, `cp`, `sed -i`), truncates, new files and folders
- ✅ Background jobs get their own entry and never get mixed into your commands
- ✅ Undo `git reset --hard`, `git checkout .`, `git restore .` and `git clean -fd`, with a short report
- ⬜ Undo from any terminal, even after closing the one where the mistake happened (a small
  `systemctl --user` service)
- ⬜ `whoops show <n>`: see what an undo would bring back before doing it
- ⬜ `whoops restore <file>`: bring back one file instead of a whole command
- ⬜ `whoops log <file>`: a file's history across commands
- ⬜ Redo: undo an undo
- ⬜ `sudo` commands

## Storage

- ✅ Deleted files kept with hard links (no copy); files on another drive copied, up to 512 MB
- ✅ Sessions cleaned up a week after they end
- ✅ A total size limit for saved files (5 GB, or `WHOOPZ_STASH_LIMIT`), oldest dropped first,
  anything from the last hour always kept
- ✅ `whoops status` to see how much is kept
- ✅ Saved files removed after a week even if the terminal stays open
- ⬜ Free space sooner when the drive is nearly full

## Platforms and packaging

- ✅ One static binary for any Linux 5.5 or newer, x86_64 and ARM64
- ⬜ ARM64 tested on real hardware
- ⬜ Packages: AUR, Nix, Homebrew on Linux, `.deb` and `.rpm`
- ⬜ Shell completions and a man page

## Speed

- ⬜ Published overhead numbers for everyday commands (`git status`, `npm install`,
  `cargo build`)
