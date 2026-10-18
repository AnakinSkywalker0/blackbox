//! blackbox desktop window: a status page for this computer, with plain-English explanations
//! of every slowdown and a chart per resource.
//!
//! It reads the same database the `bb` recorder writes, so it works whenever `bb` has been
//! recording. It never samples anything itself.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod logic;

use bb_core::model::{describe_sensors, Confidence, Finding, Sample};
use bb_core::rules::{analyze, kind_of, Thresholds};
use bb_core::store::{default_db_path, Store};
use bb_core::timeparse::parse_duration;
use chrono::Local;
use eframe::egui::{self, Color32, Margin, RichText, Sense, Stroke, StrokeKind};
use egui_plot::{HoverPosition, Line, Plot, PlotPoints, Polygon};
use logic::{axis_label, clock, downsample, duration_text, find_bb, find_moments, headline, heat_summary, home_status, recorder_pid, run_bb, split_at_gaps, strip, Health, HeatSummary, Moment, Point, Range, Slot};
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
    heat: HeatSummary,
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
    let empty = |error: Option<String>| Loaded { range: req.range, points: vec![], moments: vec![], heat: HeatSummary::default(), latest: None, count: 0, first: None, last: None, now, error };
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
        heat: heat_summary(&light),
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Res {
    Cpu,
    Memory,
    Disk,
    Gpu,
    Heat,
}

