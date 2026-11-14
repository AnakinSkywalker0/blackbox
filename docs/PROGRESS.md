# blackbox: what has been done so far

A plain account of the project to date: what exists, what was verified, what went wrong and was fixed, and what is still open. Written 2026-10-03.

## What blackbox is

`bb` is a flight recorder for a computer's performance. It records CPU, memory, disk, GPU, temperature, battery and the busiest programs in the background. When something felt slow, `bb why 15:40` explains the likely cause in plain English, with evidence. Everything stays on the machine. The only network use is `bb update`, which runs only when typed.

Repository: https://github.com/AnakinSkywalker0/blackbox (public, MIT). Rust workspace: `bb-core` (engine), `bb-cli` (the `bb` program), `bb-gui` (desktop window).

## Timeline

1. **v0.1, built from the original README.** Sampler, compact SQLite storage with pruning and thinning, five rules, `bb run/top/why/status/bench`. Needed the Windows SDK installed to link on this PC.
2. **Easy setup.** `bb install` (PATH, background recording, start at login), `bb start/stop/uninstall`.
3. **Pushed to GitHub** as a private repo, later made public.
4. **v0.2: more it can see and explain.** Battery and power state, temperature, CPU frequency, GPU load and NVIDIA throttle reasons, disk latency and queue. New rules: throttling blamed on heat, Battery Saver or battery limits; GPU throttled or maxed; slow disk; short CPU spikes; brief disk stalls; recording gaps. `bb sensors`.
5. **`bb update`.** Checks GitHub releases, verifies the SHA-256, proves the new binary runs, swaps it in and restarts the recorder.
6. **CI and release workflows** (Windows and Linux, later macOS). Windows installer built with Inno Setup.
7. **v0.3.0** first public release.
8. **Making it trustworthy.** `bb selftest` causes real slowdowns (CPU hog, short burst, heavy disk, memory pressure) plus a quiet control run and reports "detected N of M" and false alarms. `bb feedback right|partly|wrong` records whether each answer was correct, locally.
9. **Reach.** Linux and macOS install, uninstall and update. `install.ps1` and `install.sh` one-liners. Fixed-name "latest" download links. Scoop bucket (auto-updated after each release). Winget manifest and a pull request to Microsoft's catalog.
10. **v0.4.0, v0.4.1, v0.4.2.** See the bug list below.
11. **`bb-gui`.** An egui desktop window: recorder status with start/stop, timeline charts with explained moments shaded, a Why panel with feedback buttons, a Sensors tab, an About tab. Merged to `main`, not yet released or packaged.

## Where it works

| | Windows | Linux | macOS |
|---|---|---|---|
| Record, `why`, `top`, `status` | yes | yes | yes |
| `install`, `uninstall`, `update` | yes | yes | yes |
| Most sensors | all | battery, thermal, swap, disk busy, NVIDIA | basics only |
| Tested on | this PC and CI | real Ubuntu (WSL) and CI | CI only |

## Ways to install today

- One-liner for Windows, or `install.sh` for Linux and macOS (see the main README).
- Download `bb-setup-x64.exe` from the Releases page.
- Scoop bucket.
- `bb update` once installed.
- Winget is not available yet (see "Open items").

## Verification

- About 120 automated tests (26 CLI, 80 core, 14 GUI logic), run on Windows, Linux and macOS in CI.
- `bb selftest` with real induced load: 4 of 4 on this laptop (including memory pressure), 3 of 3 on clean GitHub runners for Windows and Linux, 2 of 2 judgeable on macOS.
- Release workflow installs, runs, upgrades and uninstalls the Windows installer on a clean machine, including under a harness that waits for the whole process tree.
- Real end-to-end runs of the Linux installer in Ubuntu, `bb update` against real releases, and Scoop and winget installs from local manifests.

## Bugs found by testing, and fixed

- A static "high memory plus swap" reading was reported on an idle machine and outranked real causes. Memory pressure now needs measured paging.
- Averages hid short memory squeezes. Added a short-squeeze rule.
- A fixed disk-throughput threshold missed a saturated slow disk. Heavy disk now also uses busy time of the busiest disk.
- The self-test's memory scenario didn't create real pressure (Windows compresses memory). It now forces paging, with safety caps.
- The self-test left 2 GB per run in the temp folder (12 GB found). Fixed, and later runs clean up old leftovers.
- A fresh Windows account may have no registry `Run` key, which broke `install` and `uninstall`. Fixed.
- The recorder starving under full CPU load. It now runs at slightly higher priority.
- Thinning old data never shrank the database file. Now it does (82 MB to 10 MB per day).
- The installer left the background recorder running, and `bb.exe` needed the Visual C++ runtime DLL. Both are believed to be why Microsoft's automated install test timed out. Fixed in 0.4.0 and 0.4.2.
- Charts drew lines across periods when the recorder was off. Now they break at gaps.

## Open items

1. **Winget.** PR https://github.com/microsoft/winget-pkgs/pull/445691 is open for 0.4.2. The CLA is signed. The install-validation check failed twice on earlier versions and is being re-run for 0.4.2. Not yet in the catalog.
2. **Code signing.** The Windows installer is unsigned, so Windows shows an "unknown publisher" warning. See `docs/SIGNING.md`; it needs an application or purchase only the owner can make.
3. **The GUI** is on `main` but not released, not in the installer, and has no tray icon. A desktop shortcut was requested and not created yet.
4. **Not verified for real:** heat, GPU throttling and battery causes (only synthetic tests), a slow or failing disk, machine stalls, and a real macOS or Linux desktop login autostart.
5. **Pitch-deck files** (`docs/blackbox-pitch-deck.*`) were created by another session and were committed to the public repo by mistake. Left in place at the owner's request.

## Ideas not yet built

Windows Event Log correlation ("this started after the driver update"), network and per-program GPU, disk health, an HTML report, a tray icon with notifications, automatic winget updates.
