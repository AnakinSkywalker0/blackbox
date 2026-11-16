//! `bb watch <command...>`: run a command, show its output live, and if it fails, say why.
//!
//! The exit code is always the command's own. The command's output is passed through as
//! it arrives and also kept (the last 64 KB) so a failure can be diagnosed from what the
//! command actually printed. Because the output is read through pipes, programs that only
//! colour or animate their output on a terminal will print it plainly.

use crate::failures::diagnose_failure;
use bb_core::failure::{decode_text, tail_utf8, Failure, MAX_OUTPUT_BYTES};
use bb_core::store::Store;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a command gets to stop by itself after Ctrl-C before it is killed.
const GRACE: Duration = Duration::from_millis(1500);
/// How long to wait for the output pipes to close once the command has exited. A
/// background process the command started can hold them open indefinitely.
const PIPE_WAIT: Duration = Duration::from_secs(2);
/// Exit code reported when the command was stopped by Ctrl-C and gave none of its own.
pub const INTERRUPTED_CODE: i32 = 130;

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It ran and exited with this code.
    Exited(i32),
    /// Ctrl-C arrived. The code is the command's own if it stopped by itself, else 130.
    Interrupted(i32),
    /// It never started (no such program, not allowed).
    NotStarted { code: i32, message: String },
}

impl Outcome {
    pub fn code(&self) -> i32 {
        match self {
            Outcome::Exited(c) | Outcome::Interrupted(c) => *c,
            Outcome::NotStarted { code, .. } => *code,
        }
    }
}

pub struct Run {
    pub outcome: Outcome,
    /// Everything it printed (stdout and stderr, in arrival order), last part only.
    pub output: Vec<u8>,
    pub duration_ms: i64,
}

/// Exit codes that mean "stopped with Ctrl-C", so no diagnosis is wanted.
fn is_ctrl_c_code(code: i32) -> bool {
    code == 130 || code == -1073741510 // 0xC000013A, STATUS_CONTROL_C_EXIT
}

/// Finds the program the way the shell would. Rust's own search on Windows only tries
/// `.exe`, so `npm` (really `npm.cmd`) would not be found.
pub fn resolve_program(prog: &str, path_var: Option<&str>, pathext: Option<&str>, windows: bool) -> PathBuf {
    if !windows || prog.contains(['\\', '/']) || Path::new(prog).extension().is_some() {
        return PathBuf::from(prog);
    }
    let exts: Vec<String> = pathext.unwrap_or(".COM;.EXE;.BAT;.CMD").split(';').filter(|e| !e.is_empty()).map(|e| e.to_string()).collect();
    for dir in path_var.unwrap_or("").split(';').filter(|d| !d.is_empty()) {
        for ext in &exts {
            let candidate = Path::new(dir).join(format!("{prog}{ext}"));
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from(prog)
}

/// The command as a person would have typed it, for the record.
pub fn command_line(prog: &str, args: &[String]) -> String {
    let q = |s: &str| if s.is_empty() || s.contains([' ', '\t', '"']) { format!("\"{}\"", s.replace('"', "\\\"")) } else { s.to_string() };
    std::iter::once(prog).chain(args.iter().map(String::as_str)).map(q).collect::<Vec<_>>().join(" ")
}

/// Keeps only the last `cap` bytes, trimming in big steps so it isn't done per read.
struct Tail {
    buf: Vec<u8>,
    cap: usize,
}

impl Tail {
    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        if self.buf.len() > self.cap * 2 {
            let cut = self.buf.len() - self.cap;
            self.buf.drain(..cut);
        }
    }
}

/// Copies one of the command's output streams to ours as it arrives, and into `tail`.
fn pump(mut from: impl Read + Send + 'static, mut to: Option<Box<dyn Write + Send>>, tail: Arc<Mutex<Tail>>, done: mpsc::Sender<()>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // A closed pipe on our side (`bb watch x | head`) must not stop us draining
                    // the command, or it would block on a full pipe.
                    if let Some(w) = to.as_mut() {
                        if w.write_all(&buf[..n]).and_then(|_| w.flush()).is_err() {
                            to = None;
                        }
                    }
                    if let Ok(mut t) = tail.lock() {
                        t.push(&buf[..n]);
                    }
                }
            }
        }
        let _ = done.send(());
    });
}

