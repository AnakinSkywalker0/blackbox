//! Battery, temperature, GPU and disk-latency readings.
//!
//! Everything here is best effort. A sensor that isn't available on this machine
//! comes back as `None`, so the rules never mistake "unknown" for "fine".

use crate::model::{Sensors, GPU_HW, GPU_POWER, GPU_THERMAL};
use std::path::Path;

pub struct SensorReader {
    #[cfg(windows)]
    win: win::Win,
    #[cfg(target_os = "linux")]
    swap_out: SwapRate,
    #[cfg(target_os = "linux")]
    disk_busy: DiskBusy,
    nvml: Option<nvml_wrapper::Nvml>,
}

impl Default for SensorReader {
    fn default() -> Self {
        Self::new()
    }
}

impl SensorReader {
    pub fn new() -> SensorReader {
        SensorReader {
            #[cfg(windows)]
            win: win::Win::new(),
            #[cfg(target_os = "linux")]
            swap_out: SwapRate::default(),
            #[cfg(target_os = "linux")]
            disk_busy: DiskBusy::default(),
            // Loads nvml.dll / libnvidia-ml.so at runtime; absent on non-NVIDIA machines.
            nvml: nvml_wrapper::Nvml::init().ok(),
        }
    }

    /// Reads every available sensor. Call about once a second: rate counters
    /// such as disk latency measure the time since the previous call.
    pub fn read(&mut self) -> Sensors {
        let mut s = Sensors::default();
        #[cfg(windows)]
        self.win.read(&mut s);
        #[cfg(target_os = "linux")]
        {
            read_battery_sysfs(Path::new("/sys/class/power_supply"), &mut s);
            s.temp_c = read_thermal_sysfs(Path::new("/sys/class/thermal"));
            s.page_out = self.swap_out.read();
            s.disk_busy = self.disk_busy.read();
        }
        if let Some(nvml) = &self.nvml {
            read_nvml(nvml, &mut s);
        }
        s
    }

    /// Names the sensor sources that produced data, for `bb sensors`.
    pub fn gpu_name(&self) -> Option<String> {
        self.nvml.as_ref()?.device_by_index(0).ok()?.name().ok()
    }
}

fn read_nvml(nvml: &nvml_wrapper::Nvml, s: &mut Sensors) {
    use nvml_wrapper::bitmasks::device::ThrottleReasons as R;
    use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
    let Ok(dev) = nvml.device_by_index(0) else { return };
    s.gpu_temp_c = dev.temperature(TemperatureSensor::Gpu).ok().map(|t| t as f32);
    if let Ok(r) = dev.current_throttle_reasons() {
        let mut bits = 0;
        if r.intersects(R::SW_THERMAL_SLOWDOWN | R::HW_THERMAL_SLOWDOWN) {
            bits |= GPU_THERMAL;
        }
        if r.intersects(R::SW_POWER_CAP | R::HW_POWER_BRAKE_SLOWDOWN) {
            bits |= GPU_POWER;
        }
        if r.intersects(R::HW_SLOWDOWN) {
            bits |= GPU_HW;
        }
        s.gpu_throttle = Some(bits);
    }
}

// ---- Linux (std only, testable anywhere) -----------------------------------------

/// Milliseconds each whole disk has spent busy, from the text of `/proc/diskstats`.
/// Partitions are skipped so the same I/O isn't counted twice.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn parse_io_ticks(text: &str) -> Vec<(String, u64)> {
    let whole_disk = |n: &str| {
        let digits_end = n.trim_end_matches(|c: char| c.is_ascii_digit());
        // sda, vda, xvda, hda; nvme0n1; mmcblk0. Their partitions end in a number (sda1, nvme0n1p1).
        (n.starts_with("sd") || n.starts_with("vd") || n.starts_with("xvd") || n.starts_with("hd")) && digits_end.len() == n.len()
            || (n.starts_with("nvme") && !n.contains('p') )
            || (n.starts_with("mmcblk") && !n.contains('p'))
    };
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            // major minor name, then 11+ counters; io_ticks is the 10th counter (index 12).
            let (name, ticks) = (f.get(2)?, f.get(12)?.parse::<u64>().ok()?);
            whole_disk(name).then(|| (name.to_string(), ticks))
        })
        .collect()
}

/// Turns the busy-milliseconds counters into the busiest disk's percent busy.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct DiskBusy {
    last: Option<(std::time::Instant, Vec<(String, u64)>)>,
}

