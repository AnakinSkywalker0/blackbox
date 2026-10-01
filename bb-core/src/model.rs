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

