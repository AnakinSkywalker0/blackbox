//! Failed commands: listing them and printing their diagnosis. Shared by `bb why`,
//! `bb failures` and, later, `bb explain` and `bb watch`, so they all word things alike.

use bb_core::failure::{diagnose_text, exit_code_finding, machine_context, Failure, TextVerdict};
use bb_core::model::Finding;
use bb_core::store::Store;
use chrono::{Local, TimeZone};

type Res = Result<(), String>;

fn clock(ts: i64) -> String {
    Local.timestamp_opt(ts, 0).single().map_or_else(|| ts.to_string(), |t| t.format("%a %H:%M:%S").to_string())
}

fn short(s: &str, max: usize) -> String {
    let one_line = s.lines().next().unwrap_or("").trim();
    match one_line.char_indices().nth(max) {
        Some((i, _)) => format!("{}...", &one_line[..i]),
        None => one_line.to_string(),
    }
}

/// The numbered "cause / evidence / hint" block, in the same shape `bb why` uses.
pub fn findings_lines(findings: &[Finding]) -> Vec<String> {
    let mut out = Vec::new();
    for (i, f) in findings.iter().enumerate() {
        out.push(format!("{:>2}. [{}] {}", i + 1, f.confidence.label(), f.title));
        for e in &f.evidence {
            out.push(format!("      - {e}"));
        }
        if let Some(h) = &f.hint {
            out.push(format!("      > {h}"));
        }
    }
    out
}

pub fn print_findings(findings: &[Finding], indent: &str) {
    for l in findings_lines(findings) {
        println!("{indent}{l}");
    }
}

/// What to say about one piece of text. Shared by `bb why` (for a recorded failure) and
/// `bb explain` (for pasted text), so the same text always gets the same answer.
pub fn verdict_lines(verdict: &TextVerdict, had_text: bool) -> Vec<String> {
    match verdict {
        TextVerdict::Empty => {
            let why = if had_text { "nothing but blank space" } else { "no output" };
            vec![format!("Nothing to diagnose: {why}.")]
        }
        TextVerdict::Binary => vec!["This looks like binary data, not text, so there is nothing to diagnose.".into()],
        TextVerdict::NoError => vec!["Nothing in this looks like an error.".into()],
        TextVerdict::Unrecognised { exit_code } => {
            vec![format!("Exit code {exit_code}, and the text matches no error blackbox knows. It can't say why this failed.")]
        }
        TextVerdict::Findings(f) => findings_lines(f),
    }
}

/// Prints what is known about one failure. Honest about what it could not tell.
pub fn print_verdict(verdict: &TextVerdict, had_text: bool, indent: &str) {
    for l in verdict_lines(verdict, had_text) {
        println!("{indent}{l}");
    }
}

/// Everything known about why one failed command failed.
pub struct FailureDiagnosis {
    /// The words to print: what the error text (or, failing that, the exit code) says.
    pub lines: Vec<String>,
    /// The same as structured findings (empty when nothing was recognised).
    pub findings: Vec<Finding>,
    /// What the machine was doing while it ran, from the vitals recorder.
    pub context: Vec<Finding>,
}

/// The text (if it was captured) and what the machine was doing while the command ran
/// (if the recorder was on). Shared by `bb why` and the MCP server.
pub fn diagnose_failure(store: &Store, f: &Failure) -> FailureDiagnosis {
    let (lines, findings) = match &f.output {
        Some(text) => {
            let v = diagnose_text(text, Some(f.exit_code));
            let findings = match &v {
                TextVerdict::Findings(fs) => fs.clone(),
                _ => Vec::new(),
            };
            (verdict_lines(&v, true), findings)
        }
        // Nothing was captured, so only the exit code can speak.
        None => match exit_code_finding(f.exit_code) {
            Some(code) => (findings_lines(std::slice::from_ref(&code)), vec![code]),
            None => (
                vec!["Only the exit code was recorded (the shell hook can't see output). `bb watch <command>` captures it.".to_string()],
                Vec::new(),
            ),
        },
    };
    let context = machine_context(store, f).unwrap_or_default();
    FailureDiagnosis { lines, findings, context }
}

/// One failed command with its diagnosis.
pub fn print_failure(store: &Store, f: &Failure) {
    let took = f.duration_ms.map(|ms| format!(", {:.1}s", ms as f64 / 1000.0)).unwrap_or_default();
    println!("  {}  exit {}  {}{took}", clock(f.ts), f.exit_code, short(&f.command, 70));
    let d = diagnose_failure(store, f);
    for l in &d.lines {
        println!("    {l}");
    }
    if !d.context.is_empty() {
        println!("    The machine was also struggling while it ran:");
        print_findings(&d.context, "    ");
    }
}

/// The failed-commands section of `bb why`. Prints nothing when there are none, so
/// `bb why` output is unchanged for anyone who doesn't use failure capture.
pub fn print_in_window(store: &Store, from: i64, to: i64) {
    let Ok(failures) = store.failures_between(from, to) else { return };
    if failures.is_empty() {
        return;
    }
    println!("\nFailed commands in this window:\n");
    for f in &failures {
        print_failure(store, f);
        println!();
    }
}

/// `bb failures`: the most recent failed commands.
pub fn list(db: &std::path::Path, limit: u32) -> Res {
    let store = Store::open(db).map_err(|e| format!("can't open database {}: {e}", db.display()))?;
    let rows = store.recent_failures(limit.max(1)).map_err(|e| e.to_string())?;
    if rows.is_empty() {
        println!("No failed commands recorded yet. `bb hook install <shell>` notices them; `bb watch <command>` captures their output.");
        return Ok(());
    }
    println!("{:<5} {:<19} {:<5} {}", "ID", "WHEN", "EXIT", "COMMAND");
    for f in &rows {
        let day = Local.timestamp_opt(f.ts, 0).single().map_or_else(|| f.ts.to_string(), |t| t.format("%d %b %H:%M:%S").to_string());
        let mark = if f.output.is_some() { "" } else { "  (exit code only)" };
        println!("{:<5} {:<19} {:<5} {}{mark}", f.id, day, f.exit_code, short(&f.command, 60));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_keeps_the_first_line_and_cuts_long_ones() {
        assert_eq!(short("a\nb", 10), "a");
        assert_eq!(short(&"x".repeat(100), 5), "xxxxx...");
        assert_eq!(short("", 5), "");
    }

    #[test]
    fn a_pasted_error_and_a_recorded_failure_get_the_same_words() {
        // `bb explain` and the failed-commands section of `bb why` both render through
        // verdict_lines, so identical text can't be diagnosed two different ways.
        let text = "error: linker `link.exe` not found
  = note: program not found";
        let pasted = verdict_lines(&diagnose_text(text, None), true);
        let stored = verdict_lines(&diagnose_text(text, Some(1)), true);
        assert_eq!(pasted, stored);
        assert!(pasted[0].contains("[high] The C/C++ linker is missing"));
        // Same shape as `bb why`: numbered, evidence with "-", advice with ">".
        assert!(pasted[1].trim_start().starts_with("- "));
        assert!(pasted[2].trim_start().starts_with("> "));
    }

    #[test]
    fn every_kind_of_nothing_has_its_own_honest_sentence() {
        let all: Vec<String> = [
            TextVerdict::Empty,
            TextVerdict::Binary,
            TextVerdict::NoError,
            TextVerdict::Unrecognised { exit_code: 4 },
        ]
        .iter()
        .map(|v| verdict_lines(v, true).join(" "))
        .collect();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}
