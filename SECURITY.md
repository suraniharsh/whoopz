# Security

## What Whoopz is not

Whoopz is a **safety net, not a sandbox**. It helps you recover from mistakes. It does not stop
a program that wants to cause harm, and it is not a security boundary. These are known limits,
not vulnerabilities:

- A program can get past it: through io_uring, or by changing a path in its memory while the
  watcher reads it.
- Any program in a watched shell can write to the hook's pipe, so it could say "the command
  ended" early and keep its changes from being saved.
- Writing into the middle of a file without emptying it first (in-place edits) isn't saved.
- Programs in a sandbox or container (bubblewrap, flatpak, podman, `unshare -m`) see a
  different filesystem; their changes aren't saved.
- 32-bit ARM programs on 64-bit ARM aren't watched.
- `sudo`, `su`, `doas` and `pkexec` run outside the watched shell, so what they change isn't
  saved. Any program in a watched shell can ask for them, and whatever they start isn't watched.

## Secrets in saved files

Whoopz treats every file the same, so it also saves files that hold secrets. If a command deletes
or replaces a login token, a key or a password file, the old copy stays in the stash until it's
removed, a week later at most. Nobody else can open that folder, but a backup of your home folder
may include it, and anything running as you can read it.

Undo puts back what was there, so undoing a command that changed a login file brings the old
login back. To remove saved files sooner, delete a finished session's folder from
`~/.local/share/whoopz/sessions/`.

## What Whoopz promises

Whoopz never changes a file on a program's behalf. Every change goes ahead in the kernel, as
the program, with its own permissions, security rules and view of the filesystem. Whoopz only
saves what is about to be lost, into a folder only you can open.

Whoopz starts only those four programs on a watched shell's behalf, from the paths it found when
the session began. It refuses requests from other users, from programs in a sandbox, and from
other terminal sessions. They run with your own rights until they ask for a password, as they
would without Whoopz.

## What to report

Report privately anything that lets Whoopz:

- lose or damage files it was meant to protect, or touch files outside what a command changed
- expose saved files (the stash) to other users on the machine
- run with more privileges than the user who started it

## How to report

Use GitHub's private vulnerability reporting: the **Security** tab → **Report a vulnerability**.
Please don't open a public issue for these.
