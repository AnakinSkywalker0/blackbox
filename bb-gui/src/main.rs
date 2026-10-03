//! blackbox desktop window: live status, timeline charts and plain-English explanations.
//!
//! It reads the same database the `bb` recorder writes, so it works whenever `bb` has been
//! recording. It never samples anything itself.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod logic;

use bb_core::model::{describe_sensors, Confidence, Finding, Sample};
use bb_core::rules::{analyze, Thresholds};
use bb_core::store::{default_db_path, Store};
use bb_core::timeparse::{parse_duration, parse_when};
use chrono::Local;
use eframe::egui::{self, Color32, RichText, Stroke};
use egui_plot::{HoverPosition, Line, Plot, PlotPoints, Polygon};
use logic::{axis_label, clock, downsample, find_bb, find_moments, recorder_pid, run_bb, split_at_gaps, Moment, Point, Range};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REFRESH: Duration = Duration::from_secs(5);
const MOMENT_REFRESH_SECS: i64 = 60;

// ---- background loading -----------------------------------------------------------------

struct Request {
    range: Range,
    /// Rescan for explained moments even if the cached ones are recent.
    rescan: bool,
}

struct Loaded {
    range: Range,
    points: Vec<Point>,
    moments: Vec<Moment>,
    latest: Option<Sample>,
    count: u64,
    first: Option<i64>,
    last: Option<i64>,
    now: i64,
    error: Option<String>,
}

/// Loads data on its own thread so the window never freezes. Requests that pile up are
/// collapsed into the newest one.
fn spawn_worker(db: PathBuf, ctx: egui::Context) -> (Sender<Request>, Receiver<Loaded>) {
    let (req_tx, req_rx) = channel::<Request>();
    let (out_tx, out_rx) = channel::<Loaded>();
    std::thread::spawn(move || {
        let mut cache: Option<(Range, i64, Vec<Moment>)> = None;
        while let Ok(mut req) = req_rx.recv() {
            while let Ok(newer) = req_rx.try_recv() {
                req = Request { range: newer.range, rescan: req.rescan || newer.rescan };
            }
            let loaded = load(&db, &req, &mut cache);
            if out_tx.send(loaded).is_err() {
                break;
            }
            ctx.request_repaint();
        }
    });
    (req_tx, out_rx)
}

fn load(db: &Path, req: &Request, cache: &mut Option<(Range, i64, Vec<Moment>)>) -> Loaded {
    let now = Local::now().timestamp();
    let empty = |error: Option<String>| Loaded { range: req.range, points: vec![], moments: vec![], latest: None, count: 0, first: None, last: None, now, error };
    let store = match Store::open(db) {
        Ok(s) => s,
        Err(e) => return empty(Some(format!("can't open the database: {e}"))),
    };
    let from = now - req.range.secs();
    let light = match store.window_light(from, now) {
        Ok(v) => v,
        Err(e) => return empty(Some(format!("can't read the database: {e}"))),
    };
    let stats = store.stats().ok();
    let moments = match cache {
        Some((r, at, m)) if *r == req.range && now - *at < MOMENT_REFRESH_SECS && !req.rescan => m.clone(),
        _ => {
            let best = store.best_mhz().unwrap_or(0);
            let m = find_moments(&store, from, now, best, req.range.moment_window());
            *cache = Some((req.range, now, m.clone()));
            m
        }
    };
    Loaded {
        range: req.range,
        points: downsample(&light, 1200),
        moments,
        latest: light.last().cloned(),
        count: stats.map_or(0, |s| s.samples),
        first: stats.and_then(|s| s.first_ts),
        last: stats.and_then(|s| s.last_ts),
        now,
        error: None,
    }
}

// ---- explanations -------------------------------------------------------------------------

struct Explained {
    from: i64,
    to: i64,
    samples: usize,
    findings: Vec<Finding>,
}

/// Same as `bb why`: explain `[from, to]`, and remember the answer so it can be judged.
fn explain(db: &Path, from: i64, to: i64) -> Result<Explained, String> {
    let store = Store::open(db).map_err(|e| e.to_string())?;
    let samples = store.window(from, to).map_err(|e| e.to_string())?;
    let best = store.best_mhz().map_err(|e| e.to_string())?;
    let findings = if samples.is_empty() { Vec::new() } else { analyze(&samples, best, &Thresholds::default()) };
    if !samples.is_empty() {
        let titles: Vec<String> = findings.iter().map(|f| f.title.clone()).collect();
        let _ = store.log_why(Local::now().timestamp(), from, to, &titles, VERSION);
    }
    Ok(Explained { from, to, samples: samples.len(), findings })
}

