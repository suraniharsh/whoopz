//! The rule list attached to the shell: which system calls pause and ask the watcher.
//!
//! It's a tiny program in **classic BPF**, a mini machine language the kernel runs before
//! every system call. The program can only look at this (`struct seccomp_data`):
//!
//! ```text
//! offset  0  nr      which call (a number; differs per architecture)
//! offset  4  arch    which architecture's numbers those are (64-bit x86, 32-bit x86, ARM…)
//! offset  8  ip      where in the program the call was made
//! offset 16  args    the six arguments, 8 bytes each. A path is only an address here.
//! ```
//!
//! and answers ALLOW (go ahead) or USER_NOTIF (pause and ask the watcher). Everything not
//! listed is allowed straight through, inside the kernel, so reads and writes never leave it
//! and the terminal stays fast.
//!
//! The program, roughly:
//!
//! ```text
//!     load arch
//!     arch == this machine's? no → goto other_arch
//!     load nr
//!     nr == unlinkat?  yes → notify          (one line per call we pause)
//!     nr == openat?    yes → flags_2         (open: only if it empties or creates a file)
//!     …
//!     allow
//! other_arch:                                 (64-bit x86 only: 32-bit programs)
//!     arch == i386? no → allow
//!     load nr, compare against the 32-bit numbers, same as above
//! flags_2:
//!     load args[2]; (it & (O_TRUNC|O_CREAT)) != 0? → notify, else allow
//! notify: return USER_NOTIF
//! allow:  return ALLOW
//! ```
//!
//! Writing it directly (instead of using libseccomp, a C library) keeps Whoopz free of
//! anything that has to be installed on the machine, so a single static binary runs anywhere.

use std::collections::HashMap;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};

use libc::{BPF_ABS, BPF_JEQ, BPF_JMP, BPF_JSET, BPF_K, BPF_LD, BPF_RET, BPF_W, sock_filter};

/// Architecture IDs the kernel puts in `seccomp_data.arch` (from `<linux/audit.h>`).
#[cfg(target_arch = "x86_64")]
pub const AUDIT_ARCH_NATIVE: u32 = 0xC000_003E; // AUDIT_ARCH_X86_64
#[cfg(target_arch = "aarch64")]
pub const AUDIT_ARCH_NATIVE: u32 = 0xC000_00B7; // AUDIT_ARCH_AARCH64
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH_I386: u32 = 0x4000_0003;

/// x32 programs (64-bit x86 with 32-bit pointers) use the 64-bit numbers with this bit set.
#[cfg(target_arch = "x86_64")]
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

/// What makes an open worth pausing: it empties the file, or might create one.
const OPEN_FLAGS: u32 = (libc::O_TRUNC | libc::O_CREAT) as u32;

/// When to pause a call.
#[derive(Clone, Copy)]
pub enum When {
    Always,
    /// Only if argument `n` (the open flags) has O_TRUNC or O_CREAT set.
    OpenFlagsIn(u8),
}

pub struct Call {
    pub nr: u32,
    pub name: &'static str,
    pub when: When,
}

const fn call(nr: libc::c_long, name: &'static str, when: When) -> Call {
    Call {
        nr: nr as u32,
        name,
        when,
    }
}