impl Res {
    fn label(self) -> &'static str {
        match self {
            Res::Cpu => "CPU",
            Res::Memory => "Memory",
            Res::Disk => "Disk",
            Res::Gpu => "GPU",
            Res::Heat => "Heat",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum View {
    Home,
    /// An explanation, opened from Home or a chart. `back` is where Back returns to.
    Detail,
    Res(Res),
    Sensors,
    About,
}

enum Action {
    Go(View),
    SetRange(Range),
    Explain { from: i64, to: i64 },
    Back,
    Recorder(&'static str),
    Verdict(&'static str),
}

struct App {
    db: PathBuf,
    bb: Option<PathBuf>,
    req_tx: Sender<Request>,
    out_rx: Receiver<Loaded>,
    /// The range currently loaded. Home always shows 24 hours; charts use `chart_range`.
    range: Range,
    chart_range: Range,
    data: Option<Loaded>,
    loading: bool,
    last_request: Instant,
    view: View,
    back: View,
    recorder: Option<u32>,
    last_pid_check: Instant,
    explained: Option<Explained>,
    why_error: Option<String>,
    verdict: Option<String>,
    notice: Option<(String, Instant)>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, db: PathBuf, start: Startup) -> Self {
        theme::install(&cc.egui_ctx);
        let (req_tx, out_rx) = spawn_worker(db.clone(), cc.egui_ctx.clone());
        let bb = std::env::current_exe().ok().and_then(|e| find_bb(&e));
        let view = start.view.unwrap_or(View::Home);
        let mut app = App {
            recorder: recorder_pid(&db),
            db,
            bb,
            req_tx,
            out_rx,
            range: Range::H24,
            chart_range: Range::H1,
            data: None,
            loading: false,
            last_request: Instant::now(),
            view: View::Home,
            back: View::Home,
            last_pid_check: Instant::now(),
            explained: None,
            why_error: None,
            verdict: None,
            notice: None,
        };
        app.go(view);
        app.request(false);
        if let Some(span) = start.explain {
            match parse_duration(&span) {
                Ok(s) => {
                    let now = Local::now().timestamp();
                    app.run_explain(now - s.max(2), now);
                }
                Err(e) => app.tell(format!("Couldn't read --explain {span}: {e}")),
            }
        }
        app
    }

    fn wanted_range(&self) -> Range {
        match self.view {
            View::Res(_) => self.chart_range,
            View::Detail => self.range,
            _ => Range::H24,
        }
    }

    fn go(&mut self, view: View) {
        self.view = view;
        let want = self.wanted_range();
        if want != self.range {
            self.range = want;
            self.request(true);
        }
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
        if self.view != View::Detail {
            self.back = self.view;
        }
        self.view = View::Detail;
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::Go(v) => self.go(v),
            Action::SetRange(r) => {
                self.chart_range = r;
                self.go(self.view);
            }
            Action::Explain { from, to } => self.run_explain(from, to),
            Action::Back => self.go(self.back),
            Action::Recorder(verb) => {
                let Some(bb) = self.bb.clone() else { return };
                let args: &[&str] = if verb == "start" { &["start", "--quiet"] } else { &["stop"] };
                match run_bb(&bb, &self.db, args) {
                    Ok(_) => self.tell(if verb == "start" { "Recording started" } else { "Recording stopped" }),
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

// ---- look and feel ------------------------------------------------------------------------

mod theme {
    use eframe::egui::{self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke, TextStyle, Theme, ThemePreference, Visuals};
    use std::sync::Arc;

    /// The colours every screen draws with, for the current light or dark mode.
    #[derive(Clone, Copy)]
    pub struct Pal {
        pub bg: Color32,
        pub side: Color32,
        pub card: Color32,
        pub hover: Color32,
        pub selected: Color32,
        pub border: Color32,
        pub border_strong: Color32,
        pub text: Color32,
        pub sub: Color32,
        pub muted: Color32,
        pub accent: Color32,
        pub accent_bg: Color32,
        pub ok: Color32,
        pub ok_bg: Color32,
        pub warn: Color32,
        pub warn_bg: Color32,
        pub bad: Color32,
        pub bad_bg: Color32,
        pub chart: [Color32; 4],
    }

    pub fn pal(dark: bool) -> Pal {
        let c = Color32::from_rgb;
        if dark {
            Pal {
                bg: c(24, 24, 23),
                side: c(30, 30, 29),
                card: c(34, 34, 33),
                hover: c(44, 44, 42),
                selected: c(52, 52, 50),
                border: c(54, 54, 52),
                border_strong: c(84, 84, 80),
                text: c(237, 237, 234),
                sub: c(180, 178, 169),
                muted: c(136, 135, 128),
                accent: c(133, 183, 235),
                accent_bg: c(12, 44, 76),
                ok: c(151, 196, 89),
                ok_bg: c(39, 80, 10),
                warn: c(250, 199, 117),
                warn_bg: c(80, 50, 8),
                bad: c(240, 149, 149),
                bad_bg: c(100, 30, 30),
                chart: [c(133, 183, 235), c(93, 202, 165), c(239, 159, 39), c(240, 149, 149)],
            }
        } else {
            Pal {
                bg: c(255, 255, 255),
                side: c(247, 247, 245),
                card: c(255, 255, 255),
                hover: c(241, 241, 238),
                selected: c(232, 232, 228),
                border: c(226, 225, 220),
                border_strong: c(196, 195, 188),
                text: c(26, 26, 25),
                sub: c(95, 94, 90),
                muted: c(136, 135, 128),
                accent: c(24, 95, 165),
                accent_bg: c(230, 241, 251),
                ok: c(59, 109, 17),
                ok_bg: c(234, 243, 222),
                warn: c(133, 79, 11),
                warn_bg: c(250, 238, 218),
                bad: c(163, 45, 45),
                bad_bg: c(252, 235, 235),
                chart: [c(55, 138, 221), c(29, 158, 117), c(186, 117, 23), c(226, 75, 74)],
            }
        }
    }

    fn visuals(dark: bool) -> Visuals {
        let p = pal(dark);
        let mut v = if dark { Visuals::dark() } else { Visuals::light() };
        v.panel_fill = p.bg;
        v.window_fill = p.card;
        v.extreme_bg_color = p.bg;
        v.faint_bg_color = p.side;
        v.override_text_color = Some(p.text);
        v.hyperlink_color = p.accent;
        v.selection.bg_fill = p.accent_bg;
        v.selection.stroke = Stroke::new(1.0, p.accent);
        v.window_stroke = Stroke::new(1.0, p.border);
        let r = CornerRadius::same(8);
        for (w, fill, stroke) in [
            (&mut v.widgets.noninteractive, Color32::TRANSPARENT, p.border),
            (&mut v.widgets.inactive, Color32::TRANSPARENT, p.border_strong),
            (&mut v.widgets.hovered, p.hover, p.border_strong),
            (&mut v.widgets.active, p.selected, p.border_strong),
            (&mut v.widgets.open, p.selected, p.border_strong),
        ] {
            w.corner_radius = r;
            w.bg_fill = fill;
            w.weak_bg_fill = fill;
            w.bg_stroke = Stroke::new(1.0, stroke);
            w.fg_stroke = Stroke::new(1.0, p.text);
            w.expansion = 0.0;
        }
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
        v
    }

    /// The system UI font where one can be found, so the window looks native. egui's own fonts
    /// stay as fallbacks for symbols the system font lacks.
    fn fonts() -> FontDefinitions {
        let mut f = FontDefinitions::default();
        let regular = ["C:\\Windows\\Fonts\\segoeui.ttf", "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf", "/usr/share/fonts/noto/NotoSans-Regular.ttf", "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"];
        let bold = ["C:\\Windows\\Fonts\\seguisb.ttf", "/usr/share/fonts/truetype/noto/NotoSans-SemiBold.ttf", "/usr/share/fonts/noto/NotoSans-SemiBold.ttf", "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"];
        let load = |paths: &[&str]| paths.iter().find_map(|p| std::fs::read(p).ok());
        if let Some(bytes) = load(&regular) {
            f.font_data.insert("ui".into(), Arc::new(FontData::from_owned(bytes)));
            f.families.entry(FontFamily::Proportional).or_default().insert(0, "ui".into());
        }
        let mut strong = vec![];
        if let Some(bytes) = load(&bold) {
            f.font_data.insert("ui-bold".into(), Arc::new(FontData::from_owned(bytes)));
            strong.push("ui-bold".to_string());
        }
        strong.extend(f.families.get(&FontFamily::Proportional).cloned().unwrap_or_default());
        f.families.insert(FontFamily::Name("bold".into()), strong);
        f
    }

    pub fn bold(size: f32) -> FontId {
        FontId::new(size, FontFamily::Name("bold".into()))
    }

    pub fn install(ctx: &egui::Context) {
        ctx.set_fonts(fonts());
        ctx.set_visuals_of(Theme::Dark, visuals(true));
        ctx.set_visuals_of(Theme::Light, visuals(false));
        ctx.set_theme(ThemePreference::System);
        ctx.all_styles_mut(|s| {
            s.spacing.item_spacing = egui::vec2(8.0, 6.0);
            s.spacing.button_padding = egui::vec2(12.0, 5.0);
            s.spacing.window_margin = Margin::same(16);
            s.interaction.selectable_labels = false;
            s.text_styles = [
                (TextStyle::Small, FontId::proportional(12.0)),
                (TextStyle::Body, FontId::proportional(14.0)),
                (TextStyle::Button, FontId::proportional(14.0)),
                (TextStyle::Heading, bold(20.0)),
                (TextStyle::Monospace, FontId::monospace(13.0)),
            ]
            .into();
        });
    }
}

use theme::{bold, pal, Pal};

// ---- small pieces -------------------------------------------------------------------------

fn sub(ui: &mut egui::Ui, p: &Pal, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).color(p.sub).size(13.0));
}

fn section(ui: &mut egui::Ui, p: &Pal, text: &str) {
    ui.add_space(14.0);
    ui.label(RichText::new(text).color(p.sub).size(13.0));
    ui.add_space(2.0);
}

fn tone(p: &Pal, c: Confidence) -> (Color32, Color32, &'static str) {
    match c {
        Confidence::High => (p.bad_bg, p.bad, "Likely"),
        Confidence::Medium => (p.warn_bg, p.warn, "Possible"),
        Confidence::Low => (p.side, p.sub, "Maybe"),
    }
}

fn chip(ui: &mut egui::Ui, bg: Color32, fg: Color32, text: &str) {
    let galley = ui.painter().layout_no_wrap(text.to_string(), egui::FontId::proportional(12.0), fg);
    let (rect, _) = ui.allocate_exact_size(galley.size() + egui::vec2(16.0, 6.0), Sense::hover());
    ui.painter().rect_filled(rect, 6.0, bg);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, fg);
}

fn card_frame(p: &Pal) -> egui::Frame {
    egui::Frame::new().fill(p.card).stroke(Stroke::new(1.0, p.border)).corner_radius(12).inner_margin(Margin::symmetric(14, 12))
}

/// A card the whole of which can be clicked. Returns true when it was.
fn click_card(ui: &mut egui::Ui, p: &Pal, id: usize, add: impl FnOnce(&mut egui::Ui)) -> bool {
    let rect = card_frame(p)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        })
        .response
        .rect;
    let r = ui.interact(rect, egui::Id::new(("card", id)), Sense::click());
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        ui.painter().rect_stroke(rect, 12.0, Stroke::new(1.0, p.border_strong), StrokeKind::Inside);
    }
    r.clicked()
}

fn range_picker(ui: &mut egui::Ui, p: &Pal, current: Range, actions: &mut Vec<Action>) {
    egui::Frame::new().fill(p.side).corner_radius(8).inner_margin(Margin::same(3)).show(ui, |ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for r in [Range::D7, Range::H24, Range::H6, Range::H1] {
            let on = r == current;
            let text = RichText::new(r.label()).size(13.0).color(if on { p.text } else { p.sub });
            let b = egui::Button::new(text).fill(if on { p.card } else { Color32::TRANSPARENT }).stroke(if on { Stroke::new(1.0, p.border) } else { Stroke::NONE }).corner_radius(6);
            if ui.add(b).clicked() && !on {
                actions.push(Action::SetRange(r));
            }
        }
    });
}

