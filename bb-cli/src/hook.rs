//! `bb hook`: a few lines in the user's shell profile that notice a failed command and
//! point at `bb why`, and `bb hook record`, which the hook calls to remember it.
//!
//! The profile edit is the risky part, so it is conservative: the profile is backed up
//! first, our lines live in one clearly marked block, installing twice changes nothing,
//! and uninstalling removes exactly that block. A damaged block (one marker missing) is
//! never touched. The hook itself does nothing visible when `bb` is missing or its
//! database can't be opened.

use bb_core::failure::Failure;
use bb_core::store::Store;
use std::path::{Path, PathBuf};
use std::process::Command;

type Res = Result<(), String>;

pub const START: &str = "# >>> blackbox hook >>>";
pub const END: &str = "# <<< blackbox hook <<<";

/// Longest command line kept. A pasted heredoc shouldn't fill the database.
const MAX_CMD_CHARS: usize = 4000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    PowerShell,
    Bash,
    Zsh,
}

impl Shell {
    pub fn parse(s: &str) -> Result<Shell, String> {
        match s.to_lowercase().as_str() {
            "powershell" | "pwsh" | "ps" => Ok(Shell::PowerShell),
            "bash" => Ok(Shell::Bash),
            "zsh" => Ok(Shell::Zsh),
            other => Err(format!("unknown shell \"{other}\". Use powershell, bash or zsh.")),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Shell::PowerShell => "powershell",
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
        }
    }
}

// ---- the lines that go into the profile -------------------------------------------------

const POWERSHELL: &str = r#"# >>> blackbox hook >>>
# Added by `bb hook install powershell`. Remove it with `bb hook uninstall powershell`.
if (-not $global:__bb_hooked) {
    $global:__bb_hooked = $true
    $global:__bb_prompt_orig = $function:prompt
    $global:__bb_last_id = -1
    function global:prompt {
        $bbOk = $?
        $bbCode = $global:LASTEXITCODE
        if (-not $bbOk) {
            try {
                $bbH = Get-History -Count 1
                if ($bbH -and $bbH.Id -ne $global:__bb_last_id) {
                    $global:__bb_last_id = $bbH.Id
                    $bbN = if ($bbCode -is [int] -and $bbCode -ne 0) { $bbCode } else { 1 }
                    if ($bbN -ne 130 -and $bbH.CommandLine -notmatch '^\s*bb(\.exe)?(\s|$)' -and (Get-Command bb -CommandType Application -ErrorAction SilentlyContinue)) {
                        & bb hook record --exit $bbN --cmd $bbH.CommandLine --cwd (Get-Location).Path *> $null
                        Write-Host "bb: that failed (exit $bbN). Run ``bb why`` to see why." -ForegroundColor DarkYellow
                    }
                }
            } catch { }
        }
        if ($global:__bb_prompt_orig) { & $global:__bb_prompt_orig }
        else { "PS $($executionContext.SessionState.Path.CurrentLocation)$('>' * ($nestedPromptLevel + 1)) " }
    }
}
# <<< blackbox hook <<<"#;

const BASH: &str = r#"# >>> blackbox hook >>>
# Added by `bb hook install bash`. Remove it with `bb hook uninstall bash`.
__bb_hook() {
  local code=$?
  if [ "$code" -ne 0 ] && [ "$code" -ne 130 ] && command -v bb >/dev/null 2>&1; then
    local line n cmd
    line=$(HISTTIMEFORMAT= history 1 2>/dev/null) || return "$code"
    read -r n cmd <<<"$line"
    if [ -n "$n" ] && [ "$n" != "${__bb_last:-}" ]; then
      __bb_last=$n
      case "$cmd" in
        bb|"bb "*) ;;
        *)
          bb hook record --exit "$code" --cmd "$cmd" --cwd "$PWD" >/dev/null 2>&1
          echo "bb: that failed (exit $code). Run \`bb why\` to see why." >&2
          ;;
      esac
    fi
  fi
  return "$code"
}
case "$(declare -p PROMPT_COMMAND 2>/dev/null)" in
  "declare -a"*) case " ${PROMPT_COMMAND[*]} " in *" __bb_hook "*) ;; *) PROMPT_COMMAND=(__bb_hook "${PROMPT_COMMAND[@]}") ;; esac ;;
  *) case ";${PROMPT_COMMAND:-};" in *";__bb_hook;"*) ;; *) PROMPT_COMMAND="__bb_hook${PROMPT_COMMAND:+;$PROMPT_COMMAND}" ;; esac ;;
