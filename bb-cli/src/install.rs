//! Background recording (`start` / `stop`) and one-command setup (`install` / `uninstall`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use sysinfo::{Pid, ProcessesToUpdate, System};

type Res = Result<(), String>;

// ---- recorder process tracking -------------------------------------------------

pub fn pid_path(db: &Path) -> PathBuf {
    db.with_extension("pid")
}

/// Pid of a live `bb` recorder for this database, if any.
pub fn running_pid(db: &Path) -> Option<u32> {
    let pid: u32 = fs::read_to_string(pid_path(db)).ok()?.trim().parse().ok()?;
    if pid == std::process::id() {
        return None;
    }
    let p = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::Some(&[p]), true);
    // Check the name too, so a reused pid doesn't look like a recorder.
    let name = sys.process(p)?.name().to_string_lossy().to_lowercase();
    name.starts_with("bb").then_some(pid)
}

/// Held by a running recorder; removes the pid file when dropped.
pub struct PidGuard(PathBuf);

impl PidGuard {
    pub fn acquire(db: &Path) -> Result<PidGuard, String> {
        if let Some(pid) = running_pid(db) {
            return Err(format!("already recording (pid {pid}). Use `bb stop` first."));
        }
        let path = pid_path(db);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(&path, std::process::id().to_string()).map_err(|e| format!("can't write {}: {e}", path.display()))?;
        Ok(PidGuard(path))
    }
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Starts the recorder in the background and returns once it is up.
pub fn start(db: &Path, run_args: &[String], quiet: bool) -> Res {
    if let Some(pid) = running_pid(db) {
        if !quiet {
            println!("Already recording (pid {pid}).");
        }
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.arg("--db").arg(db).arg("run").args(run_args);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    detach(&mut cmd);
    cmd.spawn().map_err(|e| format!("couldn't start the recorder: {e}"))?;

    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(100));
        if let Some(pid) = running_pid(db) {
            if !quiet {
                println!("Recording in the background (pid {pid}). Stop it with `bb stop`.");
            }
            return Ok(());
        }
    }
    Err("the recorder didn't start. Try `bb run` to see the error.".into())
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

pub fn stop(db: &Path, quiet: bool) -> Res {
    let Some(pid) = running_pid(db) else {
        if !quiet {
            println!("The recorder isn't running.");
        }
        return Ok(());
    };
    let status = if cfg!(windows) {
        Command::new("taskkill").args(["/PID", &pid.to_string(), "/F"]).stdout(Stdio::null()).stderr(Stdio::null()).status()
    } else {
        Command::new("kill").arg(pid.to_string()).status()
    };
    match status {
        Ok(s) if s.success() => {}
        _ => return Err(format!("couldn't stop pid {pid}")),
    }
    for _ in 0..30 {
        if running_pid(db).is_none() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = fs::remove_file(pid_path(db));
    if !quiet {
        println!("Stopped the recorder (pid {pid}).");
    }
    Ok(())
}

// ---- PATH editing (pure, so it can be tested) ----------------------------------

fn norm(entry: &str) -> String {
    entry.trim().trim_end_matches(['\\', '/']).to_lowercase()
}

/// Returns the new PATH with `dir` appended, or None if it is already there.
pub fn path_with(existing: &str, dir: &str) -> Option<String> {
    if existing.split(';').any(|e| norm(e) == norm(dir)) {
        return None;
    }
    let base = existing.trim_end_matches(';');
    Some(if base.is_empty() { dir.to_string() } else { format!("{base};{dir}") })
}

/// Returns the new PATH without `dir`, or None if it wasn't there.
pub fn path_without(existing: &str, dir: &str) -> Option<String> {
    let parts: Vec<&str> = existing.split(';').collect();
    let kept: Vec<&str> = parts.iter().copied().filter(|e| norm(e) != norm(dir)).collect();
    (kept.len() != parts.len()).then(|| kept.join(";"))
}

// ---- install / uninstall -------------------------------------------------------

#[cfg(windows)]
pub fn install_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("blackbox")
        .join("bin")
}

#[cfg(windows)]
mod win {
    use super::*;
    use winreg::enums::{RegType, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
    use winreg::{RegKey, RegValue};

    pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    pub const RUN_NAME: &str = "blackbox";

    fn open(sub: &str) -> Result<RegKey, String> {
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(sub, KEY_READ | KEY_WRITE)
            .map_err(|e| format!("can't open registry key {sub}: {e}"))
    }

    fn decode(bytes: &[u8]) -> String {
        let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units).trim_end_matches('\0').to_string()
    }

