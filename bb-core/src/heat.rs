//! The heat test: the same fixed load on every machine, so cooling can be compared.
//!
//! `bb heat test` idles for 30 s, runs every core flat out for 3 minutes, then lets the
//! machine cool for a minute, sampling once a second throughout. This module turns those
//! samples into a few numbers (idle temperature, temperature under sustained load, how much
//! speed the CPU held, how fast it cooled) and a 0-100 score.
//!
//! The score is rated against the CPU's own temperature limit and rough ranges for laptops.
//! It is not a percentile against other people's machines: that needs shared results, which
//! blackbox doesn't collect. The result is kept in a plain record so it can be shared later.

use crate::model::Sample;

pub const IDLE_SECS: u64 = 30;
pub const LOAD_SECS: u64 = 180;
pub const COOL_SECS: u64 = 60;
/// The last part of the load phase, which is what "sustained" means here: by then a laptop
/// has used up any short boost allowance.
const SUSTAINED_SECS: usize = 60;

/// What one heat test measured.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HeatRun {
    /// Median temperature while idle, if the machine was quiet enough to call it idle.
    pub idle_c: Option<f32>,
    pub peak_c: f32,
    /// Average temperature over the last minute of load.
    pub load_c: f32,
    /// Seconds into the load before the temperature first reached 90 C.
    pub secs_to_90: Option<u32>,
    /// Average CPU speed (percent of rated maximum) over the last minute of load.
    pub speed_pct: Option<f32>,
    /// How far the temperature fell in the minute after the load stopped.
    pub recovery_c: f32,
    /// How far the temperature rose above its end-of-load value while cooling. A real CPU
    /// sensor falls as soon as the load stops; one that keeps climbing lags behind it.
    pub cool_rise_c: f32,
    /// CPU package power, in watts: at rest, and over the last minute of load.
    pub idle_power_w: Option<f32>,
    pub power_w: Option<f32>,
    /// Average whole-machine CPU during the load, to check the load really ran.
    pub load_cpu_pct: f32,
    pub on_ac: Option<bool>,
}

fn temps(samples: &[Sample]) -> Vec<f32> {
    samples.iter().filter_map(|s| s.sensors.temp_c).collect()
}

fn avg(v: &[f32]) -> Option<f32> {
    (!v.is_empty()).then(|| v.iter().sum::<f32>() / v.len() as f32)
}

fn median(mut v: Vec<f32>) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f32::total_cmp);
    Some(v[v.len() / 2])
}

/// Summarises the three phases. `None` if there was no temperature reading at all.
pub fn summarize(idle: &[Sample], load: &[Sample], cool: &[Sample]) -> Option<HeatRun> {
    let load_t = temps(load);
    let peak = load_t.iter().chain(temps(cool).iter()).copied().fold(None, |m: Option<f32>, t| Some(m.map_or(t, |m| m.max(t))))?;
    let tail = &load[load.len().saturating_sub(SUSTAINED_SECS)..];
    let idle_cpu = avg(&idle.iter().map(|s| s.cpu_pct).collect::<Vec<_>>()).unwrap_or(100.0);
    let cool_t = temps(cool);
    let end_of_load = temps(tail).last().copied().unwrap_or(peak);
    Some(HeatRun {
        // A busy "idle" phase would make the machine look hotter at rest than it is.
        idle_c: if idle_cpu < 15.0 { median(temps(idle)) } else { None },
        peak_c: peak,
        load_c: avg(&temps(tail))?,
        secs_to_90: load.iter().position(|s| s.sensors.temp_c.is_some_and(|t| t >= 90.0)).map(|i| i as u32),
        speed_pct: avg(&tail.iter().filter_map(|s| s.sensors.freq_pct).collect::<Vec<_>>()),
        recovery_c: cool_t.last().map_or(0.0, |last| (end_of_load - last).max(0.0)),
        cool_rise_c: cool_t.iter().copied().fold(end_of_load, f32::max) - end_of_load,
        idle_power_w: avg(&idle.iter().filter_map(|s| s.sensors.cpu_power_w).collect::<Vec<_>>()),
        power_w: avg(&tail.iter().filter_map(|s| s.sensors.cpu_power_w).collect::<Vec<_>>()),
        load_cpu_pct: avg(&load.iter().map(|s| s.cpu_pct).collect::<Vec<_>>()).unwrap_or(0.0),
        on_ac: load.iter().rev().find_map(|s| s.sensors.on_ac),
    })
}

