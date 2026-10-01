//! The "why" engine: a recorded window in, ranked causes out.
//! Each rule is a pure function of the window.

use crate::model::{Confidence, Finding, Sample, GPU_HW, GPU_POWER, GPU_THERMAL};
use chrono::{Local, TimeZone};
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
    /// Memory pages written out to disk per second, sustained for a few seconds, that
    /// counts as active paging. A healthy machine sits near zero.
    pub paging_pages_per_sec: f32,
    /// CPU must be at least this busy for throttling to be suspected.
    pub throttle_busy_pct: f32,
    /// Speed at or below this fraction of the maximum means throttling.
    pub throttle_clock_ratio: f32,
    pub disk_heavy_bps: u64,
    /// Temperature at which throttling is blamed on heat.
    pub hot_temp_c: f32,
    /// Temperature worth reporting even without a visible slowdown.
    pub very_hot_temp_c: f32,
    /// Share of samples a condition must hold in to count (battery saver, on battery, GPU throttle).
    pub state_fraction: f32,
    pub battery_low_pct: u8,
    pub gpu_busy_pct: f32,
    pub gpu_hot_c: f32,
    /// Typical disk request time that makes a disk "slow to respond".
    pub disk_latency_ms: f32,
    pub disk_latency_bad_ms: f32,
    pub disk_queue_len: f32,
    /// A short burst: CPU averaging this over `spike_secs` consecutive samples.
    pub spike_cpu_pct: f32,
    pub spike_secs: usize,
    /// A single disk stall at least this long.
    pub spike_disk_ms: f32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            hog_cpu_pct: 25.0,
            saturated_cpu_pct: 85.0,
            mem_full_pct: 90.0,
            mem_high_pct: 80.0,
            swap_high_bytes: 1 << 30,
            paging_pages_per_sec: 200.0,
            throttle_busy_pct: 60.0,
            throttle_clock_ratio: 0.75,
            disk_heavy_bps: 80 * 1024 * 1024,
            hot_temp_c: 85.0,
            very_hot_temp_c: 90.0,
            state_fraction: 0.5,
            battery_low_pct: 15,
            gpu_busy_pct: 90.0,
            gpu_hot_c: 85.0,
            disk_latency_ms: 50.0,
            disk_latency_bad_ms: 100.0,
            disk_queue_len: 4.0,
            spike_cpu_pct: 95.0,
            spike_secs: 5,
            spike_disk_ms: 300.0,
        }
    }
}

/// A stable name for the kind of cause a finding title describes, so feedback can be
/// tallied per rule. Program names inside titles don't matter.
pub fn kind_of(title: &str) -> &'static str {
    let t = title;
    if t.contains("large share of the CPU") { "cpu_hog" }
    else if t.contains("saturated by many programs") { "cpu_saturated" }
    else if t.starts_with("Memory pressure") { "memory" }
    else if t.contains("throttling") && t.contains("CPU") { "cpu_throttle" }
    else if t.contains("Battery Saver was limiting") { "battery_saver" }
    else if t.contains("nearly empty battery") { "battery_low" }
    else if t.starts_with("GPU throttled") || t.starts_with("GPU limited") || t.starts_with("GPU slowed") { "gpu_throttle" }
    else if t.contains("GPU was maxed out") { "gpu_busy" }
    else if t.contains("GPU was running very hot") { "gpu_hot" }
    else if t.contains("machine was running very hot") { "hot" }
    else if t == "Heavy disk activity" { "disk_heavy" }
    else if t.contains("slow to respond") { "disk_slow" }
    else if t.contains("short CPU spike") { "cpu_spike" }
    else if t.contains("brief disk stall") { "disk_stall" }
    else if t.contains("machine stalled") { "stall" }
    else { "other" }
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

/// Per-program averages over a set of samples. A sample that doesn't list a
/// program counts as zero for it, so brief bursts are not overstated.
#[derive(Debug, Clone)]
struct ProgAvg {
    name: String,
    max_count: u32,
    cpu: f32,
    disk_bps: f64,
    mem_bytes: f64,
}