    /// Reads the user PATH without expanding %VARS%, so rewriting it loses nothing.
    fn read_path(k: &RegKey) -> (String, RegType) {
        match k.get_raw_value("Path") {
            Ok(v) => (decode(&v.bytes), v.vtype),
            Err(_) => (String::new(), RegType::REG_EXPAND_SZ),
        }
    }

    fn write_path(k: &RegKey, s: &str, vtype: RegType) -> Res {
        let bytes: Vec<u8> = s.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect();
        k.set_raw_value("Path", &RegValue { bytes: bytes.into(), vtype })
            .map_err(|e| format!("couldn't update PATH: {e}"))
    }

    /// Tells running programs (Explorer, new terminals) that the environment changed.
    fn broadcast() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
        };
        let env: Vec<u16> = "Environment\0".encode_utf16().collect();
        unsafe {
            SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, env.as_ptr() as isize, SMTO_ABORTIFHUNG, 3000, std::ptr::null_mut());
        }
    }

    pub fn add_to_path(dir: &Path) -> Result<bool, String> {
        let k = open("Environment")?;
        let (cur, vt) = read_path(&k);
        match path_with(&cur, &dir.to_string_lossy()) {
            Some(new) => {
                write_path(&k, &new, vt)?;
                broadcast();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub fn remove_from_path(dir: &Path) -> Result<bool, String> {
        let k = open("Environment")?;
        let (cur, vt) = read_path(&k);
        match path_without(&cur, &dir.to_string_lossy()) {
            Some(new) => {
                write_path(&k, &new, vt)?;
                broadcast();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub fn set_autostart(exe: &Path) -> Res {
        open(RUN_KEY)?
            .set_value(RUN_NAME, &format!("\"{}\" start --quiet", exe.display()))
            .map_err(|e| format!("couldn't set up start at login: {e}"))
    }

    pub fn clear_autostart() -> Result<bool, String> {
        Ok(open(RUN_KEY)?.delete_value(RUN_NAME).is_ok())
    }
}

#[cfg(windows)]
pub fn install(db: &Path, no_autostart: bool) -> Res {
    let dir = install_dir();
    let dest = dir.join("bb.exe");
    let me = std::env::current_exe().map_err(|e| e.to_string())?;

    fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let same = fs::canonicalize(&me).ok() == fs::canonicalize(&dest).ok();
    if !same {
        // An installed recorder holds its exe open, so stop it before replacing.
        stop(db, true)?;
        fs::copy(&me, &dest).map_err(|e| format!("can't copy to {}: {e}", dest.display()))?;
    }
    println!("Installed:        {}", dest.display());
    match win::add_to_path(&dir)? {
        true => println!("PATH:             added (open a new terminal to use `bb`)"),
        false => println!("PATH:             already set"),
    }
    if no_autostart {
        println!("Start at login:   skipped");
    } else {
        win::set_autostart(&dest)?;
        println!("Start at login:   on");
    }
    // Start from the installed copy so the original can be deleted.
    let status = Command::new(&dest).arg("--db").arg(db).args(["start"]).status().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("installed, but the recorder failed to start".into());
    }
    println!("\nDone. Try `bb status` in a new terminal, and `bb why \"10m ago\"` when something feels slow.");
    Ok(())
}

#[cfg(windows)]
pub fn uninstall(db: &Path, purge: bool) -> Res {
    let dir = install_dir();
    stop(db, true)?;
    if win::clear_autostart()? {
        println!("Start at login:   removed");
    }
    if win::remove_from_path(&dir)? {
        println!("PATH:             removed");
    }
    if purge {
        for ext in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(format!("{}{}", db.display(), ext));
        }
        println!("Recorded data:    deleted");
    } else {
        println!("Recorded data:    kept at {} (use --purge to delete)", db.display());
    }
    // A running exe can't delete itself, so hand the cleanup to a short-lived shell.
    let exe = dir.join("bb.exe");
    if exe.exists() {
        let _ = Command::new("cmd")
            .args(["/C", &format!("ping -n 3 127.0.0.1 >nul & del /f /q \"{}\" & rmdir \"{}\"", exe.display(), dir.display())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    println!("Uninstalled.");
    Ok(())
}

// ---- Linux and macOS ---------------------------------------------------------------

/// The XDG autostart entry that starts the recorder when a desktop session begins.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=blackbox\nComment=Background performance recorder\nExec=\"{}\" start --quiet\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
        exe.display()
    )
}

/// The macOS LaunchAgent that starts the recorder at login. `bb start` returns at once and
/// leaves the recorder running, so launchd must not clean up the process group.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn launch_agent_plist(exe: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>{LAUNCH_LABEL}</string>\n  <key>ProgramArguments</key>\n  <array><string>{}</string><string>start</string><string>--quiet</string></array>\n  <key>RunAtLoad</key><true/>\n  <key>AbandonProcessGroup</key><true/>\n</dict>\n</plist>\n",
        exe.display()
    )
}

#[cfg(unix)]
const LAUNCH_LABEL: &str = "io.github.anakinskywalker0.blackbox";
#[cfg(not(unix))]
#[allow(dead_code)]
const LAUNCH_LABEL: &str = "io.github.anakinskywalker0.blackbox";

#[cfg(unix)]
fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from).ok_or_else(|| "HOME isn't set".to_string())
}

