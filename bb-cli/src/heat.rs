//! `bb heat test`: a fixed load, the same on every machine, and a cooling score.
//! `bb heat`: past results.

use bb_core::heat::{advice, check, parse_record, record_json, score, summarize, tjmax, COOL_SECS, IDLE_SECS, LOAD_SECS};
use bb_core::model::Sample;
use bb_core::sampler::Sampler;
use chrono::{Local, TimeZone};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

type Res = Result<(), String>;

fn results_path(db: &Path) -> PathBuf {
    db.with_file_name("heat-tests.jsonl")
}

fn cpu_name() -> String {
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu_all();
    sys.cpus().first().map(|c| c.brand().trim().to_string()).unwrap_or_default()
}

/// The laptop or PC model, as the firmware reports it.
fn machine_name() -> String {
    #[cfg(windows)]
    {
        use winreg::enums::HKEY_LOCAL_MACHINE;
        if let Ok(k) = winreg::RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(r"HARDWARE\DESCRIPTION\System\BIOS") {
            let get = |v: &str| k.get_value::<String, _>(v).unwrap_or_default();
            return format!("{} {}", get("SystemManufacturer"), get("SystemProductName")).trim().to_string();
        }
    }
    String::new()
}

/// Samples about once a second for `secs` seconds of wall-clock time, printing progress every
/// 15 s. Timed by the clock, not by counting samples: under full load one sample can take
/// longer than a second.
fn phase(sampler: &mut Sampler, secs: u64, label: &str) -> Vec<Sample> {
    let start = std::time::Instant::now();
    let mut out = Vec::new();
    let mut next_report = 15;
    while start.elapsed().as_secs() < secs {
        let s = sampler.sample_after(Duration::from_secs(1), Local::now().timestamp());
        let at = start.elapsed().as_secs();
        if at >= next_report {
            next_report += 15;
            let t = s.sensors.temp_c.map_or("?".into(), |t| format!("{t:.0} C"));
            let w = s.sensors.cpu_power_w.map_or(String::new(), |w| format!(", {w:.0} W"));
            let f = s.sensors.freq_pct.map_or(String::new(), |f| format!(", speed {f:.0}%"));
            println!("  {label} {at:>3} s: {t}{w}{f}");
        }
        out.push(s);
    }
    out
}

/// One `bb selftest-worker cpu` process per core, the same load `bb selftest` uses. Separate
/// processes at normal priority really reach 100%; threads inside this process were held back
/// by Windows and starved the sampler. Each worker also stops on its own after `secs`.
fn start_load(secs: u64) -> Result<Vec<std::process::Child>, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let n = std::thread::available_parallelism().map_or(2, |n| n.get());
    (0..n)
        .map(|_| {
            let mut c = std::process::Command::new(&exe);
            c.args(["selftest-worker", "cpu", "--secs", &secs.to_string()]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            }
            c.spawn().map_err(|e| format!("couldn't start the load: {e}"))
        })
        .collect()
}