/// Runs the command to completion (or Ctrl-C), passing its output through when `echo` is set.
pub fn run_command(program: &str, args: &[String], interrupted: &AtomicBool, echo: bool) -> Run {
    let started = Instant::now();
    let elapsed = || started.elapsed().as_millis() as i64;
    let exe = resolve_program(program, std::env::var("PATH").ok().as_deref(), std::env::var("PATHEXT").ok().as_deref(), cfg!(windows));
    let mut child = match Command::new(&exe).args(args).stdin(Stdio::inherit()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            let (code, what) = match e.kind() {
                std::io::ErrorKind::NotFound => (127, format!("command not found: {program}")),
                std::io::ErrorKind::PermissionDenied => (126, format!("permission denied: {program}")),
                _ => (126, format!("can't run {program}: {e}")),
            };
            return Run { outcome: Outcome::NotStarted { code, message: what.clone() }, output: what.into_bytes(), duration_ms: elapsed() };
        }
    };

    let tail = Arc::new(Mutex::new(Tail { buf: Vec::new(), cap: MAX_OUTPUT_BYTES }));
    let (done_tx, done_rx) = mpsc::channel();
    let out: Option<Box<dyn Write + Send>> = echo.then(|| Box::new(std::io::stdout()) as Box<dyn Write + Send>);
    let err: Option<Box<dyn Write + Send>> = echo.then(|| Box::new(std::io::stderr()) as Box<dyn Write + Send>);
    let mut pumps = 0;
    if let Some(s) = child.stdout.take() {
        pump(s, out, tail.clone(), done_tx.clone());
        pumps += 1;
    }
    if let Some(s) = child.stderr.take() {
        pump(s, err, tail.clone(), done_tx.clone());
        pumps += 1;
    }
    drop(done_tx);

    let mut asked_to_stop: Option<Instant> = None;
    let mut killed = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {}
            Err(_) => break None,
        }
        if interrupted.load(Ordering::SeqCst) {
            let since = *asked_to_stop.get_or_insert_with(Instant::now);
            if since.elapsed() >= GRACE && !killed {
                let _ = child.kill();
                killed = true;
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    };

    // Let the last of the output arrive, but never hang on a pipe someone else holds open.
    for _ in 0..pumps {
        if done_rx.recv_timeout(PIPE_WAIT).is_err() {
            break;
        }
    }
    let output = tail.lock().map(|t| t.buf.clone()).unwrap_or_default();
    let was_interrupted = interrupted.load(Ordering::SeqCst);
    let code = status.and_then(|s| s.code());
    let outcome = match (was_interrupted, code, killed) {
        (true, Some(c), false) => Outcome::Interrupted(c),
        (true, _, _) => Outcome::Interrupted(INTERRUPTED_CODE),
        (false, Some(c), _) => Outcome::Exited(c),
        // Ended by a signal with no code (Unix): the shell convention is 128 + signal; 1 if unknown.
        (false, None, _) => Outcome::Exited(signal_code(status)),
    };
    Run { outcome, output, duration_ms: elapsed() }
}

#[cfg(unix)]
fn signal_code(status: Option<std::process::ExitStatus>) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.and_then(|s| s.signal()).map_or(1, |sig| 128 + sig)
}

#[cfg(not(unix))]
fn signal_code(_: Option<std::process::ExitStatus>) -> i32 {
    1
}

/// What was captured, as text for the record and the diagnosis.
fn captured_text(output: &[u8]) -> Option<String> {
    if output.is_empty() {
        return None;
    }
    let text = decode_text(output).unwrap_or_else(|| String::from_utf8_lossy(output).into_owned());
    Some(tail_utf8(&text, MAX_OUTPUT_BYTES).to_string())
}