fn status_dot(ui: &mut egui::Ui, color: Color32, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), Sense::hover());
    ui.painter().circle_filled(rect.center(), size / 2.0, color);
}

// ---- charts -------------------------------------------------------------------------------

/// One chart. Returns the time that was clicked, if any.
#[allow(clippy::too_many_arguments)]
fn chart(ui: &mut egui::Ui, p: &Pal, id: &str, unit: &'static str, height: f32, ymax: f64, span: (i64, i64), lines: Vec<(&'static str, Color32, Vec<[f64; 2]>)>, moments: &[Moment], guides: &[(f64, Color32)]) -> Option<i64> {
    let plot = Plot::new(id)
        .height(height)
        .allow_scroll(false)
        .allow_zoom(false)
        .allow_drag(false)
        .allow_boxed_zoom(false)
        .show_background(false)
        .show_grid([false, true])
        .grid_spacing(48.0..=200.0)
        .include_x(span.0 as f64)
        .include_x(span.1 as f64)
        .include_y(0.0)
        .include_y(ymax)
        .y_axis_min_width(40.0)
        .x_axis_formatter(|mark, _| axis_label(mark.value as i64))
        .y_axis_formatter(move |mark, _| format!("{:.0}{unit}", mark.value))
        .label_formatter(move |pos| match pos {
            HoverPosition::NearDataPoint { plot_name, position, .. } => Some(format!("{}\n{plot_name}: {:.1}{unit}", clock(position.x as i64), position.y)),
            HoverPosition::Elsewhere { position } => Some(clock(position.x as i64)),
        });
    let p = *p;
    let out = plot.show(ui, |plot| {
        for m in moments {
            let (a, b) = (m.from as f64, m.to.max(m.from + 1) as f64);
            // A light wash of the confidence colour, so the lines stay readable on top of it.
            let (_, fg, _) = tone(&p, m.confidence);
            let wash = Color32::from_rgba_unmultiplied(fg.r(), fg.g(), fg.b(), 34);
            plot.polygon(Polygon::new("", PlotPoints::from(vec![[a, 0.0], [b, 0.0], [b, ymax], [a, ymax]])).fill_color(wash).stroke(Stroke::NONE));
        }
        for (y, color) in guides {
            plot.line(Line::new("", PlotPoints::from(vec![[span.0 as f64, *y], [span.1 as f64, *y]])).color(color.gamma_multiply(0.7)).width(1.0).style(egui_plot::LineStyle::dashed_loose()));
        }
        for (name, color, pts) in lines {
            plot.line(Line::new(name, PlotPoints::from(pts)).color(color).width(1.8));
        }
    });
    if out.response.hovered() && !moments.is_empty() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
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

fn moment_at(moments: &[Moment], t: i64) -> Option<&Moment> {
    moments.iter().find(|m| t >= m.from - 30 && t <= m.to + 30)
}

// ---- screens ------------------------------------------------------------------------------

fn loading_or_error(ui: &mut egui::Ui, p: &Pal, app: &App) -> bool {
    match &app.data {
        None => {
            ui.add_space(24.0);
            ui.horizontal(|ui| {
                ui.spinner();
                sub(ui, p, "Reading the recording");
            });
            true
        }
        Some(d) if d.error.is_some() => {
            ui.add_space(24.0);
            ui.colored_label(p.bad, d.error.as_deref().unwrap_or_default());
            true
        }
        _ => false,
    }
}

fn home(app: &App, ui: &mut egui::Ui, p: &Pal, actions: &mut Vec<Action>) {
    if loading_or_error(ui, p, app) {
        return;
    }
    let Some(d) = &app.data else { return };
    let (health, title, line) = home_status(d.now, app.recorder.is_some(), d.latest.as_ref().map(|s| s.ts), &d.moments);
    let color = match health {
        Health::Good => p.ok,
        Health::Slow => p.warn,
        Health::Unknown => p.muted,
    };
    ui.horizontal(|ui| {
        ui.add_space(2.0);
        status_dot(ui, color, 12.0);
        ui.add_space(4.0);
        ui.label(RichText::new(title).font(bold(22.0)));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if d.latest.is_some() && ui.button("Explain the last 10 minutes").clicked() {
                actions.push(Action::Explain { from: d.now - 600, to: d.now });
            }
        });
    });
    ui.horizontal(|ui| {
        ui.add_space(26.0);
        sub(ui, p, line);
    });

    section(ui, p, "Last 24 hours");
    let slots = strip(d.now, Range::H24.secs(), 48, &d.points, &d.moments);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), Sense::click());
    let w = rect.width() / slots.len() as f32;
    let hovered = resp.hover_pos().map(|pos| (((pos.x - rect.left()) / w) as usize).min(slots.len() - 1));
    for (i, s) in slots.iter().enumerate() {
        let r = egui::Rect::from_min_size(egui::pos2(rect.left() + i as f32 * w, rect.top()), egui::vec2((w - 2.0).max(1.0), rect.height()));
        let (fill, stroke) = match s {
            Slot::NoData => (p.side, Stroke::NONE),
            Slot::Fine => (p.ok_bg, Stroke::NONE),
            Slot::Slow(_, c) => {
                let (bg, fg, _) = tone(p, *c);
                (bg, Stroke::new(1.0, fg))
            }
        };
        let fill = if hovered == Some(i) { fill.gamma_multiply(0.75) } else { fill };
        ui.painter().rect(r, 3.0, fill, stroke, StrokeKind::Inside);
    }
    if let Some(i) = hovered {
        let block = Range::H24.secs() / slots.len() as i64;
        let from = d.now - Range::H24.secs() + i as i64 * block;
        let what = match slots[i] {
            Slot::NoData => "Not recording".to_string(),
            Slot::Fine => "No slowdowns".to_string(),
            Slot::Slow(k, _) => format!("{}. Click to see why.", headline(d.moments[k].kind)),
        };
        if matches!(slots[i], Slot::Slow(..)) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        resp.clone().on_hover_text(format!("{} to {}\n{what}", axis_label(from), axis_label(from + block)));
        if resp.clicked() {
            if let Slot::Slow(k, _) = slots[i] {
                actions.push(Action::Explain { from: d.moments[k].from, to: d.moments[k].to });
            }
        }
    }
    ui.horizontal(|ui| {
        let small = |t: String| RichText::new(t).color(p.muted).size(11.0);
        ui.label(small(axis_label(d.now - Range::H24.secs())));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(small("Now".into()));
        });
    });
    ui.horizontal(|ui| {
        for (fill, label) in [(p.ok_bg, "No slowdowns"), (p.warn_bg, "Slowdown"), (p.side, "Not recording")] {
            let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), Sense::hover());
            ui.painter().rect_filled(r, 2.0, fill);
            ui.label(RichText::new(label).color(p.muted).size(11.0));
            ui.add_space(8.0);
        }
    });

    section(ui, p, "Slowdowns");
    if d.moments.is_empty() {
        card_frame(p).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("Nothing slowed this computer down in the last 24 hours").font(bold(15.0)));
            sub(ui, p, if d.points.is_empty() { "There's no recording for this period yet." } else { "When something does, it shows up here with the reason." });
        });
        return;
    }
    for (i, m) in d.moments.iter().enumerate().rev().take(50) {
        let clicked = click_card(ui, p, i, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(headline(m.kind)).font(bold(15.0)));
                    sub(ui, p, format!("{} to {} · {} · {}", axis_label(m.from), axis_label(m.to), duration_text(m.to - m.from), m.title));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new("›").size(20.0).color(p.muted));
                    let (bg, fg, label) = tone(p, m.confidence);
                    chip(ui, bg, fg, label);
                });
            });
        });
        if clicked {
            actions.push(Action::Explain { from: m.from, to: m.to });
        }
        ui.add_space(2.0);
    }
}

