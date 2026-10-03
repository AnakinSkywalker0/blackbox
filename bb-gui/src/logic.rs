//! Everything the window needs that isn't drawing: chart data, explained moments, finding
//! the recorder and the `bb` program. Kept free of any GUI types so it can be tested.

use bb_core::model::{Confidence, Sample};
use bb_core::rules::{analyze, kind_of, Thresholds};
use bb_core::store::Store;
use chrono::{Local, TimeZone};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// ---- time ranges -----------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Range {
    M15,
    H1,
    H6,
    H24,
    D7,
}

impl Range {
    pub const ALL: [Range; 5] = [Range::M15, Range::H1, Range::H6, Range::H24, Range::D7];

    pub fn secs(self) -> i64 {
        match self {
            Range::M15 => 15 * 60,
            Range::H1 => 3600,
            Range::H6 => 6 * 3600,
            Range::H24 => 24 * 3600,
            Range::D7 => 7 * 24 * 3600,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Range::M15 => "15 min",
            Range::H1 => "1 hour",
            Range::H6 => "6 hours",
            Range::H24 => "24 hours",
            Range::D7 => "7 days",
        }
    }

    /// How long a stretch each explanation covers when scanning this range. Short ranges use
    /// the same 4 minutes as `bb why`; long ones widen it so the scan stays fast.
    pub fn moment_window(self) -> i64 {
        (self.secs() / 400).max(240)
    }
}

/// `HH:MM:SS` in local time.
pub fn clock(ts: i64) -> String {
    Local.timestamp_opt(ts, 0).single().map_or_else(|| ts.to_string(), |t| t.format("%H:%M:%S").to_string())
}

/// `HH:MM` for a chart axis, with the date added when it isn't today.
pub fn axis_label(ts: i64) -> String {
    let Some(t) = Local.timestamp_opt(ts, 0).single() else { return ts.to_string() };
    if t.date_naive() == Local::now().date_naive() { t.format("%H:%M").to_string() } else { t.format("%a %H:%M").to_string() }
}

// ---- chart points ----------------------------------------------------------------------

/// One plotted point. Spiky signals keep their maximum in a bucket so a short burst is
/// still visible when many seconds are squeezed into one pixel.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Point {
    pub ts: f64,
    pub cpu: f64,
    pub mem: f64,
    pub disk_mb: f64,
    pub gpu: Option<f64>,
    pub temp: Option<f64>,
    pub gpu_temp: Option<f64>,
    /// Lowest CPU speed in the bucket (percent of rated), so a throttling dip stays visible.
    pub freq: Option<f64>,
}

fn mem_pct(s: &Sample) -> f64 {
    if s.mem_total == 0 { 0.0 } else { s.mem_used as f64 * 100.0 / s.mem_total as f64 }
}

fn min_opt(vals: impl Iterator<Item = Option<f32>>) -> Option<f64> {
    vals.flatten().map(f64::from).fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.min(v))))
}

fn max_opt(vals: impl Iterator<Item = Option<f32>>) -> Option<f64> {
    vals.flatten().map(f64::from).fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v))))
}

/// Shrinks `samples` to at most `max_points` points.
pub fn downsample(samples: &[Sample], max_points: usize) -> Vec<Point> {
    if samples.is_empty() || max_points == 0 {
        return Vec::new();
    }
    let bucket = samples.len().div_ceil(max_points).max(1);
    samples
        .chunks(bucket)
        .map(|c| {
            let n = c.len() as f64;
            Point {
                ts: c.iter().map(|s| s.ts as f64).sum::<f64>() / n,
                cpu: c.iter().map(|s| s.cpu_pct as f64).fold(0.0, f64::max),
                mem: c.iter().map(mem_pct).sum::<f64>() / n,
                disk_mb: c.iter().map(|s| s.disk_bps as f64 / 1_048_576.0).fold(0.0, f64::max),
                gpu: max_opt(c.iter().map(|s| s.sensors.gpu_pct)),
                temp: max_opt(c.iter().map(|s| s.sensors.temp_c)),
                gpu_temp: max_opt(c.iter().map(|s| s.sensors.gpu_temp_c)),
                freq: min_opt(c.iter().map(|s| s.sensors.freq_pct)),
            }
        })
        .collect()
}

