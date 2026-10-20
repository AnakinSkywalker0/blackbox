# Roadmap

What's planned for blackbox, with the reason for each item and how to tell when it's done. Items are sized to fit in one focused piece of work. Shipped work is listed in the [README](README.md#roadmap) and in the [releases](https://github.com/AnakinSkywalker0/blackbox/releases).

Status: **v0.5.1 released**, with `bb fix`, `bb blame` and the desktop window.

---

## 1. Trust: don't lose data, don't guess

| Item | Why | Done when |
|---|---|---|
| **Retention relative to the last sample** | `Store::maintain` cuts at `now - retention_days` ([store.rs](bb-core/src/store.rs)). A machine left off for 8 days loses its whole history the moment the recorder starts again. This has happened. | Pruning keeps the newest `retention_days` of *recorded* time. A test starts a recorder on a database whose newest sample is 10 days old, and the data survives. |
| **Replay corpus for the rules** | Rule changes are checked against synthetic windows only. Real recordings with known causes would catch regressions that synthetic data can't. | `bb export --window` writes an anonymised window. A `tests/corpus/` of real windows with expected top causes runs in CI. |
| **Calibrate thresholds from feedback** | Thresholds in `Thresholds` are educated guesses. `bb feedback` already records right/wrong per explanation kind. | `bb feedback --summary` suggests which thresholds to move and by how much, with the evidence. |
| **Confidence that means something** | high/medium/low is set per rule by hand. | Confidence comes from how far past the threshold the evidence is and how long it lasted, and it's covered by tests. |

## 2. See more causes

| Item | Why | Done when |
|---|---|---|
| **Windows Event Log correlation** | "It got slow right after the driver update" is the explanation people most want, and Windows already logs installs, crashes and driver loads. | `bb why` adds a finding when an update, driver install, app crash or WHEA error happened in or just before the window. Unit-tested with recorded event XML. |
| **Network** | A stalled download or a saturated link is invisible today, so `bb why` says "no clear cause". | Per-interface throughput and per-program network use are recorded, plus a rule for "network saturated by X". |
| **Per-program GPU use** | GPU load is whole-machine only, so a maxed GPU has no culprit. | Windows GPU Engine counters are attributed to programs, and the "GPU maxed" finding names one. |
| **Disk health** | A failing disk shows up as "slow disk" with no reason. | SMART / NVMe health readings where available without admin, shown in `bb sensors` and cited by the slow-disk rule. |
| **AMD and Intel GPU temperature** | GPU heat and throttle reasons are NVIDIA-only. | Read through vendor libraries where present, otherwise "not available", same as today. |

## 3. Desktop window (`bb-gui`)

| Item | Why | Done when |
|---|---|---|
| **Tray icon and notifications** | The value is in noticing a slowdown *as it happens*. | A tray icon with recorder status, and an optional notification when a high-confidence finding appears. |
| **Screenshot tests in CI** | Every screen can already be rendered offscreen from a fixture database (`UPDATE_SNAPSHOTS=1 cargo test -p bb-gui screenshots -- --ignored`), but only by hand, since it needs a GPU adapter. | CI renders them with a software adapter and fails when a screen changes unexpectedly. |
| **Linux and macOS builds** | The window is Windows-tested only. | CI builds and smoke-tests it on all three. |

## 4. Share and report

| Item | Why | Done when |
|---|---|---|
| **HTML report** | People ask for help in forums with screenshots of Task Manager. | `bb report 15:40` writes one self-contained HTML file with the charts, findings and evidence, safe to attach. |
| **JSON output** | Scripts and other tools can't use `bb why` today. | `bb why --json` with a documented, versioned schema. |

## 5. Distribution and polish

| Item | Why | Done when |
|---|---|---|
| **Winget listing** | The PR to `microsoft/winget-pkgs` hasn't passed install validation yet. | `winget install AnakinSkywalker0.blackbox` works. |
| **Code signing** | Unsigned installers trigger SmartScreen. Needs a certificate only the owner can obtain (see [docs/SIGNING.md](docs/SIGNING.md)). | Release artifacts are signed in CI. |
| **Real desktop-login autostart on Linux and macOS** | Created and checked in CI, but never exercised by an actual login. | Verified on a real Ubuntu desktop and a real Mac, and documented. |
| **Split `rules.rs`** | It's 1,300 lines covering every rule. | One module per cause, same public `analyze` API, no test changes needed. |

---

## Later

- Snapshot installed apps, startup items, drivers and services over time, so `bb why` can say "this got slow after the 14 Sept driver update" even without the Event Log.
- Compare two time ranges: "why is this week slower than last week?"
- A plugin point for rules, so a new cause can be added without touching the core.