fn detail(app: &App, ui: &mut egui::Ui, p: &Pal, actions: &mut Vec<Action>) {
    if ui.button("‹  Back").clicked() {
        actions.push(Action::Back);
    }
    ui.add_space(10.0);
    if let Some(e) = &app.why_error {
        ui.colored_label(p.bad, e);
        return;
    }
    let Some(ex) = &app.explained else { return };
    sub(ui, p, format!("{} to {} · {} · {} samples", axis_label(ex.from), axis_label(ex.to), duration_text(ex.to - ex.from), ex.samples));
    ui.add_space(4.0);
    if ex.samples == 0 {
        ui.label(RichText::new("Nothing was recorded then").font(bold(22.0)));
        sub(ui, p, "The recorder wasn't running at that time, so there's nothing to explain.");
        return;
    }
    let Some(top) = ex.findings.first() else {
        ui.label(RichText::new("No clear cause").font(bold(22.0)));
        ui.add_space(4.0);
        sub(ui, p, "CPU, memory, disk, GPU, heat and power all looked normal. It may be something blackbox can't see yet, such as the network.");
        feedback(app, ui, p, actions);
        return;
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(headline(kind_of(&top.title))).font(bold(22.0)));
        let (bg, fg, label) = tone(p, top.confidence);
        chip(ui, bg, fg, &format!("{label} cause"));
    });
    sub(ui, p, &top.title);
    finding_body(ui, p, top);
    if ex.findings.len() > 1 {
        section(ui, p, "Also seen");
        for f in &ex.findings[1..] {
            card_frame(p).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&f.title).font(bold(14.0)));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (bg, fg, label) = tone(p, f.confidence);
                        chip(ui, bg, fg, label);
                    });
                });
                for e in &f.evidence {
                    sub(ui, p, e);
                }
            });
            ui.add_space(4.0);
        }
    }
    feedback(app, ui, p, actions);
}