#[cfg(target_os = "linux")]
impl DiskBusy {
    fn read(&mut self) -> Option<f32> {
        let now_ticks = parse_io_ticks(&std::fs::read_to_string("/proc/diskstats").ok()?);
        let now = std::time::Instant::now();
        let busy = self.last.as_ref().and_then(|(t, prev)| {
            let ms = now.duration_since(*t).as_secs_f32().max(0.05) * 1000.0;
            now_ticks
                .iter()
                .filter_map(|(n, v)| prev.iter().find(|(pn, _)| pn == n).map(|(_, pv)| (v.saturating_sub(*pv) as f32 / ms * 100.0).min(100.0)))
                .fold(None, |m: Option<f32>, b| Some(m.map_or(b, |m| m.max(b))))
        });
        self.last = Some((now, now_ticks));
        busy
    }
}

/// Pages swapped out since boot, from the text of `/proc/vmstat`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn parse_pswpout(text: &str) -> Option<u64> {
    text.lines().find_map(|l| l.strip_prefix("pswpout ")?.trim().parse().ok())
}

/// Turns the ever-growing swap-out counter into pages per second.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct SwapRate {
    last: Option<(std::time::Instant, u64)>,
}

#[cfg(target_os = "linux")]
impl SwapRate {
    fn read(&mut self) -> Option<f32> {
        let n = parse_pswpout(&std::fs::read_to_string("/proc/vmstat").ok()?)?;
        let now = std::time::Instant::now();
        let rate = self.last.map(|(t, prev)| n.saturating_sub(prev) as f32 / now.duration_since(t).as_secs_f32().max(0.05));
        self.last = Some((now, n));
        rate
    }
}

/// Reads battery level and AC state from a `power_supply` directory.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn read_battery_sysfs(root: &Path, s: &mut Sensors) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let read = |f: &str| std::fs::read_to_string(p.join(f)).ok().map(|v| v.trim().to_string());
        match read("type").as_deref() {
            Some("Battery") => {
                if s.battery_pct.is_none() {
                    s.battery_pct = read("capacity").and_then(|v| v.parse().ok());
                }
            }
            Some("Mains") => {
                if let Some(on) = read("online") {
                    s.on_ac = Some(s.on_ac.unwrap_or(false) || on == "1");
                }
            }
            _ => {}
        }
    }
    // A desktop has no battery, so an AC reading alone says nothing useful.
    if s.battery_pct.is_none() {
        s.on_ac = None;
    }
}

/// Hottest thermal zone in degrees C, from a `thermal` directory.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn read_thermal_sysfs(root: &Path) -> Option<f32> {
    let mut best: Option<f32> = None;
    for e in std::fs::read_dir(root).ok()?.flatten() {
        let name = e.file_name();
        if !name.to_string_lossy().starts_with("thermal_zone") {
            continue;
        }
        let milli: Option<f32> = std::fs::read_to_string(e.path().join("temp")).ok().and_then(|v| v.trim().parse().ok());
        if let Some(c) = milli.map(|m| m / 1000.0).filter(|c| plausible_temp(*c)) {
            best = Some(best.map_or(c, |b| b.max(c)));
        }
    }
    best
}

/// Sensors sometimes report 0 or garbage when unsupported.
pub(crate) fn plausible_temp(c: f32) -> bool {
    (10.0..=130.0).contains(&c)
}

/// Converts a Windows thermal-zone value (tenths of a kelvin) to degrees C.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn decikelvin_to_c(v: f64) -> f32 {
    (v / 10.0 - 273.15) as f32
}

/// The busiest physical disk's percent busy, from "% Idle Time" instances (skipping "_Total").
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn busiest_disk(idle: &[(String, f64)]) -> Option<f32> {
    idle.iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("_total"))
        .map(|(_, idle)| (100.0 - *idle as f32).clamp(0.0, 100.0))
        .fold(None, |m: Option<f32>, b| Some(m.map_or(b, |m| m.max(b))))
}