/// Splits points into runs wherever the recorder wasn't running, so a chart doesn't draw a
/// straight line across a gap and imply data that was never recorded. A gap is a hole much
/// bigger than the usual spacing (at least 30 s, and 4x the typical step).
pub fn split_at_gaps(points: &[Point]) -> Vec<&[Point]> {
    if points.len() < 2 {
        return if points.is_empty() { Vec::new() } else { vec![points] };
    }
    let mut steps: Vec<f64> = points.windows(2).map(|w| w[1].ts - w[0].ts).collect();
    steps.sort_by(f64::total_cmp);
    let max_gap = (steps[steps.len() / 2] * 4.0).max(30.0);
    let mut runs = Vec::new();
    let mut start = 0;
    for i in 1..points.len() {
        if points[i].ts - points[i - 1].ts > max_gap {
            runs.push(&points[start..i]);
            start = i;
        }
    }
    runs.push(&points[start..]);
    runs
}

// ---- heat exposure ---------------------------------------------------------------------

pub const WARM_C: f32 = 80.0;
pub const HOT_C: f32 = 90.0;
pub const CRITICAL_C: f32 = 95.0;

/// How hot the machine ran over a stretch of time, from the raw recorded samples.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HeatSummary {
    /// Seconds that had a temperature reading at all.
    pub measured_secs: i64,
    pub peak_c: Option<f32>,
    pub peak_ts: Option<i64>,
    pub avg_c: Option<f32>,
    /// Typical temperature while the CPU was nearly idle (below 10%). High values here point
    /// at poor cooling rather than heavy work.
    pub idle_c: Option<f32>,
    pub secs_over_warm: i64,
    pub secs_over_hot: i64,
    pub secs_over_critical: i64,
    /// Seconds spent both hot (80 C or more) and running well below full speed.
    pub secs_slowed_by_heat: i64,
    pub gpu_peak_c: Option<f32>,
    pub gpu_heat_throttle_secs: i64,
}

impl HeatSummary {
    pub fn mins(secs: i64) -> String {
        if secs < 60 { format!("{secs} s") } else if secs < 3600 { format!("{:.0} min", secs as f64 / 60.0) } else { format!("{:.1} h", secs as f64 / 3600.0) }
    }

    /// One plain sentence on how the machine's temperature looked, with a colour hint.
    pub fn verdict(&self) -> (&'static str, String) {
        let Some(peak) = self.peak_c else {
            return ("muted", "No temperature readings in this range. This machine may not expose a temperature sensor.".into());
        };
        if self.secs_over_critical > 0 || self.secs_slowed_by_heat >= 120 {
            let extra = if self.secs_slowed_by_heat >= 120 { format!(" and it ran below full speed for {} while hot", Self::mins(self.secs_slowed_by_heat)) } else { String::new() };
            return ("bad", format!("Running too hot: peaked at {peak:.0} C{extra}. Check vents and fans for dust, and avoid soft surfaces."));
        }
        if self.secs_over_hot > 0 {
            return ("warn", format!("Hot at times: {} at 90 C or more (peak {peak:.0} C). Fine briefly under heavy load, worth watching.", Self::mins(self.secs_over_hot)));
        }
        if let Some(idle) = self.idle_c.filter(|i| *i >= 65.0) {
            return ("warn", format!("Warm even when idle ({idle:.0} C typical). That usually means dust, dried thermal paste or a weak fan."));
        }
        ("good", format!("Temperatures look healthy (peak {peak:.0} C)."))
    }
}

/// Summarises heat over `samples` (oldest first). Each sample counts for the time until the
/// next one, capped at a minute so a recording gap isn't counted as time spent hot.
pub fn heat_summary(samples: &[Sample]) -> HeatSummary {
    let mut h = HeatSummary::default();
    let (mut sum, mut n) = (0.0f64, 0u64);
    let mut idle: Vec<f32> = Vec::new();
    for (i, s) in samples.iter().enumerate() {
        let dt = samples.get(i + 1).map_or(1, |nx| (nx.ts - s.ts).clamp(0, 60));
        if let Some(t) = s.sensors.temp_c {
            h.measured_secs += dt;
            sum += f64::from(t);
            n += 1;
            if h.peak_c.is_none_or(|p| t > p) {
                h.peak_c = Some(t);
                h.peak_ts = Some(s.ts);
            }
            if t >= WARM_C { h.secs_over_warm += dt; }
            if t >= HOT_C { h.secs_over_hot += dt; }
            if t >= CRITICAL_C { h.secs_over_critical += dt; }
            if t >= WARM_C && s.sensors.freq_pct.is_some_and(|f| f < 70.0) && s.cpu_pct >= 30.0 {
                h.secs_slowed_by_heat += dt;
            }
            if s.cpu_pct < 10.0 { idle.push(t); }
        }
        if let Some(g) = s.sensors.gpu_temp_c {
            if h.gpu_peak_c.is_none_or(|p| g > p) { h.gpu_peak_c = Some(g); }
        }
        if s.sensors.gpu_throttle.is_some_and(|b| b & bb_core::model::GPU_THERMAL != 0) {
            h.gpu_heat_throttle_secs += dt;
        }
    }
    if n > 0 { h.avg_c = Some((sum / n as f64) as f32); }
    if idle.len() >= 30 {
        idle.sort_by(f32::total_cmp);
        h.idle_c = Some(idle[idle.len() / 2]);
    }
    h
}