fn finding_body(ui: &mut egui::Ui, p: &Pal, f: &Finding) {
    section(ui, p, "Evidence");
    for e in &f.evidence {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            status_dot(ui, p.muted, 5.0);
            ui.label(e);
        });
        ui.add_space(4.0);
        let y = ui.cursor().top();
        ui.painter().hline(ui.max_rect().x_range(), y, Stroke::new(1.0, p.border));
    }
    if let Some(h) = &f.hint {
        ui.add_space(12.0);
        egui::Frame::new().fill(p.accent_bg).corner_radius(8).inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("What you can do").font(bold(13.0)).color(p.accent));
            ui.label(h);
        });
    }
}

fn feedback(app: &App, ui: &mut egui::Ui, p: &Pal, actions: &mut Vec<Action>) {
    ui.add_space(16.0);
    ui.horizontal(|ui| match &app.verdict {
        Some(_) => sub(ui, p, "Thanks. Saved on this computer only."),
        None => {
            sub(ui, p, "Was this right?");
            for (label, v) in [("Yes", "right"), ("Partly", "partly"), ("No", "wrong")] {
                if ui.button(label).clicked() {
                    actions.push(Action::Verdict(v));
                }
            }
        }
    });
}

fn resource(app: &App, ui: &mut egui::Ui, p: &Pal, res: Res, actions: &mut Vec<Action>) {
    let gb = |b: u64| b as f64 / 1_073_741_824.0;
    let now_text = app.data.as_ref().and_then(|d| d.latest.as_ref()).map(|s| match res {
        Res::Cpu => format!("{:.0}%", s.cpu_pct),
        Res::Memory => format!("{:.1} of {:.0} GB", gb(s.mem_used), gb(s.mem_total)),
        Res::Disk => format!("{:.1} MB/s", s.disk_bps as f64 / 1_048_576.0),
        Res::Gpu => s.sensors.gpu_pct.map_or("not available".into(), |g| format!("{g:.0}%")),
        Res::Heat => s.sensors.temp_c.map_or("not available".into(), |t| format!("{t:.0} C")),
    });
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            sub(ui, p, format!("{} now", res.label()));
            ui.label(RichText::new(now_text.unwrap_or_else(|| "–".into())).font(bold(28.0)));
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            range_picker(ui, p, app.chart_range, actions);
            if app.loading {
                ui.spinner();
            }
        });
    });
    ui.add_space(8.0);
    if loading_or_error(ui, p, app) {
        return;
    }
    let Some(d) = &app.data else { return };
    if d.points.is_empty() {
        ui.add_space(16.0);
        ui.label(RichText::new("Nothing recorded in this range").font(bold(16.0)));
        sub(ui, p, "Pick a longer range, or start recording from the sidebar.");
        return;
    }
    let span = (d.now - app.chart_range.secs(), d.now);
    if res == Res::Heat && d.points.iter().all(|x| x.temp.is_none() && x.gpu_temp.is_none()) {
        heat_details(ui, p, d, span);
        return;
    }
    if res == Res::Gpu && d.points.iter().all(|x| x.gpu.is_none()) {
        sub(ui, p, "This computer doesn't report GPU load.");
        return;
    }
    let [c0, c1, c2, c3] = p.chart;
    let h = if res == Res::Heat { (ui.available_height() * 0.38).clamp(140.0, 260.0) } else { (ui.available_height() - 90.0).clamp(160.0, 420.0) };
    let clicked = match res {
        Res::Cpu => chart(ui, p, "cpu", "%", h, 100.0, span, lines("CPU", c0, &d.points, |x| Some(x.cpu)), &d.moments, &[]),
        Res::Memory => chart(ui, p, "mem", "%", h, 100.0, span, lines("Memory used", c1, &d.points, |x| Some(x.mem)), &d.moments, &[]),
        Res::Disk => {
            let max = d.points.iter().map(|x| x.disk_mb).fold(10.0, f64::max) * 1.1;
            chart(ui, p, "disk", " MB/s", h, max, span, lines("Disk", c2, &d.points, |x| Some(x.disk_mb)), &d.moments, &[])
        }
        Res::Gpu => chart(ui, p, "gpu", "%", h, 100.0, span, lines("GPU", c0, &d.points, |x| x.gpu), &d.moments, &[]),
        Res::Heat => {
            let tmax = d.points.iter().flat_map(|x| [x.temp, x.gpu_temp]).flatten().fold(95.0, f64::max) + 5.0;
            let l = [lines("System", c3, &d.points, |x| x.temp), lines("GPU", c0, &d.points, |x| x.gpu_temp)].concat();
            chart(ui, p, "heat", " C", h, tmax, span, l, &d.moments, &[(80.0, p.warn), (90.0, p.bad)])
        }
    };
    if let Some(m) = clicked.and_then(|t| moment_at(&d.moments, t)) {
        actions.push(Action::Explain { from: m.from, to: m.to });
    }
    sub(ui, p, if d.moments.is_empty() { "No slowdowns in this range." } else { "Shaded stretches are slowdowns. Click one to see why." });
    if res == Res::Heat {
        heat_details(ui, p, d, span);
    }
}

