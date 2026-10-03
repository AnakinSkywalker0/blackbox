//! `bb blame`: who is costing you heat, battery and CPU, and what changed lately.

use bb_core::blame::{changes, tally, Change, Cost, Tally};
use bb_core::store::Store;
use chrono::Local;
use std::path::Path;

type Res = Result<(), String>;

fn hours(secs: f64) -> String {
    if secs < 60.0 { "under a minute".to_string() } else if secs < 90.0 { format!("{secs:.0} s") } else if secs < 5400.0 { format!("{:.0} min", secs / 60.0) } else { format!("{:.1} h", secs / 3600.0) }
}

/// A leaderboard: the top programs by `value`, with their share of `total`.
fn board(title: &str, programs: &[Cost], total: f64, value: impl Fn(&Cost) -> f64, unit: impl Fn(f64) -> String) {
    let mut rows: Vec<(&Cost, f64)> = programs.iter().map(|c| (c, value(c))).filter(|(_, v)| *v > 0.0).collect();
    rows.sort_by(|a, b| b.1.total_cmp(&a.1));
    println!("\n{title}");
    if rows.is_empty() || total <= 0.0 {
        println!("  (nothing to report)");
        return;
    }
    let mut shown = 0.0;
    for (c, v) in rows.into_iter().take(5) {
        shown += v;
        println!("  {:<28} {:>5.0}%   {}", c.name, v / total * 100.0, unit(v));
    }
    let rest = (1.0 - shown / total) * 100.0;
    if rest >= 5.0 {
        println!("  {:<28} {:>5.0}%   (smaller programs, and things not recorded)", "everything else", rest);
    }
}

pub fn run(db: &Path, days: i64) -> Res {
    let store = Store::open(db).map_err(|e| format!("can't open database {}: {e}", db.display()))?;
    let now = Local::now().timestamp();
    let from = now - days.clamp(1, 30) * 86_400;
    let all = store.window(from, now).map_err(|e| e.to_string())?;
    let t: Tally = tally(&all);
    if t.covered_secs < 600.0 {
        println!("Not enough recorded yet ({} of data). Leave `bb start` running for a day or two and try again.", hours(t.covered_secs));
        return Ok(());
    }
    println!("Looking at {} of recording.", hours(t.covered_secs));
    println!("Machine used {} of full-CPU time; {} of it hot (80 C or more).", hours(t.total_cpu_secs), hours(t.total_hot_secs));

    board("Who ran the machine hot (share of the hot time):", &t.programs, t.total_hot_secs, |c| c.hot_secs, |v| format!("about {} of heat", hours(v)));
    board("Who used the battery (share of CPU used unplugged):", &t.programs, t.total_battery_cpu_secs, |c| c.battery_cpu_secs, |v| format!("{} of full-CPU time", hours(v)));
    board("Who used the most CPU overall:", &t.programs, t.total_cpu_secs, |c| c.cpu_secs, |v| format!("{} of full-CPU time", hours(v)));
    let mut by_mem: Vec<&Cost> = t.programs.iter().collect();
    by_mem.sort_by(|a, b| b.avg_mem_mb.total_cmp(&a.avg_mem_mb));
    println!("\nWho holds the most memory (average while running):");
    for c in by_mem.into_iter().take(5) {
        println!("  {:<28} {:>6.0} MB", c.name, c.avg_mem_mb);
    }

    // What changed: the last 24 hours against the days before.
    let split = now - 86_400;
    let recent = tally(&all.iter().filter(|s| s.ts >= split).cloned().collect::<Vec<_>>());
    let earlier = tally(&all.iter().filter(|s| s.ts < split).cloned().collect::<Vec<_>>());
    println!("\nWhat got heavier in the last 24 hours, compared with before:");
    let ch = changes(&recent, &earlier);
    if recent.covered_secs < 600.0 || earlier.covered_secs < 3600.0 {
        println!("  (need at least an hour of older recording and 10 minutes from today to compare)");
    } else if ch.is_empty() {
        println!("  Nothing stands out. Usage looks the same as before.");
    }
    for c in ch.iter().take(6) {
        match c {
            Change::New { name, cpu } => println!("  NEW      {name} appeared and averages {cpu:.0}% of the CPU"),
            Change::CpuUp { name, was, now } => println!("  CPU UP   {name}: {was:.0}% -> {now:.0}% of the CPU"),
            Change::MemUp { name, was_mb, now_mb } => println!("  MEMORY   {name}: {was_mb:.0} MB -> {now_mb:.0} MB"),
        }
    }
    println!("\nHeat and battery shares are estimates: each program is blamed for its share of the CPU in use");
    println!("at the time. Only each moment's busiest programs are recorded, so small ones don't appear.");
    println!("Next step: `bb fix` can lower a heavy program's priority or trim what starts at login.");
    Ok(())
}