// ---- explained moments -----------------------------------------------------------------

/// A stretch of time `bb why` has an explanation for.
#[derive(Clone, Debug, PartialEq)]
pub struct Moment {
    pub from: i64,
    pub to: i64,
    pub title: String,
    pub kind: &'static str,
    pub confidence: Confidence,
}

/// Joins neighbouring moments that have the same kind of cause into one longer stretch.
pub fn merge_moments(moments: Vec<Moment>) -> Vec<Moment> {
    let mut out: Vec<Moment> = Vec::new();
    for m in moments {
        match out.last_mut() {
            Some(last) if last.kind == m.kind && m.from <= last.to + 1 => {
                last.to = last.to.max(m.to);
                last.confidence = last.confidence.max(m.confidence);
            }
            _ => out.push(m),
        }
    }
    out
}

/// Scans `[from, to]` in chunks of `window` seconds and records the top explanation for each
/// chunk that has one. Chunks with too little data are skipped.
pub fn find_moments(store: &Store, from: i64, to: i64, best_mhz: u32, window: i64) -> Vec<Moment> {
    let thresholds = Thresholds::default();
    let mut found = Vec::new();
    let mut a = from;
    while a < to {
        let b = (a + window).min(to);
        if let Ok(samples) = store.window(a, b) {
            if samples.len() >= 8 {
                if let Some(top) = analyze(&samples, best_mhz, &thresholds).into_iter().next() {
                    found.push(Moment {
                        from: samples[0].ts,
                        to: samples[samples.len() - 1].ts,
                        kind: kind_of(&top.title),
                        title: top.title,
                        confidence: top.confidence,
                    });
                }
            }
        }
        a = b + 1;
    }
    merge_moments(found)
}

// ---- the recorder and the bb program ---------------------------------------------------

/// Pid of a live `bb` recorder for this database, from the pid file `bb run` keeps.
pub fn recorder_pid(db: &Path) -> Option<u32> {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let pid: u32 = std::fs::read_to_string(db.with_extension("pid")).ok()?.trim().parse().ok()?;
    let p = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::Some(&[p]), true);
    // Check the name too, so a reused pid doesn't look like a recorder.
    let name = sys.process(p)?.name().to_string_lossy().to_lowercase();
    name.starts_with("bb").then_some(pid)
}

fn bb_file_name() -> &'static str {
    if cfg!(windows) { "bb.exe" } else { "bb" }
}

/// Finds `bb`: next to this program first (how the installer lays them out), then on PATH.
pub fn find_bb(gui_exe: &Path) -> Option<PathBuf> {
    if let Some(dir) = gui_exe.parent() {
        let beside = dir.join(bb_file_name());
        if beside.is_file() {
            return Some(beside);
        }
    }
    std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join(bb_file_name())).find(|c| c.is_file()))
}