// ---- the app ------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Timeline,
    Why,
    Sensors,
    About,
}

enum Action {
    SetRange(Range),
    Explain { from: i64, to: i64 },
    ExplainTyped,
    Quick { span: &'static str },
    Recorder(&'static str),
    Verdict(&'static str),
}

struct App {
    db: PathBuf,
    bb: Option<PathBuf>,
    req_tx: Sender<Request>,
    out_rx: Receiver<Loaded>,
    range: Range,
    data: Option<Loaded>,
    loading: bool,
    last_request: Instant,
    tab: Tab,
    recorder: Option<u32>,
    last_pid_check: Instant,
    when: String,
    span: String,
    explained: Option<Explained>,
    why_error: Option<String>,
    verdict: Option<String>,
    notice: Option<(String, Instant)>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, db: PathBuf, start: Startup) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let (req_tx, out_rx) = spawn_worker(db.clone(), cc.egui_ctx.clone());
        let bb = std::env::current_exe().ok().and_then(|e| find_bb(&e));
        let mut app = App {
            recorder: recorder_pid(&db),
            db,
            bb,
            req_tx,
            out_rx,
            range: Range::H1,
            data: None,
            loading: false,
            last_request: Instant::now(),
            tab: Tab::Timeline,
            last_pid_check: Instant::now(),
            when: "now".into(),
            span: "4m".into(),
            explained: None,
            why_error: None,
            verdict: None,
            notice: None,
        };
        app.request(false);
        if let Some(t) = start.tab {
            app.tab = t;
        }
        if let Some(span) = start.explain {
            app.when = "now".into();
            app.span = span;
            app.explain_typed();
        }
        app
    }

    fn request(&mut self, rescan: bool) {
        let _ = self.req_tx.send(Request { range: self.range, rescan });
        self.loading = true;
        self.last_request = Instant::now();
    }

    fn tell(&mut self, msg: impl Into<String>) {
        self.notice = Some((msg.into(), Instant::now()));
    }

    fn run_explain(&mut self, from: i64, to: i64) {
        self.verdict = None;
        match explain(&self.db, from, to) {
            Ok(e) => {
                self.explained = Some(e);
                self.why_error = None;
            }
            Err(e) => self.why_error = Some(e),
        }
        self.tab = Tab::Why;
    }

    fn explain_typed(&mut self) {
        let now = Local::now();
        let centre = match parse_when(&self.when, now) {
            Ok(c) => c,
            Err(e) => {
                self.why_error = Some(e);
                self.tab = Tab::Why;
                return;
            }
        };
        let span = match parse_duration(&self.span) {
            Ok(s) => s.max(2),
            Err(e) => {
                self.why_error = Some(e);
                self.tab = Tab::Why;
                return;
            }
        };
        // "now" means the recent past, not a window that is half in the future.
        let (from, to) = if centre >= now.timestamp() - 1 { (centre - span, centre) } else { (centre - span / 2, centre + span / 2) };
        self.run_explain(from, to);
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::SetRange(r) => {
                self.range = r;
                self.request(true);
            }
            Action::Explain { from, to } => self.run_explain(from, to),
            Action::ExplainTyped => self.explain_typed(),
            Action::Quick { span } => {
                self.when = "now".into();
                self.span = span.into();
                self.explain_typed();
            }
            Action::Recorder(verb) => {
                let Some(bb) = self.bb.clone() else { return };
                let args: &[&str] = if verb == "start" { &["start", "--quiet"] } else { &["stop"] };
                match run_bb(&bb, &self.db, args) {
                    Ok(_) => self.tell(if verb == "start" { "Recording started." } else { "Recording stopped." }),
                    Err(e) => self.tell(format!("Couldn't {verb} the recorder: {e}")),
                }
                self.recorder = recorder_pid(&self.db);
            }
            Action::Verdict(v) => match Store::open(&self.db).and_then(|s| s.set_verdict(v, None)) {
                Ok(_) => self.verdict = Some(v.to_string()),
                Err(e) => self.why_error = Some(e.to_string()),
            },
        }
    }

