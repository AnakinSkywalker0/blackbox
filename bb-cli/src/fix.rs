//! `bb fix`: turn a diagnosis into safe, reversible actions, and measure the effect.
//!
//! Three kinds of action, none needing administrator rights:
//!   * lower the priority of a program that is hogging the CPU (so everything else stays responsive),
//!   * leave the Power saver plan when plugged in,
//!   * turn off a start-at-login entry (the same switch Task Manager uses).
//!
//! Every action is asked about one at a time, recorded in `fixes.json` beside the database, and
//! undone with `bb fix --undo`. Nothing is ever closed, deleted or uninstalled.

use bb_core::model::{ProcRow, Sample};
use bb_core::sampler::Sampler;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

type Res = Result<(), String>;

/// Programs that must never be touched, whatever they are doing.
const PROTECTED: &[&str] = &[
    "system", "system idle process", "registry", "memory compression", "smss.exe", "csrss.exe", "wininit.exe",
    "winlogon.exe", "services.exe", "lsass.exe", "svchost.exe", "dwm.exe", "explorer.exe", "fontdrvhost.exe",
    "audiodg.exe", "bb.exe", "bb", "bb-gui.exe", "bb-gui", "msmpeng.exe",
];
/// A program must average at least this much of the whole machine's CPU to be worth lowering.
const HOG_PCT: f32 = 15.0;
const BELOW_NORMAL: u32 = 0x4000;
const NORMAL: u32 = 0x20;

pub const POWER_SAVER: &str = "a1841308-3541-4fab-bc81-f71556f20b4a";
pub const BALANCED: &str = "381b4222-f694-41f0-9685-ff5bb260df2e";

