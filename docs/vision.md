# Vision

Whoopz is an undo for the Linux terminal that is **always on**. You never have to remember to
use it before a command. When a command goes wrong, `whoops` puts your files back.

## Principles

1. **After the mistake, not before.** Mistakes are exactly the commands you didn't expect to go
   wrong. Anything that only works if you planned ahead is out of scope.
2. **Every program, not a list of commands.** Whoopz watches system calls, so Python scripts,
   Go binaries and anything else are covered the same way `rm` is.
3. **You never notice it.** Reads are never paused. If the terminal feels slower, Whoopz has
   failed. Speed regressions are bugs.
4. **Never lose data trying to save it.** When unsure, refuse and explain instead of guessing.
   Undo checks the real state of the files before it changes anything.
5. **A safety net, not a sandbox.** Whoopz is not a security tool and makes no security promises.

## v1 scope

- Linux only, bash and zsh
- Watches delete, rename, overwrite, truncate and create
- `whoops`, `whoops list`, `whoops <n>`
- The stash is cleaned after 7 days or past a size cap

## Not in v1

- `sudo` commands
- Files on a different drive than the stash
- macOS, Windows, other shells
- A GUI

## Out of scope

- Undoing anything that leaves the machine (`git push`, network calls)
- Undoing changes databases make to their own files
- Sandboxing or preview modes (use [`try`](https://github.com/binpash/try) for that)

If you want to build something on the "not in v1" list, open an issue first.
