//! The "why" engine: a recorded window in, ranked causes out.
//! Each rule is a pure function of the window.

use crate::model::{Confidence, Finding, Sample};
use std::collections::HashMap;

const GB: f64 = 1024.0 * 1024.0 * 1024.0;
const MB: f64 = 1024.0 * 1024.0;

#[derive(Debug, Clone)]
pub struct Thresholds {
    /// A single program averaging this share of total CPU is a hog.
    pub hog_cpu_pct: f32,
    /// Whole-machine CPU at or above this counts as saturated.
    pub saturated_cpu_pct: f32,
    pub mem_full_pct: f32,
    pub mem_high_pct: f32,
    pub swap_high_bytes: u64,
    /// CPU must be at least this busy for throttling to be suspected.
    pub throttle_busy_pct: f32,
    /// Clock at or below this fraction of the best seen means throttling.
    pub throttle_clock_ratio: f32,
    pub disk_heavy_bps: u64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            hog_cpu_pct: 25.0,
            saturated_cpu_pct: 85.0,
            mem_full_pct: 90.0,
            mem_high_pct: 80.0,
            swap_high_bytes: 1 << 30,
            throttle_busy_pct: 60.0,
            throttle_clock_ratio: 0.75,
            disk_heavy_bps: 80 * 1024 * 1024,
        }
    }
}

/// Advice for programs we recognise, matched case-insensitively.
pub fn hint_for(program: &str) -> Option<&'static str> {
    match program.to_lowercase().as_str() {
        "msmpeng.exe" => Some(
            "Windows Defender real-time scanning. Add project/build folders under \
             Windows Security > Virus & threat protection > Exclusions.",
        ),
        "searchindexer.exe" => Some("Windows Search indexing. Usually settles on its own."),
        "tiworker.exe" => Some("Windows Update working in the background."),
        "onedrive.exe" => Some("OneDrive syncing. Pause it or move big working folders out."),
        "vmmem" => Some(
            "WSL2 or a virtual machine holding memory. Limit it in .wslconfig, \
             or run `wsl --shutdown`.",
        ),
        _ => None,
    }
}

/// Per-program averages over the window. A sample that doesn't list a program
/// counts as zero for it, so brief bursts are not overstated.
#[derive(Debug, Clone)]
struct ProgAvg {
    name: String,
    max_count: u32,
    cpu: f32,
    disk_bps: f64,
    mem_bytes: f64,
}

fn per_program(samples: &[Sample]) -> Vec<ProgAvg> {
    let n = samples.len() as f64;
    let mut map: HashMap<&str, ProgAvg> = HashMap::new();
    for s in samples {
        for p in &s.procs {
            let e = map.entry(p.name.as_str()).or_insert_with(|| ProgAvg {
                name: p.name.clone(),
                max_count: 0,
                cpu: 0.0,
                disk_bps: 0.0,
                mem_bytes: 0.0,
            });
            e.max_count = e.max_count.max(p.count);
            e.cpu += p.cpu_pct;
            e.disk_bps += p.disk_bps as f64;
            e.mem_bytes += p.mem_bytes as f64;
        }
    }
    map.into_values()
        .map(|mut e| {
            e.cpu = (e.cpu as f64 / n) as f32;
            e.disk_bps /= n;
            e.mem_bytes /= n;
            e
        })
        .collect()
}

fn label(p: &ProgAvg) -> String {
    if p.max_count > 1 {
        format!("{} ({} processes)", p.name, p.max_count)
    } else {
        p.name.clone()
    }
}

fn mean<F: Fn(&Sample) -> f64>(samples: &[Sample], f: F) -> f64 {
    samples.iter().map(f).sum::<f64>() / samples.len() as f64
}

