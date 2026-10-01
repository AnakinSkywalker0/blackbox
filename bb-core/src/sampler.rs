//! Reads CPU, memory, disk and per-program usage from the OS.

use crate::model::{ProcRow, Sample};
use std::collections::HashMap;
use std::time::Instant;
use sysinfo::{ProcessesToUpdate, System};

pub struct Sampler {
    sys: System,
    top_n: usize,
    last: Instant,
}

impl Sampler {
    /// Creates a sampler and primes the CPU counters. The first `sample()` after
    /// a short wait gives real numbers.
    pub fn new(top_n: usize) -> Sampler {
        let mut sys = System::new();
        sys.refresh_cpu_all();
        sys.refresh_memory();
        sys.refresh_processes(ProcessesToUpdate::All, true);
        Sampler { sys, top_n: top_n.max(1), last: Instant::now() }
    }

    /// Takes one sample stamped with unix time `ts`.
    pub fn sample(&mut self, ts: i64) -> Sample {
        let elapsed = self.last.elapsed().as_secs_f64().max(0.05);
        self.last = Instant::now();
        self.sys.refresh_cpu_all();
        self.sys.refresh_memory();
        self.sys.refresh_processes(ProcessesToUpdate::All, true);

        let ncpu = self.sys.cpus().len().max(1) as f32;
        let clock_mhz = {
            let c = self.sys.cpus();
            if c.is_empty() { 0 } else { (c.iter().map(|c| c.frequency()).sum::<u64>() / c.len() as u64) as u32 }
        };

        let (procs, disk_total) = group_processes(
            self.sys.processes().values().map(|p| RawProc {
                name: p.name().to_string_lossy().into_owned(),
                cpu: p.cpu_usage() / ncpu,
                disk_bytes: {
                    let d = p.disk_usage();
                    d.read_bytes + d.written_bytes
                },
                mem: p.memory(),
            }),
            elapsed,
        );

        Sample {
            ts,
            cpu_pct: self.sys.global_cpu_usage().clamp(0.0, 100.0),
            mem_used: self.sys.used_memory(),
            mem_total: self.sys.total_memory(),
            swap_used: self.sys.used_swap(),
            clock_mhz,
            disk_bps: disk_total,
            procs: top_programs(procs, self.top_n),
        }
    }

    /// Takes a sample after `wait` and returns it, for one-off views like `bb top`.
    pub fn sample_after(&mut self, wait: std::time::Duration, ts: i64) -> Sample {
        std::thread::sleep(wait);
        self.sample(ts)
    }
}

pub struct RawProc {
    pub name: String,
    /// Share of total machine CPU, 0..100.
    pub cpu: f32,
    pub disk_bytes: u64,
    pub mem: u64,
}

/// Groups processes by name. Returns the programs and total disk bytes/sec.
pub fn group_processes(raw: impl Iterator<Item = RawProc>, elapsed_secs: f64) -> (Vec<ProcRow>, u64) {
    let mut map: HashMap<String, (u32, f32, u64, u64)> = HashMap::new();
    for p in raw {
        let e = map.entry(p.name).or_default();
        e.0 += 1;
        e.1 += p.cpu;
        e.2 += p.disk_bytes;
        e.3 += p.mem;
    }
    let mut total = 0u64;
    let rows = map
        .into_iter()
        .map(|(name, (count, cpu, disk, mem))| {
            let bps = (disk as f64 / elapsed_secs) as u64;
            total += bps;
            ProcRow { name, count, cpu_pct: cpu.max(0.0), disk_bps: bps, mem_bytes: mem }
        })
        .collect();
    (rows, total)
}

/// Keeps the union of the top `n` programs by CPU, disk and memory.
pub fn top_programs(rows: Vec<ProcRow>, n: usize) -> Vec<ProcRow> {
    let mut keep = vec![false; rows.len()];
    let mut idx: Vec<usize> = (0..rows.len()).collect();
    type Key = fn(&ProcRow) -> f64;
    let keys: [Key; 3] = [|r| r.cpu_pct as f64, |r| r.disk_bps as f64, |r| r.mem_bytes as f64];
    for key in keys {
        idx.sort_by(|&a, &b| key(&rows[b]).total_cmp(&key(&rows[a])));
        for &i in idx.iter().take(n) {
            if key(&rows[i]) > 0.0 {
                keep[i] = true;
            }
        }
    }
    rows.into_iter().zip(keep).filter_map(|(r, k)| k.then_some(r)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(name: &str, cpu: f32, disk: u64, mem: u64) -> RawProc {
        RawProc { name: name.into(), cpu, disk_bytes: disk, mem }
    }

    #[test]
    fn groups_by_name() {
        let (rows, total) = group_processes(
            vec![raw("chrome.exe", 5.0, 1000, 10), raw("chrome.exe", 7.0, 1000, 20), raw("a", 1.0, 0, 5)].into_iter(),
            1.0,
        );
        let c = rows.iter().find(|r| r.name == "chrome.exe").unwrap();
        assert_eq!((c.count, c.cpu_pct, c.disk_bps, c.mem_bytes), (2, 12.0, 2000, 30));
        assert_eq!(total, 2000);
    }

    #[test]
    fn disk_rate_uses_elapsed() {
        let (_, total) = group_processes(vec![raw("a", 0.0, 4000, 1)].into_iter(), 2.0);
        assert_eq!(total, 2000);
    }

    #[test]
    fn keeps_union_of_top_n() {
        let rows = vec![
            ProcRow { name: "cpu".into(), count: 1, cpu_pct: 50.0, disk_bps: 0, mem_bytes: 0 },
            ProcRow { name: "disk".into(), count: 1, cpu_pct: 0.0, disk_bps: 9, mem_bytes: 0 },
            ProcRow { name: "mem".into(), count: 1, cpu_pct: 0.0, disk_bps: 0, mem_bytes: 9 },
            ProcRow { name: "idle".into(), count: 1, cpu_pct: 0.0, disk_bps: 0, mem_bytes: 0 },
        ];
        let kept = top_programs(rows, 1);
        let names: Vec<_> = kept.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["cpu", "disk", "mem"]);
    }

    #[test]
    fn live_sample_is_sane() {
        let mut s = Sampler::new(5);
        let smp = s.sample_after(std::time::Duration::from_millis(300), 1);
        assert!(smp.mem_total > 0);
        assert!(smp.mem_used <= smp.mem_total);
        assert!(!smp.procs.is_empty());
    }
}
