# Vision

Whoopz is an undo for the Linux terminal that is **always on**. You never have to remember to
use it before a command. When a command goes wrong, `whoops` puts your files back.

What's planned is in [the roadmap](roadmap.md).

## Whoopz will never…

If you're here to find out whether a feature will happen, start with this list.

- **…need you to plan ahead.** No "run it through Whoopz first" mode, no preview of what a
  command *would* do. Mistakes are exactly the commands you didn't expect to go wrong. For
  trying a command in a sandbox first, use [`try`](https://github.com/binpash/try).
- **…be a security tool.** Whoopz is a safety net for honest mistakes. It doesn't stop a
  program that wants to cause harm, and it makes no security promises.
- **…undo what left your machine.** `git push`, an email, an API call: once it's out, it's out.
- **…run on macOS or Windows.** Whoopz is built on a Linux kernel feature (seccomp user
  notification) that other systems don't have, and there's no way to give the same
  always-on promise without it.
- **…guess.** When it isn't sure a file can be put back safely, it says so and leaves it. It
  never overwrites something new to restore something old.

## Principles

1. **After the mistake, not before.** Anything that only works if you planned ahead is out of
   scope.
2. **Every program, not a list of commands.** Whoopz watches system calls, so Python scripts,
   Go binaries, `git` and anything else are covered the same way `rm` is.
3. **You never notice it.** Reads are never paused. If the terminal feels slower, Whoopz has
   failed. Speed regressions are bugs.
4. **Never lose data trying to save it.** Undo checks the real state of the files before it
   changes anything, and skips what's in the way.
5. **Your setup stays yours.** Whoopz never edits your shell's config files, and your shell
   works the same inside it. If Whoopz can't start, `--fallback` gives you your shell anyway.
6. **One file, nothing to install.** A static binary that runs on any Linux 5.5 or newer.

## Also out of scope

- Undoing changes databases make to their own files (they keep their own logs for that).
- Recording what programs *read*. Only changes are saved.

Want to build something that isn't on the roadmap? Open an issue first, so no one spends
time on a pull request that doesn't fit.
