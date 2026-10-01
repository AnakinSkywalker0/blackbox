mod install;
mod selftest;
mod update;

use bb_core::model::{Sample, Sensors, GPU_HW, GPU_POWER, GPU_THERMAL};
use bb_core::sensors::SensorReader;
use bb_core::rules::{analyze, Thresholds};
use bb_core::sampler::Sampler;
use bb_core::store::{default_db_path, Store};
use bb_core::timeparse::{parse_duration, parse_when};
use chrono::{DateTime, Local, TimeZone};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "bb", version, about = "A flight recorder for your computer's performance")]
struct Cli {
    /// Use a different database file
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args)]
struct RunOpts {
    /// Seconds between samples
    #[arg(long, default_value_t = 1)]
    interval: u64,
    /// Days of history to keep
    #[arg(long, default_value_t = 7)]
    retention_days: u32,
    /// Programs kept per sample, per category (CPU, disk, memory)
    #[arg(long, default_value_t = 5)]
    top_n: usize,
}

impl RunOpts {
    fn to_args(&self) -> Vec<String> {
        vec![
            "--interval".into(), self.interval.to_string(),
            "--retention-days".into(), self.retention_days.to_string(),
            "--top-n".into(), self.top_n.to_string(),
        ]
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Record one sample per interval until Ctrl+C
    Run(RunOpts),
    /// Record in the background (no terminal needed)
    Start {
        #[command(flatten)]
        opts: RunOpts,
        /// Print nothing (used by start at login)
        #[arg(long)]
        quiet: bool,
    },
    /// Stop the background recorder
    Stop,
    /// Set up bb: copy it onto your PATH, start recording, and start at every login
    Install {
        /// Don't start recording automatically at login
        #[arg(long)]
        no_autostart: bool,
    },
    /// Check GitHub for a newer version and install it (the only command that uses the network)
    Update {
        /// Only report whether an update exists; install nothing
        #[arg(long)]
        check: bool,
    },
    /// Undo `install`: stop recording, remove start at login and the PATH entry
    Uninstall {
        /// Also delete all recorded data
        #[arg(long)]
        purge: bool,
    },
    /// Show the biggest programs right now
    Top {
        /// How many programs to show
        #[arg(short, default_value_t = 10)]
        n: usize,
        /// Refresh every second until Ctrl+C
        #[arg(long)]
        watch: bool,
    },
    /// Explain why the machine was slow around WHEN
    Why {
        /// now, HH:MM, HH:MM:SS or "10m ago"
        #[arg(default_value = "now")]
        when: String,
        /// Total window analysed, centred on WHEN (30s, 90s, 10m...)
        #[arg(long, default_value = "4m")]
        span: String,
    },
    /// Tell blackbox whether the last `bb why` was right (stays on this machine)
    Feedback {
        /// right, partly or wrong
        verdict: Option<String>,
        /// An optional note, for example what the real cause was
        #[arg(short, long)]
        note: Option<String>,
        /// Show how often each kind of explanation was judged right
        #[arg(long)]
        summary: bool,
        /// Print all feedback as JSON, to share if you want to help tune the rules
        #[arg(long)]
        export: bool,
    },
    /// Show what has been recorded
    Status,
    /// Show which sensors (battery, temperature, GPU, disk latency) work on this machine
    Sensors,
    /// Cause real slowdowns on this machine and check that `bb why` names them
    Selftest {
        /// Run only these scenarios (control, cpu_hog, cpu_spike, disk_heavy, memory)
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
        /// Also run the memory-pressure test (makes the machine sluggish for ~20 s)
        #[arg(long)]
        memory: bool,
    },
    #[command(hide = true)]
    SelftestWorker {
        kind: String,
        #[arg(long)]
        secs: u64,
        #[arg(long, default_value_t = 0)]
        mb: u64,
    },
    /// Measure blackbox's own cost on this machine
    Bench {
        /// How long to measure, in seconds
        #[arg(long, default_value_t = 15)]
        secs: u64,
    },
}

fn main() {
    update::clean_up_old_version();
    let cli = Cli::parse();
    let db = cli.db.unwrap_or_else(default_db_path);
    let result = match cli.cmd {
        Cmd::Run(o) => run(&db, o.interval, o.retention_days, o.top_n),
        Cmd::Start { opts, quiet } => install::start(&db, &opts.to_args(), quiet),
        Cmd::Stop => install::stop(&db, false),
        Cmd::Install { no_autostart } => install::install(&db, no_autostart),
        Cmd::Uninstall { purge } => install::uninstall(&db, purge),
        Cmd::Update { check } => update::run(&db, check),
        Cmd::Top { n, watch } => top(n, watch),
        Cmd::Why { when, span } => why(&db, &when, &span),
        Cmd::Feedback { verdict, note, summary, export } => feedback(&db, verdict, note.as_deref().unwrap_or(""), summary, export),
        Cmd::Status => status(&db),
        Cmd::Sensors => sensors(),
        Cmd::Selftest { only, memory } => selftest::run(&only, memory, install::running_pid(&db).is_some()),
        Cmd::SelftestWorker { kind, secs, mb } => selftest::worker(&kind, secs, mb),
        Cmd::Bench { secs } => bench(secs),
    };
    if let Err(e) = result {
        eprintln!("bb: {e}");
        std::process::exit(1);
    }
}

type Res = Result<(), String>;

fn open(db: &std::path::Path) -> Result<Store, String> {
    Store::open(db).map_err(|e| format!("can't open database {}: {e}", db.display()))
}

fn fmt_time(ts: i64) -> String {
    match Local.timestamp_opt(ts, 0).single() {
        Some(t) => t.format("%a %H:%M:%S").to_string(),
        None => ts.to_string(),
    }
}

fn fmt_bytes(b: u64) -> String {
    let b = b as f64;
    if b >= 1e9 { format!("{:.1} GB", b / 1_073_741_824.0) }
    else if b >= 1e6 { format!("{:.0} MB", b / 1_048_576.0) }
    else { format!("{:.0} KB", b / 1024.0) }
}

fn stop_flag() -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let _ = ctrlc::set_handler(move || s.store(true, Ordering::SeqCst));
    stop
}