/// Where `bb install` puts the program (the usual per-user bin folder).
#[cfg(unix)]
pub fn install_dir() -> PathBuf {
    home().unwrap_or_else(|_| PathBuf::from(".")).join(".local").join("bin")
}

#[cfg(target_os = "macos")]
fn autostart_file(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents").join(format!("{LAUNCH_LABEL}.plist"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn autostart_file(home: &Path) -> PathBuf {
    home.join(".config/autostart/blackbox.desktop")
}

#[cfg(unix)]
fn autostart_contents(exe: &Path) -> String {
    if cfg!(target_os = "macos") { launch_agent_plist(exe) } else { desktop_entry(exe) }
}

/// Best effort: tells launchd about the agent now, so it needn't wait for the next login.
#[cfg(target_os = "macos")]
fn launchctl(verb: &str, plist: &Path) {
    let uid = Command::new("id").arg("-u").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    let _ = Command::new("launchctl").args([verb, &format!("gui/{uid}")]).arg(plist).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

#[cfg(unix)]
pub fn install(db: &Path, no_autostart: bool) -> Res {
    use std::os::unix::fs::PermissionsExt;
    let home = home()?;
    let dir = install_dir();
    let dest = dir.join("bb");
    let me = std::env::current_exe().map_err(|e| e.to_string())?;

    fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    if fs::canonicalize(&me).ok() != fs::canonicalize(&dest).ok() {
        stop(db, true)?;
        // Write beside the target and rename, so a running copy is never half-overwritten.
        let tmp = dir.join("bb.new");
        fs::copy(&me, &tmp).map_err(|e| format!("can't copy to {}: {e}", dir.display()))?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
        fs::rename(&tmp, &dest).map_err(|e| format!("can't install to {}: {e}", dest.display()))?;
    }
    println!("Installed:        {}", dest.display());

    let on_path = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d == dir));
    if on_path {
        println!("PATH:             already set");
    } else {
        println!("PATH:             {} is not on your PATH yet. Add this to your shell profile:", dir.display());
        println!("                  export PATH=\"$HOME/.local/bin:$PATH\"");
    }

    if no_autostart {
        println!("Start at login:   skipped");
    } else {
        let file = autostart_file(&home);
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("can't create {}: {e}", parent.display()))?;
        }
        fs::write(&file, autostart_contents(&dest)).map_err(|e| format!("can't write {}: {e}", file.display()))?;
        #[cfg(target_os = "macos")]
        launchctl("bootstrap", &file);
        println!("Start at login:   on ({})", file.display());
    }
    let status = Command::new(&dest).arg("--db").arg(db).arg("start").status().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("installed, but the recorder failed to start".into());
    }
    println!("\nDone. Try `bb status`, and `bb why \"10m ago\"` when something feels slow.");
    Ok(())
}