/// Runs `bb --db <db> <args...>` without showing a console window and returns its output.
pub fn run_bb(bb: &Path, db: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new(bb);
    cmd.arg("--db").arg(db).args(args).stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output().map_err(|e| format!("couldn't run {}: {e}", bb.display()))?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if out.status.success() { Ok(text) } else { Err(if err.is_empty() { text } else { err }) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bb_core::model::{ProcRow, Sensors};

    fn sample(ts: i64, cpu: f32) -> Sample {
        Sample { ts, cpu_pct: cpu, mem_used: 8 << 30, mem_total: 16 << 30, ..Default::default() }
    }

    #[test]
    fn ranges_have_sensible_windows() {
        assert_eq!(Range::M15.moment_window(), 240);
        assert_eq!(Range::H24.moment_window(), 240);
        assert!(Range::D7.moment_window() > 1000);
        assert!(Range::ALL.windows(2).all(|w| w[0].secs() < w[1].secs()));
    }

    #[test]
    fn downsample_keeps_small_data_and_caps_big_data() {
        let few: Vec<Sample> = (0..10).map(|i| sample(i, 5.0)).collect();
        assert_eq!(downsample(&few, 100).len(), 10);
        let many: Vec<Sample> = (0..10_000).map(|i| sample(i, 5.0)).collect();
        let d = downsample(&many, 500);
        assert!(d.len() <= 500 && d.len() >= 400, "{}", d.len());
        assert!(downsample(&[], 100).is_empty());
        assert!(downsample(&few, 0).is_empty());
    }

    fn pt(ts: f64) -> Point {
        Point { ts, ..Default::default() }
    }

    #[test]
    fn a_recording_gap_splits_the_chart_line() {
        // One point a second, then nothing for 16 minutes, then one a second again.
        let mut pts: Vec<Point> = (0..60).map(|i| pt(i as f64)).collect();
        pts.extend((0..60).map(|i| pt(1000.0 + i as f64)));
        let runs = split_at_gaps(&pts);
        assert_eq!(runs.len(), 2);
        assert_eq!((runs[0].len(), runs[1].len()), (60, 60));
        // Steady data is one run, and tiny inputs don't panic.
        assert_eq!(split_at_gaps(&pts[..60]).len(), 1);
        assert_eq!(split_at_gaps(&pts[..1]).len(), 1);
        assert!(split_at_gaps(&[]).is_empty());
    }

    #[test]
    fn coarse_old_data_is_not_mistaken_for_gaps() {
        // 10 second spacing (thinned history) must stay one continuous line.
        let pts: Vec<Point> = (0..100).map(|i| pt(i as f64 * 10.0)).collect();
        assert_eq!(split_at_gaps(&pts).len(), 1);
    }

    #[test]
    fn a_short_spike_survives_downsampling() {
        let mut v: Vec<Sample> = (0..3000).map(|i| sample(i, 4.0)).collect();
        v[1500].cpu_pct = 100.0; // one second of 100% among 3000
        let d = downsample(&v, 100);
        assert_eq!(d.iter().map(|p| p.cpu).fold(0.0, f64::max), 100.0);
    }

    #[test]
    fn optional_sensors_stay_none_when_missing_and_use_the_max_when_present() {
        let mut a = sample(0, 1.0);
        let mut b = sample(1, 1.0);
        let d = downsample(&[a.clone(), b.clone()], 1);
        assert_eq!((d[0].gpu, d[0].temp), (None, None));
        a.sensors = Sensors { gpu_pct: Some(30.0), temp_c: Some(60.0), ..Default::default() };
        b.sensors = Sensors { gpu_pct: Some(90.0), temp_c: Some(70.0), ..Default::default() };
        let d = downsample(&[a, b], 1);
        assert_eq!((d[0].gpu, d[0].temp), (Some(90.0), Some(70.0)));
    }

    fn moment(from: i64, to: i64, kind: &'static str, c: Confidence) -> Moment {
        Moment { from, to, title: kind.to_string(), kind, confidence: c }
    }

    #[test]
    fn neighbouring_moments_of_one_kind_merge() {
        let merged = merge_moments(vec![
            moment(0, 100, "cpu_hog", Confidence::Medium),
            moment(101, 200, "cpu_hog", Confidence::High),
            moment(201, 300, "memory", Confidence::Medium),
            moment(500, 600, "cpu_hog", Confidence::Low),
        ]);
        assert_eq!(merged.len(), 3);
        assert_eq!((merged[0].from, merged[0].to, merged[0].confidence), (0, 200, Confidence::High));
        assert_eq!(merged[1].kind, "memory");
        assert_eq!(merged[2].from, 500, "a gap keeps moments apart");
    }

    #[test]
    fn finds_a_real_cpu_hog_in_stored_history() {
        let mut st = Store::open_in_memory().unwrap();
        // 20 minutes: quiet, then 4 minutes of one program hogging the CPU, then quiet.
        let samples: Vec<Sample> = (0..1200)
            .map(|i| {
                let hot = (600..840).contains(&i);
                Sample {
                    ts: 10_000 + i,
                    cpu_pct: if hot { 80.0 } else { 6.0 },
                    mem_used: 6 << 30,
                    mem_total: 16 << 30,
                    procs: vec![ProcRow { name: if hot { "build.exe".into() } else { "code.exe".into() }, count: 3, cpu_pct: if hot { 70.0 } else { 3.0 }, disk_bps: 0, mem_bytes: 1 << 30 }],
                    ..Default::default()
                }
            })
            .collect();
        st.insert_many(&samples).unwrap();
        let m = find_moments(&st, 10_000, 11_199, 3600, 240);
        assert!(!m.is_empty(), "the hog should be found");
        assert!(m.iter().all(|x| x.kind == "cpu_hog"), "{m:?}");
        assert!(m[0].title.starts_with("build.exe"));
        // The moment sits inside the hot period, give or take a chunk.
        assert!(m[0].from >= 10_000 + 360 && m[0].to <= 10_000 + 1080, "{m:?}");
    }

    #[test]
    fn a_quiet_history_has_no_moments() {
        let mut st = Store::open_in_memory().unwrap();
        let samples: Vec<Sample> = (0..600).map(|i| sample(5000 + i, 5.0)).collect();
        st.insert_many(&samples).unwrap();
        assert!(find_moments(&st, 5000, 5599, 3600, 240).is_empty());
    }

    #[test]
    fn sparse_windows_are_skipped() {
        let mut st = Store::open_in_memory().unwrap();
        st.insert_many(&[sample(1, 99.0), sample(2, 99.0)]).unwrap();
        assert!(find_moments(&st, 0, 100, 3600, 240).is_empty());
    }

    #[test]
    fn find_bb_prefers_the_folder_beside_the_gui() {
        let dir = std::env::temp_dir().join(format!("bb-gui-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gui = dir.join(if cfg!(windows) { "bb-gui.exe" } else { "bb-gui" });
        std::fs::write(&gui, b"x").unwrap();
        assert_ne!(find_bb(&gui), Some(dir.join(bb_file_name())), "no bb beside it yet");
        std::fs::write(dir.join(bb_file_name()), b"x").unwrap();
        assert_eq!(find_bb(&gui), Some(dir.join(bb_file_name())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recorder_pid_is_none_without_a_pid_file() {
        let db = std::env::temp_dir().join(format!("bb-gui-nopid-{}.db", std::process::id()));
        assert_eq!(recorder_pid(&db), None);
    }

    fn hot_sample(ts: i64, temp: f32, cpu: f32, freq: f32) -> Sample {
        Sample { ts, cpu_pct: cpu, sensors: Sensors { temp_c: Some(temp), freq_pct: Some(freq), ..Default::default() }, ..Default::default() }
    }

    #[test]
    fn heat_summary_counts_time_over_each_limit() {
        let mut v: Vec<Sample> = (0..100).map(|i| hot_sample(i, 60.0, 5.0, 100.0)).collect();
        v.extend((100..160).map(|i| hot_sample(i, 92.0, 90.0, 55.0)));
        v.extend((160..170).map(|i| hot_sample(i, 96.0, 90.0, 100.0)));
        let h = heat_summary(&v);
        assert_eq!(h.peak_c, Some(96.0));
        assert_eq!(h.peak_ts, Some(160));
        assert_eq!(h.secs_over_warm, 70);
        assert_eq!(h.secs_over_hot, 70);
        assert_eq!(h.secs_over_critical, 10);
        assert_eq!(h.secs_slowed_by_heat, 60, "hot and well below full speed");
        assert_eq!(h.idle_c, Some(60.0));
        assert_eq!(h.verdict().0, "bad");
    }

    #[test]
    fn a_recording_gap_is_not_counted_as_heat() {
        let v = vec![hot_sample(0, 96.0, 90.0, 100.0), hot_sample(10_000, 50.0, 5.0, 100.0)];
        assert_eq!(heat_summary(&v).secs_over_critical, 60);
    }

    #[test]
    fn no_sensor_gives_an_honest_message_and_healthy_data_is_good() {
        let none = heat_summary(&[sample(0, 5.0), sample(1, 5.0)]);
        assert_eq!((none.peak_c, none.verdict().0), (None, "muted"));
        let v: Vec<Sample> = (0..100).map(|i| hot_sample(i, 45.0, 5.0, 100.0)).collect();
        assert_eq!(heat_summary(&v).verdict().0, "good");
        let warm_idle: Vec<Sample> = (0..100).map(|i| hot_sample(i, 70.0, 3.0, 100.0)).collect();
        assert_eq!(heat_summary(&warm_idle).verdict().0, "warn");
    }

    #[test]
    fn clock_formats_are_short() {
        assert_eq!(clock(0).len(), 8);
        assert!(axis_label(Local::now().timestamp()).len() == 5);
    }
}
