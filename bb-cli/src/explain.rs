//! `bb explain`: diagnose error text that wasn't captured by blackbox, with the same
//! rules and the same wording `bb why` uses for a recorded failure.

use crate::failures::{print_verdict, verdict_lines};
use bb_core::failure::{decode_text, diagnose_text, tail_utf8, TextVerdict};
use std::io::{IsTerminal, Read};
use std::path::Path;
use std::process::Command;

type Res = Result<(), String>;

/// Never read more than this. Log files can be huge, and the end is what matters.
const MAX_INPUT_BYTES: u64 = 4 * 1024 * 1024;
/// How much of the end is searched.
const SEARCH_BYTES: usize = 256 * 1024;

fn read_limited(r: impl Read) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    r.take(MAX_INPUT_BYTES).read_to_end(&mut buf).map_err(|e| format!("couldn't read the input: {e}"))?;
    Ok(buf)
}

/// Clipboard readers to try, in order, for this OS.
fn clipboard_commands() -> Vec<(&'static str, Vec<&'static str>)> {
    if cfg!(windows) {
        vec![("powershell", vec!["-NoProfile", "-NonInteractive", "-Command", "Get-Clipboard -Raw"])]
    } else if cfg!(target_os = "macos") {
        vec![("pbpaste", vec![])]
    } else {
        vec![("wl-paste", vec!["--no-newline"]), ("xclip", vec!["-selection", "clipboard", "-o"]), ("xsel", vec!["--clipboard", "--output"])]
    }
}

fn read_clipboard() -> Result<Vec<u8>, String> {
    for (prog, args) in clipboard_commands() {
        if let Ok(out) = Command::new(prog).args(&args).output() {
            if out.status.success() {
                return Ok(out.stdout);
            }
        }
    }
    Err("couldn't read the clipboard (no clipboard tool found). Pipe the text in, or pass a file.".into())
}

/// The diagnosis of raw input bytes. Pure, so it can be tested without a terminal.
pub fn diagnose_bytes(bytes: &[u8], exit: Option<i32>) -> TextVerdict {
    match decode_text(bytes) {
        None => TextVerdict::Binary,
        Some(text) => diagnose_text(tail_utf8(&text, SEARCH_BYTES), exit),
    }
}

/// What `bb explain` prints, as lines.
pub fn report(bytes: &[u8], exit: Option<i32>) -> Vec<String> {
    let verdict = diagnose_bytes(bytes, exit);
    let mut out = Vec::new();
    if matches!(verdict, TextVerdict::Findings(_)) {
        out.push("Most likely causes:".to_string());
        out.push(String::new());
    }
    out.extend(verdict_lines(&verdict, !bytes.is_empty()));
    out
}

pub fn run(file: Option<&Path>, clipboard: bool, exit: Option<i32>) -> Res {
    let bytes = if clipboard {
        read_clipboard()?
    } else if let Some(path) = file.filter(|p| p.as_os_str() != "-") {
        read_limited(std::fs::File::open(path).map_err(|e| format!("can't open {}: {e}", path.display()))?)?
    } else {
        let stdin = std::io::stdin();
        if stdin.is_terminal() {
            let end = if cfg!(windows) { "Ctrl-Z then Enter" } else { "Ctrl-D" };
            eprintln!("Paste the error text, then press {end}. (Or: bb explain <file>, bb explain --clipboard, or pipe it in.)");
        }
        read_limited(stdin.lock())?
    };
    let verdict = diagnose_bytes(&bytes, exit);
    if matches!(verdict, TextVerdict::Findings(_)) {
        println!("Most likely causes:\n");
    }
    print_verdict(&verdict, !bytes.is_empty(), "");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explains_error_text_like_bb_why_does() {
        let r = report(b"npm ERR! code EACCES\nnpm ERR! Error: EACCES: permission denied, mkdir '/usr/lib'", Some(243));
        assert_eq!(r[0], "Most likely causes:");
        assert!(r[2].contains("[high] Permission was denied"), "{r:?}");
        assert!(r.iter().any(|l| l.trim_start().starts_with("- ") && l.contains("EACCES")));
    }

    #[test]
    fn empty_input_is_said_to_be_empty() {
        assert_eq!(report(b"", None), vec!["Nothing to diagnose: no output."]);
        assert_eq!(report(b"  \r\n\t", None), vec!["Nothing to diagnose: nothing but blank space."]);
    }

    #[test]
    fn binary_junk_is_not_diagnosed() {
        let junk: Vec<u8> = (0..500u32).map(|i| (i * 53 % 256) as u8 | 0x80).collect();
        assert_eq!(report(&junk, Some(1)), vec!["This looks like binary data, not text, so there is nothing to diagnose."]);
        assert_eq!(report(b"abc\0\0\0permission denied", None).len(), 1);
    }

    #[test]
    fn ordinary_text_is_not_called_an_error() {
        assert_eq!(report(b"Build finished in 2.1s\nAll 40 tests passed", Some(0)), vec!["Nothing in this looks like an error."]);
    }

    #[test]
    fn an_unknown_failure_admits_it_does_not_know() {
        let r = report(b"the frobnicator desynchronised", Some(9));
        assert_eq!(r.len(), 1);
        assert!(r[0].starts_with("Exit code 9") && r[0].contains("can't say why"));
    }

    #[test]
    fn utf16_redirect_output_from_powershell_is_read() {
        let text = "error: linker `link.exe` not found";
        let bytes: Vec<u8> = [0xFF, 0xFE].into_iter().chain(text.encode_utf16().flat_map(|u| u.to_le_bytes())).collect();
        assert!(report(&bytes, None).iter().any(|l| l.contains("The C/C++ linker is missing")));
    }

    #[test]
    fn only_the_end_of_a_huge_log_is_searched() {
        let mut log = b"permission denied\n".to_vec();
        log.extend(std::iter::repeat(b'x').take(SEARCH_BYTES * 2));
        log.extend_from_slice(b"\nconnection refused\n");
        let r = report(&log, None);
        assert!(r.iter().any(|l| l.contains("network connection failed")));
        assert!(!r.iter().any(|l| l.contains("Permission was denied")));
    }

    #[test]
    fn input_is_capped() {
        let big = std::io::repeat(b'a');
        assert_eq!(read_limited(big).unwrap().len() as u64, MAX_INPUT_BYTES);
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_panic() {
        let p = std::env::temp_dir().join("bb-explain-does-not-exist.txt");
        assert!(run(Some(&p), false, None).unwrap_err().contains("can't open"));
    }

    #[test]
    fn the_file_path_reads_a_file() {
        let p = std::env::temp_dir().join(format!("bb-explain-{}.txt", std::process::id()));
        std::fs::write(&p, "ENOSPC: no space left on device").unwrap();
        assert!(run(Some(&p), false, None).is_ok());
        let _ = std::fs::remove_file(&p);
    }
}