/// Sleeps until `deadline`, waking early on Ctrl+C.
fn sleep_until(deadline: Instant, stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(100)));
    }
}

fn run(db: &std::path::Path, interval: u64, retention_days: u32, top_n: usize) -> Res {
    let interval = Duration::from_secs(interval.max(1));
    let _pid = install::PidGuard::acquire(db)?;
    bb_core::sampler::raise_priority();
    let mut store = open(db)?;
    let stop = stop_flag();
    let mut sampler = Sampler::new(top_n);
    println!("Recording to {} every {}s. Press Ctrl+C to stop.", db.display(), interval.as_secs());

    let mut next = Instant::now() + interval;
    let mut last_maintain = Instant::now() - Duration::from_secs(3600);
    let mut count = 0u64;
    while !stop.load(Ordering::SeqCst) {
        sleep_until(next, &stop);
        if stop.load(Ordering::SeqCst) {
            break;
        }
        next += interval;
        if next < Instant::now() {
            next = Instant::now() + interval; // fell behind (e.g. laptop slept)
        }
        let now = Local::now().timestamp();
        let sample = sampler.sample(now);
        if let Err(e) = store.insert(&sample) {
            eprintln!("bb: write failed: {e}");
        }
        count += 1;
        if last_maintain.elapsed() >= Duration::from_secs(600) {
            last_maintain = Instant::now();
            if let Err(e) = store.maintain(now, retention_days) {
                eprintln!("bb: cleanup failed: {e}");
            }
        }
        if count % 60 == 0 {
            println!("{}  {} samples recorded", fmt_time(now), count);
        }
    }
    let _ = store.compact();
    println!("Stopped after {count} samples.");
    Ok(())
}

/// One line per sensor: its label and reading, or None when unavailable.
fn describe(x: &Sensors) -> Vec<(&'static str, Option<String>)> {
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

fn sensors() -> Res {
    let mut r = SensorReader::new();
    r.read(); // rate counters need a first reading to measure against
    std::thread::sleep(Duration::from_secs(1));
    let x = r.read();
    println!("Sensor readings on this machine:\n");
    let rows = describe(&x);
    for (label, v) in &rows {
        match v {
            Some(v) => println!("  {label:<16} {v}"),
            None => println!("  {label:<16} not available"),
        }
    }
    if let Some(name) = r.gpu_name() {
        println!("\nNVIDIA GPU monitored: {name}");
    }
    let missing = rows.iter().filter(|(_, v)| v.is_none()).count();
    if missing > 0 {
        println!("\n{missing} sensor(s) unavailable. `bb why` simply skips rules that need them.");
    }
    Ok(())
}

fn print_top(s: &Sample, n: usize) {
    println!(
        "CPU {:.0}%   memory {} / {}   swap {}   clock {} MHz   disk {}/s",
        s.cpu_pct, fmt_bytes(s.mem_used), fmt_bytes(s.mem_total),
        fmt_bytes(s.swap_used), s.clock_mhz, fmt_bytes(s.disk_bps)
    );
    let extras: Vec<String> = describe(&s.sensors)
        .into_iter()
        .filter_map(|(l, v)| v.map(|v| format!("{l}: {v}")))
        .collect();
    if !extras.is_empty() {
        println!("{}", extras.join("   "));
    }
    let mut rows = s.procs.clone();
    rows.sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct).then(b.mem_bytes.cmp(&a.mem_bytes)));
    println!("\n{:<34} {:>6} {:>10} {:>10}", "PROGRAM", "CPU%", "MEMORY", "DISK/s");
    for p in rows.iter().take(n) {
        let name = if p.count > 1 { format!("{} ({})", p.name, p.count) } else { p.name.clone() };
        let name: String = name.chars().take(34).collect();
        println!("{:<34} {:>5.1}% {:>10} {:>10}", name, p.cpu_pct, fmt_bytes(p.mem_bytes), fmt_bytes(p.disk_bps));
    }
}