/// Saves a failed run (unless asked not to) and prints why it failed, to stderr so the
/// command's own stdout stays clean for pipes. Returns nothing: a problem here never
/// changes the exit code.
pub fn report_failure(db: &Path, command: &str, run: &Run, record: bool) {
    let mut failure = Failure {
        id: 0,
        ts: chrono::Local::now().timestamp(),
        command: command.to_string(),
        exit_code: run.outcome.code(),
        cwd: std::env::current_dir().ok().map(|p| p.display().to_string()),
        output: captured_text(&run.output),
        duration_ms: Some(run.duration_ms),
    };
    // The real database gives the machine's vitals during the run; without it (or with
    // recording off) an empty in-memory one still lets the text be diagnosed.
    let store = Store::open(db).ok();
    let mut saved = None;
    if record {
        match &store {
            Some(s) => match s.insert_failure(&failure) {
                Ok(id) => {
                    failure.id = id;
                    saved = Some(id);
                }
                Err(e) => eprintln!("bb: couldn't save this failure: {e}"),
            },
            None => eprintln!("bb: couldn't open the database at {}, so this failure wasn't saved", db.display()),
        }
    }
    let scratch;
    let store_ref = match &store {
        Some(s) => s,
        None => match Store::open_in_memory() {
            Ok(s) => {
                scratch = s;
                &scratch
            }
            Err(_) => return,
        },
    };
    let d = diagnose_failure(store_ref, &failure);
    let mut err = std::io::stderr().lock();
    let took = format!("{:.1}s", run.duration_ms as f64 / 1000.0);
    let _ = writeln!(err, "\nbb: that failed (exit {}, {took}). Most likely:", failure.exit_code);
    for l in &d.lines {
        let _ = writeln!(err, "  {l}");
    }
    if !d.context.is_empty() {
        let _ = writeln!(err, "  The machine was also struggling while it ran:");
        for l in crate::failures::findings_lines(&d.context) {
            let _ = writeln!(err, "  {l}");
        }
    }
    if let Some(id) = saved {
        let _ = writeln!(err, "  (saved as failure #{id}; `bb failures` lists them)");
    }
}

