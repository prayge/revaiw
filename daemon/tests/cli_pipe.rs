//! The CLI is piped into `head` and friends: a reader that goes away must end it quietly, not with a panic.
use std::process::{Command, Stdio};

#[test]
fn a_closed_pipe_ends_the_cli_quietly() {
    let d = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(d.path()).args(args).status().unwrap().success());
    git(&["init", "-q"]);
    git(&["remote", "add", "origin", "https://example.invalid/r.git"]);
    let mut child = Command::new(env!("CARGO_BIN_EXE_revaiw")).arg("report").current_dir(d.path())
        .env("XDG_STATE_HOME", d.path()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    drop(child.stdout.take()); // the reader is gone before the first line is written
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("panicked"), "{err}");
}