fn top(n: usize, watch: bool) -> Res {
    // Keep enough programs per category that the merged list can fill `n` rows.
    let mut sampler = Sampler::new(n.max(1) * 3);
    let stop = stop_flag();
    loop {
        let s = sampler.sample_after(Duration::from_secs(1), Local::now().timestamp());
        if watch {
            print!("\x1b[2J\x1b[H");
        }
        print_top(&s, n);
        if !watch || stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        if watch {
            println!("\nCtrl+C to quit");
        }
    }
}

fn why(db: &std::path::Path, when: &str, span: &str) -> Res {
    let now: DateTime<Local> = Local::now();
    let centre = parse_when(when, now)?;
    let span = parse_duration(span)?.max(2);
    let half = span / 2;
    // "now" means the recent past, not a window that is half in the future.
    let (from, to) = if centre >= now.timestamp() - 1 {
        (centre - span, centre)
    } else {
        (centre - half, centre + half)
    };

    let store = open(db)?;
    let samples = store.window(from, to).map_err(|e| e.to_string())?;
    if samples.is_empty() {
        println!("Nothing was recorded then.");
        match store.stats().map_err(|e| e.to_string())? {
            s if s.samples == 0 => println!("The database is empty. Start the recorder with `bb run`."),
            s => println!(
                "Recorded data covers {} to {}. See `bb status`.",
                fmt_time(s.first_ts.unwrap_or(0)),
                fmt_time(s.last_ts.unwrap_or(0))
            ),
        }
        return Ok(());
    }

    println!("Window: {} to {}  ({} samples)\n", fmt_time(from), fmt_time(to), samples.len());
    let best = store.best_mhz().map_err(|e| e.to_string())?;
    let findings = analyze(&samples, best, &Thresholds::default());
    // Remember the answer so the user can say whether it was right.
    let titles: Vec<String> = findings.iter().map(|f| f.title.clone()).collect();
    let _ = store.log_why(Local::now().timestamp(), from, to, &titles, env!("CARGO_PKG_VERSION"));
    const ASK: &str = "Was this right? Tell blackbox with: bb feedback right | partly | wrong [--note TEXT]   (stays on this machine)";
    if findings.is_empty() {
        println!("No clear cause. CPU, memory, disk, GPU, heat and power all looked normal in this window.");
        println!("Try a shorter or different --span. It may also be something blackbox can't see yet (network, or a sensor `bb sensors` shows as unavailable).");
        println!("\n{ASK}");
        return Ok(());
    }
    println!("Most likely causes:\n");
    for (i, f) in findings.iter().enumerate() {
        println!("{:>2}. [{}] {}", i + 1, f.confidence.label(), f.title);
        for e in &f.evidence {
            println!("      - {e}");
        }
        if let Some(h) = &f.hint {
            println!("      > {h}");
        }
        println!();
    }
    println!("{ASK}");
    Ok(())
}

fn feedback(db: &std::path::Path, verdict: Option<String>, note: &str, summary: bool, export: bool) -> Res {
    let store = open(db)?;
    let rows = store.why_log().map_err(|e| e.to_string())?;
    if export {
        let json: Vec<serde_json::Value> = rows
            .iter()
            .map(|w| serde_json::json!({
                "asked_at": w.ts, "window_from": w.from_ts, "window_to": w.to_ts,
                "explanations": w.titles, "version": w.version, "verdict": w.verdict, "note": w.note,
            }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&json).map_err(|e| e.to_string())?);
        return Ok(());
    }
    if summary || verdict.is_none() {
        print_feedback_summary(&rows);
        if verdict.is_none() {
            return Ok(());
        }
    }
    let Some(v) = verdict else { return Ok(()) };
    let v = v.to_lowercase();
    if !["right", "partly", "wrong"].contains(&v.as_str()) {
        return Err(format!("'{v}' isn't right, partly or wrong"));
    }
    let note = (!note.trim().is_empty()).then(|| note.trim());
    match store.set_verdict(&v, note).map_err(|e| e.to_string())? {
        None => Err("there's no `bb why` answer to judge yet. Run `bb why` first.".into()),
        Some(w) => {
            let top = w.titles.first().map_or("no clear cause", |t| t.as_str());
            println!("Recorded: the answer about {} was {v}.", top);
            println!("Thanks. This stays on your machine. See the totals with `bb feedback --summary`.");
            Ok(())
        }
    }
}

