//! `bb watch`, run as the real binary: exit codes, live output, saving and diagnosis.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn bb() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bb"))
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bb-watch-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// `sh -c` / `cmd /c` with a script that prints to both streams and exits with `code`.
fn script(out: &str, err: &str, code: i32) -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".into(), "/c".into(), format!("echo {out}& echo {err} 1>&2& exit {code}")]
    } else {
        vec!["sh".into(), "-c".into(), format!("echo '{out}'; echo '{err}' >&2; exit {code}")]
    }
}

fn watch(db: &std::path::Path, extra: &[&str], cmd: &[String]) -> Output {
    bb().arg("--db").arg(db).arg("watch").args(extra).arg("--").args(cmd).output().unwrap()
}

fn s(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn a_passing_command_passes_through_untouched() {
    let d = tmp("pass");
    let o = watch(&d.join("bb.db"), &[], &script("hello-out", "hello-err", 0));
    assert_eq!(o.status.code(), Some(0));
    assert!(s(&o.stdout).contains("hello-out"));
    assert!(s(&o.stderr).contains("hello-err"));
    assert!(!s(&o.stderr).contains("bb:"), "a success must be silent: {}", s(&o.stderr));
    assert!(!d.join("bb.db").exists(), "nothing to record, so no database is made");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_failing_command_keeps_its_exit_code_and_is_diagnosed_and_saved() {
    let d = tmp("fail");
    let db = d.join("bb.db");
    for code in [1, 3, 42] {
        let o = watch(&db, &[], &script("some output", "npm ERR! EACCES: permission denied", code));
        assert_eq!(o.status.code(), Some(code));
        let err = s(&o.stderr);
        assert!(err.contains(&format!("bb: that failed (exit {code}")), "{err}");
        assert!(err.contains("Permission was denied"), "{err}");
        assert!(s(&o.stdout).contains("some output"), "stdout stays the command's own");
        assert!(!s(&o.stdout).contains("bb:"));
    }
    let list = bb().arg("--db").arg(&db).arg("failures").output().unwrap();
    let list = s(&list.stdout);
    assert_eq!(list.lines().filter(|l| l.contains("some output")).count(), 3, "{list}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn no_record_diagnoses_but_saves_nothing() {
    let d = tmp("norec");
    let db = d.join("bb.db");
    let o = watch(&db, &["--no-record"], &script("x", "ENOSPC: no space left on device", 5));
    assert_eq!(o.status.code(), Some(5));
    assert!(s(&o.stderr).contains("The disk is full"));
    assert!(!s(&o.stderr).contains("saved as failure"));
    let n = bb().arg("--db").arg(&db).arg("failures").output().unwrap();
    assert!(s(&n.stdout).contains("No failed commands recorded yet"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_command_that_does_not_exist_exits_127_and_says_so() {
    let d = tmp("nocmd");
    let o = watch(&d.join("bb.db"), &[], &["bb-definitely-not-a-program".to_string(), "--flag".to_string()]);
    assert_eq!(o.status.code(), Some(127));
    let err = s(&o.stderr);
    assert!(err.contains("command not found: bb-definitely-not-a-program"), "{err}");
    assert!(err.contains("A program or command was not found"), "{err}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn flags_after_the_command_belong_to_the_command() {
    // `-m` and `--no-record` after the program name must reach the program, not bb.
    let d = tmp("flags");
    let o = bb().arg("--db").arg(d.join("bb.db")).args(["watch", "bb", "explain", "--exit", "9", "--no-record"]).stdin(Stdio::null()).output().unwrap();
    // `bb explain --no-record` is an unknown flag to explain, so the child fails with clap's code 2.
    assert_eq!(o.status.code(), Some(2), "{}", s(&o.stderr));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn output_arrives_while_the_command_is_still_running() {
    let d = tmp("live");
    let cmd: Vec<String> = if cfg!(windows) {
        vec!["cmd".into(), "/c".into(), "echo first& ping -n 4 127.0.0.1 >nul& echo second".into()]
    } else {
        vec!["sh".into(), "-c".into(), "echo first; sleep 3; echo second".into()]
    };
    let mut child = bb().arg("--db").arg(d.join("bb.db")).arg("watch").arg("--").args(&cmd).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let t0 = Instant::now();
    let mut lines = BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    lines.read_line(&mut first).unwrap();
    let got_first_after = t0.elapsed();
    assert!(first.contains("first"), "{first:?}");
    assert!(got_first_after < Duration::from_millis(2000), "the first line must not wait for the command to finish: {got_first_after:?}");
    let status = child.wait().unwrap();
    assert!(status.success());
    assert!(t0.elapsed() > Duration::from_millis(2000), "the command really did run for a while");
    let _ = std::fs::remove_dir_all(&d);
}

#[cfg(unix)]
#[test]
fn ctrl_c_stops_the_command_and_prints_no_diagnosis() {
    use std::os::unix::process::ExitStatusExt;
    let d = tmp("sigint");
    let mut child = bb().arg("--db").arg(d.join("bb.db")).args(["watch", "--", "sleep", "30"]).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(600));
    Command::new("kill").args(["-INT", &child.id().to_string()]).status().unwrap();
    let t0 = Instant::now();
    let out = child.wait_with_output().unwrap();
    assert!(t0.elapsed() < Duration::from_secs(10));
    assert!(!s(&out.stderr).contains("that failed"), "{}", s(&out.stderr));
    assert!(out.status.code().is_some() || out.status.signal().is_some());
    let _ = std::fs::remove_dir_all(&d);
}