fn heat_details(ui: &mut egui::Ui, p: &Pal, d: &Loaded, span: (i64, i64)) {
    let h = &d.heat;
    let (level, text) = h.verdict();
    let (bg, fg) = match level {
        "bad" => (p.bad_bg, p.bad),
        "warn" => (p.warn_bg, p.warn),
        "good" => (p.ok_bg, p.ok),
        _ => (p.side, p.sub),
    };
    ui.add_space(8.0);
    egui::Frame::new().fill(bg).corner_radius(8).inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(text).color(fg));
    });
    if h.peak_c.is_none() {
        return;
    }
    let t = |v: Option<f32>| v.map_or("n/a".to_string(), |c| format!("{c:.0} C"));
    let peak_at = h.peak_ts.map(|ts| format!(" at {}", axis_label(ts))).unwrap_or_default();
    let rows = [
        ("Peak", format!("{}{peak_at}", t(h.peak_c))),
        ("Typical when idle", h.idle_c.map_or("not enough idle time to tell".to_string(), |c| format!("{c:.0} C"))),
        ("Time at 80 C or more", HeatSummary::mins(h.secs_over_warm)),
        ("Time at 90 C or more", HeatSummary::mins(h.secs_over_hot)),
        ("Slowed down while hot", HeatSummary::mins(h.secs_slowed_by_heat)),
    ];
    section(ui, p, "This range");
    egui::Grid::new("heat").num_columns(2).spacing([32.0, 6.0]).show(ui, |ui| {
        for (k, v) in rows {
            ui.label(RichText::new(k).color(p.sub));
            ui.label(v);
            ui.end_row();
        }
    });
    section(ui, p, "CPU speed and load. Speed dropping while it's busy and hot means heat is slowing it down.");
    let [c0, ..] = p.chart;
    let l = [lines("CPU speed", c0, &d.points, |x| x.freq), lines("CPU load", p.muted, &d.points, |x| Some(x.cpu))].concat();
    chart(ui, p, "speed", "%", 140.0, 100.0, span, l, &[], &[]);
}

fn sensors(app: &App, ui: &mut egui::Ui, p: &Pal) {
    ui.label(RichText::new("Sensors").font(bold(22.0)));
    let Some(s) = app.data.as_ref().and_then(|d| d.latest.as_ref()) else {
        sub(ui, p, "No recent readings. Start recording to see them.");
        return;
    };
    sub(ui, p, format!("What this computer reports, as of {}", clock(s.ts)));
    ui.add_space(10.0);
    let gb = |b: u64| b as f64 / 1_073_741_824.0;
    let mut rows: Vec<(String, Option<String>)> = vec![
        ("CPU".into(), Some(format!("{:.0}%", s.cpu_pct))),
        ("Memory".into(), Some(format!("{:.1} of {:.1} GB used", gb(s.mem_used), gb(s.mem_total)))),
        ("Swap / pagefile".into(), Some(format!("{:.1} GB in use", gb(s.swap_used)))),
        ("Disk throughput".into(), Some(format!("{:.1} MB/s", s.disk_bps as f64 / 1_048_576.0))),
    ];
    rows.extend(describe_sensors(&s.sensors).into_iter().map(|(k, v)| (k.to_string(), v)));
    for (k, v) in rows {
        ui.horizontal(|ui| {
            ui.label(RichText::new(k).color(p.sub));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| match v {
                Some(v) => ui.label(v),
                None => ui.label(RichText::new("not available here").color(p.muted)),
            });
        });
        ui.add_space(3.0);
        let y = ui.cursor().top();
        ui.painter().hline(ui.max_rect().x_range(), y, Stroke::new(1.0, p.border));
        ui.add_space(3.0);
    }
}