/// The calls to pause, with this architecture's numbers.
pub fn native_calls() -> Vec<Call> {
    use When::*;
    #[allow(unused_mut)] // only extended on x86_64
    let mut calls = vec![
        call(libc::SYS_unlinkat, "unlinkat", Always),
        call(libc::SYS_renameat, "renameat", Always),
        call(libc::SYS_renameat2, "renameat2", Always),
        call(libc::SYS_truncate, "truncate", Always),
        call(libc::SYS_ftruncate, "ftruncate", Always),
        call(libc::SYS_mkdirat, "mkdirat", Always),
        call(libc::SYS_linkat, "linkat", Always),
        call(libc::SYS_symlinkat, "symlinkat", Always),
        // openat2's flags sit behind a pointer the filter can't follow, so pause all of them.
        call(libc::SYS_openat2, "openat2", Always),
        call(libc::SYS_openat, "openat", OpenFlagsIn(2)),
    ];
    // Older architectures also have the original, non-"at" versions.
    #[cfg(target_arch = "x86_64")]
    calls.extend([
        call(libc::SYS_unlink, "unlink", Always),
        call(libc::SYS_rmdir, "rmdir", Always),
        call(libc::SYS_rename, "rename", Always),
        call(libc::SYS_creat, "creat", Always),
        call(libc::SYS_mkdir, "mkdir", Always),
        call(libc::SYS_link, "link", Always),
        call(libc::SYS_symlink, "symlink", Always),
        call(libc::SYS_open, "open", OpenFlagsIn(1)),
    ]);
    calls
}

/// The same calls for 32-bit x86 programs running on a 64-bit x86 kernel. They use the old
/// 32-bit numbers, which never change (Linux promises not to break existing programs).
#[cfg(target_arch = "x86_64")]
pub fn i386_calls() -> Vec<Call> {
    use When::*;
    vec![
        call(5, "open", OpenFlagsIn(1)),
        call(8, "creat", Always),
        call(9, "link", Always),
        call(10, "unlink", Always),
        call(38, "rename", Always),
        call(39, "mkdir", Always),
        call(40, "rmdir", Always),
        call(83, "symlink", Always),
        call(92, "truncate", Always),
        call(93, "ftruncate", Always),
        call(193, "truncate64", Always),
        call(194, "ftruncate64", Always),
        call(295, "openat", OpenFlagsIn(2)),
        call(296, "mkdirat", Always),
        call(301, "unlinkat", Always),
        call(302, "renameat", Always),
        call(303, "linkat", Always),
        call(304, "symlinkat", Always),
        call(353, "renameat2", Always),
        call(437, "openat2", Always),
    ]
}

/// The name of a paused call, from the architecture and number the kernel reports.
pub fn name_of(arch: u32, nr: i32) -> Option<&'static str> {
    #[cfg(target_arch = "x86_64")]
    if arch == AUDIT_ARCH_I386 {
        return find(&i386_calls(), nr as u32);
    }
    if arch != AUDIT_ARCH_NATIVE {
        return None;
    }
    let nr = nr as u32;
    #[cfg(target_arch = "x86_64")]
    let nr = nr & !X32_SYSCALL_BIT;
    find(&native_calls(), nr)
}

fn find(calls: &[Call], nr: u32) -> Option<&'static str> {
    calls.iter().find(|c| c.nr == nr).map(|c| c.name)
}

// --- a tiny assembler for classic BPF ---

/// Where a jump goes: the next instruction, or a named place.
enum To {
    Next,
    Label(String),
}

enum Line {
    Stmt(u16, u32),
    Jump(u16, u32, To, To),
    Label(String),
}

/// Byte offsets into `seccomp_data`.
const NR: u32 = 0;
const ARCH: u32 = 4;

/// The low 32 bits of argument `n`. Flags are a C `int`, so the low half is all we need.
fn arg_low(n: u8) -> u32 {
    let start = 16 + 8 * n as u32;
    if cfg!(target_endian = "little") {
        start
    } else {
        start + 4
    }
}

fn load(offset: u32) -> Line {
    Line::Stmt((BPF_LD | BPF_W | BPF_ABS) as u16, offset)
}
fn jump_if_equal(k: u32, yes: To, no: To) -> Line {
    Line::Jump((BPF_JMP | BPF_JEQ | BPF_K) as u16, k, yes, no)
}
fn jump_if_any_bit(k: u32, yes: To, no: To) -> Line {
    Line::Jump((BPF_JMP | BPF_JSET | BPF_K) as u16, k, yes, no)
}
fn ret(value: u32) -> Line {
    Line::Stmt((BPF_RET | BPF_K) as u16, value)
}
fn label(name: &str) -> Line {
    Line::Label(name.into())
}
fn to(name: &str) -> To {
    To::Label(name.into())
}