#[cfg(unix)]
pub fn uninstall(db: &Path, purge: bool) -> Res {
    let home = home()?;
    stop(db, true)?;
    let file = autostart_file(&home);
    if file.exists() {
        #[cfg(target_os = "macos")]
        launchctl("bootout", &file);
        fs::remove_file(&file).map_err(|e| format!("can't remove {}: {e}", file.display()))?;
        println!("Start at login:   removed");
    }
    let exe = install_dir().join("bb");
    if exe.exists() {
        fs::remove_file(&exe).map_err(|e| format!("can't remove {}: {e}", exe.display()))?;
        println!("Program:          removed ({})", exe.display());
    }
    if purge {
        for ext in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(format!("{}{}", db.display(), ext));
        }
        println!("Recorded data:    deleted");
    } else {
        println!("Recorded data:    kept at {} (use --purge to delete)", db.display());
    }
    println!("Uninstalled.");
    Ok(())
}

#[cfg(not(any(windows, unix)))]
pub fn install(_db: &Path, _no_autostart: bool) -> Res {
    Err("`install` isn't supported on this platform. Copy bb onto your PATH and run `bb start`.".into())
}

#[cfg(not(any(windows, unix)))]
pub fn uninstall(_db: &Path, _purge: bool) -> Res {
    Err("`uninstall` isn't supported on this platform. Run `bb stop` and delete the bb binary.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIR: &str = r"C:\Users\me\AppData\Local\blackbox\bin";

    #[test]
    fn linux_autostart_entry_is_valid() {
        let e = desktop_entry(Path::new("/home/me/.local/bin/bb"));
        assert!(e.starts_with("[Desktop Entry]\nType=Application\n"));
        assert!(e.contains("Exec=\"/home/me/.local/bin/bb\" start --quiet\n"));
        assert!(e.contains("Terminal=false"));
    }

    #[test]
    fn macos_launch_agent_is_valid() {
        let p = launch_agent_plist(Path::new("/Users/me/.local/bin/bb"));
        assert!(p.contains("<key>Label</key><string>io.github.anakinskywalker0.blackbox</string>"));
        assert!(p.contains("<string>/Users/me/.local/bin/bb</string><string>start</string><string>--quiet</string>"));
        assert!(p.contains("<key>RunAtLoad</key><true/>"));
        assert!(p.contains("<key>AbandonProcessGroup</key><true/>"));
        assert!(p.trim_end().ends_with("</plist>"));
    }

    #[test]
    fn adds_once() {
        let p = path_with(r"C:\a;C:\b", DIR).unwrap();
        assert_eq!(p, format!(r"C:\a;C:\b;{DIR}"));
        assert!(path_with(&p, DIR).is_none());
    }

    #[test]
    fn add_ignores_case_and_trailing_slash() {
        let existing = r"C:\a;c:\users\ME\appdata\local\blackbox\bin\";
        assert!(path_with(existing, DIR).is_none());
    }

    #[test]
    fn add_to_empty_and_trailing_semicolon() {
        assert_eq!(path_with("", DIR).unwrap(), DIR);
        assert_eq!(path_with(r"C:\a;", DIR).unwrap(), format!(r"C:\a;{DIR}"));
    }

    #[test]
    fn keeps_unexpanded_entries() {
        let p = path_with(r"%SystemRoot%\system32;%USERPROFILE%\bin", DIR).unwrap();
        assert!(p.starts_with(r"%SystemRoot%\system32;%USERPROFILE%\bin;"));
    }

    #[test]
    fn removes_only_ours() {
        let p = format!(r"C:\a;{DIR};C:\b");
        assert_eq!(path_without(&p, DIR).unwrap(), r"C:\a;C:\b");
        assert!(path_without(r"C:\a;C:\b", DIR).is_none());
    }

    #[test]
    fn pid_guard_blocks_second_recorder_and_cleans_up() {
        let db = std::env::temp_dir().join(format!("bbtest-{}.db", std::process::id()));
        {
            let _g = PidGuard::acquire(&db).unwrap();
            assert!(pid_path(&db).exists());
        }
        assert!(!pid_path(&db).exists());
    }
}