/// `bb watch`: never returns. Exits with the command's own exit code.
pub fn run(db: &Path, argv: &[String], record: bool) -> ! {
    let (program, args) = match argv.split_first() {
        Some((p, a)) => (p.as_str(), a),
        None => {
            eprintln!("bb: watch needs a command, for example: bb watch cargo build");
            std::process::exit(2);
        }
    };
    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = interrupted.clone();
    let _ = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst));

    let result = run_command(program, args, &interrupted, true);
    let code = result.outcome.code();
    let skip = matches!(result.outcome, Outcome::Interrupted(_)) || is_ctrl_c_code(code);
    if let Outcome::NotStarted { message, .. } = &result.outcome {
        eprintln!("bb: {message}");
    }
    if code != 0 && !skip {
        report_failure(db, &command_line(program, args), &result, record);
    }
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet(prog: &str, args: &[&str]) -> Run {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        run_command(prog, &args, &AtomicBool::new(false), false)
    }

    /// A command that exits with `code` after printing `out` to stdout and `err` to stderr.
    fn shell(out: &str, err: &str, code: i32) -> (String, Vec<String>) {
        if cfg!(windows) {
            ("cmd".into(), vec!["/c".into(), format!("echo {out}& echo {err} 1>&2& exit {code}")])
        } else {
            ("sh".into(), vec!["-c".into(), format!("echo {out}; echo {err} >&2; exit {code}")])
        }
    }

    fn text(run: &Run) -> String {
        String::from_utf8_lossy(&run.output).into_owned()
    }

    #[test]
    fn a_passing_command_reports_zero_and_keeps_its_output() {
        let (p, a) = shell("hello", "warn", 0);
        let r = run_command(&p, &a, &AtomicBool::new(false), false);
        assert_eq!(r.outcome, Outcome::Exited(0));
        let t = text(&r);
        assert!(t.contains("hello") && t.contains("warn"), "{t:?}");
    }

    #[test]
    fn a_failing_command_reports_its_own_exit_code() {
        for code in [1, 2, 7, 42, 255] {
            let (p, a) = shell("x", "boom", code);
            assert_eq!(run_command(&p, &a, &AtomicBool::new(false), false).outcome, Outcome::Exited(code));
        }
    }

    #[test]
    fn a_program_that_does_not_exist_is_127_with_a_message_the_diagnosis_understands() {
        let r = quiet("bb-no-such-program-zz", &["a"]);
        assert_eq!(r.outcome.code(), 127);
        assert!(matches!(r.outcome, Outcome::NotStarted { .. }));
        let t = text(&r);
        assert!(t.contains("command not found: bb-no-such-program-zz"));
        let v = bb_core::failure::diagnose_text(&t, Some(127));
        assert!(matches!(&v, bb_core::failure::TextVerdict::Findings(f) if f[0].title.contains("not found")));
    }

    #[test]
    fn ctrl_c_stops_the_command_and_means_no_diagnosis() {
        let interrupted = Arc::new(AtomicBool::new(false));
        let flag = interrupted.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            flag.store(true, Ordering::SeqCst);
        });
        let (p, a): (&str, Vec<String>) = if cfg!(windows) {
            ("ping", vec!["-n".into(), "30".into(), "127.0.0.1".into()])
        } else {
            ("sleep", vec!["30".into()])
        };
        let t0 = Instant::now();
        let r = run_command(p, &a, &interrupted, false);
        assert!(t0.elapsed() < Duration::from_secs(10), "the command must not be waited for: {:?}", t0.elapsed());
        assert!(matches!(r.outcome, Outcome::Interrupted(_)), "{:?}", r.outcome);
        assert_eq!(r.outcome.code(), INTERRUPTED_CODE);
    }

    #[test]
    fn only_the_tail_of_huge_output_is_kept() {
        let mut t = Tail { buf: Vec::new(), cap: 100 };
        for i in 0..1000u32 {
            t.push(format!("line {i}\n").as_bytes());
        }
        assert!(t.buf.len() <= 200 + 20);
        assert!(String::from_utf8_lossy(&t.buf).ends_with("line 999\n"));
    }

    #[test]
    fn commands_are_recorded_the_way_they_were_typed() {
        assert_eq!(command_line("cargo", &["build".into(), "--release".into()]), "cargo build --release");
        assert_eq!(command_line("git", &["commit".into(), "-m".into(), "a b".into()]), "git commit -m \"a b\"");
        assert_eq!(command_line("x", &["".into(), "say \"hi\"".into()]), "x \"\" \"say \\\"hi\\\"\"");
    }

    #[test]
    fn windows_programs_are_found_with_pathext_so_npm_cmd_works() {
        let d = std::env::temp_dir().join(format!("bb-watch-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("npm.cmd"), "@echo hi").unwrap();
        let path = format!("C:\\nowhere;{}", d.display());
        // PATHEXT is upper case; the file system doesn't care, so compare ignoring case.
        let found = resolve_program("npm", Some(&path), Some(".COM;.EXE;.BAT;.CMD"), true);
        assert_eq!(found.to_string_lossy().to_lowercase(), d.join("npm.cmd").to_string_lossy().to_lowercase());
        // Left alone: explicit paths, names with an extension, other platforms, and unknown programs.
        assert_eq!(resolve_program("C:\\x\\npm", Some(&path), None, true), PathBuf::from("C:\\x\\npm"));
        assert_eq!(resolve_program("npm.cmd", Some(&path), None, true), PathBuf::from("npm.cmd"));
        assert_eq!(resolve_program("npm", Some(&path), None, false), PathBuf::from("npm"));
        assert_eq!(resolve_program("nothing", Some(&path), None, true), PathBuf::from("nothing"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn ctrl_c_exit_codes_are_recognised() {
        assert!(is_ctrl_c_code(130) && is_ctrl_c_code(-1073741510));
        assert!(!is_ctrl_c_code(1) && !is_ctrl_c_code(0));
    }

    #[test]
    fn a_failed_run_is_saved_with_its_output_and_diagnosable() {
        let d = std::env::temp_dir().join(format!("bb-watch-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let db = d.join("bb.db");
        let run = Run { outcome: Outcome::Exited(101), output: b"error: linker `link.exe` not found\n".to_vec(), duration_ms: 1500 };
        report_failure(&db, "cargo build", &run, true);
        let got = Store::open(&db).unwrap().recent_failures(5).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].command.as_str(), got[0].exit_code), ("cargo build", 101));
        assert!(got[0].output.as_deref().unwrap().contains("link.exe"));
        assert_eq!(got[0].duration_ms, Some(1500));
        // With recording off nothing is written.
        report_failure(&db, "other", &run, false);
        assert_eq!(Store::open(&db).unwrap().recent_failures(5).unwrap().len(), 1);
        // An unusable database never panics or hides the diagnosis.
        report_failure(&d, "x", &run, true);
        let _ = std::fs::remove_dir_all(&d);
    }
}