/// "Compare the loaded call number against each call; jump to its check if it matches."
fn dispatch(calls: &[Call]) -> Vec<Line> {
    calls
        .iter()
        .map(|c| {
            let target = match c.when {
                When::Always => to("notify"),
                When::OpenFlagsIn(n) => to(&format!("flags_{n}")),
            };
            jump_if_equal(c.nr, target, To::Next)
        })
        .collect()
}

fn source() -> Vec<Line> {
    let allow = libc::SECCOMP_RET_ALLOW;
    let mut lines = vec![
        load(ARCH),
        jump_if_equal(AUDIT_ARCH_NATIVE, To::Next, to("other_arch")),
        load(NR),
    ];
    // x32 calls: clear the x32 bit so they match the 64-bit numbers.
    #[cfg(target_arch = "x86_64")]
    lines.push(Line::Stmt(
        (libc::BPF_ALU | libc::BPF_AND | BPF_K) as u16,
        !X32_SYSCALL_BIT,
    ));
    lines.extend(dispatch(&native_calls()));
    lines.push(ret(allow));

    lines.push(label("other_arch"));
    #[cfg(target_arch = "x86_64")]
    {
        lines.push(jump_if_equal(AUDIT_ARCH_I386, To::Next, to("allow")));
        lines.push(load(NR));
        lines.extend(dispatch(&i386_calls()));
    }
    // Any other architecture (rare: 32-bit ARM programs on 64-bit ARM) isn't watched.
    lines.push(ret(allow));

    for n in [1, 2] {
        lines.push(label(&format!("flags_{n}")));
        lines.push(load(arg_low(n)));
        lines.push(jump_if_any_bit(OPEN_FLAGS, to("notify"), to("allow")));
    }
    lines.push(label("notify"));
    lines.push(ret(libc::SECCOMP_RET_USER_NOTIF));
    lines.push(label("allow"));
    lines.push(ret(allow));
    lines
}

/// Turn labels into the relative jump distances BPF uses (forward only, at most 255).
fn assemble(lines: Vec<Line>) -> Vec<sock_filter> {
    let mut at = HashMap::new();
    let mut index = 0usize;
    for line in &lines {
        match line {
            Line::Label(name) => {
                at.insert(name.clone(), index);
            }
            _ => index += 1,
        }
    }
    let distance = |from: usize, target: &To| -> u8 {
        match target {
            To::Next => 0,
            To::Label(name) => u8::try_from(at[name] - from - 1)
                .expect("BPF jumps go forward at most 255 instructions"),
        }
    };

    let mut program = Vec::new();
    for line in &lines {
        let here = program.len();
        match line {
            Line::Label(_) => {}
            Line::Stmt(code, k) => program.push(sock_filter {
                code: *code,
                jt: 0,
                jf: 0,
                k: *k,
            }),
            Line::Jump(code, k, yes, no) => program.push(sock_filter {
                code: *code,
                jt: distance(here, yes),
                jf: distance(here, no),
                k: *k,
            }),
        }
    }
    program
}

/// The filter program. Built in the parent, so the child (after fork) doesn't allocate.
pub fn program() -> Vec<sock_filter> {
    assemble(source())
}