/// The least average CPU load during the test for the result to count.
pub const MIN_LOAD_PCT: f32 = 85.0;

/// Why a run can't be scored, if it can't. A score is only given when the machine was really
/// fully loaded and the temperature sensor visibly follows the CPU: warmer under load than at
/// rest, and falling (not climbing) once the load stops.
pub fn check(run: &HeatRun) -> Result<(), String> {
    if run.load_cpu_pct < MIN_LOAD_PCT {
        return Err(format!(
            "the CPU only averaged {:.0}% busy during the load (it needs {MIN_LOAD_PCT:.0}%+), so the machine wasn't fully loaded. Close busy programs and try again.",
            run.load_cpu_pct
        ));
    }
    if let Some(idle) = run.idle_c {
        if run.load_c < idle + 5.0 {
            return Err(format!(
                "the temperature sensor didn't follow the load: {idle:.0} C at rest but {:.0} C after 2 minutes of full load. It's measuring something other than the CPU (on many laptops a chassis sensor the fans cool).",
                run.load_c
            ));
        }
    }
    if run.cool_rise_c >= 3.0 {
        return Err(format!(
            "the temperature sensor lags: it rose {:.0} C after the load stopped, when a CPU cools at once. Its readings can't be timed against the load.",
            run.cool_rise_c
        ));
    }
    Ok(())
}