// ---- what can be done, and the record of having done it ---------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Action {
    /// Every process of the program, with the priority class each had before.
    Priority { name: String, pids: Vec<(u32, u32)> },
    PowerPlan { old_guid: String, old_name: String, new_guid: String, new_name: String },
    Startup { name: String },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Measure {
    pub cpu_pct: f32,
    pub freq_pct: Option<f32>,
    /// Program name and its average share of total CPU, biggest first.
    pub top: Vec<(String, f32)>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Batch {
    pub ts: i64,
    pub actions: Vec<Action>,
    pub before: Measure,
    pub after: Option<Measure>,
}

fn log_path(db: &Path) -> PathBuf {
    db.with_file_name("fixes.json")
}

fn read_log(db: &Path) -> Vec<Batch> {
    std::fs::read_to_string(log_path(db)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn write_log(db: &Path, log: &[Batch]) -> Res {
    let text = serde_json::to_string_pretty(log).map_err(|e| e.to_string())?;
    std::fs::write(log_path(db), text).map_err(|e| format!("can't write {}: {e}", log_path(db).display()))
}

// ---- pure decisions (tested) ----------------------------------------------------------------------

pub fn is_protected(name: &str) -> bool {
    let n = name.to_lowercase();
    PROTECTED.contains(&n.as_str())
}

/// Programs worth lowering: heavy, and not on the protected list. Biggest first, at most two.
pub fn pick_hogs(top: &[(String, f32)]) -> Vec<(String, f32)> {
    top.iter().filter(|(n, c)| *c >= HOG_PCT && !is_protected(n)).take(2).cloned().collect()
}

/// Parses `Power Scheme GUID: 381b4222-...  (Balanced)` from `powercfg /getactivescheme`.
pub fn parse_power_scheme(out: &str) -> Option<(String, String)> {
    let after = out.split("GUID:").nth(1)?.trim();
    let guid: String = after.chars().take_while(|c| c.is_ascii_hexdigit() || *c == '-').collect();
    if guid.len() != 36 {
        return None;
    }
    let name = after.split_once('(').and_then(|(_, r)| r.split_once(')')).map(|(n, _)| n.trim().to_string()).unwrap_or_default();
    Some((guid.to_lowercase(), name))
}

/// Leaving Power saver only makes sense when plugged in; on battery it is doing its job.
pub fn plan_power(active: Option<(String, String)>, on_ac: Option<bool>) -> Option<Action> {
    let (guid, name) = active?;
    (on_ac == Some(true) && guid == POWER_SAVER).then(|| Action::PowerPlan {
        old_guid: guid,
        old_name: name,
        new_guid: BALANCED.into(),
        new_name: "Balanced".into(),
    })
}

/// The 12-byte value Windows keeps under `StartupApproved\Run`: first byte 02 = on, 03 = off.
pub fn startup_approved(enabled: bool) -> Vec<u8> {
    let mut v = vec![if enabled { 2 } else { 3 }, 0, 0, 0];
    v.extend_from_slice(&[0u8; 8]);
    v
}

pub fn describe_delta(before: &Measure, after: &Measure) -> Vec<String> {
    let mut out = vec![format!("whole machine CPU: {:.0}% -> {:.0}%", before.cpu_pct, after.cpu_pct)];
    if let (Some(a), Some(b)) = (before.freq_pct, after.freq_pct) {
        out.push(format!("CPU speed: {a:.0}% -> {b:.0}% of rated maximum"));
    }
    for (name, was) in before.top.iter().take(2) {
        let now = after.top.iter().find(|(n, _)| n == name).map_or(0.0, |(_, c)| *c);
        out.push(format!("{name}: {was:.0}% -> {now:.0}% of the CPU"));
    }
    out
}

// ---- measuring --------------------------------------------------------------------------------------

/// Averages the machine over `secs` seconds of live sampling.
fn measure(secs: u64) -> Measure {
    let mut sampler = Sampler::new(8);
    let samples: Vec<Sample> = (0..secs.max(2)).map(|i| sampler.sample_after(Duration::from_secs(1), i as i64)).collect();
    let n = samples.len().max(1) as f32;
    let freq: Vec<f32> = samples.iter().filter_map(|s| s.sensors.freq_pct).collect();
    let mut by_name: Vec<(String, f32)> = Vec::new();
    for row in samples.iter().flat_map(|s| s.procs.iter()) {
        add_row(&mut by_name, row, n);
    }
    by_name.sort_by(|a, b| b.1.total_cmp(&a.1));
    Measure {
        cpu_pct: samples.iter().map(|s| s.cpu_pct).sum::<f32>() / n,
        freq_pct: (!freq.is_empty()).then(|| freq.iter().sum::<f32>() / freq.len() as f32),
        top: by_name,
    }
}

fn add_row(acc: &mut Vec<(String, f32)>, row: &ProcRow, n: f32) {
    match acc.iter_mut().find(|(name, _)| *name == row.name) {
        Some(e) => e.1 += row.cpu_pct / n,
        None => acc.push((row.name.clone(), row.cpu_pct / n)),
    }
}

// ---- the command ------------------------------------------------------------------------------------

struct Proposal {
    action: Action,
    /// One line saying what happens, and why.
    text: String,
    /// Only applied when asked for by name, because it can't be judged from measurements.
    optional: bool,
}

fn ask(question: &str) -> bool {
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).is_ok() && matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

pub fn run(db: &Path, undo: bool, yes: bool, dry_run: bool, only: &[String]) -> Res {
    if !platform::SUPPORTED {
        return Err("`bb fix` only works on Windows so far. Nothing was changed.".into());
    }
    if undo {
        return undo_last(db);
    }
    println!("Watching the machine for 8 seconds...");
    let before = measure(8);

    let mut proposals: Vec<Proposal> = Vec::new();
    for (name, cpu) in pick_hogs(&before.top) {
        let pids: Vec<(u32, u32)> = platform::pids_named(&name).into_iter().filter(|(_, class)| *class == NORMAL).collect();
        if !pids.is_empty() {
            proposals.push(Proposal {
                text: format!("Lower the priority of {name} ({} process(es)); together they used {cpu:.0}% of the CPU, so other programs stay responsive", pids.len()),
                action: Action::Priority { name: name.clone(), pids },
                optional: false,
            });
        }
    }
    if let Some(a) = plan_power(platform::active_power_plan(), platform::on_ac()) {
        proposals.push(Proposal { action: a, text: "Switch from Power saver to Balanced; you're plugged in, and Power saver caps CPU speed".into(), optional: false });
    }
    for name in platform::enabled_startup_items() {
        proposals.push(Proposal { action: Action::Startup { name: name.clone() }, text: format!("Don't start \"{name}\" at login (you can turn it back on)"), optional: true });
    }
    let wanted = |p: &Proposal| match only.is_empty() {
        true => !p.optional,
        false => only.iter().any(|o| match &p.action {
            Action::Priority { .. } => o == "priority",
            Action::PowerPlan { .. } => o == "power",
            Action::Startup { .. } => o == "startup",
        }),
    };
    proposals.retain(wanted);

    if proposals.is_empty() {
        println!("Nothing worth changing right now: no program is hogging the CPU, and the power plan is fine.");
        println!("Tip: `bb fix --only startup` lists start-at-login entries you can turn off.");
        return Ok(());
    }
    println!("\nProposed changes (none need administrator rights; `bb fix --undo` reverses them):");
    for (i, p) in proposals.iter().enumerate() {
        println!("  {}. {}", i + 1, p.text);
    }
    if dry_run {
        println!("\nDry run: nothing was changed.");
        return Ok(());
    }

    let mut done: Vec<Action> = Vec::new();
    for p in &proposals {
        if !yes && !ask(&format!("\nApply: {}?", p.text)) {
            continue;
        }
        match platform::apply(&p.action) {
            Ok(()) => done.push(p.action.clone()),
            Err(e) => println!("  couldn't apply that: {e}"),
        }
    }
    if done.is_empty() {
        println!("Nothing was changed.");
        return Ok(());
    }
    println!("\nApplied {} change(s). Measuring again for 8 seconds...", done.len());
    std::thread::sleep(Duration::from_secs(2));
    let after = measure(8);
    println!("\nBefore -> after:");
    for line in describe_delta(&before, &after) {
        println!("  {line}");
    }
    println!("\nThe numbers depend on what you were doing in those two windows. Lowering priority shifts CPU");
    println!("toward your other programs; it doesn't make the busy program use less. Power plan changes show");
    println!("up in CPU speed. Start-at-login changes take effect next time you sign in.");
    let mut log = read_log(db);
    log.push(Batch { ts: chrono::Local::now().timestamp(), actions: done, before, after: Some(after) });
    write_log(db, &log)
}

fn undo_last(db: &Path) -> Res {
    let mut log = read_log(db);
    let Some(batch) = log.pop() else {
        println!("Nothing to undo.");
        return Ok(());
    };
    for a in batch.actions.iter().rev() {
        match platform::revert(a) {
            Ok(msg) => println!("  {msg}"),
            Err(e) => println!("  couldn't undo one change: {e}"),
        }
    }
    write_log(db, &log)?;
    println!("Undone.");
    Ok(())
}

// ---- Windows implementation --------------------------------------------------------------------------

#[cfg(windows)]
mod platform {
    use super::*;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
    use winreg::{RegKey, RegValue};

    pub const SUPPORTED: bool = true;
    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

    fn powercfg(args: &[&str]) -> Result<String, String> {
        use std::os::windows::process::CommandExt;
        let out = std::process::Command::new("powercfg").args(args).creation_flags(0x0800_0000).output().map_err(|e| format!("couldn't run powercfg: {e}"))?;
        if out.status.success() { Ok(String::from_utf8_lossy(&out.stdout).into_owned()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) }
    }

    pub fn active_power_plan() -> Option<(String, String)> {
        parse_power_scheme(&powercfg(&["/getactivescheme"]).ok()?)
    }

    pub fn on_ac() -> Option<bool> {
        let mut sensors = bb_core::sensors::SensorReader::new();
        sensors.read().on_ac
    }

    /// Every process called `name`, with its current priority class.
    pub fn pids_named(name: &str) -> Vec<(u32, u32)> {
        use sysinfo::{ProcessesToUpdate, System};
        let mut sys = System::new();
        sys.refresh_processes(ProcessesToUpdate::All, true);
        sys.processes().iter().filter(|(_, p)| p.name().to_string_lossy().eq_ignore_ascii_case(name)).filter_map(|(pid, _)| get_class(pid.as_u32()).map(|c| (pid.as_u32(), c))).collect()
    }

    fn get_class(pid: u32) -> Option<u32> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{GetPriorityClass, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let c = GetPriorityClass(h);
            CloseHandle(h);
            (c != 0).then_some(c)
        }
    }

    fn set_class(pid: u32, class: u32) -> Res {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenProcess, SetPriorityClass, PROCESS_SET_INFORMATION};
        unsafe {
            let h = OpenProcess(PROCESS_SET_INFORMATION, 0, pid);
            if h.is_null() {
                return Err(format!("pid {pid} is gone or protected"));
            }
            let ok = SetPriorityClass(h, class) != 0;
            CloseHandle(h);
            if ok { Ok(()) } else { Err(format!("Windows refused to change pid {pid}")) }
        }
    }

    pub fn enabled_startup_items() -> Vec<String> {
        let hk = RegKey::predef(HKEY_CURRENT_USER);
        let Ok(run) = hk.open_subkey_with_flags(RUN, KEY_READ) else { return Vec::new() };
        let approved = hk.open_subkey_with_flags(APPROVED, KEY_READ).ok();
        run.enum_values()
            .filter_map(Result::ok)
            .map(|(n, _)| n)
            .filter(|n| !n.eq_ignore_ascii_case("blackbox"))
            .filter(|n| !approved.as_ref().and_then(|a| a.get_raw_value(n).ok()).is_some_and(|v| v.bytes.first().is_some_and(|b| b % 2 == 1)))
            .collect()
    }

    fn set_startup(name: &str, enabled: bool) -> Res {
        let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey_with_flags(APPROVED, KEY_WRITE).map_err(|e| e.to_string())?;
        key.set_raw_value(name, &RegValue { bytes: startup_approved(enabled).into(), vtype: winreg::enums::RegType::REG_BINARY }).map_err(|e| e.to_string())
    }

    pub fn apply(a: &Action) -> Res {
        match a {
            Action::Priority { pids, .. } => {
                let ok = pids.iter().filter(|(pid, _)| set_class(*pid, BELOW_NORMAL).is_ok()).count();
                if ok == 0 { Err("Windows refused every process".into()) } else { Ok(()) }
            }
            Action::PowerPlan { new_guid, .. } => powercfg(&["/setactive", new_guid]).map(|_| ()),
            Action::Startup { name } => set_startup(name, false),
        }
    }

    pub fn revert(a: &Action) -> Result<String, String> {
        match a {
            Action::Priority { name, pids } => {
                let ok = pids.iter().filter(|(pid, old)| set_class(*pid, *old).is_ok()).count();
                let gone = if ok < pids.len() { "; the rest have exited" } else { "" };
                Ok(format!("restored the priority of {ok} of {} {name} process(es){gone}", pids.len()))
            }
            Action::PowerPlan { old_guid, old_name, .. } => powercfg(&["/setactive", old_guid]).map(|_| format!("switched back to {old_name}")),
            Action::Startup { name } => set_startup(name, true).map(|_| format!("\"{name}\" will start at login again")),
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;
    pub const SUPPORTED: bool = false;
    pub fn active_power_plan() -> Option<(String, String)> { None }
    pub fn on_ac() -> Option<bool> { None }
    pub fn pids_named(_: &str) -> Vec<(u32, u32)> { Vec::new() }
    pub fn enabled_startup_items() -> Vec<String> { Vec::new() }
    pub fn apply(_: &Action) -> Res { Err("unsupported".into()) }
    pub fn revert(_: &Action) -> Result<String, String> { Err("unsupported".into()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn top(v: &[(&str, f32)]) -> Vec<(String, f32)> {
        v.iter().map(|(n, c)| (n.to_string(), *c)).collect()
    }

    #[test]
    fn only_heavy_unprotected_programs_are_lowered() {
        let hogs = pick_hogs(&top(&[("System", 40.0), ("chrome.exe", 30.0), ("build.exe", 20.0), ("Svchost.exe", 18.0), ("code.exe", 16.0), ("tiny.exe", 3.0)]));
        let names: Vec<&str> = hogs.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["chrome.exe", "build.exe"], "system programs are skipped, and at most two are picked");
        assert!(pick_hogs(&top(&[("a.exe", 14.9)])).is_empty());
        assert!(is_protected("EXPLORER.EXE") && is_protected("bb.exe"));
    }

    #[test]
    fn power_scheme_output_is_parsed() {
        let out = "Power Scheme GUID: a1841308-3541-4fab-bc81-f71556f20b4a  (Power saver)\r\n";
        assert_eq!(parse_power_scheme(out), Some((POWER_SAVER.into(), "Power saver".into())));
        assert_eq!(parse_power_scheme("nonsense"), None);
        assert_eq!(parse_power_scheme("GUID: short"), None);
    }

    #[test]
    fn power_saver_is_only_left_when_plugged_in() {
        let saver = || Some((POWER_SAVER.to_string(), "Power saver".to_string()));
        assert!(matches!(plan_power(saver(), Some(true)), Some(Action::PowerPlan { new_guid, .. }) if new_guid == BALANCED));
        assert_eq!(plan_power(saver(), Some(false)), None, "on battery it is doing its job");
        assert_eq!(plan_power(saver(), None), None, "unknown power source means don't touch it");
        assert_eq!(plan_power(Some((BALANCED.into(), "Balanced".into())), Some(true)), None);
        assert_eq!(plan_power(None, Some(true)), None);
    }

    #[test]
    fn startup_switch_uses_the_task_manager_format() {
        let off = startup_approved(false);
        let on = startup_approved(true);
        assert_eq!((off.len(), off[0]), (12, 3));
        assert_eq!((on.len(), on[0]), (12, 2));
    }

    #[test]
    fn the_before_after_report_names_each_change() {
        let before = Measure { cpu_pct: 80.0, freq_pct: Some(50.0), top: top(&[("build.exe", 60.0), ("code.exe", 5.0)]) };
        let after = Measure { cpu_pct: 70.0, freq_pct: Some(90.0), top: top(&[("code.exe", 8.0)]) };
        let lines = describe_delta(&before, &after);
        assert_eq!(lines[0], "whole machine CPU: 80% -> 70%");
        assert_eq!(lines[1], "CPU speed: 50% -> 90% of rated maximum");
        assert_eq!(lines[2], "build.exe: 60% -> 0% of the CPU");
    }

    #[test]
    fn actions_survive_the_undo_log() {
        let b = Batch {
            ts: 5,
            actions: vec![
                Action::Priority { name: "a.exe".into(), pids: vec![(7, NORMAL)] },
                Action::Startup { name: "Thing".into() },
            ],
            before: Measure::default(),
            after: None,
        };
        let text = serde_json::to_string(&vec![b.clone()]).unwrap();
        let back: Vec<Batch> = serde_json::from_str(&text).unwrap();
        assert_eq!(back[0].actions, b.actions);
        let db = std::env::temp_dir().join(format!("bb-fix-{}.db", std::process::id()));
        write_log(&db, &back).unwrap();
        assert_eq!(read_log(&db).len(), 1);
        let _ = std::fs::remove_file(log_path(&db));
        assert!(read_log(&db).is_empty());
    }
}