    fn pump(&mut self, ctx: &egui::Context) {
        while let Ok(l) = self.out_rx.try_recv() {
            // Ignore an answer for a range the user has already moved away from.
            if l.range == self.range {
                self.data = Some(l);
                self.loading = false;
            }
        }
        if self.last_pid_check.elapsed() > Duration::from_secs(2) {
            self.recorder = recorder_pid(&self.db);
            self.last_pid_check = Instant::now();
        }
        if !self.loading && self.last_request.elapsed() >= REFRESH {
            self.request(false);
        }
        if self.notice.as_ref().is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(6)) {
            self.notice = None;
        }
        ctx.request_repaint_after(Duration::from_millis(1000));
    }
}

// ---- drawing ------------------------------------------------------------------------------

const BLUE: Color32 = Color32::from_rgb(90, 160, 255);
const GREEN: Color32 = Color32::from_rgb(110, 210, 140);
const ORANGE: Color32 = Color32::from_rgb(255, 170, 70);
const PURPLE: Color32 = Color32::from_rgb(190, 130, 255);
const RED: Color32 = Color32::from_rgb(255, 110, 110);
const MUTED: Color32 = Color32::from_rgb(140, 148, 144);

fn confidence_color(c: Confidence) -> Color32 {
    match c {
        Confidence::High => RED,
        Confidence::Medium => ORANGE,
        Confidence::Low => MUTED,
    }
}

fn pill(ui: &mut egui::Ui, c: Confidence) {
    ui.label(RichText::new(format!(" {} ", c.label())).color(Color32::BLACK).background_color(confidence_color(c)).small());
}

/// One chart row. Returns the time that was clicked, if any.
#[allow(clippy::too_many_arguments)]
fn chart(
    ui: &mut egui::Ui,
    id: &str,
    title: &str,
    unit: &'static str,
    height: f32,
    ymax: f64,
    span: (i64, i64),
    lines: Vec<(&'static str, Color32, Vec<[f64; 2]>)>,
    moments: &[Moment],
) -> Option<i64> {
    ui.label(RichText::new(title).strong());
    let plot = Plot::new(id)
        .height(height)
        .allow_scroll(false)
        .allow_zoom(false)
        .allow_boxed_zoom(false)
        .include_x(span.0 as f64)
        .include_x(span.1 as f64)
        .include_y(0.0)
        .include_y(ymax)
        .y_axis_min_width(46.0)
        .link_axis("time", [true, false])
        .link_cursor("time", [true, false])
        .x_axis_formatter(|mark, _| axis_label(mark.value as i64))
        .label_formatter(move |pos| match pos {
            HoverPosition::NearDataPoint { plot_name, position, .. } => Some(format!("{}\n{plot_name}: {:.1}{unit}", clock(position.x as i64), position.y)),
            HoverPosition::Elsewhere { position } => Some(clock(position.x as i64)),
        });
    let out = plot.show(ui, |p| {
        for m in moments {
            let (a, b) = (m.from as f64, m.to.max(m.from + 1) as f64);
            let band = vec![[a, 0.0], [b, 0.0], [b, ymax], [a, ymax]];
            let c = confidence_color(m.confidence);
            p.polygon(
                Polygon::new("", PlotPoints::from(band))
                    .fill_color(Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 38))
                    .stroke(Stroke::NONE),
            );
        }
        for (name, color, pts) in lines {
            p.line(Line::new(name, PlotPoints::from(pts)).color(color).width(1.6));
        }
    });
    if out.response.clicked() {
        if let Some(pos) = out.response.interact_pointer_pos() {
            return Some(out.transform.value_from_position(pos).x as i64);
        }
    }
    None
}

/// One chart line per unbroken stretch of recording, so gaps stay empty.
fn lines(name: &'static str, color: Color32, points: &[Point], f: impl Fn(&Point) -> Option<f64>) -> Vec<(&'static str, Color32, Vec<[f64; 2]>)> {
    split_at_gaps(points)
        .into_iter()
        .map(|run| (name, color, run.iter().filter_map(|p| f(p).map(|y| [p.ts, y])).collect::<Vec<_>>()))
        .filter(|(_, _, pts)| !pts.is_empty())
        .collect()
}