/// The temperature at which the CPU slows itself down to protect itself (Tjmax), and whether
/// it is known for this model or assumed.
pub fn tjmax(cpu: &str) -> (f32, bool) {
    let c = cpu.to_lowercase();
    if c.contains("intel") {
        // Intel's mobile and desktop Core processors of the last several generations
        // throttle at 100 C.
        (100.0, c.contains("core"))
    } else if c.contains("ryzen") {
        // Most Ryzen laptop chips are rated 95 to 100 C; use the lower one.
        (95.0, false)
    } else {
        (100.0, false)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Part {
    pub name: &'static str,
    pub points: f32,
    pub max: f32,
    pub note: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Score {
    /// 0 to 100.
    pub total: u32,
    pub band: &'static str,
    pub parts: Vec<Part>,
}

fn ramp(x: f32, zero_at: f32, full_at: f32) -> f32 {
    ((x - zero_at) / (full_at - zero_at)).clamp(0.0, 1.0)
}

/// Rough bands. They describe the score, not a measured share of machines.
pub fn band(total: u32) -> &'static str {
    match total {
        85.. => "Excellent cooling",
        70..=84 => "Good cooling",
        50..=69 => "Typical for a gaming laptop",
        30..=49 => "Runs hot",
        _ => "Needs attention",
    }
}

/// Scores a heat test. Parts the machine can't measure (no CPU speed counter, a busy idle
/// phase) are left out and the rest scaled up, rather than counted as zero.
pub fn score(run: &HeatRun, tjmax: f32) -> Score {
    let margin = tjmax - run.load_c;
    let mut parts = vec![Part {
        name: "Headroom under load",
        points: ramp(margin, 0.0, 25.0) * 40.0,
        max: 40.0,
        note: format!("{:.0} C after 2 minutes of full load, {:.0} C below where the CPU protects itself ({tjmax:.0} C)", run.load_c, margin.max(0.0)),
    }];
    if let Some(s) = run.speed_pct {
        parts.push(Part { name: "Speed held", points: ramp(s, 50.0, 100.0) * 30.0, max: 30.0, note: format!("kept {s:.0}% of its rated speed while hot") });
    }
    if let Some(i) = run.idle_c {
        parts.push(Part { name: "Idle temperature", points: ramp(-i, -70.0, -45.0) * 15.0, max: 15.0, note: format!("{i:.0} C at rest") });
    }
    parts.push(Part { name: "Cooling down", points: ramp(run.recovery_c, 0.0, 25.0) * 15.0, max: 15.0, note: format!("dropped {:.0} C in the minute after the load", run.recovery_c) });
    let (got, max) = parts.iter().fold((0.0, 0.0), |(g, m), p| (g + p.points, m + p.max));
    let total = (got / max * 100.0).round() as u32;
    Score { total, band: band(total), parts }
}

/// One piece of advice for the most telling problem, checked in order of how specific it is.
pub fn advice(run: &HeatRun, s: &Score) -> String {
    let slowed = run.speed_pct.is_some_and(|p| p < 80.0);
    if s.total >= 85 {
        "Nothing to do: this machine handles sustained load well.".into()
    } else if run.idle_c.is_some_and(|i| i >= 65.0) {
        "Hot even at rest usually means dust in the fans or vents, or old thermal paste. Cleaning is the biggest win.".into()
    } else if slowed && run.load_c >= 90.0 {
        "It slows itself down to stay under its limit. A cooling pad, a hard flat surface, or the laptop maker's fan boost mode (on MSI: Cooler Boost in MSI Center) helps.".into()
    } else if slowed {
        "It slows down under load without being very hot, which points at a power limit (a quiet or battery power mode) rather than heat.".into()
    } else if run.recovery_c < 10.0 {
        "Slow to cool down points at weak airflow: raise the back of the laptop, keep the vents clear, and check the fans spin up.".into()
    } else {
        "It runs close to its limit under load. Use it on a hard surface, keep the vents clear, and consider the fan boost mode for long sessions.".into()
    }
}

/// A heat test result as it is saved, one JSON object per line, so results can be compared
/// over time and, later, shared.
pub fn record_json(ts: i64, cpu: &str, machine: &str, version: &str, run: &HeatRun, s: &Score) -> String {
    let opt = |v: Option<f32>| v.map_or("null".to_string(), |x| format!("{x:.1}"));
    let esc = |t: &str| t.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "{{\"ts\":{ts},\"version\":\"{}\",\"cpu\":\"{}\",\"machine\":\"{}\",\"score\":{},\"idle_c\":{},\"peak_c\":{:.1},\"load_c\":{:.1},\"secs_to_90\":{},\"speed_pct\":{},\"recovery_c\":{:.1},\"on_ac\":{}}}",
        esc(version),
        esc(cpu),
        esc(machine),
        s.total,
        opt(run.idle_c),
        run.peak_c,
        run.load_c,
        run.secs_to_90.map_or("null".to_string(), |x| x.to_string()),
        opt(run.speed_pct),
        run.recovery_c,
        run.on_ac.map_or("null".to_string(), |b| b.to_string()),
    )
}

/// Reads the `ts` and `score` back out of a saved line, without a JSON library.
pub fn parse_record(line: &str) -> Option<(i64, u32)> {
    let num = |key: &str| -> Option<&str> {
        let rest = line.split(&format!("\"{key}\":")).nth(1)?;
        Some(rest.split([',', '}']).next()?.trim())
    };
    Some((num("ts")?.parse().ok()?, num("score")?.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Sensors;

    fn s(cpu: f32, temp: f32, freq: Option<f32>) -> Sample {
        Sample { cpu_pct: cpu, sensors: Sensors { temp_c: Some(temp), freq_pct: freq, on_ac: Some(true), ..Default::default() }, ..Default::default() }
    }

    /// A machine that idles at `idle`, climbs to `hot` over the first minute of load, holds
    /// `speed`, and falls by `drop` while cooling.
    fn run(idle: f32, hot: f32, speed: f32, drop: f32) -> HeatRun {
        let i: Vec<Sample> = (0..30).map(|_| s(3.0, idle, Some(100.0))).collect();
        let l: Vec<Sample> = (0..180).map(|k| s(100.0, idle + (hot - idle) * (k as f32 / 60.0).min(1.0), Some(speed))).collect();
        let c: Vec<Sample> = (0..60).map(|k| s(3.0, hot - drop * (k as f32 / 59.0), Some(100.0))).collect();
        summarize(&i, &l, &c).unwrap()
    }

    #[test]
    fn the_phases_are_measured() {
        let r = run(45.0, 92.0, 70.0, 30.0);
        assert_eq!(r.idle_c, Some(45.0));
        assert!((r.load_c - 92.0).abs() < 0.01);
        assert_eq!(r.peak_c, 92.0);
        assert!((r.speed_pct.unwrap() - 70.0).abs() < 0.01);
        assert!((r.recovery_c - 30.0).abs() < 0.01);
        // 45 -> 92 over 60 s crosses 90 C at about 57 s.
        assert!(matches!(r.secs_to_90, Some(55..=58)), "{:?}", r.secs_to_90);
        assert_eq!(r.on_ac, Some(true));
    }

    #[test]
    fn a_busy_idle_phase_is_not_used_as_idle() {
        let i: Vec<Sample> = (0..30).map(|_| s(60.0, 70.0, None)).collect();
        let l: Vec<Sample> = (0..180).map(|_| s(100.0, 85.0, None)).collect();
        let r = summarize(&i, &l, &[]).unwrap();
        assert_eq!(r.idle_c, None);
        assert_eq!(r.recovery_c, 0.0);
        assert!(summarize(&[], &[Sample::default()], &[]).is_none(), "no sensor, no result");
    }

    #[test]
    fn good_cooling_scores_high_and_bad_cooling_low() {
        let cool = score(&run(40.0, 72.0, 100.0, 30.0), 100.0);
        let hot = score(&run(68.0, 99.0, 55.0, 4.0), 100.0);
        assert!(cool.total >= 85, "{cool:?}");
        assert_eq!(cool.band, "Excellent cooling");
        assert!(hot.total < 30, "{hot:?}");
        assert_eq!(hot.band, "Needs attention");
        let typical = score(&run(50.0, 90.0, 85.0, 20.0), 100.0);
        assert!((40..85).contains(&typical.total), "{typical:?}");
    }

    #[test]
    fn missing_measurements_are_left_out_not_counted_as_zero() {
        let mut r = run(40.0, 72.0, 100.0, 30.0);
        r.speed_pct = None;
        r.idle_c = None;
        let sc = score(&r, 100.0);
        assert_eq!(sc.parts.len(), 2);
        assert!(sc.total >= 85, "{sc:?}");
    }

    #[test]
    fn a_partial_load_or_a_sensor_that_does_not_track_is_refused() {
        assert!(check(&run(45.0, 90.0, 90.0, 20.0)).is_ok());
        let mut low = run(45.0, 90.0, 90.0, 20.0);
        low.load_cpu_pct = 60.0;
        assert!(check(&low).unwrap_err().contains("60%"));
        // What this MSI laptop's thermal zone did: 80 C at rest, 66 C under full load.
        let inverted = run(80.0, 66.0, 100.0, 0.0);
        assert!(check(&inverted).unwrap_err().contains("didn't follow the load"));
        // Rising after the load stops.
        let i: Vec<Sample> = (0..30).map(|_| s(3.0, 50.0, None)).collect();
        let l: Vec<Sample> = (0..180).map(|_| s(100.0, 70.0, None)).collect();
        let c: Vec<Sample> = (0..60).map(|k| s(3.0, if k < 10 { 70.0 } else { 79.0 }, None)).collect();
        let lagging = summarize(&i, &l, &c).unwrap();
        assert!((lagging.cool_rise_c - 9.0).abs() < 0.01);
        assert!(check(&lagging).unwrap_err().contains("lags"));
    }

    #[test]
    fn package_power_is_averaged_per_phase() {
        let mut i: Vec<Sample> = (0..30).map(|_| s(3.0, 45.0, None)).collect();
        let mut l: Vec<Sample> = (0..180).map(|_| s(100.0, 85.0, None)).collect();
        i.iter_mut().for_each(|x| x.sensors.cpu_power_w = Some(12.0));
        l.iter_mut().enumerate().for_each(|(k, x)| x.sensors.cpu_power_w = Some(if k < 120 { 110.0 } else { 60.0 }));
        let r = summarize(&i, &l, &[]).unwrap();
        assert_eq!((r.idle_power_w, r.power_w), (Some(12.0), Some(60.0)), "sustained means the last minute, after boost");
    }

    #[test]
    fn tjmax_is_known_for_intel_core() {
        assert_eq!(tjmax("Intel(R) Core(TM) i7-14700HX"), (100.0, true));
        assert_eq!(tjmax("AMD Ryzen 7 7840HS").0, 95.0);
        assert!(!tjmax("Something else").1);
    }

    #[test]
    fn advice_targets_the_weakest_part() {
        let r = run(68.0, 80.0, 100.0, 25.0);
        assert!(advice(&r, &score(&r, 100.0)).contains("dust"));
        let r = run(45.0, 97.0, 60.0, 25.0);
        assert!(advice(&r, &score(&r, 100.0)).contains("slows itself down"));
    }

    #[test]
    fn records_round_trip() {
        let r = run(45.0, 92.0, 70.0, 30.0);
        let sc = score(&r, 100.0);
        let line = record_json(1_700_000_000, "Intel \"X\"", "MSI", "0.5.2", &r, &sc);
        assert_eq!(parse_record(&line), Some((1_700_000_000, sc.total)));
        assert!(line.contains("\\\"X\\\""));
        assert!(line.starts_with('{') && line.ends_with('}'));
    }
}
