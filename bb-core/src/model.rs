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

/// One recorded moment.
#[derive(Debug, Clone, PartialEq)]
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
