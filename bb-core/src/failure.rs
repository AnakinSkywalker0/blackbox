//! Failed commands: what is stored about one, and a plain-English diagnosis of its
//! error text. The diagnosis is pattern based and says so: it only names a cause it
//! can point at in the text, and otherwise says it does not recognise the error.

use crate::model::{Confidence, Finding};
use crate::rules::{analyze, Thresholds};
use crate::store::Store;

/// Longest output kept per failure. The end is kept, because that is where errors are.
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// One failed command, as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub id: i64,
    pub ts: i64,
    pub command: String,
    pub exit_code: i32,
    pub cwd: Option<String>,
    /// The tail of what the command printed, when it was captured (`bb watch`); the shell
    /// hook can only record that a command failed, not what it printed.
    pub output: Option<String>,
    pub duration_ms: Option<i64>,
}

/// The last `max` bytes of `s`, cut on a character boundary.
pub fn tail_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// What `diagnose_text` made of its input. Each case is a different honest answer.
#[derive(Debug, Clone)]
pub enum TextVerdict {
    Empty,
    Binary,
    /// Exit code 0 or unknown, and nothing in the text looks like an error.
    NoError,
    /// A failure with no pattern we know.
    Unrecognised { exit_code: i32 },
    Findings(Vec<Finding>),
}

struct Rule {
    title: &'static str,
    /// Lower-case phrases; any one in a line matches it.
    needles: &'static [&'static str],
    hint: &'static str,
    confidence: Confidence,
}

const RULES: &[Rule] = &[
    Rule {
        title: "The C/C++ linker is missing",
        needles: &["linker `link.exe` not found", "linker `cc` not found", "link.exe` not found", "cannot find -l"],
        hint: "Install the Visual Studio Build Tools with the C++ workload (Windows), or build-essential / Xcode command line tools (Linux, macOS), then retry.",
        confidence: Confidence::High,
    },
    Rule {
        title: "A program or command was not found",
        needles: &["is not recognized as an internal or external command", "is not recognized as the name of a cmdlet", "command not found", ": not found"],
        hint: "Check the spelling, or install the program and make sure its folder is on PATH. A new terminal is needed after changing PATH.",
        confidence: Confidence::High,
    },
    Rule {
        title: "Permission was denied",
        needles: &["permission denied", "access is denied", "eacces", "operation not permitted"],
        hint: "The account running this can't read, write or run that file. Check the path and who owns it. Running as administrator is a last resort, not a fix.",
        confidence: Confidence::High,
    },
    Rule {
        title: "A file or folder was not found",
        needles: &["no such file or directory", "cannot find the path specified", "cannot find the file specified", "enoent", "the system cannot find"],
        hint: "Check the path and the current folder. A relative path depends on where the command was run.",
        confidence: Confidence::High,
    },
    Rule {
        title: "A network port is already in use",
        needles: &["address already in use", "eaddrinuse", "only one usage of each socket address"],
        hint: "Another program is listening on that port. Stop it, or pick a different port.",
        confidence: Confidence::High,
    },
    Rule {
        title: "The disk is full",
        needles: &["no space left on device", "not enough space on the disk", "enospc", "disk quota exceeded"],
        hint: "Free some space on that drive, then retry.",
        confidence: Confidence::High,
    },
    Rule {
        title: "The program ran out of memory",
        needles: &["out of memory", "cannot allocate memory", "memoryerror", "oom-kill", "heap limit allocation failed"],
        hint: "Close other programs, or give the job less to hold at once.",
        confidence: Confidence::High,
    },
    Rule {
        title: "A network connection failed",
        needles: &[
            "connection refused",
            "econnrefused",
            "could not resolve host",
            "name or service not known",
            "temporary failure in name resolution",
            "getaddrinfo",
            "connection timed out",
            "operation timed out",
            "etimedout",
            "network is unreachable",
        ],
        hint: "Check the address, your connection and any VPN or proxy. If it's your own server, check that it's running.",
        confidence: Confidence::Medium,
    },
    Rule {
        title: "A security certificate was not trusted",
        needles: &["certificate verify failed", "ssl certificate problem", "unable to get local issuer certificate", "unable to verify the first certificate"],
        hint: "The site's certificate chain can't be verified here. A company proxy or a wrong system clock is the usual cause.",
        confidence: Confidence::High,
    },
    Rule {
        title: "Authentication failed",
        needles: &["authentication failed", "permission denied (publickey)", "could not read username", "401 unauthorized", "403 forbidden", "bad credentials"],
        hint: "The login, token or key was rejected. Check that it's current and has the right access.",
        confidence: Confidence::Medium,
    },
    Rule {
        title: "A required module or package is missing",
        needles: &["modulenotfounderror", "no module named", "cannot find module", "cannot find package", "unresolved import", "can't find crate"],
        hint: "Install the dependency (pip install, npm install, cargo add...), and check that you're in the right environment.",
        confidence: Confidence::High,
    },
    Rule {
        title: "The code does not compile",
        needles: &["error[e", "syntaxerror", "cannot find symbol", ": error c", "error ts", "could not compile", "compilation failed"],
        hint: "Fix the first error shown; later ones are often knock-on effects of it.",
        confidence: Confidence::High,
    },
    Rule {
        title: "A test failed",
        needles: &["test result: failed", "tests failed", "assertionerror", "assertion failed", "failures:"],
        hint: "Read the first failing test's message; the rest may share its cause.",
        confidence: Confidence::Medium,
    },
    Rule {
        title: "Git could not merge",
        needles: &["automatic merge failed", "merge conflict", "unmerged files"],
        hint: "Open the conflicted files, resolve the <<<<<<< markers, then add and commit.",
        confidence: Confidence::High,
    },
    Rule {
        title: "A file is in use by another process",
        needles: &["used by another process", "text file busy", "resource busy", "sharing violation"],
        hint: "Close the program holding the file (an editor, antivirus or a previous run) and retry.",
        confidence: Confidence::Medium,
    },
];