/// Attach the filter to the calling process and return the listener for its notices.
///
/// Runs in the child between fork and exec, so it doesn't allocate.
pub fn attach(program: &[sock_filter]) -> io::Result<OwnedFd> {
    let prog = libc::sock_fprog {
        len: program.len() as u16,
        filter: program.as_ptr() as *mut sock_filter,
    };
    // SAFETY: plain system calls; `prog` points at a valid program that outlives the call.
    unsafe {
        // Required before a normal user may attach a filter. It also means setuid programs
        // like `sudo` can't gain privileges inside the shell.
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            libc::SECCOMP_FILTER_FLAG_NEW_LISTENER,
            &prog as *const libc::sock_fprog,
        );
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedFd::from_raw_fd(fd as i32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run the program the way the kernel would, for one call.
    fn run(program: &[sock_filter], arch: u32, nr: u32, args: [u64; 6]) -> u32 {
        let mut data = [0u8; 64];
        data[0..4].copy_from_slice(&nr.to_ne_bytes());
        data[4..8].copy_from_slice(&arch.to_ne_bytes());
        for (i, a) in args.iter().enumerate() {
            data[16 + 8 * i..24 + 8 * i].copy_from_slice(&a.to_ne_bytes());
        }
        let word =
            |off: u32| u32::from_ne_bytes(data[off as usize..off as usize + 4].try_into().unwrap());
        let (mut a, mut pc) = (0u32, 0usize);
        loop {
            let i = program[pc];
            let code = i.code as u32;
            pc += 1;
            if code == BPF_LD | BPF_W | BPF_ABS {
                a = word(i.k);
            } else if code == libc::BPF_ALU | libc::BPF_AND | BPF_K {
                a &= i.k;
            } else if code == BPF_JMP | BPF_JEQ | BPF_K {
                pc += if a == i.k { i.jt } else { i.jf } as usize;
            } else if code == BPF_JMP | BPF_JSET | BPF_K {
                pc += if a & i.k != 0 { i.jt } else { i.jf } as usize;
            } else if code == BPF_RET | BPF_K {
                return i.k;
            } else {
                panic!("unexpected instruction {code:#x}");
            }
        }
    }

    const NOTIFY: u32 = libc::SECCOMP_RET_USER_NOTIF;
    const ALLOW: u32 = libc::SECCOMP_RET_ALLOW;

    fn nr(n: libc::c_long) -> u32 {
        n as u32
    }

    #[test]
    fn pauses_file_changes_and_allows_everything_else() {
        let p = program();
        let native = AUDIT_ARCH_NATIVE;
        assert_eq!(run(&p, native, nr(libc::SYS_unlinkat), [0; 6]), NOTIFY);
        assert_eq!(run(&p, native, nr(libc::SYS_renameat2), [0; 6]), NOTIFY);
        assert_eq!(run(&p, native, nr(libc::SYS_read), [0; 6]), ALLOW);
        assert_eq!(run(&p, native, nr(libc::SYS_write), [0; 6]), ALLOW);
    }

    #[test]
    fn pauses_opens_only_when_they_empty_or_create() {
        let p = program();
        let openat = nr(libc::SYS_openat);
        let with = |flags: i32| [0, 0, flags as u64, 0, 0, 0];
        let native = AUDIT_ARCH_NATIVE;
        assert_eq!(
            run(&p, native, openat, with(libc::O_WRONLY | libc::O_TRUNC)),
            NOTIFY
        );
        assert_eq!(run(&p, native, openat, with(libc::O_CREAT)), NOTIFY);
        assert_eq!(run(&p, native, openat, with(libc::O_RDONLY)), ALLOW);
        assert_eq!(run(&p, native, openat, with(libc::O_APPEND)), ALLOW);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn watches_32_bit_and_x32_programs_too() {
        let p = program();
        assert_eq!(run(&p, AUDIT_ARCH_I386, 10, [0; 6]), NOTIFY); // unlink
        assert_eq!(run(&p, AUDIT_ARCH_I386, 3, [0; 6]), ALLOW); // read
        let x32_unlinkat = nr(libc::SYS_unlinkat) | X32_SYSCALL_BIT;
        assert_eq!(run(&p, AUDIT_ARCH_NATIVE, x32_unlinkat, [0; 6]), NOTIFY);
        assert_eq!(name_of(AUDIT_ARCH_I386, 10), Some("unlink"));
        assert_eq!(
            name_of(AUDIT_ARCH_NATIVE, x32_unlinkat as i32),
            Some("unlinkat")
        );
    }

    #[test]
    fn other_architectures_are_allowed() {
        assert_eq!(run(&program(), 0x4000_0028, 10, [0; 6]), ALLOW); // 32-bit ARM
    }
}