fn about(app: &App, ui: &mut egui::Ui, p: &Pal) {
    ui.label(RichText::new("blackbox").font(bold(22.0)));
    sub(ui, p, format!("Desktop window, version {VERSION}"));
    ui.add_space(10.0);
    ui.label("blackbox records your computer's vitals in the background, so when something felt slow it can tell you why. Everything stays on this computer.");
    ui.add_space(10.0);
    egui::Grid::new("about").num_columns(2).spacing([32.0, 6.0]).show(ui, |ui| {
        let mut row = |k: &str, v: String| {
            ui.label(RichText::new(k).color(p.sub));
            ui.label(v);
            ui.end_row();
        };
        row("Database", app.db.display().to_string());
        if let Some(d) = &app.data {
            row("Samples recorded", d.count.to_string());
            if let (Some(a), Some(b)) = (d.first, d.last) {
                row("Covers", format!("{} to {}", axis_label(a), axis_label(b)));
            }
        }
        row("Recorder program", app.bb.as_ref().map_or("not found".to_string(), |b| b.display().to_string()));
    });
    ui.add_space(10.0);
    ui.hyperlink_to("Project page and documentation", "https://github.com/AnakinSkywalker0/blackbox");
}

fn nav(ui: &mut egui::Ui, p: &Pal, label: &str, on: bool) -> bool {
    let text = RichText::new(label).size(14.0).color(if on { p.text } else { p.sub });
    let b = egui::Button::new(text).fill(if on { p.selected } else { Color32::TRANSPARENT }).stroke(Stroke::NONE).corner_radius(8).min_size(egui::vec2(ui.available_width(), 32.0));
    let r = ui.add(b);
    r.clicked()
}

fn sidebar(app: &App, ui: &mut egui::Ui, p: &Pal, actions: &mut Vec<Action>) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(8.0);
        ui.label(RichText::new("blackbox").font(bold(17.0)));
    });
    ui.add_space(14.0);
    ui.style_mut().spacing.button_padding = egui::vec2(10.0, 6.0);
    ui.spacing_mut().item_spacing.y = 2.0;
    let mut items = vec![(View::Home, "Home"), (View::Res(Res::Cpu), "CPU"), (View::Res(Res::Memory), "Memory"), (View::Res(Res::Disk), "Disk")];
    if app.data.as_ref().is_some_and(|d| d.points.iter().any(|x| x.gpu.is_some())) {
        items.push((View::Res(Res::Gpu), "GPU"));
    }
    items.push((View::Res(Res::Heat), "Heat"));
    let current = if app.view == View::Detail { app.back } else { app.view };
    for (v, label) in items {
        if nav(ui, p, label, current == v) {
            actions.push(Action::Go(v));
        }
    }
    ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
        ui.add_space(6.0);
        let have_bb = app.bb.is_some();
        let (label, verb) = if app.recorder.is_some() { ("Stop recording", "stop") } else { ("Start recording", "start") };
        let b = egui::Button::new(RichText::new(label).size(13.0)).min_size(egui::vec2(ui.available_width(), 28.0));
        if ui.add_enabled(have_bb, b).on_disabled_hover_text("The bb program wasn't found next to this window or on PATH").clicked() {
            actions.push(Action::Recorder(verb));
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            let (color, text) = if app.recorder.is_some() { (p.ok, "Recording") } else { (p.muted, "Not recording") };
            status_dot(ui, color, 8.0);
            ui.label(RichText::new(text).color(p.sub).size(13.0));
        });
        ui.add_space(10.0);
        for (v, label) in [(View::About, "About"), (View::Sensors, "Sensors")] {
            if nav(ui, p, label, current == v) {
                actions.push(Action::Go(v));
            }
        }
    });
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.pump(&ctx);
        let p = pal(ui.visuals().dark_mode);
        let mut actions: Vec<Action> = Vec::new();

        egui::Panel::left("nav")
            .exact_size(176.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(p.side).stroke(Stroke::new(1.0, p.border)).inner_margin(Margin::same(10)))
            .show(ui, |ui| sidebar(self, ui, &p, &mut actions));

        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.bg).inner_margin(Margin::symmetric(28, 22))).show(ui, |ui| {
            if let Some((msg, _)) = &self.notice {
                egui::Frame::new().fill(p.side).corner_radius(8).inner_margin(Margin::symmetric(12, 6)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    sub(ui, &p, msg.clone());
                });
                ui.add_space(8.0);
            }
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.view {
                View::Home => home(self, ui, &p, &mut actions),
                View::Detail => detail(self, ui, &p, &mut actions),
                View::Res(r) => resource(self, ui, &p, r, &mut actions),
                View::Sensors => sensors(self, ui, &p),
                View::About => about(self, ui, &p),
            });
        });

        for a in actions {
            self.apply(a);
        }
    }
}

/// Where to open: `--view home|cpu|memory|disk|gpu|heat|sensors|about` (the older `--tab`
/// names still work), and `--explain 10m` to open straight on an explanation of the last 10
/// minutes (handy as a desktop shortcut).
#[derive(Default)]
struct Startup {
    view: Option<View>,
    explain: Option<String>,
}