esac
# <<< blackbox hook <<<"#;

const ZSH: &str = r#"# >>> blackbox hook >>>
# Added by `bb hook install zsh`. Remove it with `bb hook uninstall zsh`.
__bb_hook() {
  local code=$?
  if [[ $code -ne 0 && $code -ne 130 ]] && (( $+commands[bb] )) && [[ ${HISTCMD:-0} != ${__bb_last:-} ]]; then
    __bb_last=${HISTCMD:-0}
    local cmd
    cmd=$(fc -ln -1 2>/dev/null)
    cmd=${cmd# }
    if [[ $cmd != bb && $cmd != "bb "* ]]; then
      bb hook record --exit "$code" --cmd "$cmd" --cwd "$PWD" >/dev/null 2>&1
      print -u2 -- "bb: that failed (exit $code). Run \`bb why\` to see why."
    fi
  fi
  return $code
}
autoload -Uz add-zsh-hook && add-zsh-hook precmd __bb_hook
# <<< blackbox hook <<<"#;

fn block_for(shell: Shell) -> &'static str {
    match shell {
        Shell::PowerShell => POWERSHELL,
        Shell::Bash => BASH,
        Shell::Zsh => ZSH,
    }
}

// ---- editing the profile text (pure, so it can be tested) --------------------------------

/// Where our block sits in `lines`, as inclusive indexes. `Err` when the markers are
/// unbalanced, which means someone edited it and we must not guess.
fn find_block(lines: &[&str]) -> Result<Option<(usize, usize)>, String> {
    let starts: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| l.trim() == START).map(|(i, _)| i).collect();
    let ends: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| l.trim() == END).map(|(i, _)| i).collect();
    match (starts.as_slice(), ends.as_slice()) {
        ([], []) => Ok(None),
        ([s], [e]) if s < e => Ok(Some((*s, *e))),
        _ => Err("the blackbox block in this profile is damaged (a marker is missing, doubled or out of order). Fix or delete the lines between \"# >>> blackbox hook >>>\" and \"# <<< blackbox hook <<<\" by hand, then retry.".into()),
    }
}

fn eol_of(original: &str) -> &'static str {
    if original.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

pub fn has_block(original: &str) -> Result<bool, String> {
    let lines: Vec<&str> = original.split_inclusive('\n').collect();
    Ok(find_block(&lines)?.is_some())
}

/// `original` with our block added, or replaced when it is already there.
pub fn add_block(original: &str, block: &str) -> Result<String, String> {
    let eol = eol_of(original);
    let block = block.replace('\n', eol) + eol;
    let lines: Vec<&str> = original.split_inclusive('\n').collect();
    if let Some((s, e)) = find_block(&lines)? {
        return Ok(format!("{}{}{}", lines[..s].concat(), block, lines[e + 1..].concat()));
    }
    let mut out = original.to_string();
    if !out.is_empty() {
        if !out.ends_with('\n') {
            out.push_str(eol);
        }
        out.push_str(eol); // a blank line between their content and ours
    }
    out.push_str(&block);
    Ok(out)
}

/// `original` without our block (and the blank line we put before it).
pub fn remove_block(original: &str) -> Result<String, String> {
    let lines: Vec<&str> = original.split_inclusive('\n').collect();
    let Some((s, e)) = find_block(&lines)? else {
        return Ok(original.to_string());
    };
    let from = if s > 0 && lines[s - 1].trim().is_empty() { s - 1 } else { s };
    Ok(format!("{}{}", lines[..from].concat(), lines[e + 1..].concat()))
}

// ---- profile files -----------------------------------------------------------------------

const BOM: &str = "\u{feff}";

