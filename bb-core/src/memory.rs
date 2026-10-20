//! Memory over time: which programs look like they are leaking.
//!
//! A leak here means a program whose memory climbs steadily for an hour or more without ever
//! coming back down. A program that grows and then releases (a browser opening and closing
//! tabs, a build that finishes) is not a leak, and neither is one that is simply big.
//!
//! Only each moment's top programs are recorded, so a program is judged on the samples where
//! it was among the biggest. That is fine for leaks worth reporting: they end up big.

use crate::model::Sample;

/// A program whose memory kept climbing.
#[derive(Clone, Debug, PartialEq)]
pub struct Leak {
    pub name: String,
    pub from_ts: i64,
    pub to_ts: i64,
    pub start_mb: f64,
    pub end_mb: f64,
}

impl Leak {
    pub fn mb_per_hour(&self) -> f64 {
        let hours = ((self.to_ts - self.from_ts) as f64 / 3600.0).max(1e-9);
        (self.end_mb - self.start_mb) / hours
    }
}

/// Growth this small is ordinary churn, whatever its shape.
const MIN_GROWTH_MB: f64 = 500.0;
/// And it must be a real fraction of the program's size, so a 20 GB database server that
/// moves by 500 MB isn't called a leak.
const MIN_GROWTH_RATIO: f64 = 1.5;
const MIN_SPAN_SECS: i64 = 3600;
const PARTS: usize = 6;
/// A part may dip this much below the previous one (garbage collection, cache trims) and the
/// climb still counts as steady.
const DIP_TOLERANCE: f64 = 0.05;

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// Is this series of (time, MB) points a steady climb? Returns (start MB, end MB) if so.
/// The span is cut into equal parts and each part's median compared, so single spikes don't
/// count either way.
pub fn steady_climb(points: &[(i64, f64)]) -> Option<(f64, f64)> {
    let (first, last) = (points.first()?.0, points.last()?.0);
    if last - first < MIN_SPAN_SECS || points.len() < PARTS * 4 {
        return None;
    }
    let width = (last - first) as f64 / PARTS as f64;
    let mut parts: Vec<Vec<f64>> = vec![Vec::new(); PARTS];
    for (ts, mb) in points {
        let i = (((ts - first) as f64 / width) as usize).min(PARTS - 1);
        parts[i].push(*mb);
    }
    // A part with no samples means the program wasn't among the biggest then (or wasn't
    // running), so the climb can't be called steady.
    if parts.iter().any(|p| p.len() < 2) {
        return None;
    }
    let medians: Vec<f64> = parts.iter_mut().map(|p| median(p)).collect();
    let steady = medians.windows(2).all(|w| w[1] >= w[0] * (1.0 - DIP_TOLERANCE));
    let (start, end) = (medians[0], medians[PARTS - 1]);
    (steady && end - start >= MIN_GROWTH_MB && end >= start * MIN_GROWTH_RATIO).then_some((start, end))
}

/// Programs that look like they are leaking memory over `samples` (oldest first), biggest
/// growth first. Each program is judged on its most recent unbroken run, so a restart (memory
/// drops back) starts the clock again rather than hiding a leak behind an old peak.
pub fn leaks(samples: &[Sample]) -> Vec<Leak> {
    use std::collections::HashMap;
    let mut series: HashMap<&str, Vec<(i64, f64)>> = HashMap::new();
    for s in samples {
        for p in &s.procs {
            series.entry(p.name.as_str()).or_default().push((s.ts, p.mem_bytes as f64 / 1_048_576.0));
        }
    }
    let mut out: Vec<Leak> = series
        .into_iter()
        .filter_map(|(name, pts)| {
            let run = since_last_restart(&pts);
            let (start_mb, end_mb) = steady_climb(run)?;
            Some(Leak { name: name.to_string(), from_ts: run[0].0, to_ts: run[run.len() - 1].0, start_mb, end_mb })
        })
        .collect();
    out.sort_by(|a, b| (b.end_mb - b.start_mb).total_cmp(&(a.end_mb - a.start_mb)));
    out
}