fn timeline(app: &App, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        for r in Range::ALL {
            if ui.selectable_label(app.range == r, r.label()).clicked() {
                actions.push(Action::SetRange(r));
            }
        }
        ui.add_space(12.0);
        if app.loading {
            ui.spinner();
        }
        ui.label(RichText::new("Shaded stretches have an explanation. Click one, or use the list below.").color(MUTED).small());
    });
    let Some(d) = &app.data else {
        ui.add_space(20.0);
        ui.label("Loading...");
        return;
    };
    if let Some(e) = &d.error {
        ui.colored_label(RED, e);
        return;
    }
    if d.points.is_empty() {
        ui.add_space(20.0);
        ui.heading("Nothing recorded in this time range");
        ui.label("Start the recorder with the button at the top, or pick a longer range.");
        return;
    }
    let span = (d.now - app.range.secs(), d.now);
    let chart_h = ((ui.available_height() - 250.0) / 4.0).clamp(70.0, 160.0);
    let mut clicked: Option<i64> = None;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let pick = |c: Option<i64>, clicked: &mut Option<i64>| {
            if c.is_some() {
                *clicked = c;
            }
        };
        let c = chart(ui, "cpu", "CPU", "%", chart_h, 100.0, span, lines("CPU", BLUE, &d.points, |p| Some(p.cpu)), &d.moments);
        pick(c, &mut clicked);
        let c = chart(ui, "mem", "Memory used", "%", chart_h, 100.0, span, lines("Memory", GREEN, &d.points, |p| Some(p.mem)), &d.moments);
        pick(c, &mut clicked);
        let disk_max = d.points.iter().map(|p| p.disk_mb).fold(10.0, f64::max) * 1.1;
        let c = chart(ui, "disk", "Disk throughput", " MB/s", chart_h, disk_max, span, lines("Disk", ORANGE, &d.points, |p| Some(p.disk_mb)), &d.moments);
        pick(c, &mut clicked);
        if d.points.iter().any(|p| p.gpu.is_some()) {
            let c = chart(ui, "gpu", "GPU", "%", chart_h, 100.0, span, lines("GPU", PURPLE, &d.points, |p| p.gpu), &d.moments);
            pick(c, &mut clicked);
        }
        if d.points.iter().any(|p| p.temp.is_some() || p.gpu_temp.is_some()) {
            let tmax = d.points.iter().flat_map(|p| [p.temp, p.gpu_temp]).flatten().fold(60.0, f64::max) + 10.0;
            let c = chart(
                ui,
                "temp",
                "Temperature",
                " C",
                chart_h,
                tmax,
                span,
                [lines("System", RED, &d.points, |p| p.temp), lines("GPU", PURPLE, &d.points, |p| p.gpu_temp)].concat(),
                &d.moments,
            );
            pick(c, &mut clicked);
        }

        ui.add_space(10.0);
        ui.separator();
        ui.label(RichText::new(format!("Explained moments ({})", d.moments.len())).strong());
        if d.moments.is_empty() {
            ui.label(RichText::new("None in this range. Nothing slow enough to explain.").color(MUTED));
        }
        for m in d.moments.iter().rev().take(60) {
            ui.horizontal(|ui| {
                pill(ui, m.confidence);
                ui.label(RichText::new(format!("{} to {}", clock(m.from), clock(m.to))).monospace());
                ui.label(&m.title);
                if ui.small_button("Explain").clicked() {
                    actions.push(Action::Explain { from: m.from, to: m.to });
                }
            });
        }
    });
    if let Some(t) = clicked {
        if let Some(m) = d.moments.iter().find(|m| t >= m.from - 30 && t <= m.to + 30) {
            actions.push(Action::Explain { from: m.from, to: m.to });
        }
    }
}