/// Takes the busiest engine across GPU adapters from Windows "GPU Engine" counter
/// instances. Instance names look like
/// `pid_123_luid_0x0_0x1f_phys_0_eng_0_engtype_3d`; load from all processes on
/// the same engine type of the same adapter adds up, and the busiest total wins
/// (this is how Task Manager shows GPU %).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn busiest_gpu_engine(instances: &[(String, f64)]) -> Option<f32> {
    use std::collections::HashMap;
    let mut totals: HashMap<(String, String), f64> = HashMap::new();
    for (name, v) in instances {
        let name = name.to_lowercase();
        let (Some(l), Some(t)) = (name.find("luid_"), name.find("engtype_")) else { continue };
        let luid = name[l..].split("_phys").next().unwrap_or("").to_string();
        let engine = name[t + 8..].to_string();
        *totals.entry((luid, engine)).or_default() += v.max(0.0);
    }
    totals.values().cloned().fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v)))).map(|v| v.min(100.0) as f32)
}

// ---- Windows: performance counters and power status ---------------------------------

#[cfg(windows)]
mod win {
    use super::*;
    use windows_sys::Win32::System::Performance::*;
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// A PDH query that closes itself.
    struct Query(PDH_HQUERY);

    impl Query {
        fn new() -> Option<Query> {
            let mut h: PDH_HQUERY = std::ptr::null_mut();
            let rc = unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut h) };
            (rc == 0 && !h.is_null()).then_some(Query(h))
        }

        fn add(&self, path: &str) -> Option<PDH_HCOUNTER> {
            let mut c: PDH_HCOUNTER = std::ptr::null_mut();
            let rc = unsafe { PdhAddEnglishCounterW(self.0, wide(path).as_ptr(), 0, &mut c) };
            (rc == 0 && !c.is_null()).then_some(c)
        }

        fn collect(&self) -> bool {
            unsafe { PdhCollectQueryData(self.0) == 0 }
        }
    }

    impl Drop for Query {
        fn drop(&mut self) {
            unsafe { PdhCloseQuery(self.0) };
        }
    }

    // PDH handles are plain pointers owned by the query; we only use them from one thread.
    unsafe impl Send for Query {}

    fn valid(status: u32) -> bool {
        // PDH_CSTATUS_VALID_DATA = 0, PDH_CSTATUS_NEW_DATA = 1.
        status == 0 || status == 1
    }

    fn value(c: PDH_HCOUNTER) -> Option<f64> {
        let mut v: PDH_FMT_COUNTERVALUE = unsafe { std::mem::zeroed() };
        let rc = unsafe { PdhGetFormattedCounterValue(c, PDH_FMT_DOUBLE, std::ptr::null_mut(), &mut v) };
        (rc == 0 && valid(v.CStatus)).then(|| unsafe { v.Anonymous.doubleValue })
    }

    /// All instances of a wildcard counter, as (instance name, value).
    fn array(c: PDH_HCOUNTER) -> Vec<(String, f64)> {
        let (mut size, mut count) = (0u32, 0u32);
        let rc = unsafe { PdhGetFormattedCounterArrayW(c, PDH_FMT_DOUBLE, &mut size, &mut count, std::ptr::null_mut()) };
        if rc != PDH_MORE_DATA || size == 0 {
            return Vec::new();
        }
        // u64 elements keep the buffer 8-byte aligned for the item structs.
        let mut buf = vec![0u64; (size as usize).div_ceil(8)];
        let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
        let rc = unsafe { PdhGetFormattedCounterArrayW(c, PDH_FMT_DOUBLE, &mut size, &mut count, items) };
        if rc != 0 {
            return Vec::new();
        }
        (0..count as usize)
            .filter_map(|i| {
                let it = unsafe { &*items.add(i) };
                if !valid(it.FmtValue.CStatus) || it.szName.is_null() {
                    return None;
                }
                let mut len = 0;
                while unsafe { *it.szName.add(len) } != 0 {
                    len += 1;
                }
                let name = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(it.szName, len) });
                Some((name, unsafe { it.FmtValue.Anonymous.doubleValue }))
            })
            .collect()
    }

    /// GPU engines come and go as programs start and stop, and a wildcard counter
    /// only knows the instances that existed when it was added. So the query is
    /// rebuilt periodically; the new one is primed a tick ahead so rates are valid.
    struct GpuProbe {
        q: Query,
        c: PDH_HCOUNTER,
    }

    impl GpuProbe {
        fn new() -> Option<GpuProbe> {
            let q = Query::new()?;
            let c = q.add(r"\GPU Engine(*)\Utilization Percentage")?;
            q.collect();
            Some(GpuProbe { q, c })
        }
    }

    pub struct Win {
        main: Option<Query>,
        queue: Option<PDH_HCOUNTER>,
        latency: Option<PDH_HCOUNTER>,
        freq: Option<PDH_HCOUNTER>,
        temp_hp: Option<PDH_HCOUNTER>,
        temp: Option<PDH_HCOUNTER>,
        page_out: Option<PDH_HCOUNTER>,
        disk_idle: Option<PDH_HCOUNTER>,
        gpu: Option<GpuProbe>,
        gpu_next: Option<GpuProbe>,
        ticks: u64,
    }

    const GPU_REBUILD_EVERY: u64 = 30;

    impl Win {
        pub fn new() -> Win {
            let main = Query::new();
            let add = |p: &str| main.as_ref().and_then(|q| q.add(p));
            let w = Win {
                queue: add(r"\PhysicalDisk(_Total)\Avg. Disk Queue Length"),
                latency: add(r"\PhysicalDisk(_Total)\Avg. Disk sec/Transfer"),
                freq: add(r"\Processor Information(_Total)\% of Maximum Frequency"),
                temp_hp: add(r"\Thermal Zone Information(*)\High Precision Temperature"),
                temp: add(r"\Thermal Zone Information(*)\Temperature"),
                page_out: add(r"\Memory\Pages Output/sec"),
                // Every physical disk, so the busiest one counts. "_Total" averages a pinned
                // disk with an idle one and would hide it.
                disk_idle: add(r"\PhysicalDisk(*)\% Idle Time"),
                gpu: GpuProbe::new(),
                gpu_next: None,
                ticks: 0,
                main: None,
            };
            if let Some(q) = &main {
                q.collect(); // prime the rate counters
            }
            Win { main, ..w }
        }

        pub fn read(&mut self, s: &mut Sensors) {
            self.ticks += 1;
            read_power(s);

            if let Some(q) = &self.main {
                if q.collect() {
                    s.disk_queue = self.queue.and_then(value).map(|v| v as f32);
                    s.disk_latency_ms = self.latency.and_then(value).map(|v| (v * 1000.0) as f32);
                    s.freq_pct = self.freq.and_then(value).map(|v| v as f32);
                    s.page_out = self.page_out.and_then(value).map(|v| v as f32);
                    s.disk_busy = self.disk_idle.and_then(|c| busiest_disk(&array(c)));
                    let zones = self
                        .temp_hp
                        .map(array)
                        .filter(|z| !z.is_empty())
                        .map(|z| z.into_iter().map(|(_, v)| decikelvin_to_c(v)).collect::<Vec<_>>())
                        .or_else(|| {
                            self.temp.map(array).map(|z| z.into_iter().map(|(_, v)| decikelvin_to_c(v * 10.0)).collect())
                        })
                        .unwrap_or_default();
                    s.temp_c = zones.into_iter().filter(|c| plausible_temp(*c)).fold(None, |m: Option<f32>, c| {
                        Some(m.map_or(c, |m| m.max(c)))
                    });
                }
            }

            if let Some(next) = self.gpu_next.take() {
                self.gpu = Some(next);
            }
            if self.ticks % GPU_REBUILD_EVERY == 0 {
                self.gpu_next = GpuProbe::new();
            }
            if let Some(g) = &self.gpu {
                if g.q.collect() {
                    s.gpu_pct = busiest_gpu_engine(&array(g.c));
                }
            }
        }
    }

    fn read_power(s: &mut Sensors) {
        let mut p: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
        if unsafe { GetSystemPowerStatus(&mut p) } == 0 {
            return;
        }
        // BatteryFlag bit 128 means the machine has no battery (a desktop); 255 is unknown.
        if p.BatteryFlag & 128 != 0 || p.BatteryFlag == 255 {
            return;
        }
        s.on_ac = match p.ACLineStatus {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        };
        s.battery_pct = (p.BatteryLifePercent <= 100).then_some(p.BatteryLifePercent);
        s.battery_saver = Some(p.SystemStatusFlag & 1 != 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("bb-sensors-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    #[test]
    fn battery_on_battery() {
        let d = tmp("bat");
        write(&d, "BAT0/type", "Battery\n");
        write(&d, "BAT0/capacity", "42\n");
        write(&d, "AC/type", "Mains\n");
        write(&d, "AC/online", "0\n");
        let mut s = Sensors::default();
        read_battery_sysfs(&d, &mut s);
        assert_eq!((s.battery_pct, s.on_ac), (Some(42), Some(false)));
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn desktop_without_battery_reports_nothing() {
        let d = tmp("desk");
        write(&d, "AC/type", "Mains\n");
        write(&d, "AC/online", "1\n");
        let mut s = Sensors::default();
        read_battery_sysfs(&d, &mut s);
        assert_eq!(s, Sensors::default());
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn thermal_picks_hottest_plausible_zone() {
        let d = tmp("therm");
        write(&d, "thermal_zone0/temp", "45000\n");
        write(&d, "thermal_zone1/temp", "83500\n");
        write(&d, "thermal_zone2/temp", "0\n"); // unsupported sensor
        write(&d, "cooling_device0/temp", "99000\n"); // not a zone
        assert_eq!(read_thermal_sysfs(&d), Some(83.5));
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn missing_dirs_are_none() {
        let mut s = Sensors::default();
        read_battery_sysfs(Path::new("/definitely/not/here"), &mut s);
        assert_eq!(s, Sensors::default());
        assert_eq!(read_thermal_sysfs(Path::new("/definitely/not/here")), None);
    }

    #[test]
    fn busiest_disk_beats_the_average() {
        let idle = |n: &str, v: f64| (n.to_string(), v);
        // C: pinned (0% idle), D: idle (100%); "_Total" would say 50% busy.
        let got = busiest_disk(&[idle("0 C:", 0.0), idle("1 D:", 100.0), idle("_Total", 50.0)]);
        assert_eq!(got, Some(100.0));
        assert_eq!(busiest_disk(&[idle("_Total", 10.0)]), None);
        assert_eq!(busiest_disk(&[]), None);
        assert_eq!(busiest_disk(&[idle("0 C:", 130.0)]), Some(0.0)); // out-of-range readings are clamped
    }

    #[test]
    fn reads_disk_busy_time_for_whole_disks_only() {
        let diskstats = "\
   8       0 sda 100 0 800 50 200 0 1600 70 0 4000 120 0 0 0 0
   8       1 sda1 90 0 700 40 190 0 1500 60 0 3900 100 0 0 0 0
 259       0 nvme0n1 10 0 80 5 20 0 160 7 0 1234 12 0 0 0 0
 259       1 nvme0n1p1 9 0 70 4 19 0 150 6 0 1200 10 0 0 0 0
   7       0 loop0 1 0 2 0 0 0 0 0 0 5 0 0 0 0 0
 252       0 vda 1 0 2 0 0 0 0 0 0 777 0 0 0 0 0
";
        let got = parse_io_ticks(diskstats);
        let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["sda", "nvme0n1", "vda"]);
        assert_eq!(got[0].1, 4000);
        assert_eq!(got[1].1, 1234);
        assert!(parse_io_ticks("").is_empty());
    }

    #[test]
    fn parses_swap_out_counter() {
        let vmstat = "nr_free_pages 12345\npswpin 77\npswpout 4242\npgpgin 5\n";
        assert_eq!(parse_pswpout(vmstat), Some(4242));
        assert_eq!(parse_pswpout("pswpin 1\n"), None);
        assert_eq!(parse_pswpout(""), None);
    }

    #[test]
    fn kelvin_conversion() {
        assert!((decikelvin_to_c(3322.0) - 59.05).abs() < 0.01);
    }

    #[test]
    fn gpu_engines_sum_per_adapter_and_type() {
        let inst = |n: &str, v: f64| (n.to_string(), v);
        let got = busiest_gpu_engine(&[
            inst("pid_1_luid_0x0_0xAA_phys_0_eng_0_engtype_3D", 30.0),
            inst("pid_2_luid_0x0_0xAA_phys_0_eng_0_engtype_3D", 25.0),
            inst("pid_2_luid_0x0_0xAA_phys_0_eng_1_engtype_Copy", 10.0),
            inst("pid_3_luid_0x0_0xBB_phys_0_eng_0_engtype_3D", 40.0),
        ]);
        assert_eq!(got, Some(55.0));
        assert_eq!(busiest_gpu_engine(&[]), None);
        assert_eq!(busiest_gpu_engine(&[("pid_1_luid_0x0_0x1_phys_0_eng_0_engtype_3D".into(), 250.0)]), Some(100.0));
    }

    #[test]
    fn live_read_does_not_panic() {
        let mut r = SensorReader::new();
        let _ = r.read();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _ = r.read();
    }
}