/// Reads a profile as UTF-8. Returns the text without a byte order mark, and whether
/// there was one, so it can be put back. Anything that isn't UTF-8 is refused rather
/// than rewritten wrongly.
fn read_profile(path: &Path) -> Result<Option<(String, bool)>, String> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("can't read {}: {e}", path.display())),
    };
    let text = String::from_utf8(bytes)
        .map_err(|_| format!("{} isn't UTF-8 text (maybe UTF-16), so blackbox won't edit it. Add the hook by hand or re-save the file as UTF-8.", path.display()))?;
    Ok(Some(match text.strip_prefix(BOM) {
        Some(rest) => (rest.to_string(), true),
        None => (text, false),
    }))
}

fn write_profile(path: &Path, text: &str, bom: bool) -> Res {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("bb-tmp");
    let data = if bom { format!("{BOM}{text}") } else { text.to_string() };
    std::fs::write(&tmp, data).map_err(|e| format!("can't write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("can't replace {}: {e}", path.display())
    })
}

fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".bb-backup");
    path.with_file_name(name)
}

/// Installs (or refreshes) the block in the profile at `path`.
pub fn install_into(path: &Path, shell: Shell) -> Result<Outcome, String> {
    let existing = read_profile(path)?;
    let (original, bom) = existing.clone().unwrap_or((String::new(), false));
    let updated = add_block(&original, block_for(shell))?;
    if updated == original {
        return Ok(Outcome::Unchanged);
    }
    // Back up the file as it was before blackbox first touched it; never overwrite that.
    let backup = backup_path(path);
    if existing.is_some() && !backup.exists() {
        std::fs::copy(path, &backup).map_err(|e| format!("can't back up {}: {e}", path.display()))?;
    }
    write_profile(path, &updated, bom)?;
    Ok(if has_block(&original)? { Outcome::Updated } else { Outcome::Added })
}

/// Removes the block from the profile at `path`. A profile that only ever held our block
/// is deleted rather than left empty.
pub fn uninstall_from(path: &Path) -> Result<Outcome, String> {
    let Some((original, bom)) = read_profile(path)? else {
        return Ok(Outcome::Unchanged);
    };
    if !has_block(&original)? {
        return Ok(Outcome::Unchanged);
    }
    let updated = remove_block(&original)?;
    if updated.trim().is_empty() {
        std::fs::remove_file(path).map_err(|e| format!("can't remove {}: {e}", path.display()))?;
    } else {
        write_profile(path, &updated, bom)?;
    }
    Ok(Outcome::Removed)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Added,
    Updated,
    Removed,
    Unchanged,
}

// ---- finding the profiles ----------------------------------------------------------------

