# blackbox (`bb`)

A flight recorder for your computer's performance.

Windows shows you numbers (Task Manager) or raw traces (Performance Recorder), but never tells you **why** it was slow. blackbox records your machine's vitals in the background. When something felt slow, you ask it:

```
bb why 15:40
```

and it answers in plain English with evidence.

> **Status: v0.2.** Records CPU, memory, pagefile, disk throughput and latency, clock speed, temperature, battery/power state, GPU load and the busiest programs, and explains a dozen kinds of slowdown, including short spikes and freezes. See [Limits](#limits-read-this) before trusting it blindly.

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

**Prebuilt (Windows):** download `bb-v*-setup-x64.exe` from the [Releases page](../../releases) and run it. No admin rights and no Rust needed. It puts `bb` on your PATH, starts recording, and starts it at every login. Uninstall from Windows Settings > Apps. Windows may warn about an unknown publisher because the installer is not code-signed yet: click **More info**, then **Run anyway**.

**Winget** (once the package is accepted into the winget catalog, see [packaging/winget](packaging/winget/README.md)):

```
winget install AnakinSkywalker0.blackbox
winget upgrade AnakinSkywalker0.blackbox
```

Each release also ships a plain zip (run `bb.exe install`), a Linux build and `.sha256` checksums.

**Or build it from source.** No admin rights are needed to build or run.

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

### `bb sensors`

Shows which extra sensors work on this machine (battery and power, temperature, CPU speed, GPU load and temperature, GPU throttling, disk latency and queue) with their current readings. A sensor your hardware or drivers don't expose shows "not available", and `bb why` skips the rules that need it.

```
$ bb sensors
  Battery / power  100%, plugged in
  Temperature      63 C (hottest system sensor)
  CPU speed        74% of rated maximum
  GPU load         1%
  GPU temperature  45 C
  GPU throttling   none
  Disk latency     0.2 ms per request
  Disk queue       0.01 requests waiting
```

### `bb update`

Checks GitHub for a newer release and installs it.

```
$ bb update
Current version: 0.2.0
Update available: 0.2.0 -> 0.3.0
Downloading bb-v0.3.0-windows-x86_64.zip...
Checksum OK (3641 KB).
Updated C:\Users\you\AppData\Local\blackbox\bin\bb.exe to 0.3.0.
Recording restarted.
```

| Option | Meaning |
|---|---|
| `--check` | Only report whether an update exists. Installs nothing |

What it does: downloads the release zip and its SHA-256 checksum from this project's GitHub releases (and nowhere else), refuses to continue if they don't match, runs the new program once to confirm it reports the expected version, then swaps it in and restarts the recorder if one was running. If anything fails, your current version and your recording are left exactly as they were. Your recorded data is kept, and an older database is upgraded in place. A downgrade is not supported, because a database written by a newer version may not open in an older one.

Updating is Windows-only for now. On Linux `bb update` tells you when a newer version exists and where to download it.

It never runs on its own. Nothing checks for updates in the background.

### `bb start` / `bb stop`

`bb start` records in the background with no terminal (it accepts the same options as `bb run`). `bb stop` stops it. Only one recorder runs per database.

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
| **CPU throttling** | CPU **60%+** busy while it runs at **75% or less** of its rated maximum speed | On Windows the speed is measured (the OS "% of Maximum Frequency" counter). Otherwise it is inferred from the best clock ever recorded. The cause is named when known: **heat** (85 C+), **Battery Saver**, or **battery power limits**, each with advice. |
| **Battery Saver / low battery** | Battery Saver on for half the window while the CPU was 30%+ busy, or a battery at 15% or less | Low confidence. Windows slows things down in both cases. |
| **Running very hot** | The hottest system sensor averaged **90 C+** and heat isn't already blamed for throttling | Low confidence. The sensor is a system thermal zone, which may not be the CPU core itself. |
| **GPU throttled** | GPU 50%+ busy and the driver reports a **heat**, **power cap** or hardware slowdown for half the window | NVIDIA only (needs the driver's NVML). Says whether it was heat or the power budget, and notes if you were on battery. |
| **GPU maxed out / very hot** | GPU load **90%+**, or GPU temperature **85 C+** | GPU load works on any GPU vendor on Windows. |
| **Heavy disk activity** | Total disk throughput **80 MB/s+** | Names the program doing most of the I/O. |
| **Slow disk** | Disk requests averaged **50 ms+** (counting only seconds with disk use), or **4+** requests waiting | Catches a slow or failing disk that moves little data. Names the busiest program. |
| **Short CPU spike** | CPU averaged **95%+** for **5 consecutive seconds** while the whole window looked fine | Names the busiest program then, and the exact time to zoom in on. |
| **Brief disk stall** | A single disk request time of **300 ms+** that the average hides | Gives the time and the program doing the most I/O. |
| **Machine stalled** | A **5 to 60 second hole** in the recording | The recorder samples every second, so a hole means everything was stuck. A laptop that briefly slept looks the same. |

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

A short burst that the whole window hides, found on Windows (also a deliberate test, 10 seconds of every core busy inside a quiet 2 minutes):

```
$ bb why now --span 2m
Most likely causes:

 1. [medium] A short CPU spike around 22:27:24
      - 22:27:22 to 22:27:26: CPU averaged 100%
      - the whole window averaged only 30%, so the burst is easy to miss
      - busiest then: powershell.exe (30 processes) at 80%
      > Run `bb why 22:27:24 --span 30s` to zoom in.
```

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
| `bb update` | Install a newer release. See [`bb update`](#bb-update) |

Start at login uses the current user's `Run` registry entry, so it works on battery. A console window may flash for a moment at login while the recorder launches.

---

## How much it costs

Measured by `bb bench` (your numbers will differ, so run `bb bench` yourself):

| Cost | Linux (small test machine) | Windows 11 laptop |
|---|---|---|
| Work per sample | about 2.7 ms average, 5 ms worst | about 20 ms average, 38 ms worst |
| CPU at 1 sample/second | about 0.3% of one core | about 2% of one core |
| Memory | about 5 MB resident | about 38 MB resident |

The Windows recorder runs at slightly above normal priority, so it keeps sampling while the CPU is pegged. That is exactly when the data matters most. It uses about 2% of one core, so other programs don't notice.

**Disk usage**, measured with a full day of synthetic data at the worst case of 24 program rows per sample:

| Data | Size |
|---|---|
| One day at full 1-second resolution | about 82 MB |
| One day after thinning to 1 sample per 10 s | about 10 MB |
| 7 days retained (1 full day + 6 thinned) | about 140 MB |

Data older than 24 hours is automatically thinned to one sample every 10 seconds, so older history is coarser but still answers "what happened Tuesday afternoon?". Real usage is typically lower than the worst case.

Thinning returns the space to the OS, so the file shrinks after the first day rather than staying at its peak.

---

## Where your data lives

| OS | Default database |
|---|---|
| Windows | `%LOCALAPPDATA%\blackbox\bb.db` |
| Linux | `$XDG_DATA_HOME/blackbox/bb.db`, or `~/.local/share/blackbox/bb.db` |

- Everything stays on your machine. blackbox makes **no network connections**, with one exception: `bb update`, which only runs when you type it. It asks GitHub for the latest release and downloads it. It sends no data about you or your machine (only a `bb/<version>` user agent).
- It records program **names** and resource numbers. It does not record window titles, file names, keystrokes or screen contents.
- To erase everything: delete the `bb.db` file (and the `bb.db-wal` / `bb.db-shm` files next to it) while `bb run` is stopped.

---

## Limits (read this)

blackbox v0.1 is honest about what it can't see. Don't over-trust it.

- **Sensors vary by machine.** Run `bb sensors` to see what yours exposes. Anything unavailable is skipped, and a slowdown that needs it may come back as "no clear cause".
- **Temperature is one system sensor,** a thermal zone that Windows exposes without admin rights. On some machines that is not the CPU core, so real CPU heat can be missed. Reading the core temperature needs a kernel driver, which blackbox does not install.
- **GPU heat and throttle reasons are NVIDIA only** (via the driver's NVML). GPU load works on any vendor on Windows, but AMD and Intel GPUs get no temperature or throttle data yet. Only the first NVIDIA GPU is read.
- **Battery, temperature, GPU and disk latency are Windows-first.** On Linux, battery and a thermal-zone temperature are read, but CPU speed percent, GPU load and disk latency are not. Those rules just don't fire. The Linux sensor code is covered by tests but has not been run on real Linux hardware.
- **Throttling is measured on Windows** (the OS "% of Maximum Frequency" counter). Elsewhere it is inferred from clock speed against the best clock in your own recording, so a brand-new database has a weak baseline. The *cause* (heat, Battery Saver, battery limits) is a best guess from the other sensors.
- **Only top programs are stored** (top 5 per category per second). A slowdown caused by hundreds of tiny processes may not name a culprit.
- **Spikes need to last.** CPU bursts shorter than 5 seconds, and disk stalls that don't show up as a long request time, can still be averaged away. Use a shorter `--span`. Samples older than 24 hours are 10 seconds apart, so spike and stall detection only works on recent data.
- **Thresholds are educated guesses**, not tuned on lots of real machines. Expect some wrong or missing explanations at first.
- **Updates are checked against GitHub, not signed.** `bb update` verifies the download against the checksum published with the release, which catches corruption and tampering in transit. It does not prove who built the release: that rests on trusting the GitHub account. The Windows executable is not code-signed yet, so SmartScreen may warn on first run.
- **Without administrator rights**, Windows may hide details for some protected system processes. They can show lower numbers than reality.
- **Verified so far:** the rule engine, storage, migration and sensor parsing are covered by automated tests (60+). Detection of an induced CPU hog and an induced short CPU burst was checked end to end on Windows, and of a CPU hog on Linux. **Not verified end to end:** heat, GPU throttle and battery causes (the sensors are read correctly, but no real slowdown of those kinds has been induced), and a genuinely slow disk (a fast NVMe never got slow enough to trigger the rule).

---

## Troubleshooting

**`bb why` says "Nothing was recorded then."**
The recorder wasn't running at that time. Check with `bb status`; it shows the time range actually covered.

**`bb why` says "No clear cause."**
CPU, memory, disk, GPU, heat and power all looked normal in that window. Try a shorter or different `--span`, or the slowdown was something blackbox can't see yet (network, or a sensor that `bb sensors` shows as unavailable).

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
    src/sensors.rs      battery, temperature, GPU and disk-latency readings
    src/store.rs        compact SQLite storage, pruning, thinning
    src/rules.rs        the "why" engine: data in, ranked causes out
    src/timeparse.rs    parses "15:40", "10m ago", ...
    src/model.rs        shared data types
  bb-cli/               the `bb` command-line program
    src/main.rs         commands
    src/install.rs      start/stop, install/uninstall
    src/update.rs       bb update (the only code that uses the network)
  installer/            Inno Setup script for the Windows installer
  packaging/winget/     winget manifest generator and publishing notes
```

Run the tests:

```
cargo test
```

The rule engine is tested by feeding it synthetic slowdowns (a CPU hog, Defender scanning, memory exhaustion, throttling from heat or battery, a maxed-out or throttled GPU, a slow disk, short spikes and stalls, a healthy machine) and checking it names the right cause, and nothing on a healthy one.

Measure the storage footprint yourself:

```
cargo test --release -p bb-core size_of -- --ignored --nocapture
```

**Adding or tuning a rule:** the thresholds live in `Thresholds` in `bb-core/src/rules.rs`, and each rule is a pure function of a recorded window, so you can add a test that builds a fake slowdown and asserts what `analyze` reports.

---

## Roadmap

| Version | Planned |
|---|---|
| **v0.1** | Recorder, compact storage, `why`/`top`/`status`/`bench`, five causes, automated tests |
| **v0.2** (now) | Battery and power, temperature, GPU, disk latency, short-spike and stall detection, one-command `install`, background `start`/`stop`, `bb sensors` |
| **v0.3** | A test suite that *causes* real slowdowns (CPU stress, memory stress, big disk copy, heat) and checks blackbox names them. Aim: report "detected N of M induced slowdowns". AMD/Intel GPU temperature, prebuilt releases, an HTML report |
| **Later** | Snapshot installed apps, startup items, drivers and services over time so `why` can say "this got slow right after the 14 Sept driver update" |

---

## License

MIT