fn why(app: &mut App, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
    ui.horizontal_wrapped(|ui| {
        ui.label("When");
        ui.add(egui::TextEdit::singleline(&mut app.when).hint_text("now, 15:40, 10m ago").desired_width(130.0));
        ui.label("Span");
        ui.add(egui::TextEdit::singleline(&mut app.span).hint_text("4m").desired_width(60.0));
        if ui.button("Explain").clicked() {
            actions.push(Action::ExplainTyped);
        }
        ui.separator();
        if ui.button("Last 10 minutes").clicked() {
            actions.push(Action::Quick { span: "10m" });
        }
        if ui.button("Last hour").clicked() {
            actions.push(Action::Quick { span: "1h" });
        }
    });
    ui.separator();
    if let Some(e) = &app.why_error {
        ui.colored_label(RED, e);
    }
    let Some(ex) = &app.explained else {
        ui.add_space(16.0);
        ui.heading("Why was it slow?");
        ui.label("Pick a moment on the Timeline, or type a time above and press Explain. Try \"10m ago\" or \"15:40\".");
        return;
    };
    ui.label(RichText::new(format!("{} to {}   ({} samples)", clock(ex.from), clock(ex.to), ex.samples)).monospace().color(MUTED));
    ui.add_space(6.0);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if ex.samples == 0 {
            ui.heading("Nothing was recorded then");
            ui.label("The recorder wasn't running at that time. Check the Timeline for the range that was covered.");
            return;
        }
        if ex.findings.is_empty() {
            ui.heading("No clear cause");
            ui.label("CPU, memory, disk, GPU, heat and power all looked normal in this window. Try a shorter or different span. It may also be something blackbox can't see yet, such as network.");
        }
        for (i, f) in ex.findings.iter().enumerate() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}.", i + 1)).strong());
                    pill(ui, f.confidence);
                    ui.label(RichText::new(&f.title).strong().size(16.0));
                });
                for e in &f.evidence {
                    ui.label(format!("   - {e}"));
                }
                if let Some(h) = &f.hint {
                    ui.add_space(2.0);
                    ui.colored_label(GREEN, format!("   > {h}"));
                }
            });
            ui.add_space(4.0);
        }
        if ex.samples > 0 {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("Was this right?");
                match &app.verdict {
                    Some(v) => {
                        ui.colored_label(GREEN, format!("Thanks, recorded as \"{v}\" (stays on this machine)."));
                    }
                    None => {
                        for (label, v) in [("Right", "right"), ("Partly", "partly"), ("Wrong", "wrong")] {
                            if ui.button(label).clicked() {
                                actions.push(Action::Verdict(v));
                            }
                        }
                    }
                }
            });
        }
    });
}

fn sensors(app: &App, ui: &mut egui::Ui) {
    let Some(s) = app.data.as_ref().and_then(|d| d.latest.as_ref()) else {
        ui.add_space(16.0);
        ui.label("No recent samples. Start the recorder to see live readings.");
        return;
    };
    ui.heading("Latest reading");
    ui.label(RichText::new(format!("at {}", clock(s.ts))).color(MUTED));
    ui.add_space(8.0);
    egui::Grid::new("sensors").num_columns(2).spacing([24.0, 6.0]).striped(true).show(ui, |ui| {
        let mem_gb = |b: u64| b as f64 / 1_073_741_824.0;
        for (k, v) in [
            ("CPU", format!("{:.0}%", s.cpu_pct)),
            ("Memory", format!("{:.1} of {:.1} GB used", mem_gb(s.mem_used), mem_gb(s.mem_total))),
            ("Swap / pagefile", format!("{:.1} GB in use", mem_gb(s.swap_used))),
            ("Disk throughput", format!("{:.1} MB/s", s.disk_bps as f64 / 1_048_576.0)),
        ] {
            ui.label(k);
            ui.label(v);
            ui.end_row();
        }
        for (k, v) in describe_sensors(&s.sensors) {
            ui.label(k);
            match v {
                Some(v) => ui.label(v),
                None => ui.label(RichText::new("not available on this machine").color(MUTED)),
            };
            ui.end_row();
        }
    });
}

