//! The shell hooks: how the watcher learns where one command ends and the next begins.
//!
//! The kernel only shows the watcher processes and calls, not command lines. So the shell
//! tells it: before running a command line it sends `start <the line>`, and when the prompt
//! comes back it sends `end`. Messages go down a pipe the shell inherits as fd [`MARK_FD`].
//!
//! The hook is installed without touching your own config files:
//! - **bash** starts with `--rcfile <session>/bashrc`, which loads `~/.bashrc` first.
//! - **zsh** starts with `ZDOTDIR=<session>/zdotdir`, whose files load your own ones first.

use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

/// The fd number the shell writes hook messages to. High enough that nothing else uses it.
pub const MARK_FD: i32 = 197;

pub enum Shell {
    Bash,
    Zsh,
    Other,
}

pub fn detect(shell: &Path) -> Shell {
    match shell.file_name().and_then(|n| n.to_str()) {
        Some(name) if name.starts_with("bash") => Shell::Bash,
        Some(name) if name.starts_with("zsh") => Shell::Zsh,
        _ => Shell::Other,
    }
}

/// Write the hook files into the session folder and set up `cmd` to use them.
/// Returns false for shells without a hook.
pub fn install(shell: &Shell, session_dir: &Path, cmd: &mut Command) -> io::Result<bool> {
    match shell {
        Shell::Bash => {
            let rc = session_dir.join("bashrc");
            fs::write(&rc, BASHRC)?;
            cmd.arg("--rcfile").arg(&rc).arg("-i");
            Ok(true)
        }
        Shell::Zsh => {
            let dir = session_dir.join("zdotdir");
            fs::create_dir_all(&dir)?;
            fs::write(dir.join(".zshenv"), ZSHENV)?;
            fs::write(dir.join(".zshrc"), ZSHRC)?;
            match std::env::var_os("ZDOTDIR") {
                Some(own) => cmd.env("WHOOPZ_USER_ZDOTDIR", own),
                None => cmd
                    .env(
                        "WHOOPZ_USER_ZDOTDIR",
                        std::env::var_os("HOME").unwrap_or_default(),
                    )
                    .env("WHOOPZ_ZDOTDIR_UNSET", "1"),
            };
            cmd.env("ZDOTDIR", &dir).arg("-i");
            Ok(true)
        }
        Shell::Other => Ok(false),
    }
}

const BASHRC: &str = r#"# Written by `whoops shell`: loads your own ~/.bashrc, then adds the Whoopz hook.
# The hook tells the watcher when a command line starts and when the prompt is back, so
# `whoops` can undo one command at a time.

if [[ -f ~/.bashrc ]]; then source ~/.bashrc; fi

__whoopz_send() { builtin printf '%s\0' "$1" >&"$WHOOPZ_MARK_FD"; }
__whoopz_start() { __whoopz_send "start $1"; }
__whoopz_end() { __whoopz_send end; }

# Bash saves its history file while exiting. That isn't part of the `exit` command.
if [[ -z $(trap -p EXIT) ]]; then trap '__whoopz_end' EXIT; fi

if [[ -n ${bash_preexec_imported:-}${__bp_imported:-} ]]; then
    # bash-preexec is loaded (atuin, starship and others use it): join it, don't replace it.
    preexec_functions+=(__whoopz_start)
    precmd_functions=(__whoopz_end "${precmd_functions[@]}")
else
    # Bash runs the DEBUG trap before every simple command. A new command line starts when
    # the prompt has come back (__whoopz_ready), or when bash moves on to the next line of a
    # paste: pasted lines run back to back with no prompt in between, but each one gets its
    # own history number ($HISTCMD). `a; b` typed on one line keeps one number.
    __whoopz_ready=
    __whoopz_in_prompt=
    __whoopz_line=
    __whoopz_debug() {
        [[ -n ${COMP_LINE:-} || -n $__whoopz_in_prompt ]] && return
        # The history number also moves while the prompt is drawn; that's not a command.
        [[ $BASH_COMMAND == __whoopz_prompt_* ]] && return
        [[ -z $__whoopz_ready && $HISTCMD == "$__whoopz_line" ]] && return
        __whoopz_ready=
        __whoopz_line=$HISTCMD
        local line
        line=$(HISTTIMEFORMAT= builtin history 1)
        [[ $line =~ ^\ *[0-9]+\*?\ +(.*)$ ]] && line=${BASH_REMATCH[1]}
        __whoopz_start "${line:-$BASH_COMMAND}"
    }
    __whoopz_prompt_start() { __whoopz_in_prompt=1; __whoopz_end; }
    __whoopz_prompt_done() { __whoopz_in_prompt=; __whoopz_ready=1; }
    if [[ -n $(trap -p DEBUG) ]]; then
        echo "whoops: replacing your DEBUG trap" >&2
    fi
    trap '__whoopz_debug' DEBUG
    if (( BASH_VERSINFO[0] > 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] >= 1) )); then
        PROMPT_COMMAND=(__whoopz_prompt_start "${PROMPT_COMMAND[@]}" __whoopz_prompt_done)
    else
        PROMPT_COMMAND="__whoopz_prompt_start; ${PROMPT_COMMAND:+${PROMPT_COMMAND%;}; }__whoopz_prompt_done"
    fi
fi
"#;

const ZSHENV: &str = r#"# Written by `whoops shell`: runs your own .zshenv, then keeps zsh pointed at this
# folder so the .zshrc next to this file runs next.
__whoopz_zdotdir=$ZDOTDIR
ZDOTDIR=$WHOOPZ_USER_ZDOTDIR
[[ -f $ZDOTDIR/.zshenv ]] && source $ZDOTDIR/.zshenv
WHOOPZ_USER_ZDOTDIR=$ZDOTDIR
ZDOTDIR=$__whoopz_zdotdir
unset __whoopz_zdotdir
"#;

const ZSHRC: &str = r#"# Written by `whoops shell`: points zsh back at your own config, loads your .zshrc,
# then adds the Whoopz hook.
ZDOTDIR=$WHOOPZ_USER_ZDOTDIR
[[ -n $WHOOPZ_ZDOTDIR_UNSET && $ZDOTDIR == $HOME ]] && unset ZDOTDIR
unset WHOOPZ_USER_ZDOTDIR WHOOPZ_ZDOTDIR_UNSET
[[ -f ${ZDOTDIR:-$HOME}/.zshrc ]] && source ${ZDOTDIR:-$HOME}/.zshrc

__whoopz_send() { print -rn -- "$1"$'\0' >&$WHOOPZ_MARK_FD }
__whoopz_start() { __whoopz_send "start $1" }
__whoopz_end() { __whoopz_send end }
autoload -Uz add-zsh-hook
add-zsh-hook preexec __whoopz_start
# zsh saves its history file while exiting. That isn't part of the `exit` command.
add-zsh-hook zshexit __whoopz_end
# First in line, so files your other prompt hooks write don't count as the last command's.
precmd_functions=(__whoopz_end $precmd_functions)
"#;