fn per_program(samples: &[Sample]) -> Vec<ProgAvg> {
    let n = samples.len().max(1) as f64;
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

/// Average of a sensor over the samples that have it, with how many did.
fn avg_of<F: Fn(&Sample) -> Option<f32>>(samples: &[Sample], f: F) -> Option<f32> {
    let v: Vec<f32> = samples.iter().filter_map(&f).collect();
    (!v.is_empty()).then(|| v.iter().sum::<f32>() / v.len() as f32)
}

/// Share of the samples that have a reading in which `pred` holds.
fn share_of<F: Fn(&Sample) -> Option<bool>>(samples: &[Sample], f: F) -> Option<f32> {
    let v: Vec<bool> = samples.iter().filter_map(&f).collect();
    (!v.is_empty()).then(|| v.iter().filter(|b| **b).count() as f32 / v.len() as f32)
}

fn hms(ts: i64) -> String {
    Local.timestamp_opt(ts, 0).single().map_or_else(|| ts.to_string(), |t| t.format("%H:%M:%S").to_string())
}

/// Finds the run of `k` consecutive samples (no gaps, so thinned history is
/// skipped) with the highest average of `f`. Returns the start index and average.
fn rolling_peak<F: Fn(&Sample) -> Option<f32>>(samples: &[Sample], k: usize, f: F) -> Option<(usize, f32)> {
    if k == 0 || samples.len() < k {
        return None;
    }
    let max_span = 2 * (k as i64 - 1);
    let mut best: Option<(usize, f32)> = None;
    for i in 0..=samples.len() - k {
        let w = &samples[i..i + k];
        if w[k - 1].ts - w[0].ts > max_span {
            continue;
        }
        let vals: Option<Vec<f32>> = w.iter().map(&f).collect();
        let Some(vals) = vals else { continue };
        let m = vals.iter().sum::<f32>() / k as f32;
        if best.map_or(true, |(_, b)| m > b) {
            best = Some((i, m));
        }
    }
    best
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

    // Context shared by several rules.
    let on_battery = share_of(samples, |s| s.sensors.on_ac.map(|a| !a)).is_some_and(|f| f >= 0.8);
    let saver_on = share_of(samples, |s| s.sensors.battery_saver).is_some_and(|f| f >= t.state_fraction);
    let battery_pct = avg_of(samples, |s| s.sensors.battery_pct.map(f32::from));
    let temp = avg_of(samples, |s| s.sensors.temp_c);
    let mut saver_explained = false;
    let mut heat_explained = false;

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
    let saturated = hogs == 0 && avg_cpu >= t.saturated_cpu_pct;
    if saturated {
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
    // High memory plus a big pagefile is only a problem if the system is actually
    // paging. When that is measured, require it, so a machine that just sits at 80%
    // with swap allocated doesn't get blamed in every explanation.
    let paging_measured = avg_of(samples, |s| s.sensors.page_out).is_some();
    let page_peak = rolling_peak(samples, 5, |s| s.sensors.page_out)
        .map(|(_, p)| p)
        .or_else(|| samples.iter().filter_map(|s| s.sensors.page_out).fold(None, |m: Option<f32>, v| Some(m.map_or(v, |m| m.max(v)))));
    let paging_active = page_peak.is_some_and(|p| p >= t.paging_pages_per_sec);
    let full = mem_pct >= t.mem_full_pct;
    let paging = mem_pct >= t.mem_high_pct && swap >= t.swap_high_bytes as f64 && (!paging_measured || paging_active);
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
        match (paging_active, page_peak) {
            (true, Some(p)) => evidence.push(format!("the system was pushing up to {:.0} MB/s of memory out to disk", p as f64 * 4.0 / 1024.0)),
            (false, _) if paging_measured => evidence.push("no heavy paging was measured, so the real effect may be small".into()),
            _ => {}
        }
        out.push(Finding {
            title: "Memory pressure, the machine was short on RAM".into(),
            evidence,
            confidence: if (full && swap >= t.swap_high_bytes as f64) || paging_active {
                Confidence::High
            } else {
                Confidence::Medium
            },
            hint,
            score: 58.0 + mem_pct * 0.2,
        });
    }

    // 4. CPU throttling. Prefer the OS's "percent of maximum frequency"; fall back
    // to comparing against the fastest clock ever recorded.
    let speed = avg_of(samples, |s| s.sensors.freq_pct).map(|f| {
        (f / 100.0, format!("CPU ran at {f:.0}% of its rated maximum speed"), true)
    });
    let speed = speed.or_else(|| {
        let clocks: Vec<f64> = samples.iter().filter(|s| s.clock_mhz > 0).map(|s| s.clock_mhz as f64).collect();
        if best_mhz == 0 || clocks.is_empty() {
            return None;
        }
        let avg = clocks.iter().sum::<f64>() / clocks.len() as f64;
        let ratio = (avg / best_mhz as f64) as f32;
        Some((ratio, format!("clock averaged {avg:.0} MHz, {:.0}% of the best seen ({best_mhz} MHz)", ratio * 100.0), false))
    });
    if let Some((ratio, speed_line, measured)) = speed {
        if avg_cpu >= t.throttle_busy_pct && ratio <= t.throttle_clock_ratio {
            let mut evidence = vec![format!("CPU was {avg_cpu:.0}% busy"), speed_line];
            if let Some(c) = temp {
                evidence.push(format!("temperature sensor averaged {c:.0}Â°C"));
            }
            let (title, confidence, hint, score);
            if temp.is_some_and(|c| c >= t.hot_temp_c) {
                heat_explained = true;
                title = "CPU throttling, likely from heat";
                confidence = Confidence::Medium;
                hint = Some("Check the fans and vents for dust, use a hard flat surface or a cooling pad, and close heavy background programs.");
                score = 56.0;
            } else if saver_on {
                saver_explained = true;
                evidence.push("Battery Saver was on".into());
                title = "CPU throttling, Battery Saver is limiting performance";
                confidence = Confidence::Medium;
                hint = Some("Turn Battery Saver off (Settings > System > Power & battery) or plug in.");
                score = 56.0;
            } else if on_battery {
                evidence.push(match battery_pct {
                    Some(b) => format!("running on battery at {b:.0}%"),
                    None => "running on battery".into(),
                });
                title = "CPU throttling, likely battery power limits";
                confidence = Confidence::Medium;
                hint = Some("Plug in, and set the power mode to Best performance (Settings > System > Power & battery).");
                score = 54.0;
            } else {
                if !measured {
                    evidence.push("inferred from clock speed only; can't tell heat from power limits".into());
                }
                title = "Probable CPU throttling";
                confidence = Confidence::Low;
                hint = None;
                score = 40.0 + (1.0 - ratio) * 20.0;
            }
            out.push(Finding {
                title: title.into(),
                evidence,
                confidence,
                hint: hint.map(str::to_string),
                score,
            });
        }
    }

    // 5. Battery Saver on while the machine was working (when not already blamed above).
    if saver_on && !saver_explained && avg_cpu >= 30.0 {
        let mut evidence = vec!["Windows Battery Saver was on".into(), machine_line.clone()];
        if let Some(b) = battery_pct {
            evidence.push(format!("battery at {b:.0}%"));
        }
        out.push(Finding {
            title: "Battery Saver was limiting performance".into(),
            evidence,
            confidence: Confidence::Low,
            hint: Some("Turn Battery Saver off (Settings > System > Power & battery) or plug in.".into()),
            score: 38.0,
        });
    } else if on_battery && !saver_on && !saver_explained && avg_cpu >= 30.0 && battery_pct.is_some_and(|b| b <= t.battery_low_pct as f32) {
        out.push(Finding {
            title: "Running on a nearly empty battery".into(),
            evidence: vec![
                format!("battery averaged {:.0}%", battery_pct.unwrap_or(0.0)),
                "not plugged in".into(),
                machine_line.clone(),
            ],
            confidence: Confidence::Low,
            hint: Some("Windows cuts performance on a low battery. Plug in.".into()),
            score: 30.0,
        });
    }

    // 6. Running hot, when it wasn't already the explanation for throttling.
    if let Some(c) = temp.filter(|c| *c >= t.very_hot_temp_c && !heat_explained) {
        out.push(Finding {
            title: "The machine was running very hot".into(),
            evidence: vec![
                format!("temperature sensor averaged {c:.0}Â°C"),
                "the sensor is a system thermal zone, which may not be the CPU core itself".into(),
            ],
            confidence: Confidence::Low,
            hint: Some("Check the fans and vents for dust, and use a hard flat surface or a cooling pad.".into()),
            score: 42.0,
        });
    }

    // 7. GPU: throttled, hot, or simply saturated.
    let gpu = avg_of(samples, |s| s.sensors.gpu_pct);
    let gpu_temp = avg_of(samples, |s| s.sensors.gpu_temp_c);
    let thr_share = |bit: u8| share_of(samples, |s| s.sensors.gpu_throttle.map(|g| g & bit != 0)).unwrap_or(0.0);
    let (thermal, power, hw) = (thr_share(GPU_THERMAL), thr_share(GPU_POWER), thr_share(GPU_HW));
    let gpu_throttled = thermal.max(power).max(hw) >= t.state_fraction && gpu.is_some_and(|g| g >= 50.0);
    let gpu_hot = gpu_temp.is_some_and(|c| c >= t.gpu_hot_c);
    if gpu_throttled {
        let mut evidence = vec![format!("GPU averaged {:.0}% busy", gpu.unwrap_or(0.0))];
        let (title, hint) = if thermal >= t.state_fraction || (gpu_hot && hw >= t.state_fraction) {
            evidence.push(format!("GPU reported thermal throttling in {:.0}% of samples", thermal.max(hw) * 100.0));
            (
                "GPU throttled by heat",
                "Improve airflow, use a cooling pad, or lower graphics settings and frame-rate caps.",
            )
        } else if power >= t.state_fraction {
            evidence.push(format!("GPU reported a power cap in {:.0}% of samples", power * 100.0));
            if on_battery {
                evidence.push("running on battery".into());
            }
            (
                "GPU limited by its power budget",
                "Plug in the charger and set the power mode to Best performance. Laptops cut GPU power on battery.",
            )
        } else {
            evidence.push(format!("GPU reported a hardware slowdown in {:.0}% of samples", hw * 100.0));
            ("GPU slowed by a hardware limit (heat or power)", "Plug in, check cooling, and lower graphics settings.")
        };
        if let Some(c) = gpu_temp {
            evidence.push(format!("GPU temperature averaged {c:.0}Â°C"));
        }
        out.push(Finding {
            title: title.into(),
            evidence,
            confidence: Confidence::Medium,
            hint: Some(hint.into()),
            score: 57.0,
        });
    } else if gpu.is_some_and(|g| g >= t.gpu_busy_pct) {
        let mut evidence = vec![format!("GPU averaged {:.0}% busy", gpu.unwrap_or(0.0))];
        if let Some(c) = gpu_temp {
            evidence.push(format!("GPU temperature averaged {c:.0}Â°C"));
        }
        out.push(Finding {
            title: "The GPU was maxed out".into(),
            evidence,
            confidence: Confidence::Medium,
            hint: Some("Lower graphics settings or resolution, or close other GPU-heavy programs (games, video, browser tabs).".into()),
            score: 52.0,
        });
    } else if gpu_hot {
        out.push(Finding {
            title: "The GPU was running very hot".into(),
            evidence: vec![format!("GPU temperature averaged {:.0}Â°C", gpu_temp.unwrap_or(0.0))],
            confidence: Confidence::Low,
            hint: Some("Improve airflow or use a cooling pad.".into()),
            score: 41.0,
        });
    }

    // 8. Heavy disk activity (throughput).
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

    // 9. Slow disk: requests took a long time even if little data moved. Seconds
    // with no I/O read as zero, so only count seconds where the disk was used.
    let busy_latencies: Vec<f32> = samples.iter().filter_map(|s| s.sensors.disk_latency_ms).filter(|l| *l > 0.0).collect();
    let queue = avg_of(samples, |s| s.sensors.disk_queue);
    let lat_avg = (busy_latencies.len() >= 3).then(|| busy_latencies.iter().sum::<f32>() / busy_latencies.len() as f32);
    let slow_disk = lat_avg.is_some_and(|l| l >= t.disk_latency_ms);
    if slow_disk || queue.is_some_and(|q| q >= t.disk_queue_len) {
        let mut evidence = Vec::new();
        if let Some(l) = lat_avg {
            evidence.push(format!("disk requests took {l:.0} ms on average (healthy is a few ms)"));
        }
        if let Some(q) = queue {
            evidence.push(format!("average of {q:.1} requests waiting in the queue"));
        }
        if disk > 0.0 {
            evidence.push(format!("only {:.0} MB/s was moving", disk / MB));
        }
        let top = progs.iter().max_by(|a, b| a.disk_bps.total_cmp(&b.disk_bps)).filter(|p| p.disk_bps > 0.0);
        let mut hint = None;
        if let Some(p) = top {
            evidence.push(format!("most I/O: {} at {:.1} MB/s", label(p), p.disk_bps / MB));
            hint = hint_for(&p.name).map(str::to_string);
        }
        out.push(Finding {
            title: "The disk was slow to respond".into(),
            evidence,
            confidence: if lat_avg.is_some_and(|l| l >= t.disk_latency_bad_ms) { Confidence::High } else { Confidence::Medium },
            hint: hint.or_else(|| Some("Programs were waiting on the disk. Check that it isn't nearly full, and look for a failing drive.".into())),
            score: 54.0 + lat_avg.unwrap_or(0.0).min(200.0) / 20.0,
        });
    }

    // 10. Short spikes the window average hides.
    if hogs == 0 && !saturated {
        if let Some((i, peak)) = rolling_peak(samples, t.spike_secs, |s| Some(s.cpu_pct)).filter(|(_, p)| *p >= t.spike_cpu_pct) {
            let w = &samples[i..i + t.spike_secs];
            let mut top = per_program(w);
            top.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
            let mut evidence = vec![
                format!("{} to {}: CPU averaged {:.0}%", hms(w[0].ts), hms(w[w.len() - 1].ts), peak),
                format!("the whole window averaged only {avg_cpu:.0}%, so the burst is easy to miss"),
            ];
            let mut hint = Some(format!("Run `bb why {} --span 30s` to zoom in.", hms(w[w.len() / 2].ts)));
            if let Some(p) = top.first().filter(|p| p.cpu > 5.0) {
                evidence.push(format!("busiest then: {} at {:.0}%", label(p), p.cpu));
                if let Some(h) = hint_for(&p.name) {
                    hint = Some(format!("{h} {}", hint.unwrap_or_default()));
                }
            }
            out.push(Finding {
                title: format!("A short CPU spike around {}", hms(w[w.len() / 2].ts)),
                evidence,
                confidence: Confidence::Medium,
                hint,
                score: 45.0,
            });
        }
    }
    if !slow_disk {
        let worst = samples
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.sensors.disk_latency_ms.map(|l| (i, l)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .filter(|(_, l)| *l >= t.spike_disk_ms);
        if let Some((i, l)) = worst {
            let s = &samples[i];
            let mut evidence = vec![format!("disk requests took {l:.0} ms at {} (healthy is a few ms)", hms(s.ts))];
            if let Some(p) = s.procs.iter().max_by_key(|p| p.disk_bps).filter(|p| p.disk_bps > 0) {
                evidence.push(format!("most I/O then: {}", p.name));
            }
            out.push(Finding {
                title: format!("A brief disk stall around {}", hms(s.ts)),
                evidence,
                confidence: Confidence::Medium,
                hint: Some(format!("Run `bb why {} --span 30s` to zoom in.", hms(s.ts))),
                score: 44.0,
            });
        }
    }

    // 11. A hole in the recording. The recorder samples every second, so a gap
    // means the whole machine, recorder included, was stalled (or asleep).
    if let Some((from, to)) = largest_gap(samples) {
        let secs = to - from;
        out.push(Finding {
            title: format!("The machine stalled around {}", hms(from)),
            evidence: vec![
                format!("nothing was recorded between {} and {} ({secs} s)", hms(from), hms(to)),
                "the recorder samples every second, so the whole machine, including the recorder, was stuck".into(),
                "a laptop that went to sleep briefly also looks like this".into(),
            ],
            confidence: if secs <= 20 { Confidence::Medium } else { Confidence::Low },
            hint: Some(format!("Look at what happened just before: `bb why {} --span 30s`.", hms(from))),
            score: 46.0,
        });
    }

    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    out
}

/// The biggest gap (5 to 60 s) between consecutive samples, if the window was
/// recorded at full resolution. Longer gaps are sleep or a stopped recorder, and
/// thinned history has gaps by design.
fn largest_gap(samples: &[Sample]) -> Option<(i64, i64)> {
    let diffs: Vec<i64> = samples.windows(2).map(|w| w[1].ts - w[0].ts).collect();
    if diffs.is_empty() {
        return None;
    }
    let mut sorted = diffs.clone();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    if median > 2 {
        return None;
    }
    let min_gap = 5.max(median * 3);
    let (i, gap) = diffs.iter().enumerate().max_by_key(|(_, d)| **d).map(|(i, d)| (i, *d))?;
    (gap >= min_gap && gap <= 60).then(|| (samples[i].ts, samples[i + 1].ts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProcRow, Sensors};

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
    fn window(mut tweak: impl FnMut(&mut Sample)) -> Vec<Sample> {
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
                    sensors: Sensors::default(),
                    procs: vec![proc("code.exe", 6, 4.0, 1, 1.0)],
                };
                tweak(&mut s);
                s
            })
            .collect()
    }

    /// Like `window`, but `tweak` also gets the index, to build bursts.
    fn window_i(tweak: impl Fn(usize, &mut Sample)) -> Vec<Sample> {
        let mut i = 0;
        window(|s| {
            tweak(i, s);
            i += 1;
        })
    }

    fn run(w: &[Sample]) -> Vec<Finding> {
        analyze(w, 3600, &Thresholds::default())
    }

    fn titles(f: &[Finding]) -> Vec<&str> {
        f.iter().map(|f| f.title.as_str()).collect()
    }

    #[test]
    fn healthy_machine_has_no_findings() {
        assert!(run(&window(|_| {})).is_empty());
    }

    #[test]
    fn healthy_machine_with_all_sensors_has_no_findings() {
        let w = window(|s| {
            s.sensors = Sensors {
                on_ac: Some(true),
                battery_pct: Some(90),
                battery_saver: Some(false),
                temp_c: Some(55.0),
                freq_pct: Some(70.0),
                gpu_pct: Some(5.0),
                gpu_temp_c: Some(45.0),
                gpu_throttle: Some(0),
                disk_queue: Some(0.1),
                disk_latency_ms: Some(1.5),
                page_out: Some(0.0),
            }
        });
        assert!(run(&w).is_empty(), "{:?}", titles(&run(&w)));
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
        let w = window_i(|i, s| {
            if i < 10 {
                s.procs.push(proc("burst.exe", 1, 50.0, 0, 0.1));
            }
        });
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
    fn static_swap_without_paging_is_not_blamed() {
        // 81% full with 2 GB of swap allocated, but nothing is being paged out.
        let w = window(|s| {
            s.mem_used = 13 << 30;
            s.swap_used = 2 << 30;
            s.sensors.page_out = Some(0.0);
        });
        assert!(run(&w).is_empty(), "{:?}", titles(&run(&w)));
    }

    #[test]
    fn high_memory_with_active_paging_is_reported() {
        let w = window(|s| {
            s.mem_used = 13 << 30;
            s.swap_used = 2 << 30;
            s.sensors.page_out = Some(3000.0);
        });
        let f = run(&w);
        assert_eq!(f.len(), 1, "{:?}", titles(&f));
        assert!(f[0].title.contains("Memory pressure"));
        assert!(f[0].evidence.iter().any(|e| e.contains("MB/s")));
        assert_eq!(f[0].confidence, Confidence::High);
    }

    #[test]
    fn full_memory_without_paging_is_reported_with_a_caveat() {
        let w = window(|s| {
            s.mem_used = 15 << 30; // 94%
            s.sensors.page_out = Some(0.0);
        });
        let f = run(&w);
        assert_eq!(f.len(), 1, "{:?}", titles(&f));
        assert!(f[0].evidence.iter().any(|e| e.contains("no heavy paging")));
    }

    #[test]
    fn a_short_paging_burst_counts() {
        // 6 seconds of heavy paging inside a minute of 82% memory with swap.
        let w = window_i(|i, s| {
            s.mem_used = 13 << 30;
            s.swap_used = 2 << 30;
            s.sensors.page_out = Some(if (20..26).contains(&i) { 5000.0 } else { 0.0 });
        });
        assert_eq!(run(&w).len(), 1);
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

    // ---- sensors: power, heat, GPU, disk latency, spikes ----

    /// A busy CPU with two mid-sized programs, so no single hog and no saturation.
    fn busy(s: &mut Sample) {
        s.cpu_pct = 70.0;
        s.procs = vec![proc("a.exe", 1, 20.0, 0, 0.1), proc("b.exe", 1, 20.0, 0, 0.1)];
    }

    #[test]
    fn measured_throttle_on_battery_blames_power_limits() {
        let w = window(|s| {
            busy(s);
            s.sensors = Sensors { freq_pct: Some(48.0), on_ac: Some(false), battery_pct: Some(55), battery_saver: Some(false), ..Default::default() };
        });
        let f = run(&w);
        assert_eq!(f.len(), 1, "{:?}", titles(&f));
        assert!(f[0].title.contains("battery power limits"));
        assert_eq!(f[0].confidence, Confidence::Medium);
        assert!(f[0].hint.as_ref().unwrap().contains("Plug in"));
        assert!(f[0].evidence.iter().any(|e| e.contains("48%")));
    }

    #[test]
    fn measured_throttle_when_hot_blames_heat() {
        let w = window(|s| {
            busy(s);
            s.sensors = Sensors { freq_pct: Some(50.0), temp_c: Some(92.0), on_ac: Some(true), ..Default::default() };
        });
        let f = run(&w);
        // Heat explains the throttling, so there is no separate "very hot" finding.
        assert_eq!(f.len(), 1, "{:?}", titles(&f));
        assert!(f[0].title.contains("heat"));
        assert!(f[0].evidence.iter().any(|e| e.contains("92")));
    }

    #[test]
    fn measured_throttle_with_battery_saver_blames_saver() {
        let w = window(|s| {
            busy(s);
            s.sensors = Sensors { freq_pct: Some(40.0), battery_saver: Some(true), on_ac: Some(false), ..Default::default() };
        });
        let f = run(&w);
        assert_eq!(f.len(), 1, "{:?}", titles(&f));
        assert!(f[0].title.contains("Battery Saver"));
    }

    #[test]
    fn measured_throttle_with_no_cause_is_low_confidence() {
        let w = window(|s| {
            busy(s);
            s.sensors = Sensors { freq_pct: Some(50.0), on_ac: Some(true), temp_c: Some(60.0), ..Default::default() };
        });
        let f = run(&w);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].confidence, Confidence::Low);
    }

    #[test]
    fn measured_frequency_beats_the_clock_guess() {
        // The clock looks throttled, but the OS says the CPU ran at full speed.
        let w = window(|s| {
            busy(s);
            s.clock_mhz = 1500;
            s.sensors.freq_pct = Some(100.0);
        });
        assert!(run(&w).is_empty());
    }

    #[test]
    fn idle_low_frequency_is_not_throttling() {
        assert!(run(&window(|s| s.sensors.freq_pct = Some(30.0))).is_empty());
    }

    #[test]
    fn battery_saver_while_working_is_reported() {
        let w = window(|s| {
            s.cpu_pct = 40.0;
            s.sensors = Sensors { battery_saver: Some(true), on_ac: Some(false), battery_pct: Some(70), ..Default::default() };
        });
        let f = run(&w);
        assert_eq!(f.len(), 1);
        assert!(f[0].title.contains("Battery Saver"));
        assert!(f[0].hint.as_ref().unwrap().contains("Settings"));
    }

    #[test]
    fn battery_saver_while_idle_is_not_reported() {
        let w = window(|s| s.sensors = Sensors { battery_saver: Some(true), on_ac: Some(false), ..Default::default() });
        assert!(run(&w).is_empty());
    }

    #[test]
    fn nearly_empty_battery_is_low_confidence() {
        let w = window(|s| {
            s.cpu_pct = 40.0;
            s.sensors = Sensors { on_ac: Some(false), battery_pct: Some(9), battery_saver: Some(false), ..Default::default() };
        });
        let f = run(&w);
        assert_eq!(f.len(), 1);
        assert!(f[0].title.contains("nearly empty"));
        assert_eq!(f[0].confidence, Confidence::Low);
    }

    #[test]
    fn plugged_in_machine_gets_no_battery_findings() {
        let w = window(|s| {
            s.cpu_pct = 40.0;
            s.sensors = Sensors { on_ac: Some(true), battery_pct: Some(5), battery_saver: Some(false), ..Default::default() };
        });
        assert!(run(&w).is_empty());
    }

    #[test]
    fn very_hot_machine_without_throttling_is_flagged() {
        let w = window(|s| s.sensors.temp_c = Some(95.0));
        let f = run(&w);
        assert_eq!(f.len(), 1);
        assert!(f[0].title.contains("very hot"));
    }

    #[test]
    fn gpu_maxed_out() {
        let w = window(|s| s.sensors = Sensors { gpu_pct: Some(98.0), gpu_temp_c: Some(70.0), gpu_throttle: Some(0), ..Default::default() });
        let f = run(&w);
        assert_eq!(f.len(), 1);
        assert!(f[0].title.contains("GPU was maxed out"));
    }

    #[test]
    fn gpu_thermal_throttle_beats_plain_saturation() {
        let w = window(|s| {
            s.sensors = Sensors { gpu_pct: Some(99.0), gpu_temp_c: Some(88.0), gpu_throttle: Some(GPU_THERMAL), ..Default::default() }
        });
        let f = run(&w);
        assert_eq!(f.len(), 1, "{:?}", titles(&f));
        assert!(f[0].title.contains("GPU throttled by heat"));
        assert!(f[0].evidence.iter().any(|e| e.contains("88")));
    }

    #[test]
    fn gpu_power_cap_on_battery() {
        let w = window(|s| {
            s.sensors = Sensors { gpu_pct: Some(80.0), gpu_throttle: Some(GPU_POWER), on_ac: Some(false), ..Default::default() }
        });
        let f = run(&w);
        assert_eq!(f.len(), 1, "{:?}", titles(&f));
        assert!(f[0].title.contains("power budget"));
        assert!(f[0].evidence.iter().any(|e| e.contains("battery")));
    }

    #[test]
    fn idle_gpu_with_throttle_flags_is_not_reported() {
        // Drivers report power caps at idle all the time.
        let w = window(|s| s.sensors = Sensors { gpu_pct: Some(3.0), gpu_throttle: Some(GPU_POWER), ..Default::default() });
        assert!(run(&w).is_empty());
    }

    #[test]
    fn slow_disk_with_little_data_moving() {
        let w = window(|s| {
            s.disk_bps = 1 << 20;
            s.sensors = Sensors { disk_latency_ms: Some(180.0), disk_queue: Some(6.0), ..Default::default() };
            s.procs.push(proc("MsMpEng.exe", 1, 3.0, 5, 0.3));
        });
        let f = run(&w);
        assert_eq!(f.len(), 1, "{:?}", titles(&f));
        assert!(f[0].title.contains("slow to respond"));
        assert_eq!(f[0].confidence, Confidence::High);
        assert!(f[0].evidence.iter().any(|e| e.contains("180")));
        assert!(f[0].hint.as_ref().unwrap().contains("Exclusions"));
    }

    #[test]
    fn idle_seconds_do_not_dilute_disk_latency() {
        // Disk used in 20 of 60 seconds, slow each time. The idle seconds read 0 ms.
        let w = window_i(|i, s| s.sensors.disk_latency_ms = Some(if i % 3 == 0 { 120.0 } else { 0.0 }));
        let f = run(&w);
        assert!(titles(&f).iter().any(|t| t.contains("slow to respond")), "{:?}", titles(&f));
    }

    #[test]
    fn healthy_disk_latency_is_quiet() {
        let w = window(|s| s.sensors = Sensors { disk_latency_ms: Some(2.0), disk_queue: Some(0.2), ..Default::default() });
        assert!(run(&w).is_empty());
    }

    #[test]
    fn short_cpu_spike_is_found_in_a_calm_window() {
        // 6 seconds at 99% inside 60. The average is ~19%.
        let w = window_i(|i, s| {
            if (30..36).contains(&i) {
                s.cpu_pct = 99.0;
                s.procs.push(proc("build.exe", 4, 80.0, 0, 0.5));
            }
        });
        let f = run(&w);
        assert_eq!(f.len(), 1, "{:?}", titles(&f));
        assert!(f[0].title.contains("short CPU spike"));
        assert!(f[0].evidence.iter().any(|e| e.contains("build.exe")));
        assert!(f[0].hint.as_ref().unwrap().contains("--span 30s"));
    }

    #[test]
    fn two_second_blip_is_not_a_spike() {
        let w = window_i(|i, s| {
            if (30..32).contains(&i) {
                s.cpu_pct = 100.0;
            }
        });
        assert!(run(&w).is_empty());
    }

    #[test]
    fn spike_is_not_reported_when_a_hog_already_explains_the_window() {
        let w = window(|s| {
            s.cpu_pct = 99.0;
            s.procs.push(proc("hog.exe", 1, 90.0, 0, 0.1));
        });
        let f = run(&w);
        assert_eq!(f.len(), 1);
        assert!(!f[0].title.contains("spike"));
    }

    #[test]
    fn thinned_history_cannot_fake_a_spike() {
        // Samples 10 s apart (old, thinned data): five of them are not a 5 s burst.
        let w: Vec<Sample> = (0..30)
            .map(|i| Sample { ts: 1000 + i * 10, cpu_pct: if (10..15).contains(&i) { 99.0 } else { 10.0 }, mem_total: 16 << 30, ..Default::default() })
            .collect();
        assert!(run(&w).is_empty(), "{:?}", titles(&run(&w)));
    }

    #[test]
    fn brief_disk_stall_is_found() {
        let w = window_i(|i, s| {
            s.sensors.disk_latency_ms = Some(if i == 40 { 650.0 } else { 3.0 });
        });
        let f = run(&w);
        assert_eq!(f.len(), 1, "{:?}", titles(&f));
        assert!(f[0].title.contains("disk stall"));
        assert!(f[0].evidence[0].contains("650"));
    }

    #[test]
    fn every_rule_title_maps_to_a_kind() {
        // Build each finding through the real rules and check none falls into "other".
        let cases: Vec<Vec<Sample>> = vec![
            window(|s| { s.cpu_pct = 70.0; s.procs.push(proc("x.exe", 1, 55.0, 1, 0.1)); }),
            window(|s| { s.cpu_pct = 95.0; s.procs = (0..5).map(|i| proc(&format!("a{i}.exe"), 1, 18.0, 0, 0.2)).collect(); }),
            window(|s| { s.mem_used = 15 << 30; s.swap_used = 3 << 30; }),
            window(|s| { busy(s); s.sensors.freq_pct = Some(50.0); s.sensors.temp_c = Some(60.0); }),
            window(|s| { busy(s); s.sensors.freq_pct = Some(50.0); s.sensors.battery_saver = Some(true); }),
            window(|s| { s.cpu_pct = 40.0; s.sensors = Sensors { on_ac: Some(false), battery_pct: Some(9), ..Default::default() }; }),
            window(|s| s.sensors.temp_c = Some(95.0)),
            window(|s| s.sensors = Sensors { gpu_pct: Some(99.0), gpu_throttle: Some(GPU_THERMAL), gpu_temp_c: Some(88.0), ..Default::default() }),
            window(|s| s.sensors.gpu_pct = Some(98.0)),
            window(|s| s.sensors.gpu_temp_c = Some(90.0)),
            window(|s| { s.disk_bps = 200 << 20; s.procs.push(proc("c.exe", 1, 3.0, 190, 0.1)); }),
            window(|s| s.sensors.disk_latency_ms = Some(180.0)),
            window_i(|i, s| { if (30..36).contains(&i) { s.cpu_pct = 99.0; } }),
            window_i(|i, s| s.sensors.disk_latency_ms = Some(if i == 40 { 650.0 } else { 3.0 })),
            (0..60i64).filter(|i| !(30..38).contains(i)).map(|i| Sample { ts: 1000 + i, mem_total: 16 << 30, ..Default::default() }).collect(),
        ];
        let mut kinds = std::collections::BTreeSet::new();
        for w in &cases {
            let f = run(w);
            assert!(!f.is_empty());
            for x in f {
                let k = kind_of(&x.title);
                assert_ne!(k, "other", "unmapped title: {}", x.title);
                kinds.insert(k);
            }
        }
        assert!(kinds.len() >= 12, "only exercised {kinds:?}");
    }

    #[test]
    fn rolling_peak_needs_contiguous_samples() {
        let w: Vec<Sample> = (0..4).map(|i| Sample { ts: i * 10, cpu_pct: 99.0, ..Default::default() }).collect();
        assert!(rolling_peak(&w, 3, |s| Some(s.cpu_pct)).is_none());
        let w: Vec<Sample> = (0..4).map(|i| Sample { ts: i, cpu_pct: 99.0, ..Default::default() }).collect();
        assert_eq!(rolling_peak(&w, 3, |s| Some(s.cpu_pct)).map(|(_, v)| v), Some(99.0));
    }

    #[test]
    fn a_hole_in_the_recording_is_reported() {
        let w: Vec<Sample> = (0..60i64)
            .filter(|i| !(30..38).contains(i))
            .map(|i| Sample { ts: 1000 + i, cpu_pct: 8.0, mem_total: 16 << 30, ..Default::default() })
            .collect();
        let f = run(&w);
        assert_eq!(f.len(), 1, "{:?}", titles(&f));
        assert!(f[0].title.contains("stalled"));
        assert!(f[0].evidence[0].contains("9 s"));
        assert_eq!(f[0].confidence, Confidence::Medium);
    }

    #[test]
    fn small_gaps_sleep_and_thinned_history_are_not_stalls() {
        let make = |ts: Vec<i64>| -> Vec<Sample> {
            ts.into_iter().map(|t| Sample { ts: t, cpu_pct: 8.0, mem_total: 16 << 30, ..Default::default() }).collect()
        };
        // 3 s gap: just a late sample.
        assert!(run(&make((0..30).filter(|i| ![10, 11].contains(i)).collect())).is_empty());
        // 10 minutes: asleep or stopped, not a stall.
        assert!(run(&make((0..30).chain(630..660).collect())).is_empty());
        // Thinned history sampled every 10 s.
        assert!(run(&make((0..30).map(|i| i * 10).collect())).is_empty());
    }
}