fn parse_view(name: Option<&str>) -> Option<View> {
    Some(match name? {
        "home" | "timeline" | "why" => View::Home,
        "cpu" => View::Res(Res::Cpu),
        "memory" => View::Res(Res::Memory),
        "disk" => View::Res(Res::Disk),
        "gpu" => View::Res(Res::Gpu),
        "heat" | "thermal" => View::Res(Res::Heat),
        "sensors" => View::Sensors,
        "about" => View::About,
        _ => return None,
    })
}

fn parse_args(args: impl Iterator<Item = String>) -> (Option<PathBuf>, Startup) {
    let mut args = args;
    let (mut db, mut start) = (None, Startup::default());
    while let Some(a) = args.next() {
        match a.as_str() {
            "--db" => db = args.next().map(PathBuf::from),
            "--view" | "--tab" => start.view = parse_view(args.next().as_deref()),
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
        viewport: egui::ViewportBuilder::default().with_title("blackbox").with_inner_size([1040.0, 760.0]).with_min_inner_size([760.0, 520.0]),
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
        assert!(db.is_none() && s.view.is_none() && s.explain.is_none());
        let (db, s) = parse(&["--db", "x.db", "--view", "sensors"]);
        assert_eq!(db, Some(PathBuf::from("x.db")));
        assert_eq!(s.view, Some(View::Sensors));
        let (_, s) = parse(&["--explain", "10m"]);
        assert_eq!(s.explain.as_deref(), Some("10m"));
        let (_, s) = parse(&["--tab", "nonsense", "--unknown"]);
        assert!(s.view.is_none());
    }

    /// A day of quiet history with two slowdowns in it: a CPU hog 3 hours ago and heavy disk
    /// use 9 hours ago.
    fn fixture_db() -> PathBuf {
        use bb_core::model::{ProcRow, Sample};
        let db = std::env::temp_dir().join(format!("bb-gui-shots-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db);
        let now = Local::now().timestamp();
        let mut st = Store::open(&db).unwrap();
        let mut v = Vec::new();
        for t in (now - 24 * 3600..now).step_by(5) {
            let hog = (now - 3 * 3600..now - 3 * 3600 + 300).contains(&t);
            let disk = (now - 9 * 3600..now - 9 * 3600 + 240).contains(&t);
            v.push(Sample {
                ts: t,
                cpu_pct: if hog { 82.0 } else { 7.0 + (t % 7) as f32 },
                mem_used: 9 << 30,
                mem_total: 16 << 30,
                disk_bps: if disk { 160 << 20 } else { 1 << 20 },
                procs: vec![ProcRow { name: if hog { "rustc.exe".into() } else if disk { "OneDrive.exe".into() } else { "code.exe".into() }, count: 4, cpu_pct: if hog { 74.0 } else { 3.0 }, disk_bps: if disk { 150 << 20 } else { 0 }, mem_bytes: 1 << 30 }],
                ..Default::default()
            });
        }
        st.insert_many(&v).unwrap();
        db
    }

    /// Renders every screen to PNGs without opening a window. Needs a GPU adapter, so it's
    /// opt-in: `UPDATE_SNAPSHOTS=1 cargo test -p bb-gui screenshots -- --ignored`. Set
    /// `BB_GUI_SHOT_DB` to render a real recording instead of the fixture.
    #[test]
    #[ignore]
    fn screenshots() {
        let db = std::env::var_os("BB_GUI_SHOT_DB").map(PathBuf::from).unwrap_or_else(fixture_db);
        let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/gui-shots");
        let opts = egui_kittest::SnapshotOptions::new().output_path(&out);
        let mut results = egui_kittest::SnapshotResults::new();
        for (theme, suffix) in [(egui::ThemePreference::Light, "light"), (egui::ThemePreference::Dark, "dark")] {
            let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1040.0, 760.0)).build_eframe(|cc| App::new(cc, db.clone(), Startup::default()));
            h.ctx.set_theme(theme);
            let settle = |h: &mut egui_kittest::Harness<App>| {
                for _ in 0..200 {
                    h.step();
                    if h.state().data.is_some() && !h.state().loading {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(30));
                }
                h.run_steps(4);
            };
            settle(&mut h);
            h.snapshot_options(format!("home-{suffix}"), &opts);
            if let Some(m) = h.state().data.as_ref().and_then(|d| d.moments.last().cloned()) {
                h.state_mut().apply(Action::Explain { from: m.from, to: m.to });
                settle(&mut h);
                h.snapshot_options(format!("detail-{suffix}"), &opts);
            }
            for (v, name) in [(View::Res(Res::Cpu), "cpu"), (View::Res(Res::Heat), "heat"), (View::Sensors, "sensors")] {
                h.state_mut().apply(Action::Go(v));
                settle(&mut h);
                h.snapshot_options(format!("{name}-{suffix}"), &opts);
            }
            results.extend_harness(&mut h);
        }
    }

    #[test]
    fn old_tab_names_still_open_something_sensible() {
        assert_eq!(parse(&["--tab", "timeline"]).1.view, Some(View::Home));
        assert_eq!(parse(&["--tab", "thermal"]).1.view, Some(View::Res(Res::Heat)));
        assert_eq!(parse(&["--tab", "why"]).1.view, Some(View::Home));
    }
}
