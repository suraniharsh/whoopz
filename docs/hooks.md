# Shell hooks

Whoopz sees every file change any program makes, in any shell, because it watches at the
kernel level. What it can't see on its own is **where one command ends and the next begins**,
and what you typed. A shell hook tells it.

This page is for anyone adding a hook for a new shell. Hooks for **bash**, **zsh**, **fish**,
**nushell** and **elvish** exist ([`src/hooks.rs`](../src/hooks.rs)); a hook is usually 10–30 lines of shell code.

## Why a hook, and what happens without one

Without a hook, Whoopz falls back to what the kernel shows: an interactive shell usually gives
each command its own *process group*, so changes from one group belong together. That works for
programs (`rm`, `mv`, a pipeline), but it misses everything the shell does **itself**. We tested
9 shells; in most of them the classic mistake `echo x > file` is written by the shell process,
not by a separate program:

| Shell | `rm`, `mv` | `cat x > file` | `echo x > file` | Pipeline |
|---|---|---|---|---|
| bash, zsh, tcsh | ✅ | ✅ | ❌ | ✅ |
| fish | ✅ | ❌ the shell opens the file | ❌ | ❌ if it starts with a function |
| dash, xonsh | ✅ | ❌ | ❌ | ✅ |
| nushell | ✅ outside programs, ❌ built-ins (`rm`, `save`) | ❌ | ❌ | ✅ |
| elvish | ❌ no process groups at all | ❌ | ❌ | ❌ |

With a hook, every row is ✅, and `whoops list` shows the command line as you typed it.

## The protocol

When `whoops shell` starts a shell, the shell inherits:

| Variable | Meaning |
|---|---|
| `WHOOPZ_MARK_FD` | The file descriptor to write hook messages to (currently `197`). |
| `WHOOPZ_ACTIVE` | Set to `1`: this shell is watched. |
| `WHOOPZ_SOCK` | Where `whoops` (the command you type) reaches the watcher. Not used by hooks. |

A hook writes **messages** to `$WHOOPZ_MARK_FD`. Each message ends with a zero byte (`\0`):

| Message | When to send it | What it means |
|---|---|---|
| `start <command line>` | Right before a command line runs | Everything from here on belongs to this command, until the next `start` or `end`. |
| `end` | When the prompt comes back, and when the shell exits | Back at the prompt. Changes from now on aren't part of any command. |
| `ignore <path>` | Once, at startup | Files whose path starts with `<path>` are the shell's own (its history file). When **the shell itself** changes them, that isn't part of your command. |

In bash, for example:

```bash
printf '%s\0' "start rm -rf build" >&"$WHOOPZ_MARK_FD"
printf '%s\0' end >&"$WHOOPZ_MARK_FD"
printf '%s\0' "ignore $HISTFILE" >&"$WHOOPZ_MARK_FD"
```

Anything else written to the descriptor is ignored.

## Rules

Each of these was learned from a real bug, found by a test.

1. **Write the message before the command starts, from the shell process itself.** Whoopz
   reads messages before handling any change, so a `start` written before the command's first
   process exists is always applied in time. Don't send it from a background job.

2. **Write by duplicating the descriptor, never by opening a file.** `>&$WHOOPZ_MARK_FD` copies
   an existing descriptor. Something like `> /dev/fd/197` *opens* a file, which Whoopz watches.
   Check your shell can redirect to a two- or three-digit descriptor number. If it can't
   redirect to a number at all (nushell), append to `/dev/fd/$WHOOPZ_MARK_FD` instead: that
   path is the same pipe. Whoopz looks at `/dev/fd/…` from the shell's side (it's the
   shell's descriptor, not Whoopz's) and records nothing there.

3. **Send `end` before anything else runs at the prompt.** Prompt tools (starship, git status in
   the prompt) write cache files. If `end` comes after them, their files count as part of the
   last command, and undoing it would delete them. In bash, `end` is the first entry of
   `PROMPT_COMMAND`; in zsh and fish, the hook goes first in the list of prompt hooks.

4. **One `start` per command line, including each line of a paste.** When you paste several
   lines, bash runs them back to back without showing a prompt in between, so "the prompt came
   back" never happens. The bash hook also starts a new command whenever the history number
   (`$HISTCMD`) changes. `start` without a preceding `end` is fine: it ends the previous command.