fn about(app: &App, ui: &mut egui::Ui) {
    ui.heading("blackbox");
    ui.label(format!("Desktop window, version {VERSION}"));
    ui.add_space(8.0);
    ui.label("blackbox records your computer's vitals in the background so that when something felt slow, it can tell you why. Everything stays on this machine.");
    ui.add_space(8.0);
    egui::Grid::new("about").num_columns(2).spacing([24.0, 6.0]).show(ui, |ui| {
        ui.label("Database");
        ui.label(app.db.display().to_string());
        ui.end_row();
        if let Some(d) = &app.data {
            ui.label("Samples recorded");
            ui.label(d.count.to_string());
            ui.end_row();
            if let (Some(a), Some(b)) = (d.first, d.last) {
                ui.label("Covers");
                ui.label(format!("{} to {}", axis_label(a), axis_label(b)));
                ui.end_row();
            }
        }
        ui.label("Recorder program");
        ui.label(app.bb.as_ref().map_or("not found".to_string(), |p| p.display().to_string()));
        ui.end_row();
    });
    ui.add_space(10.0);
    ui.hyperlink_to("Project page and documentation", "https://github.com/AnakinSkywalker0/blackbox");
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.pump(&ctx);
        let mut actions: Vec<Action> = Vec::new();

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("blackbox");
                ui.add_space(12.0);
                match self.recorder {
                    Some(pid) => ui.colored_label(GREEN, format!("● Recording (pid {pid})")),
                    None => ui.colored_label(RED, "● Not recording"),
                };
                let have_bb = self.bb.is_some();
                let (label, verb) = if self.recorder.is_some() { ("Stop", "stop") } else { ("Start recording", "start") };
                if ui.add_enabled(have_bb, egui::Button::new(label)).on_disabled_hover_text("The bb program wasn't found next to this window or on PATH").clicked() {
                    actions.push(Action::Recorder(verb));
                }
                if let Some((msg, _)) = &self.notice {
                    ui.label(RichText::new(msg).color(ORANGE));
                }
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                for (t, label) in [(Tab::Timeline, "Timeline"), (Tab::Why, "Why was it slow?"), (Tab::Sensors, "Sensors"), (Tab::About, "About")] {
                    ui.selectable_value(&mut self.tab, t, label);
                }
            });
            ui.add_space(2.0);
        });

        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Timeline => timeline(self, ui, &mut actions),
            Tab::Why => why(self, ui, &mut actions),
            Tab::Sensors => sensors(self, ui),
            Tab::About => about(self, ui),
        });

        for a in actions {
            self.apply(a);
        }
    }
}

/// Where to open: `--tab timeline|why|sensors|about`, and `--explain 10m` to open straight on
/// an explanation of the last 10 minutes (handy as a desktop shortcut).
#[derive(Default)]
struct Startup {
    tab: Option<Tab>,
    explain: Option<String>,
}

fn parse_args(args: impl Iterator<Item = String>) -> (Option<PathBuf>, Startup) {
    let mut args = args;
    let (mut db, mut start) = (None, Startup::default());
    while let Some(a) = args.next() {
        match a.as_str() {
            "--db" => db = args.next().map(PathBuf::from),
            "--tab" => {
                start.tab = match args.next().as_deref() {
                    Some("timeline") => Some(Tab::Timeline),
                    Some("why") => Some(Tab::Why),
                    Some("sensors") => Some(Tab::Sensors),
                    Some("about") => Some(Tab::About),
                    _ => None,
                }
            }
            "--explain" => start.explain = args.next(),
            _ => {}
        }
    }
    (db, start)
}

fn main() -> eframe::Result {
    let (db, start) = parse_args(std::env::args().skip(1));
    let db = db.unwrap_or_else(default_db_path);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("blackbox").with_inner_size([1100.0, 780.0]).with_min_inner_size([760.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native("blackbox", options, Box::new(move |cc| Ok(Box::new(App::new(cc, db, start)))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(a: &[&str]) -> (Option<PathBuf>, Startup) {
        parse_args(a.iter().map(|s| s.to_string()))
    }

    #[test]
    fn startup_options_are_understood() {
        let (db, s) = parse(&[]);
        assert!(db.is_none() && s.tab.is_none() && s.explain.is_none());
        let (db, s) = parse(&["--db", "x.db", "--tab", "sensors"]);
        assert_eq!(db, Some(PathBuf::from("x.db")));
        assert!(s.tab == Some(Tab::Sensors));
        let (_, s) = parse(&["--explain", "10m"]);
        assert_eq!(s.explain.as_deref(), Some("10m"));
        let (_, s) = parse(&["--tab", "nonsense", "--unknown"]);
        assert!(s.tab.is_none());
    }
}