/// Explains a window of samples. `best_mhz` is the fastest clock ever recorded.
/// Findings come back ranked, most likely first. Empty means no clear cause.
pub fn analyze(samples: &[Sample], best_mhz: u32, t: &Thresholds) -> Vec<Finding> {
    if samples.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let avg_cpu = mean(samples, |s| s.cpu_pct as f64) as f32;
    let progs = per_program(samples);
    let machine_line = format!("whole machine averaged {:.0}% CPU", avg_cpu);

    // 1. CPU hogs (up to two).
    let mut by_cpu: Vec<&ProgAvg> = progs.iter().filter(|p| p.cpu >= t.hog_cpu_pct).collect();
    by_cpu.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
    let hogs = by_cpu.len();
    for p in by_cpu.into_iter().take(2) {
        out.push(Finding {
            title: format!("{} was using a large share of the CPU", label(p)),
            evidence: vec![
                format!("averaged {:.0}% of total CPU over the window", p.cpu),
                machine_line.clone(),
            ],
            confidence: if p.cpu >= 50.0 { Confidence::High } else { Confidence::Medium },
            hint: hint_for(&p.name).map(str::to_string),
            score: 60.0 + p.cpu * 0.4,
        });
    }

    // 2. Saturated by many programs.
    if hogs == 0 && avg_cpu >= t.saturated_cpu_pct {
        let mut top: Vec<&ProgAvg> = progs.iter().collect();
        top.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
        let mut evidence = vec![machine_line.clone()];
        for p in top.iter().take(3).filter(|p| p.cpu > 0.5) {
            evidence.push(format!("{} averaged {:.0}%", label(p), p.cpu));
        }
        out.push(Finding {
            title: "CPU was saturated by many programs at once".into(),
            evidence,
            confidence: Confidence::Medium,
            hint: None,
            score: 55.0 + avg_cpu * 0.2,
        });
    }

    // 3. Memory pressure.
    let mem_pct = mean(samples, |s| {
        if s.mem_total == 0 { 0.0 } else { s.mem_used as f64 * 100.0 / s.mem_total as f64 }
    }) as f32;
    let swap = mean(samples, |s| s.swap_used as f64);
    let full = mem_pct >= t.mem_full_pct;
    let paging = mem_pct >= t.mem_high_pct && swap >= t.swap_high_bytes as f64;
    if full || paging {
        let biggest = progs.iter().max_by(|a, b| a.mem_bytes.total_cmp(&b.mem_bytes));
        let mut evidence = vec![
            format!("memory averaged {:.0}% full", mem_pct),
            format!("pagefile/swap in use averaged {:.1} GB", swap / GB),
        ];
        let mut hint = None;
        if let Some(p) = biggest.filter(|p| p.mem_bytes > 0.0) {
            evidence.push(format!("largest memory user: {} at {:.1} GB", label(p), p.mem_bytes / GB));
            hint = hint_for(&p.name).map(str::to_string);
        }
        out.push(Finding {
            title: "Memory pressure, the machine was short on RAM".into(),
            evidence,
            confidence: if full && swap >= t.swap_high_bytes as f64 {
                Confidence::High
            } else {
                Confidence::Medium
            },
            hint,
            score: 58.0 + mem_pct * 0.2,
        });
    }

    // 4. Throttling, inferred from clock speed only.
    let clocks: Vec<f64> = samples.iter().filter(|s| s.clock_mhz > 0).map(|s| s.clock_mhz as f64).collect();
    if best_mhz > 0 && !clocks.is_empty() && avg_cpu >= t.throttle_busy_pct {
        let avg_mhz = clocks.iter().sum::<f64>() / clocks.len() as f64;
        let ratio = avg_mhz / best_mhz as f64;
        if ratio <= t.throttle_clock_ratio as f64 {
            out.push(Finding {
                title: "Probable CPU throttling".into(),
                evidence: vec![
                    format!("CPU was {:.0}% busy", avg_cpu),
                    format!(
                        "clock averaged {:.0} MHz, {:.0}% of the best seen ({} MHz)",
                        avg_mhz,
                        ratio * 100.0,
                        best_mhz
                    ),
                    "inferred from clock speed only; can't tell heat from power limits or battery saver".into(),
                ],
                confidence: Confidence::Low,
                hint: None,
                score: 40.0 + (1.0 - ratio as f32) * 20.0,
            });
        }
    }

    // 5. Heavy disk activity.
    let disk = mean(samples, |s| s.disk_bps as f64);
    if disk >= t.disk_heavy_bps as f64 {
        let top = progs.iter().max_by(|a, b| a.disk_bps.total_cmp(&b.disk_bps));
        let mut evidence = vec![format!("disk throughput averaged {:.0} MB/s", disk / MB)];
        let mut hint = None;
        if let Some(p) = top.filter(|p| p.disk_bps > 0.0) {
            evidence.push(format!("most I/O: {} at {:.0} MB/s", label(p), p.disk_bps / MB));
            hint = hint_for(&p.name).map(str::to_string);
        }
        out.push(Finding {
            title: "Heavy disk activity".into(),
            evidence,
            confidence: Confidence::Medium,
            hint,
            score: 50.0 + (disk / MB / 10.0).min(20.0) as f32,
        });
    }

    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProcRow;

    fn proc(name: &str, count: u32, cpu: f32, disk_mb: u64, mem_gb: f64) -> ProcRow {
        ProcRow {
            name: name.into(),
            count,
            cpu_pct: cpu,
            disk_bps: disk_mb << 20,
            mem_bytes: (mem_gb * GB) as u64,
        }
    }

    /// 60 seconds of a healthy 16 GB machine, with `tweak` applied to each sample.
    fn window(tweak: impl Fn(&mut Sample)) -> Vec<Sample> {
        (0..60)
            .map(|i| {
                let mut s = Sample {
                    ts: 1000 + i,
                    cpu_pct: 10.0,
                    mem_used: 6 << 30,
                    mem_total: 16 << 30,
                    swap_used: 0,
                    clock_mhz: 3600,
                    disk_bps: 2 << 20,
                    procs: vec![proc("code.exe", 6, 4.0, 1, 1.0)],
                };
                tweak(&mut s);
                s
            })
            .collect()
    }

    fn run(w: &[Sample]) -> Vec<Finding> {
        analyze(w, 3600, &Thresholds::default())
    }

    #[test]
    fn healthy_machine_has_no_findings() {
        assert!(run(&window(|_| {})).is_empty());
    }

    #[test]
    fn empty_window_has_no_findings() {
        assert!(run(&[]).is_empty());
    }

    #[test]
    fn names_a_cpu_hog() {
        let w = window(|s| {
            s.cpu_pct = 70.0;
            s.procs.push(proc("cargo.exe", 3, 55.0, 1, 0.5));
        });
        let f = run(&w);
        assert_eq!(f.len(), 1);
        assert!(f[0].title.starts_with("cargo.exe (3 processes)"));
        assert_eq!(f[0].confidence, Confidence::High);
    }

    #[test]
    fn defender_gets_actionable_hint() {
        let w = window(|s| {
            s.cpu_pct = 45.0;
            s.procs.push(proc("MsMpEng.exe", 1, 38.0, 20, 0.3));
        });
        let f = run(&w);
        assert_eq!(f[0].title.split(' ').next(), Some("MsMpEng.exe"));
        assert!(f[0].hint.as_ref().unwrap().contains("Exclusions"));
    }

    #[test]
    fn short_burst_is_diluted_not_blamed() {
        // A hog present for only 10 of 60 seconds averages ~8%.
        let w: Vec<Sample> = window(|_| {})
            .into_iter()
            .enumerate()
            .map(|(i, mut s)| {
                if i < 10 {
                    s.procs.push(proc("burst.exe", 1, 50.0, 0, 0.1));
                }
                s
            })
            .collect();
        assert!(run(&w).is_empty());
    }

    #[test]
    fn saturation_without_a_single_hog() {
        let w = window(|s| {
            s.cpu_pct = 95.0;
            s.procs = (0..5).map(|i| proc(&format!("app{i}.exe"), 1, 18.0, 0, 0.2)).collect();
        });
        let f = run(&w);
        assert_eq!(f.len(), 1);
        assert!(f[0].title.contains("saturated"));
        assert!(f[0].evidence.len() >= 2);
    }

    #[test]
    fn memory_exhaustion_names_biggest_user() {
        let w = window(|s| {
            s.mem_used = 15 << 30;
            s.swap_used = 3 << 30;
            s.procs.push(proc("vmmem", 1, 3.0, 1, 9.0));
        });
        let f = run(&w);
        assert_eq!(f.len(), 1);
        assert!(f[0].title.contains("Memory"));
        assert!(f[0].evidence.iter().any(|e| e.contains("vmmem")));
        assert!(f[0].hint.as_ref().unwrap().contains("wsl"));
        assert_eq!(f[0].confidence, Confidence::High);
    }

    #[test]
    fn heavy_paging_below_full_still_flagged() {
        let w = window(|s| {
            s.mem_used = 13 << 30; // 81%
            s.swap_used = 2 << 30;
        });
        assert_eq!(run(&w).len(), 1);
        // Same memory with no pagefile use is fine.
        assert!(run(&window(|s| s.mem_used = 13 << 30)).is_empty());
    }

    #[test]
    fn clock_drop_under_load_is_throttling() {
        let w = window(|s| {
            s.cpu_pct = 70.0;
            s.clock_mhz = 1800;
            s.procs = vec![proc("a.exe", 1, 20.0, 0, 0.1), proc("b.exe", 1, 20.0, 0, 0.1)];
        });
        let f = run(&w);
        assert_eq!(f.len(), 1);
        assert!(f[0].title.contains("throttling"));
        assert_eq!(f[0].confidence, Confidence::Low);
    }

    #[test]
    fn low_clock_while_idle_is_not_throttling() {
        assert!(run(&window(|s| s.clock_mhz = 800)).is_empty());
    }

    #[test]
    fn unknown_clock_skips_throttle_rule() {
        let w = window(|s| {
            s.cpu_pct = 70.0;
            s.clock_mhz = 0;
        });
        assert!(run(&w).is_empty());
    }

    #[test]
    fn saturated_disk_names_the_program() {
        let w = window(|s| {
            s.disk_bps = 200 << 20;
            s.procs.push(proc("robocopy.exe", 1, 3.0, 190, 0.1));
        });
        let f = run(&w);
        assert_eq!(f.len(), 1);
        assert!(f[0].evidence.iter().any(|e| e.contains("robocopy.exe")));
    }

    #[test]
    fn findings_are_ranked() {
        let w = window(|s| {
            s.cpu_pct = 80.0;
            s.mem_used = 15 << 30;
            s.swap_used = 2 << 30;
            s.disk_bps = 150 << 20;
            s.procs.push(proc("hog.exe", 1, 70.0, 100, 1.0));
        });
        let f = run(&w);
        assert!(f.len() >= 3);
        assert!(f.windows(2).all(|p| p[0].score >= p[1].score));
        assert!(f[0].title.starts_with("hog.exe"));
    }
}
