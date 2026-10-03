//! Who is costing you? Turns a long recording into a leaderboard of programs by what they
//! cost (heat, battery, total CPU) and a list of what changed lately.
//!
//! Attribution is proportional: while the machine was hot, each program is blamed for its
//! share of the CPU being used at that moment. That is an estimate, not a measurement of
//! which program produced which watt, and callers should say so.

use crate::model::Sample;

/// Temperature at or above which the machine counts as hot.
pub const HOT_C: f32 = 80.0;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cost {
    pub name: String,
    /// Machine-seconds of CPU used (100% of the whole machine for 1 s is 1.0).
    pub cpu_secs: f64,
    /// Seconds of "hot time" blamed on this program.
    pub hot_secs: f64,
    /// Machine-seconds of CPU used while unplugged.
    pub battery_cpu_secs: f64,
    pub avg_mem_mb: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tally {
    pub programs: Vec<Cost>,
    pub total_cpu_secs: f64,
    pub total_hot_secs: f64,
    pub total_battery_cpu_secs: f64,
    /// Seconds covered by the samples (gaps in recording excluded).
    pub covered_secs: f64,
}

/// Each sample counts for the time until the next, capped at a minute so a recording gap
/// isn't counted as time the machine was busy.
fn step(samples: &[Sample], i: usize) -> f64 {
    samples.get(i + 1).map_or(1, |n| (n.ts - samples[i].ts).clamp(0, 60)) as f64
}

pub fn tally(samples: &[Sample]) -> Tally {
    use std::collections::HashMap;
    let mut by: HashMap<&str, Cost> = HashMap::new();
    let mut mem_sum: HashMap<&str, (f64, f64)> = HashMap::new();
    let mut t = Tally::default();
    for (i, s) in samples.iter().enumerate() {
        let dt = step(samples, i);
        t.covered_secs += dt;
        t.total_cpu_secs += f64::from(s.cpu_pct) / 100.0 * dt;
        let hot = s.sensors.temp_c.is_some_and(|c| c >= HOT_C);
        let batt = s.sensors.on_ac == Some(false);
        if hot {
            t.total_hot_secs += dt;
        }
        if batt {
            t.total_battery_cpu_secs += f64::from(s.cpu_pct) / 100.0 * dt;
        }
        // Programs can add up to slightly more than the whole machine (rounding), so share
        // is measured against whichever is larger.
        let busy = s.procs.iter().map(|p| f64::from(p.cpu_pct)).sum::<f64>().max(f64::from(s.cpu_pct)).max(1.0);
        for p in &s.procs {
            let c = by.entry(p.name.as_str()).or_insert_with(|| Cost { name: p.name.clone(), ..Default::default() });
            let cpu = f64::from(p.cpu_pct) / 100.0 * dt;
            c.cpu_secs += cpu;
            if hot {
                c.hot_secs += dt * f64::from(p.cpu_pct) / busy;
            }
            if batt {
                c.battery_cpu_secs += cpu;
            }
            let m = mem_sum.entry(p.name.as_str()).or_default();
            m.0 += p.mem_bytes as f64 / 1_048_576.0 * dt;
            m.1 += dt;
        }
    }
    t.programs = by
        .into_values()
        .map(|mut c| {
            if let Some((sum, secs)) = mem_sum.get(c.name.as_str()) {
                c.avg_mem_mb = if *secs > 0.0 { sum / secs } else { 0.0 };
            }
            c
        })
        .collect();
    t.programs.sort_by(|a, b| b.cpu_secs.total_cmp(&a.cpu_secs));
    t
}

#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    /// Not seen before, and now using real CPU.
    New { name: String, cpu: f64 },
    CpuUp { name: String, was: f64, now: f64 },
    MemUp { name: String, was_mb: f64, now_mb: f64 },
}

/// Average CPU (percent of the whole machine) per program over the covered time. Programs
/// absent from a sample count as zero for it, so a rarely-seen program isn't overrated.
fn avg_cpu(t: &Tally) -> Vec<(String, f64)> {
    let secs = t.covered_secs.max(1.0);
    t.programs.iter().map(|c| (c.name.clone(), c.cpu_secs / secs * 100.0)).collect()
}

/// What got heavier in `recent` compared with `baseline`.
pub fn changes(recent: &Tally, baseline: &Tally) -> Vec<Change> {
    if recent.covered_secs < 600.0 || baseline.covered_secs < 3600.0 {
        return Vec::new();
    }
    let base_cpu = avg_cpu(baseline);
    let mut out = Vec::new();
    for (name, now) in avg_cpu(recent) {
        match base_cpu.iter().find(|(n, _)| *n == name) {
            None if now >= 2.0 => out.push(Change::New { name: name.clone(), cpu: now }),
            Some((_, was)) if now >= was + 3.0 && now >= was * 1.8 => out.push(Change::CpuUp { name: name.clone(), was: *was, now }),
            _ => {}
        }
        let (r, b) = (recent.programs.iter().find(|c| c.name == name), baseline.programs.iter().find(|c| c.name == name));
        if let (Some(r), Some(b)) = (r, b) {
            if r.avg_mem_mb >= b.avg_mem_mb + 300.0 && r.avg_mem_mb >= b.avg_mem_mb * 1.5 {
                out.push(Change::MemUp { name, was_mb: b.avg_mem_mb, now_mb: r.avg_mem_mb });
            }
        }
    }
    out.sort_by(|a, b| weight(b).total_cmp(&weight(a)));
    out
}