/// The points after the last sharp, lasting drop: memory falls to under half from one point
/// to the next, and the ten points after it stay under half of the ten before. That is what a
/// restart or a deliberate release looks like; falling back from a one-off spike is not.
fn since_last_restart(pts: &[(i64, f64)]) -> &[(i64, f64)] {
    let mid = |s: &[(i64, f64)]| median(&mut s.iter().map(|p| p.1).collect::<Vec<_>>());
    let cut = (1..pts.len())
        .rev()
        .find(|&i| pts[i].1 < pts[i - 1].1 * 0.5 && mid(&pts[i..(i + 10).min(pts.len())]) < mid(&pts[i.saturating_sub(10)..i]) * 0.5)
        .unwrap_or(0);
    &pts[cut..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProcRow;

    fn sample(ts: i64, procs: &[(&str, f64)]) -> Sample {
        Sample {
            ts,
            procs: procs.iter().map(|(n, mb)| ProcRow { name: n.to_string(), count: 1, cpu_pct: 1.0, disk_bps: 0, mem_bytes: (*mb * 1_048_576.0) as u64 }).collect(),
            ..Default::default()
        }
    }

    /// Three hours at one sample a minute.
    fn hours3(f: impl Fn(i64) -> Vec<(&'static str, f64)>) -> Vec<Sample> {
        (0..180).map(|m| sample(m * 60, &f(m))).collect()
    }

    #[test]
    fn a_steady_climb_is_a_leak_and_a_big_steady_program_is_not() {
        let v = hours3(|m| vec![("leaky.exe", 400.0 + m as f64 * 8.0), ("big.exe", 6000.0 + (m % 7) as f64 * 40.0)]);
        let l = leaks(&v);
        assert_eq!(l.len(), 1, "{l:?}");
        assert_eq!(l[0].name, "leaky.exe");
        assert!(l[0].end_mb > 1500.0 && l[0].start_mb < 600.0, "{l:?}");
        let rate = l[0].mb_per_hour();
        assert!((350.0..600.0).contains(&rate), "about 480 MB/h, got {rate}");
    }

    #[test]
    fn growth_that_comes_back_down_is_not_a_leak() {
        // Up for an hour and a half, then back down: a busy spell, not a leak.
        let v = hours3(|m| vec![("browser.exe", if m < 90 { 500.0 + m as f64 * 15.0 } else { 1850.0 - (m - 90) as f64 * 15.0 })]);
        assert!(leaks(&v).is_empty());
    }

    #[test]
    fn small_or_short_growth_is_ignored() {
        // +200 MB over three hours: churn.
        assert!(leaks(&hours3(|m| vec![("a.exe", 300.0 + m as f64 * 1.1)])).is_empty());
        // +300% but only 20 minutes long.
        let short: Vec<Sample> = (0..20).map(|m| sample(m * 60, &[("b.exe", 200.0 + m as f64 * 40.0)])).collect();
        assert!(leaks(&short).is_empty());
        // +600 MB on a 10 GB program is under the ratio.
        assert!(leaks(&hours3(|m| vec![("db.exe", 10_000.0 + m as f64 * 3.4)])).is_empty());
    }

    #[test]
    fn a_restart_starts_the_clock_again() {
        // Leaks for two hours, restarts, then stays flat: no longer leaking.
        let v = hours3(|m| vec![("app.exe", if m < 120 { 300.0 + m as f64 * 12.0 } else { 300.0 })]);
        assert!(leaks(&v).is_empty(), "{:?}", leaks(&v));
        // Restarts early, then leaks for over two hours: still caught, measured from the restart.
        let v = hours3(|m| vec![("app.exe", if m < 30 { 2000.0 } else { 250.0 + (m - 30) as f64 * 9.0 })]);
        let l = leaks(&v);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].from_ts, 30 * 60);
    }

    #[test]
    fn a_program_missing_from_part_of_the_time_is_not_judged() {
        let v = hours3(|m| if (60..90).contains(&m) { vec![] } else { vec![("x.exe", 300.0 + m as f64 * 10.0)] });
        assert!(leaks(&v).is_empty());
    }

    #[test]
    fn spikes_and_small_dips_do_not_break_a_climb() {
        let v = hours3(|m| {
            let base = 400.0 + m as f64 * 8.0;
            let wobble = if m % 10 == 0 { 3000.0 } else if m % 7 == 0 { -40.0 } else { 0.0 };
            vec![("leaky.exe", base + wobble)]
        });
        assert_eq!(leaks(&v).len(), 1);
    }
}