/// Tallies verdicts against the top explanation of each answer.
fn print_feedback_summary(rows: &[bb_core::store::WhyLog]) {
    use std::collections::BTreeMap;
    let judged: Vec<_> = rows.iter().filter(|w| w.verdict.is_some()).collect();
    println!("Explanations given: {}. Judged by you: {}.", rows.len(), judged.len());
    if judged.is_empty() {
        println!("After a `bb why`, run `bb feedback right`, `partly` or `wrong` to start building this up.");
        return;
    }
    // kind -> (right, partly, wrong). The verdict is counted against the top explanation.
    let mut by: BTreeMap<&str, (u32, u32, u32)> = BTreeMap::new();
    for w in &judged {
        let kind = w.titles.first().map_or("no_cause", |t| bb_core::rules::kind_of(t));
        let e = by.entry(kind).or_default();
        match w.verdict.as_deref() {
            Some("right") => e.0 += 1,
            Some("partly") => e.1 += 1,
            _ => e.2 += 1,
        }
    }
    println!("\n{:<16} {:>6} {:>7} {:>6}", "TOP EXPLANATION", "right", "partly", "wrong");
    for (k, (r, p, x)) in by {
        println!("{k:<16} {r:>6} {p:>7} {x:>6}");
    }
    println!("\nA cause that is often wrong needs its threshold tuned. `bb feedback --export` prints everything as JSON.");
}

fn status(db: &std::path::Path) -> Res {
    println!("Database: {}", db.display());
    if !db.exists() {
        println!("No database yet. Start the recorder with `bb run`.");
        return Ok(());
    }
    let size: u64 = ["", "-wal", "-shm"]
        .iter()
        .filter_map(|ext| std::fs::metadata(format!("{}{}", db.display(), ext)).ok())
        .map(|m| m.len())
        .sum();
    println!("Size:     {}", fmt_bytes(size));
    let st = open(db)?.stats().map_err(|e| e.to_string())?;
    println!("Samples:  {}", st.samples);
    if let (Some(first), Some(last)) = (st.first_ts, st.last_ts) {
        println!("Covers:   {} to {}", fmt_time(first), fmt_time(last));
        let hrs = (last - first) as f64 / 3600.0;
        println!("Span:     {hrs:.1} hours");
        if st.best_mhz > 0 {
            println!("Best clock seen: {} MHz", st.best_mhz);
        }
        let age = Local::now().timestamp() - last;
        if age > 30 {
            println!("\nWarning: the last sample is {age}s old. The recorder doesn't seem to be running. Start it with `bb start`.");
        } else {
            println!("\nRecorder looks active (last sample {age}s ago).");
        }
    } else {
        println!("No samples yet.");
    }
    Ok(())
}

fn bench(secs: u64) -> Res {
    let path = std::env::temp_dir().join(format!("bb-bench-{}.db", std::process::id()));
    let result = bench_inner(&path, secs.max(2));
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{}", path.display(), ext));
    }
    result
}

fn bench_inner(path: &std::path::Path, secs: u64) -> Res {
    let mut store = open(path)?;
    let mut sampler = Sampler::new(5);
    let mut times = Vec::new();
    println!("Measuring for {secs}s...");
    let start = Instant::now();
    let mut tick = 0u64;
    while start.elapsed() < Duration::from_secs(secs) {
        tick += 1;
        std::thread::sleep((start + Duration::from_secs(tick)).saturating_duration_since(Instant::now()));
        let t0 = Instant::now();
        let s = sampler.sample(Local::now().timestamp());
        store.insert(&s).map_err(|e| e.to_string())?;
        times.push(t0.elapsed());
    }
    let ms: Vec<f64> = times.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    let avg = ms.iter().sum::<f64>() / ms.len() as f64;
    let worst = ms.iter().cloned().fold(0.0, f64::max);

    let mut sys = sysinfo::System::new();
    let pid = sysinfo::get_current_pid().map_err(|e| e.to_string())?;
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    let rss = sys.process(pid).map(|p| p.memory()).unwrap_or(0);

    println!("\nSamples taken:     {}", ms.len());
    println!("Work per sample:   {avg:.1} ms average, {worst:.1} ms worst");
    println!("CPU at 1 sample/s: about {:.2}% of one core", avg / 10.0);
    println!("Memory:            about {} resident", fmt_bytes(rss));
    Ok(())
}



