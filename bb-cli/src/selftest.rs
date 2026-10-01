//! `bb selftest`: cause real slowdowns on this machine and check that `bb why` names them.
//!
//! Each scenario records live samples through the real pipeline (sampler, storage,
//! window query, rule engine) while child processes generate a known load, then checks
//! the explanation. A quiet control run measures false alarms. Things that can't be
//! caused from software (heat, battery limits, a failing disk) are listed as not tested.

use bb_core::model::{Finding, Sample};
use bb_core::rules::{analyze, Thresholds};
use bb_core::sampler::{raise_priority, Sampler};
use bb_core::store::Store;
use std::io::{Seek, SeekFrom, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use sysinfo::System;

type Res = Result<(), String>;

const MB: u64 = 1024 * 1024;

/// What a scenario must produce for the run to count as a detection.
enum Expect {
    /// No explanation at all (a quiet machine).
    Nothing,
    /// At least one finding accepted by this check.
    Finding(fn(&Finding) -> bool),
}

struct Scenario {
    name: &'static str,
    induced: String,
    calm_before: u64,
    load: u64,
    calm_after: u64,
    /// Worker processes to launch when the load phase starts: (kind, count, megabytes).
    workers: Vec<(&'static str, usize, u64)>,
    expect: Expect,
}

/// How a scenario ended. NOISY means the machine wasn't quiet, so the result can't be judged.
#[derive(Clone, Copy, PartialEq)]
enum Status {
    Pass,
    Fail,
    Noisy,
}

struct Outcome {
    name: &'static str,
    status: Status,
    pass: bool,
    ranked_first: bool,
    detail: String,
    extras: Vec<String>,
    samples: usize,
    expected_samples: u64,
}

// ---- workers (run as child processes of this same program) -----------------------

/// Entry point for `bb selftest-worker`. Generates one kind of load, then exits.
pub fn worker(kind: &str, secs: u64, mb: u64) -> Res {
    let end = Instant::now() + Duration::from_secs(secs);
    match kind {
        "cpu" => {
            let mut x = 1.0f64;
            while Instant::now() < end {
                for _ in 0..200_000 {
                    x = x * 1.000_000_1 + 1e-9;
                }
                std::hint::black_box(x);
            }
            Ok(())
        }
        "mem" => {
            // Touch every page so the memory is really used, not just reserved.
            let mut held: Vec<Vec<u8>> = Vec::new();
            for _ in 0..(mb / 64).max(1) {
                let mut chunk = vec![0u8; (64 * MB) as usize];
                for i in (0..chunk.len()).step_by(4096) {
                    chunk[i] = 1;
                }
                held.push(chunk);
            }
            while Instant::now() < end {
                std::thread::sleep(Duration::from_millis(200));
            }
            std::hint::black_box(&held);
            Ok(())
        }
        "disk" => {
            let path = std::env::temp_dir().join(format!("bb-selftest-{}.tmp", std::process::id()));
            let buf = vec![0xA5u8; (32 * MB) as usize];
            let mut f = std::fs::File::create(&path).map_err(|e| e.to_string())?;
            let mut written = 0u64;
            while Instant::now() < end {
                f.write_all(&buf).map_err(|e| e.to_string())?;
                f.sync_data().map_err(|e| e.to_string())?; // force it to the device
                written += buf.len() as u64;
                if written > 2048 * MB {
                    f.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
                    written = 0;
                }
            }
            drop(f);
            let _ = std::fs::remove_file(&path);
            Ok(())
        }
        other => Err(format!("unknown worker kind '{other}'")),
    }
}

fn spawn_workers(plan: &[(&'static str, usize, u64)], secs: u64) -> Vec<Child> {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut kids = Vec::new();
    for (kind, count, mb) in plan {
        for _ in 0..*count {
            let mut c = Command::new(&exe);
            c.args(["selftest-worker", kind, "--secs", &secs.to_string(), "--mb", &mb.to_string()]);
            c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            }
            if let Ok(child) = c.spawn() {
                kids.push(child);
            }
        }
    }
    kids
}

fn stop_workers(kids: &mut Vec<Child>) {
    for k in kids.iter_mut() {
        let _ = k.kill();
        let _ = k.wait();
    }
    kids.clear();
}

// ---- the checks ---------------------------------------------------------------------

fn is_cpu_cause(f: &Finding) -> bool {
    // Either one program named as the hog (this program's own workers), or saturation.
    (f.title.starts_with("bb") && f.title.contains("large share of the CPU")) || f.title.contains("saturated")
}
fn is_cpu_spike(f: &Finding) -> bool {
    f.title.contains("short CPU spike")
}
fn is_memory(f: &Finding) -> bool {
    f.title.starts_with("Memory pressure") || f.title.contains("short memory squeeze")
}
fn is_disk(f: &Finding) -> bool {
    f.title == "Heavy disk activity"
}

/// Memory to hold so that physical RAM runs out by about 1 GB, forcing the OS to page, or
/// why that isn't safe to try. Just filling RAM isn't enough: Windows compresses memory
/// and copes without paging, so it is not a slowdown.
fn memory_plan() -> Result<u64, String> {
    let mut sys = System::new();
    sys.refresh_memory();
    let (total, used) = (sys.total_memory(), sys.used_memory());
    let available = total.saturating_sub(used);
    let need = available + 1024 * MB;
    if need > 10 * 1024 * MB {
        return Err(format!("it would need {} GB, more than the 10 GB safety cap", need / (1024 * MB)));
    }
    // The extra gigabyte has to fit in the pagefile or swap, or the system could run out.
    let spare_swap = sys.total_swap().saturating_sub(sys.used_swap());
    if spare_swap < 3 * 1024 * MB {
        return Err("there isn't at least 3 GB of free swap/pagefile to absorb it safely".into());
    }
    Ok(need / MB)
}

fn scenarios(cores: usize, include_memory: bool) -> Vec<Result<Scenario, (String, String)>> {
    let mut v: Vec<Result<Scenario, (String, String)>> = vec![
        Ok(Scenario {
            name: "control",
            induced: "nothing: 25 s of an idle machine".into(),
            calm_before: 25,
            load: 0,
            calm_after: 0,
            workers: vec![],
            expect: Expect::Nothing,
        }),
        Ok(Scenario {
            name: "cpu_hog",
            induced: format!("{cores} programs spinning for 20 s"),
            calm_before: 5,
            load: 20,
            calm_after: 3,
            workers: vec![("cpu", cores, 0)],
            expect: Expect::Finding(is_cpu_cause),
        }),
        Ok(Scenario {
            name: "cpu_spike",
            induced: "all cores busy for 7 s inside a quiet 35 s".into(),
            calm_before: 20,
            load: 7,
            calm_after: 8,
            workers: vec![("cpu", cores, 0)],
            expect: Expect::Finding(is_cpu_spike),
        }),
        Ok(Scenario {
            name: "disk_heavy",
            induced: "sustained forced disk writes for 15 s".into(),
            calm_before: 3,
            load: 15,
            calm_after: 2,
            workers: vec![("disk", 1, 0)],
            expect: Expect::Finding(is_disk),
        }),
    ];
    if include_memory {
        v.push(match memory_plan() {
            Ok(mb) => Ok(Scenario {
                name: "memory",
                induced: format!("{} GB held for 15 s, 1 GB more than is free, so the OS must page", mb / 1024 + 1),
                calm_before: 2,
                load: 15,
                calm_after: 3,
                workers: vec![("mem", 1, mb)],
                expect: Expect::Finding(is_memory),
            }),
            Err(why) => Err(("memory".into(), why)),
        });
    } else {
        v.push(Err(("memory".into(), "skipped by default: it uses all free memory plus 1 GB for 15 s, so the machine can lag or freeze briefly. Save your work, then add --memory to run it".into())));
    }
    v
}

fn run_scenario(sc: &Scenario) -> Outcome {
    let total = sc.calm_before + sc.load + sc.calm_after;
    let mut sampler = Sampler::new(5);
    std::thread::sleep(Duration::from_millis(600));
    let mut samples: Vec<Sample> = Vec::new();
    let mut kids: Vec<Child> = Vec::new();
    let start = Instant::now();
    let mut started = false;
    for tick in 1..=total {
        let at = start + Duration::from_secs(tick);
        // Start and stop the load on schedule (relative to when each second began).
        let elapsed = tick - 1;
        if !started && sc.load > 0 && elapsed >= sc.calm_before {
            kids = spawn_workers(&sc.workers, sc.load);
            started = true;
        }
        if started && elapsed >= sc.calm_before + sc.load {
            stop_workers(&mut kids);
        }
        let now = Instant::now();
        if at > now {
            std::thread::sleep(at - now);
        }
        samples.push(sampler.sample(chrono::Local::now().timestamp()));
    }
    stop_workers(&mut kids);

    // Through the real storage path, as `bb why` would see it.
    let findings = match Store::open_in_memory().and_then(|mut st| {
        st.insert_many(&samples)?;
        let (from, to) = (samples.first().map_or(0, |s| s.ts), samples.last().map_or(0, |s| s.ts));
        let w = st.window(from, to)?;
        let best = st.best_mhz()?;
        Ok(analyze(&w, best, &Thresholds::default()))
    }) {
        Ok(f) => f,
        Err(e) => {
            return Outcome {
                name: sc.name,
                status: Status::Fail,
                pass: false,
                ranked_first: false,
                detail: format!("storage error: {e}"),
                extras: vec![],
                samples: samples.len(),
                expected_samples: total,
            }
        }
    };

    let (pass, ranked_first, detail, extras) = match &sc.expect {
        Expect::Nothing => {
            if findings.is_empty() {
                (true, true, "no findings, as it should be".to_string(), vec![])
            } else {
                let t: Vec<String> = findings.iter().map(|f| f.title.clone()).collect();
                // Show why each fired, and what the machine really did, so it can be judged.
                let mut why: Vec<String> = findings.iter().flat_map(|f| f.evidence.iter().map(|e| format!("because {e}"))).collect();
                why.push(observed(&samples));
                let label = if machine_was_busy(&samples) { "NOT QUIET" } else { "FALSE ALARM" };
                (false, false, format!("{label}: {}", t.join("; ")), why)
            }
        }
        Expect::Finding(ok) => match findings.iter().position(|f| ok(f)) {
            Some(i) => {
                let extras = findings.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, f)| f.title.clone()).collect();
                (true, i == 0, findings[i].title.clone(), extras)
            }
            None => {
                let seen = if findings.is_empty() { "nothing".to_string() } else { findings.iter().map(|f| f.title.clone()).collect::<Vec<_>>().join("; ") };
                (false, false, format!("MISSED. bb why said: {seen}"), vec![observed(&samples)])
            }
        },
    };
    // A failed control only counts against blackbox if the machine really was quiet.
    let status = if pass {
        Status::Pass
    } else if matches!(sc.expect, Expect::Nothing) && machine_was_busy(&samples) {
        Status::Noisy
    } else {
        Status::Fail
    };
    Outcome { name: sc.name, status, pass, ranked_first, detail, extras, samples: samples.len(), expected_samples: total }
}

/// Did the machine's own measurements show real background load during a "quiet" run?
/// If so, a finding there is not a false alarm: something really was using the machine.
fn machine_was_busy(samples: &[Sample]) -> bool {
    let n = samples.len().max(1) as f32;
    let cpu_avg = samples.iter().map(|s| s.cpu_pct).sum::<f32>() / n;
    let mem_avg = samples
        .iter()
        .map(|s| if s.mem_total == 0 { 0.0 } else { s.mem_used as f32 * 100.0 / s.mem_total as f32 })
        .sum::<f32>()
        / n;
    let disk_peak_mb = samples.iter().map(|s| s.disk_bps).max().unwrap_or(0) / MB;
    cpu_avg >= 15.0 || mem_avg >= 90.0 || disk_peak_mb >= 80
}

/// A one-line summary of what the machine actually did, to explain a miss.
fn observed(samples: &[Sample]) -> String {
    let n = samples.len().max(1) as f32;
    let mem = |s: &Sample| if s.mem_total == 0 { 0.0 } else { s.mem_used as f32 * 100.0 / s.mem_total as f32 };
    let cpu_avg = samples.iter().map(|s| s.cpu_pct).sum::<f32>() / n;
    let cpu_max = samples.iter().map(|s| s.cpu_pct).fold(0.0, f32::max);
    let mem_avg = samples.iter().map(mem).sum::<f32>() / n;
    let mem_max = samples.iter().map(mem).fold(0.0, f32::max);
    let paging = samples.iter().filter_map(|s| s.sensors.page_out).fold(0.0, f32::max);
    let disk = samples.iter().map(|s| s.disk_bps).max().unwrap_or(0) / MB;
    format!("what it saw: CPU avg {cpu_avg:.0}% (max {cpu_max:.0}%), memory avg {mem_avg:.0}% (max {mem_max:.0}%), paging peak {paging:.0} pages/s, disk peak {disk} MB/s")
}

// ---- the command --------------------------------------------------------------------

pub fn run(only: &[String], include_memory: bool, recorder_running: bool) -> Res {
    raise_priority();
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    println!("blackbox selftest: causing real slowdowns and checking that `bb why` names them.");
    println!("The machine will be busy for a few minutes. Close heavy programs first for a cleaner result.");
    if recorder_running {
        println!("Note: a recorder is running, so its history will contain these test loads. They will show up in");
        println!("`bb why` for this time. Run `bb stop` first (and `bb start` after) to keep your history clean.");
    }
    println!();

    let all = scenarios(cores, include_memory);
    let mut outcomes: Vec<Outcome> = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    let mut noisy = false;
    for item in all {
        match item {
            Err((name, why)) => skipped.push((name, why)),
            Ok(sc) => {
                if !only.is_empty() && !only.iter().any(|o| o == sc.name) {
                    continue;
                }
                println!("  running {:<10} {} ...", sc.name, sc.induced);
                let mut o = run_scenario(&sc);
                if o.name == "control" && o.status == Status::Noisy {
                    noisy = true;
                } else if noisy && o.status == Status::Fail {
                    // A busy machine can hide or mimic what we induce, so a miss proves nothing.
                    o.status = Status::Noisy;
                    o.detail = format!("{} (not judged: the machine wasn't quiet)", o.detail);
                }
                println!("  -> {}", match o.status { Status::Pass => "pass", Status::Fail => "FAIL", Status::Noisy => "noisy" });
                outcomes.push(o);
            }
        }
    }

    println!("\n{:<11} {:<5} {:<7} {}", "SCENARIO", "", "RANK", "WHAT `bb why` SAID");
    for o in &outcomes {
        let rank = if o.name == "control" { "-" } else if o.ranked_first { "1st" } else if o.pass { "later" } else { "-" };
        let tag = match o.status { Status::Pass => "PASS", Status::Fail => "FAIL", Status::Noisy => "NOISY" };
        println!("{:<11} {:<5} {:<7} {}", o.name, tag, rank, o.detail);
        for e in &o.extras {
            println!("{:<24} also reported: {e}", "");
        }
        if o.samples as u64 + 2 < o.expected_samples {
            println!("{:<24} note: only {} of {} samples were recorded, so the machine was too busy to sample every second", "", o.samples, o.expected_samples);
        }
    }

    let induced: Vec<&Outcome> = outcomes.iter().filter(|o| o.name != "control").collect();
    let detected = induced.iter().filter(|o| o.pass).count();
    let first = induced.iter().filter(|o| o.pass && o.ranked_first).count();
    let unjudged = induced.iter().filter(|o| o.status == Status::Noisy).count();
    let judged = induced.len() - unjudged;
    let false_alarms = outcomes.iter().filter(|o| o.name == "control" && o.status == Status::Fail).count();
    println!("\nDetected {detected} of {judged} induced slowdowns ({first} as the top explanation). False alarms on a quiet machine: {false_alarms}.");
    if noisy {
        println!("The machine was not quiet (something was really using it during the control run), so {unjudged} scenario(s) could not be judged. Close other programs and run again for a clean result.");
    }

    if !skipped.is_empty() {
        println!("\nNot run:");
        for (n, why) in &skipped {
            println!("  {n}: {why}");
        }
    }
    println!("\nCan't be caused from software, so only covered by synthetic tests, not real ones:");
    println!("  heat/thermal throttling, battery and power-mode limits, GPU load or throttling, a slow or failing disk, machine stalls.");

    let failed = outcomes.iter().filter(|o| o.status == Status::Fail).count();
    if failed > 0 {
        return Err(format!("{failed} scenario(s) failed"));
    }
    Ok(())
}
