//! The rule list attached to the shell: which system calls pause and ask the watcher.
//!
//! Everything not listed here is allowed straight through, inside the kernel, so reads and
//! writes never leave it and the terminal stays fast.

use anyhow::{Context, Result};
use libseccomp::{ScmpAction, ScmpFilterContext, ScmpSyscall, scmp_cmp};
use nix::libc;

/// Calls that pause every time they are made.
///
/// Some of these (`unlink`, `rename`, `open`, …) only exist on older architectures such as
/// x86_64; newer ones like aarch64 only have the `…at` versions. libseccomp skips a name on an
/// architecture that doesn't have it.
const ALWAYS: &[&str] = &[
    // delete a file or an empty folder
    "unlink",
    "unlinkat",
    "rmdir",
    // move, or replace one file with another (`mv`, and the last step of `sed -i`)
    "rename",
    "renameat",
    "renameat2",
    // shrink or empty a file
    "truncate",
    "ftruncate",
    // old way to create-or-empty a file
    "creat",
    // newer open: its flags sit behind a pointer the filter can't read, so pause all of them
    "openat2",
];

/// Build the filter. Attaching it is done by the launcher, inside the child process.
pub fn build() -> Result<ScmpFilterContext> {
    // Default answer for every call not mentioned below: allow.
    let mut ctx = ScmpFilterContext::new(ScmpAction::Allow)?;

    // A 64-bit program can still make 32-bit system calls, which have different numbers.
    // Without these, a call could slip past the filter. libseccomp translates every rule
    // below into each architecture's own numbers.
    #[cfg(target_arch = "x86_64")]
    {
        use libseccomp::ScmpArch;
        ctx.add_arch(ScmpArch::X86)?;
        ctx.add_arch(ScmpArch::X32)?;
    }

    for name in ALWAYS {
        let call = ScmpSyscall::from_name(name).with_context(|| format!("unknown call {name}"))?;
        ctx.add_rule(ScmpAction::Notify, call)?;
    }

    // `open` and `openat` are made constantly, mostly to read. Pause them only when they would
    // empty the file (O_TRUNC), which is what `> file` and `cp` onto a file do. The filter can
    // look at flags because they are a plain number; it could not look at the path.
    let truncating = libc::O_TRUNC as u64;
    ctx.add_rule_conditional(
        ScmpAction::Notify,
        ScmpSyscall::from_name("open")?,
        // open(path, flags, mode): flags are argument 1
        &[scmp_cmp!($arg1 & (truncating) == truncating)],
    )?;
    ctx.add_rule_conditional(
        ScmpAction::Notify,
        ScmpSyscall::from_name("openat")?,
        // openat(dir, path, flags, mode): flags are argument 2
        &[scmp_cmp!($arg2 & (truncating) == truncating)],
    )?;

    Ok(ctx)
}