pub fn test(db: &Path, yes: bool) -> Res {
    let mut probe = Sampler::new(3);
    let now = probe.sample_after(Duration::from_millis(500), 0);
    if now.sensors.temp_c.is_none() {
        return Err("this machine doesn't report a temperature blackbox can read, so there's nothing to measure. See `bb sensors`.".into());
    }
    println!("The heat test runs every CPU core flat out for 3 minutes, the same way on every machine,");
    println!("then watches it cool for a minute. Fans will get loud. It takes about 4.5 minutes.");
    println!("For a fair result: plug in, close heavy programs, and leave the laptop where you normally use it.");
    if now.sensors.on_ac == Some(false) {
        println!("\nYou're on battery. Laptops limit power on battery, so this result won't compare with plugged-in ones.");
    }
    if !yes {
        print!("\nStart? [y/N] ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() || !matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("Nothing was run.");
            return Ok(());
        }
    }

    // Above the load workers, so readings keep coming once a second while every core is busy.
    bb_core::sampler::raise_priority();
    let mut sampler = Sampler::new(5);
    println!("\nResting for {IDLE_SECS} s...");
    let idle = phase(&mut sampler, IDLE_SECS, "rest");
    println!("Full load for {LOAD_SECS} s...");
    let mut workers = start_load(LOAD_SECS)?;
    let load = phase(&mut sampler, LOAD_SECS, "load");
    for w in &mut workers {
        let _ = w.kill();
        let _ = w.wait();
    }
    println!("Cooling for {COOL_SECS} s...");
    let cool = phase(&mut sampler, COOL_SECS, "cool");

    let run = summarize(&idle, &load, &cool).ok_or("no temperature readings came in during the test")?;
    println!("\nMeasured:");
    println!("  CPU load during the test   {:.0}%", run.load_cpu_pct);
    if let Some(w) = run.power_w {
        println!("  CPU power, sustained       {w:.0} W (at rest {})", run.idle_power_w.map_or("?".into(), |i| format!("{i:.0} W")));
    }
    if let Some(sp) = run.speed_pct {
        println!("  CPU speed held             {sp:.0}% of rated maximum");
    }
    println!("  Temperature                {} at rest, {:.0} C under load, peak {:.0} C", run.idle_c.map_or("?".into(), |i| format!("{i:.0} C")), run.load_c, run.peak_c);
    if let Err(why) = check(&run) {
        println!("\nNo score: {why}");
        println!("Nothing was saved. The power and speed numbers above are still real.");
        return Ok(());
    }
    let cpu = cpu_name();
    let (limit, known) = tjmax(&cpu);
    let s = score(&run, limit);

    println!("\nHeat score: {} / 100  ({})", s.total, s.band);
    for p in &s.parts {
        println!("  {:<22} {:>4.0} / {:<3.0}  {}", p.name, p.points, p.max, p.note);
    }
    if let Some(t) = run.secs_to_90 {
        println!("  Reached 90 C after {t} s of load. Peak {:.0} C.", run.peak_c);
    } else {
        println!("  Never reached 90 C. Peak {:.0} C.", run.peak_c);
    }
    println!("\n{}", advice(&run, &s));
    println!("\nHow it's rated: against this CPU's own limit ({limit:.0} C{}) and rough ranges for laptops,", if known { "" } else { ", assumed" });
    println!("not against other people's machines. The temperature is the hottest sensor Windows exposes,");
    println!("which on most laptops tracks the CPU but isn't always the CPU core itself.");

    let line = record_json(Local::now().timestamp(), &cpu, &machine_name(), env!("CARGO_PKG_VERSION"), &run, &s);
    let path = results_path(db);
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|e| format!("can't save the result to {}: {e}", path.display()))?;
    writeln!(f, "{line}").map_err(|e| e.to_string())?;
    println!("Saved to {}. `bb heat` lists past results.", path.display());
    Ok(())
}

pub fn history(db: &Path) -> Res {
    let text = std::fs::read_to_string(results_path(db)).unwrap_or_default();
    let rows: Vec<(i64, u32)> = text.lines().filter_map(parse_record).collect();
    if rows.is_empty() {
        println!("No heat tests yet. Run `bb heat test` (about 4.5 minutes, fans get loud) to score this machine's cooling.");
        return Ok(());
    }
    println!("Heat test results on this machine:");
    for (ts, sc) in &rows {
        let when = Local.timestamp_opt(*ts, 0).single().map_or_else(String::new, |t| t.format("%Y-%m-%d %H:%M").to_string());
        println!("  {when}   {sc:>3} / 100   {}", bb_core::heat::band(*sc));
    }
    if let [.., (_, a), (_, b)] = rows.as_slice() {
        let d = *b as i64 - *a as i64;
        if d.abs() >= 5 {
            println!("\nThe last result is {} points {} than the one before.", d.abs(), if d > 0 { "better" } else { "worse" });
        }
    }
    Ok(())
}
