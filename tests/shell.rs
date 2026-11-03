//! End-to-end tests: run real commands in a watched shell and check what the watcher saw.

use std::process::{Command, Output};

/// Run `sh -c <script> sh <args…>` inside `whoops shell`.
fn whoops_sh(script: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_whoops"))
        .args(["shell", "--", "-c", script, "sh"])
        .args(args)
        .env("SHELL", "/bin/sh")
        .env_remove("WHOOPZ_ACTIVE")
        .output()
        .expect("could not run whoops")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn rm_is_seen_and_still_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("notes.txt");
    std::fs::write(&file, "hello").unwrap();

    let out = whoops_sh(r#"rm -- "$1""#, &[file.to_str().unwrap()]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("unlink"),
        "delete not seen: {}",
        stderr(&out)
    );
    assert!(!file.exists(), "the delete should still happen");
}

#[test]
fn mv_is_seen_and_still_moves() {
    let dir = tempfile::tempdir().unwrap();
    let from = dir.path().join("a.txt");
    let to = dir.path().join("b.txt");
    std::fs::write(&from, "hello").unwrap();

    let out = whoops_sh(
        r#"mv -- "$1" "$2""#,
        &[from.to_str().unwrap(), to.to_str().unwrap()],
    );

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("rename"),
        "move not seen: {}",
        stderr(&out)
    );
    assert!(
        !from.exists() && to.exists(),
        "the move should still happen"
    );
}

#[test]
fn overwrite_is_seen() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("notes.txt");
    std::fs::write(&file, "old").unwrap();

    let out = whoops_sh(r#"echo new > "$1""#, &[file.to_str().unwrap()]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("open"),
        "overwrite not seen: {}",
        stderr(&out)
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "new\n");
}

#[test]
fn reads_are_not_paused() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("notes.txt");
    std::fs::write(&file, "hello").unwrap();

    let out = whoops_sh(r#"cat -- "$1""#, &[file.to_str().unwrap()]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello");
    assert!(
        !stderr(&out).contains("wants"),
        "a read was paused: {}",
        stderr(&out)
    );
}

#[test]
fn exit_code_is_passed_through() {
    let out = whoops_sh("exit 3", &[]);
    assert_eq!(out.status.code(), Some(3));
}

#[test]
fn refuses_to_start_inside_itself() {
    let out = Command::new(env!("CARGO_BIN_EXE_whoops"))
        .args(["shell", "--", "-c", "true"])
        .env("WHOOPZ_ACTIVE", "1")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr(&out).contains("already inside"));
}