5. **Ignore the shell's own housekeeping.** Shells write their history file, sometimes from a
   background thread at any moment (fish rewrites its history mid-command). Send `ignore` for
   those paths at startup. It applies to changes made by the shell itself: its process, its
   threads, and helpers running the same program (elvish's history daemon). If *you* run
   `rm ~/.bash_history`, that's still saved. If the shell writes those files before its hook
   can run, add them to `housekeeping()` in `src/hooks.rs`, so Whoopz knows them from the start.

6. **Send `end` when the shell exits.** Shells save their history while exiting. Without an
   `end` first, that becomes part of the `exit` command.

7. **Don't touch the user's config files.** Load the hook the way the shell allows extra startup
   code: an rc-file option, an alternative config folder, or a "run this after config" option.
   The user's own config must run first, and unchanged.

## How the existing hooks load

| Shell | How the hook is loaded | Start / end | Ignored |
|---|---|---|---|
| bash | `bash --rcfile <session>/bashrc -i`; that file sources `~/.bashrc` first | `DEBUG` trap (first command after a prompt, or a new `$HISTCMD`) / first entry of `PROMPT_COMMAND`, `EXIT` trap. Joins `bash-preexec` if it's loaded. | `$HISTFILE` |
| zsh | `ZDOTDIR=<session>/zdotdir`; its `.zshenv` and `.zshrc` load the user's own first, then restore `ZDOTDIR` | `preexec` / first in `precmd_functions`, `zshexit` | `$HISTFILE` |
| fish | `fish -i -C 'source <session>/hook.fish'`; `-C` runs after the user's config | `fish_preexec` / `fish_postexec`, `fish_exit` | `$XDG_DATA_HOME/fish/`, `fish_variables` |
| elvish | `elvish -rc <session>/rc.elv`; elvish takes only one rc file, so that one `eval`s the user's `rc.elv` and copies what it defined into the session with `edit:add-vars` | first in `edit:after-readline` / first in `edit:before-readline` | `$XDG_STATE_HOME/elvish`, `$XDG_RUNTIME_DIR/elvish` (registered by Whoopz before elvish starts: its history daemon creates them at once) |
| nushell | `nu -e 'source <session>/hook.nu'`; `-e` runs after the user's config, then stays interactive | first in `hooks.pre_execution` / first in `hooks.pre_prompt` (no exit hook; history is saved after every command anyway) | `$nu.history-path` |

## Background jobs

You don't need to handle background jobs in a hook. Whoopz sorts every change into an entry
by itself:

- From the shell itself, or from the terminal's **foreground** process group (the command you're
  waiting on), and born after the prompt came back → **the current command**.
- Anything else (`make &`, a file watcher, fish building completions on its first start) → its
  own **`[background]`** entry. It's still saved, and can be undone with `whoops <n>`, but it
  never gets mixed into a command, and plain `whoops` leaves it alone.

## Adding a shell

1. **Find the hook points.** You need "before a command line runs", "when the prompt is back"
   and "when the shell exits". From our experiment, these exist:

   | Shell | Before / after a command |
   |---|---|
   | xonsh | `@events.on_precommand` / `@events.on_postcommand` |
   | tcsh | `alias postcmd` / `alias precmd` |
   | dash | none: falls back to process groups |

2. **Find a way to load extra startup code** without editing the user's config (rule 7).

3. **Find the shell's own files** to `ignore`: history, and anything else it rewrites by itself.
   Run a session with `WHOOPZ_DEBUG=/tmp/whoopz.log whoops shell --shell <yours>`, type a few
   commands, and look for paths the shell writes on its own.

4. **Add it to [`src/hooks.rs`](../src/hooks.rs):** a `Shell` variant, a line in `detect()`, and
   the files and arguments in `install()`.

5. **Test it** in [`tests/shell.rs`](../tests/shell.rs). The `interactive_*` tests type into a
   real shell through a fake terminal. Copy the bash ones and check at least that:
   - two commands are undone one at a time
   - `echo x > file` is undone (the shell writes that file itself)
   - the shell's history isn't in the journal
   - a background job gets its own entry

   Some shells ask the terminal questions at startup and wait for answers: fish 4 asks "what
   terminal are you?" and "where is the cursor?", nushell and xonsh ask "where is the cursor?".
   The test terminal answers both. If your shell hangs at startup in a test, it's probably
   asking something else.

6. **Run the tests in other versions of your shell** with the shell lab, which runs the whole
   suite in containers with the newest (Arch) and long-term (Ubuntu 24.04) versions of every
   shell. It needs Docker. Packaged versions differ: the lab's fish 3.7 caught a bug fish 4.9
   didn't show.

   ```sh
   scripts/shell-lab.sh                 # every image in tests/lab/
   scripts/shell-lab.sh arch fish       # one image, only tests with "fish" in the name
   ```

   If your shell isn't in an image yet, add it to the Dockerfiles in `tests/lab/`.

## Debugging

`WHOOPZ_DEBUG=1 whoops shell` prints every paused call: which process made it, its process
group, the terminal's foreground group, the path, and which command the hook says is running.
`WHOOPZ_DEBUG=/some/file` writes the same to a file instead.