fn weight(c: &Change) -> f64 {
    match c {
        Change::New { cpu, .. } => *cpu,
        Change::CpuUp { was, now, .. } => now - was,
        Change::MemUp { was_mb, now_mb, .. } => (now_mb - was_mb) / 100.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProcRow, Sensors};

    fn row(name: &str, cpu: f32, mem_mb: u64) -> ProcRow {
        ProcRow { name: name.into(), count: 1, cpu_pct: cpu, disk_bps: 0, mem_bytes: mem_mb << 20 }
    }

    fn s(ts: i64, cpu: f32, temp: Option<f32>, ac: Option<bool>, procs: Vec<ProcRow>) -> Sample {
        Sample { ts, cpu_pct: cpu, sensors: Sensors { temp_c: temp, on_ac: ac, ..Default::default() }, procs, ..Default::default() }
    }

    #[test]
    fn heat_and_battery_are_blamed_by_share() {
        let mut v = Vec::new();
        // 100 s hot on battery: build.exe 60% and code.exe 20% of the machine.
        for i in 0..100 {
            v.push(s(i, 80.0, Some(90.0), Some(false), vec![row("build.exe", 60.0, 500), row("code.exe", 20.0, 700)]));
        }
        // 100 s cool and plugged in: only code.exe.
        for i in 100..200 {
            v.push(s(i, 10.0, Some(50.0), Some(true), vec![row("code.exe", 10.0, 700)]));
        }
        let t = tally(&v);
        assert_eq!(t.covered_secs, 200.0);
        assert_eq!(t.total_hot_secs, 100.0);
        let b = t.programs.iter().find(|c| c.name == "build.exe").unwrap();
        let c = t.programs.iter().find(|c| c.name == "code.exe").unwrap();
        assert!((b.hot_secs - 75.0).abs() < 0.01, "{}", b.hot_secs);
        assert!((c.hot_secs - 25.0).abs() < 0.01, "{}", c.hot_secs);
        assert!((b.battery_cpu_secs - 60.0).abs() < 0.01);
        assert!((c.battery_cpu_secs - 20.0).abs() < 0.01, "plugged-in time isn't battery time");
        assert!((c.avg_mem_mb - 700.0).abs() < 0.01);
        assert_eq!(t.programs[0].name, "build.exe", "sorted by CPU used");
    }

    #[test]
    fn a_recording_gap_is_not_counted() {
        let v = vec![s(0, 100.0, Some(95.0), None, vec![row("a.exe", 100.0, 1)]), s(100_000, 5.0, None, None, vec![])];
        let t = tally(&v);
        assert_eq!(t.covered_secs, 61.0);
        assert_eq!(t.total_hot_secs, 60.0);
    }

    fn steady(name: &str, cpu: f32, mem: u64, secs: i64) -> Vec<Sample> {
        (0..secs).map(|i| s(i, cpu, None, None, vec![row(name, cpu, mem)])).collect()
    }

    #[test]
    fn what_got_heavier_is_reported() {
        let base = tally(&steady("teams.exe", 2.0, 400, 7200));
        let mut recent = steady("teams.exe", 15.0, 1200, 1800);
        for x in &mut recent {
            x.procs.push(row("newthing.exe", 8.0, 50));
            x.cpu_pct = 23.0;
        }
        let ch = changes(&tally(&recent), &base);
        assert!(ch.iter().any(|c| matches!(c, Change::CpuUp { name, was, now } if name == "teams.exe" && (was - 2.0).abs() < 0.01 && (now - 15.0).abs() < 0.01)), "{ch:?}");
        assert!(ch.iter().any(|c| matches!(c, Change::New { name, .. } if name == "newthing.exe")));
        assert!(ch.iter().any(|c| matches!(c, Change::MemUp { name, .. } if name == "teams.exe")));
        assert!(matches!(ch[0], Change::CpuUp { .. }), "biggest change first");
    }

    #[test]
    fn steady_use_and_thin_data_report_no_changes() {
        let a = tally(&steady("x.exe", 10.0, 500, 7200));
        let b = tally(&steady("x.exe", 11.0, 520, 1800));
        assert!(changes(&b, &a).is_empty());
        // Too little history to compare is silence, not a guess.
        let short = tally(&steady("x.exe", 50.0, 500, 100));
        assert!(changes(&short, &a).is_empty());
        assert!(changes(&b, &short).is_empty());
    }
}
