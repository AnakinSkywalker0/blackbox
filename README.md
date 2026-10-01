# blackbox (`bb`)

A flight recorder for your computer's performance.

Windows shows you numbers (Task Manager) or raw traces (Performance Recorder), but never tells you **why** it was slow. blackbox records your machine's vitals in the background. When something felt slow, you ask it:

```
bb why 15:40
```

and it answers in plain English with evidence.

> **Status: v0.1.** Records CPU, memory, pagefile, disk throughput, clock speed and the busiest programs, and explains five kinds of slowdown. See [Limits](#limits-read-this) before trusting it blindly.

---

## Contents

1. [Install](#install)
2. [Quick start](#quick-start)
3. [Commands](#commands)
4. [What `bb why` can explain](#what-bb-why-can-explain)
5. [Run it automatically at login (Windows)](#run-it-automatically-at-login-windows)
6. [How much it costs](#how-much-it-costs)
7. [Where your data lives](#where-your-data-lives)
8. [Limits (read this)](#limits-read-this)
9. [Troubleshooting](#troubleshooting)
10. [Project layout and tests](#project-layout-and-tests)
11. [Roadmap](#roadmap)

---

## Install

You build it from source. No admin rights are needed to build or run.

**1. Install Rust** from <https://rustup.rs>.

**2. On Windows, install the C++ build tools** (Rust needs a linker, and the bundled SQLite is compiled from C):
Visual Studio Build Tools, with the **"Desktop development with C++"** workload.

**3. Build:**

```
cd blackbox
cargo build --release
```

The program is `target\release\bb.exe` (Windows) or `target/release/bb` (Linux/macOS). Copy it somewhere on your `PATH`, for example `C:\Tools\bb.exe`, so you can just type `bb`.

> **Heads up:** v0.1 was developed and tested on Linux. It uses the cross-platform `sysinfo` crate, so it should behave the same on Windows, but the Windows build has not been verified yet. If the build or the first run misbehaves, see [Troubleshooting](#troubleshooting).

---

## Quick start

**Step 1. Check the overhead on your machine (15 seconds):**

```
bb bench
```

**Step 2. Start recording.** Leave this terminal open (or use [auto-start](#run-it-automatically-at-login-windows)):

```
bb run
```

**Step 3. Use your laptop normally.** Work, build, game, open 40 browser tabs.

**Step 4. When something felt slow, ask:**

```
bb why 15:40          # around 15:40 today (or yesterday, if 15:40 hasn't happened yet)
bb why "10m ago"      # around ten minutes ago
bb why now            # the last couple of minutes
```

**Step 5. Check on the recorder any time:**

```
bb status
```

Tip: the recorder only knows about moments it was running. Start it early and leave it on.

---

## Commands

Every command accepts `--db <path>` to use a different database file.

### `bb run`

Records one sample per second into the database. Runs in the foreground until you press Ctrl+C. Stopping it mid-write is safe.

| Option | Default | Meaning |
|---|---|---|
| `--interval <secs>` | `1` | Seconds between samples |
| `--retention-days <n>` | `7` | How many days of history to keep |
| `--top-n <n>` | `5` | Programs kept per sample, per category (CPU, disk, memory) |

Each sample stores system-wide CPU, memory, pagefile/swap, average clock speed and total disk throughput, plus the top programs by CPU, disk and memory. **Programs are grouped by name**, so `chrome.exe` with 30 child processes shows up as one row, "chrome.exe (30 processes)".

### `bb top`

A quick look at the biggest programs right now.

| Option | Meaning |
|---|---|
| `-n <count>` | How many programs to show (default 10) |
| `--watch` | Refresh every second until Ctrl+C |

### `bb why [WHEN]`

Explains why the machine was slow around `WHEN`.

`WHEN` accepts:

| Form | Example | Meaning |
|---|---|---|
| `now` | `bb why now` | The present (default) |
| `HH:MM` | `bb why 15:40` | Today at that time, or yesterday if it's still in the future |
| `HH:MM:SS` | `bb why 15:40:30` | Same, to the second |
| `<n>s/m/h ago` | `bb why "10m ago"` | Relative time. Quote it in your shell |

| Option | Default | Meaning |
|---|---|---|
| `--span <dur>` | `4m` | Total window analysed, centred on `WHEN`. Use `30s`, `90s`, `10m`... |

Choosing a span: use a short span (`--span 60s`) to pin down a brief freeze, a longer one (`--span 10m`) for a long sluggish stretch. A very long span averages the problem away.

### `bb status`

Shows the database path and size, how many samples exist, the time range covered, and the best clock speed seen. It warns you if the recorder doesn't look like it's running.

### `bb bench`

Measures blackbox's own cost on **your** machine: time per sample, share of a core, memory used. It writes to a throwaway database that is deleted afterward.

| Option | Default | Meaning |
|---|---|---|
| `--secs <n>` | `15` | How long to measure |

---

## What `bb why` can explain

Each finding has a title, evidence, a confidence label (high / medium / low) and, when blackbox recognises the program, an actionable hint. Findings are ranked, most likely first.

| Cause | Triggers when | Notes |
|---|---|---|
| **A program hogging the CPU** | One program averaged **25%+** of total CPU | Up to two are reported. Programs with many processes are summed. |
| **CPU saturated by many programs** | Whole-machine CPU **85%+** and no single hog | Lists the top three contributors. |
| **Memory pressure / paging** | Memory **90%+** full, or **80%+** with **1 GB+** of pagefile in use | Names the largest memory user. |
| **Probable CPU throttling** | CPU **60%+** busy while the clock averages **75% or less** of the best clock ever recorded | Inferred from clock speed only. See [Limits](#limits-read-this). |
| **Heavy disk activity** | Total disk throughput **80 MB/s+** | Names the program doing most of the I/O. |

### Programs with specific advice

| Program | What blackbox says |
|---|---|
| `MsMpEng.exe` | Windows Defender real-time scanning. Add project/build folders under *Windows Security > Virus & threat protection > Exclusions*. |
| `SearchIndexer.exe` | Windows Search indexing. Usually settles on its own. |
| `TiWorker.exe` | Windows Update working in the background. |
| `OneDrive.exe` | OneDrive syncing. Pause it or move big working folders out. |
| `vmmem` | WSL2 or a virtual machine holding memory. Limit it in `.wslconfig`, or run `wsl --shutdown`. |

If nothing matches, `bb why` says so plainly instead of inventing a cause.

### Example

```
$ bb why "20s ago" --span 20s
Window: Thu 16:17:00 to Thu 16:17:20  (14 samples)

Most likely causes:

 1. [high] sh (3 processes) was using a large share of the CPU
      - averaged 64% of total CPU over the window
      - whole machine averaged 65% CPU
```

(That run was a deliberate test: a few busy loops started on a Linux machine to check that the recorder blames the right thing.)

---

## Run it automatically at login (Windows)

blackbox is only useful if it was running *before* the slowdown. One command sets everything up:

```
target\release\bb.exe install
```

This copies `bb.exe` to `%LOCALAPPDATA%\blackbox\bin`, adds that folder to your PATH, starts recording in the background, and starts recording at every login. No admin rights needed. Open a new terminal afterwards and `bb` works anywhere.

| Command | What it does |
|---|---|
| `bb install` | Set up as above. `--no-autostart` skips start at login |
| `bb start` / `bb stop` | Record in the background, or stop it |
| `bb uninstall` | Stop recording, remove start at login and the PATH entry. `--purge` also deletes your data |

Start at login uses the current user's `Run` registry entry, so it works on battery. A console window may flash for a moment at login while the recorder launches.

---

## How much it costs

Measured by `bb bench` on a small Linux test machine (your numbers will differ, so run `bb bench` yourself):

| Cost | Measured |
|---|---|
| Work per sample | about 2.7 ms average, 5 ms worst |
| CPU | about 0.3% of one core at 1 sample/second |
| Memory | about 5 MB resident |

**Disk usage**, measured with a full day of synthetic data at the worst case of 24 program rows per sample:

| Data | Size |
|---|---|
| One day at full 1-second resolution | about 44 MB |
| One day after thinning to 1 sample per 10 s | about 4 MB |
| 7 days retained (1 full day + 6 thinned) | about 68 MB |

Data older than 24 hours is automatically thinned to one sample every 10 seconds, so older history is coarser but still answers "what happened Tuesday afternoon?". Real usage is typically lower than the worst case.

**Not yet measured:** the cost of process enumeration on Windows. That is the part most likely to differ from the numbers above, which is why `bb bench` exists.

---

## Where your data lives

| OS | Default database |
|---|---|
| Windows | `%LOCALAPPDATA%\blackbox\bb.db` |
| Linux | `$XDG_DATA_HOME/blackbox/bb.db`, or `~/.local/share/blackbox/bb.db` |

- Everything stays on your machine. blackbox makes **no network connections**.
- It records program **names** and resource numbers. It does not record window titles, file names, keystrokes or screen contents.
- To erase everything: delete the `bb.db` file (and the `bb.db-wal` / `bb.db-shm` files next to it) while `bb run` is stopped.

---

## Limits (read this)

blackbox v0.1 is honest about what it can't see. Don't over-trust it.

- **No temperatures, GPU or battery data yet.** If your slowdown was a hot GPU or a battery power limit, `bb why` may say "no clear cause".
- **Throttling is inferred, not measured.** It's flagged when the CPU is busy but running far below the fastest clock blackbox has ever seen. It cannot tell heat from power limits from battery saver. The baseline is the best clock *in your own recording*, so a brand-new database has a weak baseline.
- **"Heavy disk" means high throughput, not a queue.** A slow disk doing little work won't be flagged yet.
- **Only top programs are stored** (top 5 per category per second). A slowdown caused by hundreds of tiny processes may not name a culprit.
- **Averages hide short spikes.** A 2-second freeze inside a 4-minute window can be diluted. Use a shorter `--span`.
- **Thresholds are educated guesses**, not tuned on lots of real machines. Expect some wrong or missing explanations at first.
- **Without administrator rights**, Windows may hide details for some protected system processes. They can show lower numbers than reality.
- **Verified so far:** the rule engine and storage are covered by automated tests, and detection of an induced CPU hog was checked end to end on Linux. A full Windows run is still your job.

---

## Troubleshooting

**`bb why` says "Nothing was recorded then."**
The recorder wasn't running at that time. Check with `bb status`; it shows the time range actually covered.

**`bb why` says "No clear cause."**
CPU, memory, disk and clock all looked normal in that window. Try a shorter or different `--span`, or the slowdown was something v0.1 can't see (GPU, heat, network).

**`bb status` says the recorder doesn't seem to be running.**
The last sample is more than 30 seconds old. Start it with `bb start`.

**The build fails on Windows with a linker error.**
Install the Visual Studio Build Tools "Desktop development with C++" workload and open a new terminal.

**Clock speed shows 0 MHz.**
Some systems don't expose it. Throttling detection is silently skipped then.

**I want to use a different database.**
Pass `--db D:\somewhere\bb.db` to every command, including `run`.

---

## Project layout and tests

```
blackbox/
  Cargo.toml            workspace
  bb-core/              the engine (library)
    src/sampler.rs      reads CPU, memory, disk and per-program usage
    src/store.rs        compact SQLite storage, pruning, thinning
    src/rules.rs        the "why" engine: data in, ranked causes out
    src/timeparse.rs    parses "15:40", "10m ago", ...
    src/model.rs        shared data types
  bb-cli/               the `bb` command-line program
    src/main.rs
```

Run the tests:

```
cargo test
```

The rule engine is tested by feeding it synthetic slowdowns (a CPU hog, Defender scanning, memory exhaustion, a clock drop, saturated disk, a healthy machine) and checking it names the right cause, and nothing on a healthy one.

Measure the storage footprint yourself:

```
cargo test --release -p bb-core size_of -- --ignored --nocapture
```

**Adding or tuning a rule:** the thresholds live in `Thresholds` in `bb-core/src/rules.rs`, and each rule is a pure function of a recorded window, so you can add a test that builds a fake slowdown and asserts what `analyze` reports.

---

## Roadmap

| Version | Planned |
|---|---|
| **v0.1** (now) | Recorder, compact storage, `why`/`top`/`status`/`bench`, five causes, automated tests |
| **v0.2** | More rules, plus a test suite that *causes* real slowdowns (CPU stress, memory stress, big disk copy) and checks blackbox names them. Aim: report "detected N of M induced slowdowns" |
| **v0.3** | GPU and temperature via a sensor library, battery/power state, an HTML report |
| **Later** | Snapshot installed apps, startup items, drivers and services over time so `why` can say "this got slow right after the 14 Sept driver update" |

---

## License

MIT