fn home() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Asks PowerShell where its profile is: Documents is often redirected (OneDrive), so
/// guessing the path would put the hook where PowerShell never looks.
fn ps_profile(exe: &str) -> Option<PathBuf> {
    let out = Command::new(exe)
        .args(["-NoProfile", "-NonInteractive", "-Command", "$PROFILE.CurrentUserCurrentHost"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let p = PathBuf::from(line);
    p.is_absolute().then_some(p)
}

/// The profile files `shell` reads, as `(label, path)`.
pub fn targets(shell: Shell) -> Result<Vec<(String, PathBuf)>, String> {
    match shell {
        Shell::PowerShell => {
            let found: Vec<(String, PathBuf)> = [("Windows PowerShell", "powershell"), ("PowerShell 7", "pwsh")]
                .into_iter()
                .filter_map(|(label, exe)| ps_profile(exe).map(|p| (label.to_string(), p)))
                .fold(Vec::new(), |mut acc, item| {
                    if !acc.iter().any(|(_, p)| *p == item.1) {
                        acc.push(item);
                    }
                    acc
                });
            if found.is_empty() {
                Err("couldn't find PowerShell to ask where its profile is. Use --profile <file> to name it.".into())
            } else {
                Ok(found)
            }
        }
        Shell::Bash => {
            let name = if cfg!(target_os = "macos") { ".bash_profile" } else { ".bashrc" };
            let h = home().ok_or("can't find your home folder. Use --profile <file> to name the file.")?;
            Ok(vec![("bash".into(), h.join(name))])
        }
        Shell::Zsh => {
            let dir = std::env::var_os("ZDOTDIR").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(home);
            let d = dir.ok_or("can't find your home folder. Use --profile <file> to name the file.")?;
            Ok(vec![("zsh".into(), d.join(".zshrc"))])
        }
    }
}

/// PowerShell won't load a profile under Restricted or AllSigned, so the hook would
/// quietly do nothing. Say so, and say what the user can run themselves.
fn execution_policy_warning() -> Option<String> {
    if !cfg!(windows) {
        return None;
    }
    let out = Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", "Get-ExecutionPolicy"]).output().ok()?;
    let policy = String::from_utf8_lossy(&out.stdout).trim().to_string();
    matches!(policy.as_str(), "Restricted" | "AllSigned").then(|| {
        format!("PowerShell's execution policy is {policy}, so it won't load your profile and the hook will do nothing. To allow your own profile, run: Set-ExecutionPolicy -Scope CurrentUser RemoteSigned")
    })
}

// ---- commands ----------------------------------------------------------------------------

fn files_for(shell: Shell, profile: Option<&Path>) -> Result<Vec<(String, PathBuf)>, String> {
    match profile {
        Some(p) => Ok(vec![("profile".into(), p.to_path_buf())]),
        None => targets(shell),
    }
}

pub fn install(shell: &str, profile: Option<&Path>) -> Res {
    let shell = Shell::parse(shell)?;
    for (label, path) in files_for(shell, profile)? {
        match install_into(&path, shell)? {
            Outcome::Added => println!("Added the hook to {label}: {}", path.display()),
            Outcome::Updated => println!("Refreshed the hook in {label}: {}", path.display()),
            _ => println!("The hook is already in {label}: {}", path.display()),
        }
        let backup = backup_path(&path);
        if backup.exists() {
            println!("  Your original is saved as {}", backup.display());
        }
    }
    if shell == Shell::PowerShell && profile.is_none() {
        if let Some(w) = execution_policy_warning() {
            println!("\nNote: {w}");
        }
    }
    println!("\nOpen a new terminal to start using it. When a command fails, it prints one line offering `bb why`.");
    println!("It never runs anything for you. `bb hook uninstall {}` removes it.", shell.name());
    Ok(())
}

pub fn uninstall(shell: &str, profile: Option<&Path>) -> Res {
    let shell = Shell::parse(shell)?;
    for (label, path) in files_for(shell, profile)? {
        match uninstall_from(&path)? {
            Outcome::Removed => println!("Removed the hook from {label}: {}", path.display()),
            _ => println!("No hook in {label}: {}", path.display()),
        }
    }
    println!("Open a new terminal for it to take effect.");
    Ok(())
}

pub fn status() -> Res {
    println!("Shell hook:");
    for shell in [Shell::PowerShell, Shell::Bash, Shell::Zsh] {
        let list = match targets(shell) {
            Ok(l) => l,
            Err(_) if shell == Shell::PowerShell => continue, // no PowerShell here, nothing to report
            Err(e) => {
                println!("  {:<11} {e}", shell.name());
                continue;
            }
        };
        for (label, path) in list {
            let state = match read_profile(&path) {
                Ok(None) => "not installed (no profile file)".to_string(),
                Ok(Some((text, _))) => match has_block(&text) {
                    Ok(true) => "installed".to_string(),
                    Ok(false) => "not installed".to_string(),
                    Err(_) => "DAMAGED block, fix by hand".to_string(),
                },
                Err(e) => e,
            };
            println!("  {:<11} {state}  ({label}: {})", shell.name(), path.display());
        }
    }
    if let Some(w) = execution_policy_warning() {
        println!("\nNote: {w}");
    }
    Ok(())
}

/// Called by the hook after a command fails. It must never print or fail: a broken `bb`
/// must not break anyone's shell, so every problem is swallowed.
pub fn record(db: &Path, exit: i32, cmd: &str, cwd: Option<&str>) -> Res {
    let cmd: String = cmd.chars().take(MAX_CMD_CHARS).collect();
    let trimmed = cmd.trim();
    if trimmed.is_empty() || exit == 0 || trimmed == "bb" || trimmed.starts_with("bb ") {
        return Ok(());
    }
    let Ok(store) = Store::open(db) else { return Ok(()) };
    let _ = store.insert_failure(&Failure {
        id: 0,
        ts: chrono::Local::now().timestamp(),
        command: trimmed.to_string(),
        exit_code: exit,
        cwd: cwd.map(str::to_string),
        output: None,
        duration_ms: None,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: &str = "# >>> blackbox hook >>>\nline\n# <<< blackbox hook <<<";

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bb-hook-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn add_then_remove_restores_the_original_exactly() {
        for original in ["", "alias a=b\nexport X=1\n", "one\r\ntwo\r\n", "\n\nx\n\n"] {
            let with = add_block(original, BLOCK).unwrap();
            assert!(has_block(&with).unwrap());
            assert!(with.starts_with(original), "their content must stay first and untouched");
            assert_eq!(remove_block(&with).unwrap(), original, "round trip for {original:?}");
        }
    }

    #[test]
    fn a_profile_without_a_final_newline_only_gains_that_newline() {
        let with = add_block("export X=1", BLOCK).unwrap();
        assert_eq!(remove_block(&with).unwrap(), "export X=1\n");
    }

    #[test]
    fn adding_twice_changes_nothing_the_second_time() {
        let once = add_block("a\n", BLOCK).unwrap();
        assert_eq!(add_block(&once, BLOCK).unwrap(), once);
    }

    #[test]
    fn a_changed_block_is_replaced_in_place_and_surroundings_stay() {
        let old = "before\n\n# >>> blackbox hook >>>\nOLD\n# <<< blackbox hook <<<\nafter\n";
        let got = add_block(old, BLOCK).unwrap();
        assert_eq!(got, "before\n\n# >>> blackbox hook >>>\nline\n# <<< blackbox hook <<<\nafter\n");
    }

    #[test]
    fn a_damaged_block_is_never_touched() {
        for bad in [
            "x\n# >>> blackbox hook >>>\nno end\n",
            "x\n# <<< blackbox hook <<<\n",
            "# <<< blackbox hook <<<\n# >>> blackbox hook >>>\n",
            "# >>> blackbox hook >>>\n# <<< blackbox hook <<<\n# >>> blackbox hook >>>\n# <<< blackbox hook <<<\n",
        ] {
            assert!(add_block(bad, BLOCK).is_err(), "{bad:?}");
            assert!(remove_block(bad).is_err(), "{bad:?}");
            assert!(has_block(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn crlf_profiles_stay_crlf() {
        let with = add_block("a\r\nb\r\n", BLOCK).unwrap();
        assert!(!with.replace("\r\n", "").contains('\n'), "no bare LF may appear in a CRLF file");
    }

    #[test]
    fn install_and_uninstall_a_real_file_with_backup_and_other_content_kept() {
        let d = tmp("file");
        let p = d.join("profile.ps1");
        std::fs::write(&p, "Set-Alias g git\nfunction hi { 'hi' }\n").unwrap();

        assert_eq!(install_into(&p, Shell::Bash).unwrap(), Outcome::Added);
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.starts_with("Set-Alias g git\nfunction hi { 'hi' }\n"));
        assert!(after.contains(START) && after.contains("bb hook record"));
        assert_eq!(std::fs::read_to_string(backup_path(&p)).unwrap(), "Set-Alias g git\nfunction hi { 'hi' }\n");

        // Second install: nothing changes, backup keeps the true original.
        assert_eq!(install_into(&p, Shell::Bash).unwrap(), Outcome::Unchanged);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), after);

        assert_eq!(uninstall_from(&p).unwrap(), Outcome::Removed);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "Set-Alias g git\nfunction hi { 'hi' }\n");
        assert_eq!(uninstall_from(&p).unwrap(), Outcome::Unchanged);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_profile_that_only_held_our_block_is_deleted_on_uninstall() {
        let d = tmp("fresh");
        let p = d.join("sub").join("profile.ps1");
        assert_eq!(install_into(&p, Shell::PowerShell).unwrap(), Outcome::Added);
        assert!(p.exists());
        assert!(!backup_path(&p).exists(), "there was nothing to back up");
        uninstall_from(&p).unwrap();
        assert!(!p.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_byte_order_mark_is_kept_and_utf16_is_refused() {
        let d = tmp("bom");
        let p = d.join("p.ps1");
        std::fs::write(&p, "\u{feff}x\n").unwrap();
        install_into(&p, Shell::Bash).unwrap();
        assert!(std::fs::read(&p).unwrap().starts_with(&[0xEF, 0xBB, 0xBF]));
        uninstall_from(&p).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), "\u{feff}x\n".as_bytes());

        let u16 = d.join("u16.ps1");
        std::fs::write(&u16, [0xFF, 0xFE, b'x', 0, 0xC3, 0x28]).unwrap();
        assert!(install_into(&u16, Shell::Bash).is_err());
        assert_eq!(std::fs::read(&u16).unwrap(), [0xFF, 0xFE, b'x', 0, 0xC3, 0x28], "refused files are left alone");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_damaged_file_on_disk_is_left_untouched_by_install_and_uninstall() {
        let d = tmp("dmg");
        let p = d.join("p");
        let bad = "keep\n# >>> blackbox hook >>>\nno end\n";
        std::fs::write(&p, bad).unwrap();
        assert!(install_into(&p, Shell::Zsh).is_err());
        assert!(uninstall_from(&p).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), bad);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn every_script_is_delimited_records_and_never_runs_a_diagnosis_or_fix() {
        for shell in [Shell::PowerShell, Shell::Bash, Shell::Zsh] {
            let b = block_for(shell);
            assert_eq!(b.lines().next(), Some(START));
            assert_eq!(b.lines().last(), Some(END));
            assert!(b.contains("bb hook record"));
            // Mentions `bb why` only inside the message it prints.
            for line in b.lines().filter(|l| l.contains("bb why")) {
                assert!(line.contains("Run ") && (line.contains("echo") || line.contains("print") || line.contains("Write-Host")), "{line}");
            }
            assert!(!b.contains("bb fix"));
            assert!(b.is_ascii(), "non-ASCII breaks PowerShell 5.1 reading a BOM-less profile");
        }
    }

    #[test]
    fn shell_names_parse() {
        assert_eq!(Shell::parse("PowerShell").unwrap(), Shell::PowerShell);
        assert_eq!(Shell::parse("pwsh").unwrap(), Shell::PowerShell);
        assert_eq!(Shell::parse("zsh").unwrap(), Shell::Zsh);
        assert!(Shell::parse("fish").is_err());
    }

    #[test]
    fn record_stores_a_failure_and_skips_noise() {
        let d = tmp("rec");
        let db = d.join("bb.db");
        record(&db, 2, "make test", Some("C:\\src")).unwrap();
        record(&db, 0, "ls", None).unwrap(); // succeeded: not a failure
        record(&db, 1, "bb why", None).unwrap(); // our own commands
        record(&db, 1, "   ", None).unwrap();
        let got = Store::open(&db).unwrap().recent_failures(10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].command.as_str(), got[0].exit_code, got[0].cwd.as_deref()), ("make test", 2, Some("C:\\src")));
        assert!(got[0].output.is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_unreadable_store_is_silent_and_successful() {
        let d = tmp("bad");
        // The "database" is a directory, and another is a file used as a folder.
        assert!(record(&d, 1, "make", None).is_ok());
        let file = d.join("f");
        std::fs::write(&file, b"x").unwrap();
        assert!(record(&file.join("bb.db"), 1, "make", None).is_ok());
        let junk = d.join("junk.db");
        std::fs::write(&junk, b"not a database".repeat(100)).unwrap();
        assert!(record(&junk, 1, "make", None).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn overlong_command_lines_are_cut() {
        let d = tmp("long");
        let db = d.join("bb.db");
        record(&db, 1, &"x".repeat(MAX_CMD_CHARS * 2), None).unwrap();
        let got = Store::open(&db).unwrap().recent_failures(1).unwrap();
        assert_eq!(got[0].command.chars().count(), MAX_CMD_CHARS);
        let _ = std::fs::remove_dir_all(&d);
    }
}
