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
pub fn print_findings(findings: &[Finding], indent: &str) {
    for (i, f) in findings.iter().enumerate() {
        println!("{indent}{:>2}. [{}] {}", i + 1, f.confidence.label(), f.title);
        for e in &f.evidence {
            println!("{indent}      - {e}");
        }
        if let Some(h) = &f.hint {
            println!("{indent}      > {h}");
        }
    }
}

/// Prints what is known about one failure. Honest about what it could not tell.
pub fn print_verdict(verdict: &TextVerdict, had_text: bool, indent: &str) {
    match verdict {
        TextVerdict::Empty => {
            let why = if had_text { "nothing but blank space" } else { "no output" };
            println!("{indent}Nothing to diagnose: {why}.");
        }
        TextVerdict::Binary => println!("{indent}This looks like binary data, not text, so there is nothing to diagnose."),
        TextVerdict::NoError => println!("{indent}Nothing in this looks like an error."),
        TextVerdict::Unrecognised { exit_code } => {
            println!("{indent}Exit code {exit_code}, and the text matches no error blackbox knows. It can't say why this failed.");
        }
        TextVerdict::Findings(f) => print_findings(f, indent),
    }
}

/// One failed command with its diagnosis: the text (if it was captured) and what the
/// machine was doing while it ran (if the recorder was on).
pub fn print_failure(store: &Store, f: &Failure) {
    let took = f.duration_ms.map(|ms| format!(", {:.1}s", ms as f64 / 1000.0)).unwrap_or_default();
    println!("  {}  exit {}  {}{took}", clock(f.ts), f.exit_code, short(&f.command, 70));
    match &f.output {
        Some(text) => print_verdict(&diagnose_text(text, Some(f.exit_code)), true, "    "),
        // Nothing was captured, so only the exit code can speak.
        None => match exit_code_finding(f.exit_code) {
            Some(code) => print_findings(&[code], "    "),
            None => println!("    Only the exit code was recorded (the shell hook can't see output). `bb watch <command>` captures it."),
        },
    }
    if let Ok(ctx) = machine_context(store, f) {
        if !ctx.is_empty() {
            println!("    The machine was also struggling while it ran:");
            print_findings(&ctx, "    ");
        }
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
}