/// Removes terminal colour and cursor codes, which would split words the rules look for.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for n in chars.by_ref() {
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn looks_binary(s: &str) -> bool {
    let sample: Vec<char> = s.chars().take(4096).collect();
    if sample.contains(&'\0') {
        return true;
    }
    let odd = sample
        .iter()
        .filter(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\u{1b}'))
        .count();
    !sample.is_empty() && odd * 10 > sample.len() * 3
}

fn evidence_line(text: &str, needles: &[&str]) -> Option<String> {
    text.lines().find_map(|l| {
        let low = l.to_lowercase();
        needles.iter().any(|n| low.contains(n)).then(|| {
            let t = l.trim();
            match t.char_indices().nth(160) {
                Some((i, _)) => format!("{}...", &t[..i]),
                None => t.to_string(),
            }
        })
    })
}

/// What a bare exit code can honestly say, for the few codes that mean something.
pub fn exit_code_finding(code: i32) -> Option<Finding> {
    let (title, hint, confidence) = match code {
        130 | -1073741510 => ("The command was interrupted (Ctrl-C)", "Nothing went wrong; it was stopped on purpose.", Confidence::High),
        137 => ("The command was killed (exit 137)", "Usually the system ended it for using too much memory, or something sent it a kill signal.", Confidence::Medium),
        139 => ("The program crashed (exit 139, segmentation fault)", "A bug in the program or a library it loads. Try updating it.", Confidence::Medium),
        127 => ("A program or command was not found (exit 127)", "Check the spelling and PATH.", Confidence::Medium),
        126 => ("The file could not be run (exit 126)", "It exists but isn't executable, or isn't a program for this system.", Confidence::Medium),
        _ => return None,
    };
    Some(Finding { title: title.into(), evidence: vec![format!("exit code {code}")], confidence, hint: Some(hint.into()), score: 1.0 })
}

/// Diagnoses pasted or captured error text. `exit_code` is optional extra evidence.
pub fn diagnose_text(text: &str, exit_code: Option<i32>) -> TextVerdict {
    if text.trim().is_empty() {
        return TextVerdict::Empty;
    }
    if looks_binary(text) {
        return TextVerdict::Binary;
    }
    let clean = strip_ansi(text);
    let mut findings: Vec<Finding> = Vec::new();
    for (i, r) in RULES.iter().enumerate() {
        if let Some(line) = evidence_line(&clean, r.needles) {
            findings.push(Finding {
                title: r.title.into(),
                evidence: vec![line],
                confidence: r.confidence,
                hint: Some(r.hint.into()),
                score: 100.0 - i as f32,
            });
        }
    }
    // The text already explains a failure better than a bare exit code does.
    if findings.is_empty() {
        if let Some(f) = exit_code.and_then(exit_code_finding) {
            findings.push(f);
        }
    }
    findings.truncate(3);
    if !findings.is_empty() {
        return TextVerdict::Findings(findings);
    }
    match exit_code {
        Some(c) if c != 0 => TextVerdict::Unrecognised { exit_code: c },
        _ => TextVerdict::NoError,
    }
}

/// What the machine itself was doing while the command ran, from the vitals recorder.
/// Empty when the recorder wasn't running or nothing was wrong.
pub fn machine_context(store: &Store, f: &Failure) -> rusqlite::Result<Vec<Finding>> {
    let ran_secs = f.duration_ms.unwrap_or(0).max(0) / 1000;
    let from = f.ts - ran_secs.max(60) - 30;
    let samples = store.window(from, f.ts)?;
    if samples.is_empty() {
        return Ok(Vec::new());
    }
    Ok(analyze(&samples, store.best_mhz()?, &Thresholds::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(v: &TextVerdict) -> Vec<String> {
        match v {
            TextVerdict::Findings(f) => f.iter().map(|x| x.title.clone()).collect(),
            _ => vec![],
        }
    }

    #[test]
    fn names_a_cause_it_can_point_at_in_the_text() {
        let v = diagnose_text("error: linker `link.exe` not found\n  = note: program not found", Some(1));
        assert_eq!(titles(&v), vec!["The C/C++ linker is missing"]);
        let TextVerdict::Findings(f) = v else { panic!() };
        assert!(f[0].evidence[0].contains("link.exe"));
        assert_eq!(f[0].confidence, Confidence::High);
    }

    #[test]
    fn colour_codes_do_not_hide_a_match() {
        let v = diagnose_text("\u{1b}[31mpermis\u{1b}[0msion denied\u{1b}[0m", None);
        assert_eq!(titles(&v), vec!["Permission was denied"]);
    }

    #[test]
    fn empty_and_whitespace_input_is_empty() {
        assert!(matches!(diagnose_text("", None), TextVerdict::Empty));
        assert!(matches!(diagnose_text(" \n\t \r\n", Some(1)), TextVerdict::Empty));
    }

    #[test]
    fn binary_junk_is_refused_not_diagnosed() {
        let junk: String = (1u8..200).map(|b| (b % 8 + 1) as char).collect();
        assert!(matches!(diagnose_text(&junk, Some(1)), TextVerdict::Binary));
        assert!(matches!(diagnose_text("abc\0def permission denied", None), TextVerdict::Binary));
    }

    #[test]
    fn ordinary_text_is_not_an_error() {
        let v = diagnose_text("Compiling blackbox v0.5.1\nFinished dev profile in 3.2s", Some(0));
        assert!(matches!(v, TextVerdict::NoError));
        assert!(matches!(diagnose_text("hello world", None), TextVerdict::NoError));
    }

    #[test]
    fn a_failure_with_no_known_pattern_says_so() {
        let v = diagnose_text("something odd happened", Some(3));
        assert!(matches!(v, TextVerdict::Unrecognised { exit_code: 3 }));
    }

    #[test]
    fn exit_code_is_used_only_when_the_text_does_not_explain_it() {
        assert_eq!(titles(&diagnose_text("Killed", Some(137))), vec!["The command was killed (exit 137)"]);
        assert_eq!(titles(&diagnose_text("No such file or directory", Some(137))), vec!["A file or folder was not found"]);
        assert_eq!(titles(&diagnose_text("^C", Some(130))), vec!["The command was interrupted (Ctrl-C)"]);
    }

    #[test]
    fn at_most_three_findings_best_first() {
        let t = "permission denied\nno such file or directory\nconnection refused\nno space left on device";
        let v = titles(&diagnose_text(t, Some(1)));
        assert_eq!(v.len(), 3);
        assert_eq!(v[0], "Permission was denied");
    }

    #[test]
    fn tail_keeps_the_end_on_a_char_boundary() {
        assert_eq!(tail_utf8("abcdef", 3), "def");
        assert_eq!(tail_utf8("abc", 10), "abc");
        assert_eq!(tail_utf8("ééé", 3), "é");
    }

    #[test]
    fn long_evidence_lines_are_cut() {
        let line = format!("permission denied {}", "x".repeat(500));
        let TextVerdict::Findings(f) = diagnose_text(&line, None) else { panic!() };
        assert!(f[0].evidence[0].len() < 200 && f[0].evidence[0].ends_with("..."));
    }
}
