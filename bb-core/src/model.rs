//! Shared data types.

/// One program (all processes with the same name grouped together) in one sample.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcRow {
    pub name: String,
    /// Number of processes grouped under this name.
    pub count: u32,
    /// Share of total machine CPU, 0..100.
    pub cpu_pct: f32,
    /// Disk read+write throughput in bytes per second.
    pub disk_bps: u64,
    /// Resident memory in bytes.
    pub mem_bytes: u64,
}

/// Extra sensor readings. Every field is optional because hardware and drivers
/// differ; `None` means "not available here", never "zero".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sensors {
    /// True when plugged in.
    pub on_ac: Option<bool>,
    pub battery_pct: Option<u8>,
    /// Windows Battery Saver is on.
    pub battery_saver: Option<bool>,
    /// Hottest system temperature sensor, in degrees C.
    pub temp_c: Option<f32>,
    /// Current CPU speed as a percent of its rated maximum (Windows only).
    pub freq_pct: Option<f32>,
    /// Busiest GPU engine, 0..100.
    pub gpu_pct: Option<f32>,
    pub gpu_temp_c: Option<f32>,
    /// Why the GPU is slowing down, as `GPU_THERMAL | GPU_POWER | GPU_HW` bits.
    pub gpu_throttle: Option<u8>,
    /// Average number of disk requests waiting.
    pub disk_queue: Option<f32>,
    /// Average time for a disk request to complete, in milliseconds.
    pub disk_latency_ms: Option<f32>,
    /// Memory pages written out to disk per second to free RAM. Near zero on a healthy
    /// machine, so a high value is real memory pressure (unlike a static swap size).
    pub page_out: Option<f32>,
    /// Share of the time the disk was busy servicing requests, 0..100 (what Task Manager
    /// shows as Disk %). Unlike throughput it means the same on a slow disk and a fast one.
    pub disk_busy: Option<f32>,
}

pub const GPU_THERMAL: u8 = 1;
pub const GPU_POWER: u8 = 2;
pub const GPU_HW: u8 = 4;

/// One recorded moment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    /// Unix time in seconds.
    pub ts: i64,
    /// Whole-machine CPU, 0..100.
    pub cpu_pct: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    pub swap_used: u64,
    /// Average clock speed across cores; 0 when unavailable.
    pub clock_mhz: u32,
    /// Total disk throughput in bytes per second.
    pub disk_bps: u64,
    pub sensors: Sensors,
    pub procs: Vec<ProcRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    pub fn label(self) -> &'static str {
        match self {
            Confidence::Low => "low",
            Confidence::Medium => "medium",
            Confidence::High => "high",
        }
    }
}

/// One explained cause of slowness.
#[derive(Debug, Clone)]
pub struct Finding {
    pub title: String,
    pub evidence: Vec<String>,
    pub confidence: Confidence,
    pub hint: Option<String>,
    /// Used for ranking, higher first.
    pub score: f32,
}

/// One line per sensor: its label and reading, or `None` when unavailable on this machine.
/// Shared by `bb sensors`, `bb top` and the desktop window so they always agree.
pub fn describe_sensors(x: &Sensors) -> Vec<(&'static str, Option<String>)> {
    let power = x.on_ac.map(|ac| {
        let state = if ac { "plugged in" } else { "on battery" };
        let saver = if x.battery_saver == Some(true) { ", Battery Saver on" } else { "" };
        match x.battery_pct {
            Some(b) => format!("{b}%, {state}{saver}"),
            None => format!("{state}{saver}"),
        }
    });
    let gpu_throttle = x.gpu_throttle.map(|g| {
        let mut why = Vec::new();
        if g & GPU_THERMAL != 0 { why.push("heat"); }
        if g & GPU_POWER != 0 { why.push("power cap"); }
        if g & GPU_HW != 0 { why.push("hardware slowdown"); }
        if why.is_empty() { "none".to_string() } else { why.join(", ") }
    });
    vec![
        ("Battery / power", power),
        ("Temperature", x.temp_c.map(|c| format!("{c:.0} C (hottest system sensor)"))),
        ("CPU speed", x.freq_pct.map(|f| format!("{f:.0}% of rated maximum"))),
        ("GPU load", x.gpu_pct.map(|g| format!("{g:.0}%"))),
        ("GPU temperature", x.gpu_temp_c.map(|c| format!("{c:.0} C"))),
        ("GPU throttling", gpu_throttle),
        ("Disk latency", x.disk_latency_ms.map(|l| format!("{l:.1} ms per request"))),
        ("Disk queue", x.disk_queue.map(|q| format!("{q:.2} requests waiting"))),
        ("Disk busy", x.disk_busy.map(|b| format!("{b:.0}% of the time"))),
        ("Memory paging", x.page_out.map(|p| format!("{p:.0} pages/s written out ({:.1} MB/s)", p as f64 * 4.0 / 1024.0))),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_sensors_are_none_and_present_ones_read_naturally() {
        let rows = describe_sensors(&Sensors::default());
        assert_eq!(rows.len(), 10);
        assert!(rows.iter().all(|(_, v)| v.is_none()));

        let x = Sensors { on_ac: Some(false), battery_pct: Some(40), battery_saver: Some(true), gpu_throttle: Some(GPU_THERMAL | GPU_POWER), ..Default::default() };
        let rows = describe_sensors(&x);
        assert_eq!(rows[0].1.as_deref(), Some("40%, on battery, Battery Saver on"));
        assert_eq!(rows[5].1.as_deref(), Some("heat, power cap"));
    }
}