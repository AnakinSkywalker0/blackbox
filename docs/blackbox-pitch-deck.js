// Rebuild with:
// NODE_PATH=<directory containing pptxgenjs> node docs/blackbox-pitch-deck.js
// Claims and examples are drawn from ../README.md (v0.4 documentation).
const pptxgen = require('pptxgenjs');
const path = require('path');

const pptx = new pptxgen();
pptx.layout = 'LAYOUT_WIDE';
pptx.author = 'blackbox';
pptx.subject = 'Blackbox product overview';
pptx.title = 'Blackbox — a flight recorder for computer performance';
pptx.company = 'blackbox';
pptx.lang = 'en-US';

const S = pptx.ShapeType;
const C = {
  ink: '131715', near: '1B211E', paper: 'F8FAF7', white: 'FFFFFF',
  lime: 'C7F36B', mint: 'B7D8C2', gray: '68716C', pale: 'E7EEE6',
  muted: 'A9B5AA', darkgray: '2E3832', red: 'E79785',
};
const W = 13.333, H = 7.5;

function text(slide, value, x, y, w, h, size = 18, color = C.ink, extra = {}) {
  slide.addText(value, {
    x, y, w, h, fontFace: 'Arial', fontSize: size, color,
    margin: 0, breakLine: false, valign: 'mid', isTextBox: true,
    ...extra,
  });
}
function box(slide, x, y, w, h, fill, radius = 0.18, line = fill) {
  slide.addShape(radius ? S.roundRect : S.rect, {
    x, y, w, h, rectRadius: radius, radius,
    line: { color: line, transparency: line === fill ? 100 : 0 },
    fill: { color: fill },
  });
}
function circle(slide, x, y, d, fill) {
  slide.addShape(S.ellipse, { x, y, w: d, h: d, line: { color: fill, transparency: 100 }, fill: { color: fill } });
}
function rule(slide, x1, y1, x2, y2, color = C.pale, width = 1) {
  const dx = x2 - x1, dy = y2 - y1;
  slide.addShape(S.line, {
    x: Math.min(x1, x2), y: Math.min(y1, y2), w: Math.abs(dx), h: Math.abs(dy),
    flipV: dx * dy < 0, line: { color, width },
  });
}
function dot(slide, x, y, d, color) { circle(slide, x - d / 2, y - d / 2, d, color); }
function activity(slide, x, y, w, h, values, stroke = C.lime, width = 2.6) {
  for (let i = 1; i < values.length; i++) {
    rule(slide, x + (i - 1) * w / (values.length - 1), y + h * (1 - values[i - 1]),
      x + i * w / (values.length - 1), y + h * (1 - values[i]), stroke, width);
  }
}
function sensorIcon(slide, type, x, y, color = C.ink) {
  if (type === 'cpu') {
    activity(slide, x, y + 0.04, 0.43, 0.28, [0.20, 0.22, 0.80, 0.12, 0.66, 0.35, 0.38], color, 2.4);
  } else if (type === 'memory') {
    for (let r = 0; r < 2; r++) for (let c = 0; c < 3; c++) {
      box(slide, x + c * 0.16, y + r * 0.18, 0.10, 0.10, color, 0.025);
    }
  } else if (type === 'disk') {
    for (let r = 0; r < 3; r++) {
      rule(slide, x, y + r * 0.14, x + 0.46, y + r * 0.14, color, 2.1);
      dot(slide, x + 0.39, y + r * 0.14 - 0.015, 0.055, color);
    }
  } else {
    rule(slide, x + 0.27, y, x + 0.08, y + 0.23, color, 2.6);
    rule(slide, x + 0.08, y + 0.23, x + 0.26, y + 0.23, color, 2.6);
    rule(slide, x + 0.26, y + 0.23, x + 0.13, y + 0.47, color, 2.6);
  }
}
function base(section, dark = false) {
  const slide = pptx.addSlide();
  slide.background = { color: dark ? C.ink : C.paper };
  text(slide, 'blackbox  /  bb', 0.66, 0.38, 2.8, 0.25, 11, dark ? C.muted : C.gray, { bold: true, charSpacing: 1 });
  text(slide, section.toUpperCase(), 9.15, 0.38, 3.5, 0.25, 10, dark ? C.muted : C.gray, { align: 'right', charSpacing: 1.5 });
  text(slide, String(pptx._slides.length).padStart(2, '0'), 12.0, 7.05, 0.63, 0.22, 10, dark ? C.muted : C.gray, { align: 'right' });
  return slide;
}
function heading(slide, kicker, title, sub = '', dark = false) {
  const fg = dark ? C.white : C.ink;
  text(slide, kicker.toUpperCase(), 0.68, 0.95, 4.5, 0.26, 11, dark ? C.lime : C.gray, { bold: true, charSpacing: 1.3 });
  text(slide, title, 0.66, 1.31, 12.0, 0.78, 37, fg, { bold: true, breakLine: false });
  if (sub) text(slide, sub, 0.69, 2.13, 11.8, 0.57, 16, dark ? C.muted : C.gray);
}
function pill(slide, label, x, y, w, fill = C.pale, color = C.ink) {
  box(slide, x, y, w, 0.35, fill, 0.16);
  text(slide, label, x + 0.14, y + 0.06, w - 0.28, 0.2, 10.5, color, { bold: true });
}
function notes(slide, body) { slide.addNotes(body); }

// 1 — cover
{
  const s = base('product overview', true);
  box(s, 9.05, 1.42, 3.42, 4.47, C.near, 0.24);
  text(s, '> bb why 15:40', 9.35, 1.82, 2.9, 0.40, 18, C.lime, { fontFace: 'Courier New', bold: true });
  rule(s, 9.34, 2.45, 12.18, 2.45, C.darkgray);
  text(s, 'Most likely cause', 9.35, 2.73, 2.8, 0.28, 13, C.muted);
  text(s, 'A short CPU spike', 9.35, 3.12, 2.8, 0.42, 23, C.white, { bold: true });
  text(s, '100% CPU for 5 seconds\nHidden by the 2-minute average', 9.35, 3.73, 2.72, 0.95, 14, C.muted, { breakLine: false, valign: 'top' });
  pill(s, 'EVIDENCE, NOT GUESSWORK', 9.35, 5.04, 2.74, C.lime, C.ink);
  text(s, 'blackbox', 0.64, 1.55, 7.9, 1.05, 61, C.white, { bold: true });
  text(s, 'A flight recorder for your\ncomputer’s performance.', 0.68, 2.78, 7.7, 1.65, 31, C.white, { bold: true, breakLine: false, valign: 'top' });
  text(s, 'When something felt slow, ask why — and get an answer backed by what happened on your machine.', 0.70, 5.24, 7.4, 0.85, 19, C.muted, { valign: 'top' });
  notes(s, 'Product and sample output: README.md opening and “What bb why can explain” example. The visual is illustrative, based on the documented short-spike example.');
}

// 2 — problem
{
  const s = base('the problem');
  heading(s, 'The gap', 'The slowdown is over. The evidence is gone.', 'Existing tools make you choose between a live snapshot and a raw trace.');
  box(s, 0.68, 2.99, 11.98, 2.48, C.ink, 0.18);
  text(s, 'ILLUSTRATIVE CPU ACTIVITY', 1.03, 3.28, 4.6, 0.22, 11, C.muted, { bold: true, charSpacing: 1.0 });
  const levels = [0.14,0.20,0.16,0.23,0.18,0.28,0.17,0.21,0.14,0.26,0.22,0.18,
    0.24,0.22,0.16,0.32,0.27,0.16,0.21,0.34,0.17,0.90,0.98,0.95,
    0.79,0.37,0.22,0.20,0.25,0.18,0.22,0.14,0.28,0.17,0.25,0.16,0.22,0.20,0.18,0.24];
  rule(s, 1.05, 4.63, 12.27, 4.63, C.darkgray, 1);
  levels.forEach((v, i) => {
    const bx = 1.08 + i * 0.281;
    box(s, bx, 4.62 - v * 0.80, 0.13, Math.max(0.05, v * 0.80), i >= 21 && i <= 24 ? C.lime : '5E6A61', 0.025);
  });
  rule(s, 7.52, 3.48, 7.52, 4.79, C.lime, 1.8);
  dot(s, 7.52, 3.73, 0.13, C.lime);
  text(s, 'THE SLOWDOWN', 7.81, 3.57, 2.45, 0.26, 12, C.lime, { bold: true, charSpacing: 0.6 });
  text(s, '15:35', 1.05, 4.94, 1.2, 0.22, 11, C.muted);
  text(s, '15:40', 7.12, 4.94, 1.2, 0.22, 11, C.lime, { bold: true });
  text(s, 'NOW', 11.56, 4.94, 0.67, 0.22, 11, C.muted, { align: 'right' });
  box(s, 0.68, 5.79, 5.78, 0.87, C.white, 0.14);
  box(s, 6.84, 5.79, 5.82, 0.87, C.pale, 0.14);
  text(s, 'LIVE SNAPSHOT   The spike is gone.', 1.00, 6.08, 5.10, 0.28, 15, C.gray, { bold: true });
  text(s, 'BLACKBOX   Ask what happened at 15:40.', 7.15, 6.08, 5.14, 0.28, 15, C.ink, { bold: true });
  notes(s, 'Problem framing from README.md opening. The CPU trace is a conceptual, explicitly illustrative graphic rather than measured performance data.');
}

// 3 — experience
{
  const s = base('product experience', true);
  heading(s, 'One question', 'Ask about the moment that mattered.', 'No dashboard to watch in real time. Query a time after the fact.', true);
  box(s, 0.68, 2.91, 11.98, 3.34, C.near, 0.20);
  text(s, '$ bb why now --span 2m', 1.04, 3.25, 10.95, 0.39, 20, C.lime, { fontFace: 'Courier New', bold: true });
  rule(s, 1.03, 3.83, 12.26, 3.83, C.darkgray);
  text(s, 'Most likely cause', 1.04, 4.09, 4.2, 0.25, 12, C.muted, { charSpacing: 1.0 });
  text(s, 'A short CPU spike around 22:27:24', 1.04, 4.42, 10.90, 0.43, 24, C.white, { bold: true });
  text(s, '22:27:22–22:27:26: CPU averaged 100%   /   whole window: only 30%', 1.05, 5.13, 10.9, 0.30, 15, C.muted, { fontFace: 'Courier New' });
  text(s, 'Zoom in: bb why 22:27:24 --span 30s', 1.05, 5.70, 10.9, 0.28, 13, C.lime, { fontFace: 'Courier New' });
  notes(s, 'README.md “What bb why can explain” example, lines 321–332. Deliberate Windows test; formatted and shortened for slide legibility.');
}

// 4 — mechanism
{
  const s = base('how it works');
  heading(s, 'The pipeline', 'Always recording. Ready when you ask.', 'A small local pipeline turns raw measurements into ranked, evidence-backed causes.');
  const xs = [0.68, 4.81, 8.94];
  const stages = [
    ['01', 'Sample', 'CPU, memory, paging, disk, clock speed, GPU and busiest programs — as available.'],
    ['02', 'Keep context', 'SQLite stores one-second samples; older history is thinned and retained locally.'],
    ['03', 'Explain', 'Time-window rules rank likely causes, show evidence and label confidence.'],
  ];
  stages.forEach((d, i) => {
    box(s, xs[i], 3.07, 3.70, 2.78, C.white, 0.19);
    pill(s, d[0], xs[i] + 2.96, 3.36, 0.45, C.pale, C.gray);
    if (i === 0) {
      rule(s, xs[i] + 0.32, 4.03, xs[i] + 3.18, 4.03, C.pale, 1.5);
      activity(s, xs[i] + 0.40, 3.50, 2.72, 0.72,
        [0.42,0.43,0.38,0.45,0.41,0.47,0.30,0.90,0.95,0.29,0.44,0.40,0.45,0.41], C.ink, 2.7);
      dot(s, xs[i] + 2.03, 3.58, 0.11, C.lime);
    } else if (i === 1) {
      for (let r = 0; r < 3; r++) {
        box(s, xs[i] + 0.47, 3.44 + r * 0.20, 1.48, 0.28, C.pale, 0.10);
        rule(s, xs[i] + 0.68, 3.55 + r * 0.20, xs[i] + 1.65, 3.55 + r * 0.20, C.gray, 1.2);
      }
      rule(s, xs[i] + 2.11, 3.89, xs[i] + 3.01, 3.89, C.ink, 1.7);
      dot(s, xs[i] + 2.55, 3.89, 0.13, C.lime);
      text(s, '7d', xs[i] + 2.47, 3.47, 0.65, 0.20, 11, C.gray, { bold: true });
    } else {
      const widths = [2.08, 1.51, 0.97];
      widths.forEach((width, r) => {
        circle(s, xs[i] + 0.42, 3.43 + r * 0.25, 0.14, r === 0 ? C.lime : C.pale);
        box(s, xs[i] + 0.70, 3.44 + r * 0.25, width, 0.11, r === 0 ? C.ink : C.mint, 0.05);
      });
    }
    text(s, d[1], xs[i] + 0.29, 4.32, 3.06, 0.40, 22, C.ink, { bold: true });
    text(s, d[2], xs[i] + 0.29, 4.90, 3.05, 0.81, 14, C.gray, { valign: 'top' });
    if (i < 2) {
      circle(s, xs[i] + 3.79, 4.16, 0.28, C.lime);
      text(s, '>', xs[i] + 3.89, 4.20, 0.11, 0.12, 10, C.ink, { bold: true });
    }
  });
  text(s, 'Default: 1 sample/sec  •  7 days of history  •  1-in-10-second history after 24 hours', 0.70, 6.26, 11.9, 0.32, 13, C.gray);
  notes(s, 'README.md “bb run”, “How much it costs”, “Where your data lives”, “What bb why can explain”; implementation in bb-core/src/{sampler,sensors,store,rules}.rs. Defaults can be configured.');
}

// 5 — diagnostic breadth
{
  const s = base('diagnostic coverage');
  heading(s, 'Built for real slowdowns', 'Find the bottleneck, not just a high number.', 'Rules account for sustained pressure, brief events, and sensors that vary by machine.');
  const items = [
    ['CPU', 'Busy programs, saturation, short spikes', 'cpu'],
    ['MEMORY', 'Pressure and paging, short squeezes', 'memory'],
    ['DISK', 'Heavy activity, slow requests, stalls', 'disk'],
    ['POWER + GPU', 'Heat, throttling, battery and GPU load', 'power'],
  ];
  items.forEach((d, i) => {
    const x = 0.68 + (i % 2) * 6.13, y = 3.03 + Math.floor(i / 2) * 1.60;
    box(s, x, y, 5.78, 1.30, C.white, 0.17);
    circle(s, x + 0.26, y + 0.28, 0.65, i === 3 ? C.mint : C.lime);
    sensorIcon(s, d[2], x + 0.35, y + 0.40);
    text(s, d[0], x + 1.17, y + 0.27, 4.26, 0.32, 17, C.ink, { bold: true });
    text(s, d[1], x + 1.17, y + 0.75, 4.26, 0.30, 13, C.gray);
  });
  pill(s, 'NO MATCH? IT SAYS “NO CLEAR CAUSE.”', 0.70, 6.55, 3.45, C.pale);
  notes(s, 'README.md “What bb why can explain” and “Limits”. Some sensors and corresponding rules vary by OS/hardware. This is a capability map, not a promise that every cause is detectable on every system.');
}

// 6 — positioning
{
  const s = base('why blackbox');
  heading(s, 'A different layer', 'From telemetry to an answer.', 'Blackbox complements live monitors and trace tools with a continuous, retrospective explanation.');
  const rows = [
    ['LIVE MONITOR', 'What is using resources right now?', 'Numbers in the moment'],
    ['RAW TRACE', 'What events happened in this capture?', 'Deep investigation'],
    ['BLACKBOX', 'Why was this machine slow then?', 'Ranked cause + supporting evidence'],
  ];
  rows.forEach((d, i) => {
    const y = 2.97 + i * 1.07, dark = i === 2;
    box(s, 0.68, y, 11.98, 0.87, dark ? C.ink : C.white, 0.14);
    text(s, d[0], 0.98, y + 0.29, 2.05, 0.24, 13, dark ? C.lime : C.gray, { bold: true, charSpacing: 0.7 });
    text(s, d[1], 3.09, y + 0.25, 5.75, 0.31, 17, dark ? C.white : C.ink, { bold: dark });
    text(s, d[2], 9.11, y + 0.26, 3.08, 0.31, 13, dark ? C.muted : C.gray, { align: 'right' });
  });
  notes(s, 'Positioning based on README.md opening and product behavior. This is a qualitative jobs-to-be-done comparison, not a benchmark or claim that existing tools lack all retrospective features.');
}

// 7 — trust and evidence
{
  const s = base('evidence & trust', true);
  heading(s, 'Early validation', 'Designed to earn trust on your machine.', 'Local by default. Measurable overhead. A self-test that creates real slowdowns.', true);
  const stats = [
    ['4 / 4', 'induced slowdowns detected', 'Windows 11 laptop self-test'],
    ['0', 'false alarms in the control', 'same documented self-test'],
    ['~2%', 'of one CPU core', 'Windows 11 laptop at 1 sample/sec'],
  ];
  stats.forEach((d, i) => {
    const x = 0.68 + i * 4.14;
    box(s, x, 3.07, 3.76, 2.18, C.near, 0.18);
    text(s, d[0], x + 0.28, 3.36, 3.2, 0.69, 44, C.lime, { bold: true });
    text(s, d[1], x + 0.28, 4.22, 3.15, 0.28, 16, C.white, { bold: true });
    text(s, d[2], x + 0.28, 4.73, 3.15, 0.23, 11, C.muted);
    if (i === 0) {
      for (let j = 0; j < 4; j++) dot(s, x + 2.66 + (j % 2) * 0.23, 3.50 + Math.floor(j / 2) * 0.23, 0.12, C.lime);
    } else if (i === 1) {
      s.addShape(S.ellipse, { x: x + 2.70, y: 3.45, w: 0.34, h: 0.34,
        line: { color: C.muted, width: 2.3 }, fill: { color: C.near, transparency: 100 } });
    } else {
      rule(s, x + 2.62, 3.83, x + 3.17, 3.83, C.muted, 2.6);
      rule(s, x + 2.62, 3.83, x + 2.76, 3.83, C.lime, 3.6);
    }
  });
  text(s, 'All recordings stay local. Program names and resource usage only — no window titles, file names, keystrokes or screen content.', 0.70, 5.74, 11.8, 0.64, 17, C.white);
  text(s, 'Validation is early: heat, GPU throttle, battery causes and true machine stalls are not yet end-to-end verified.', 0.70, 6.57, 11.8, 0.28, 11.5, C.muted);
  notes(s, 'README.md “bb selftest”, “How much it costs”, “Where your data lives”, “Limits”. Windows result is induced CPU hog, CPU burst, heavy disk and memory pressure; 0 false alarms on quiet control; repeated runs are documented. Benchmark ~2% one core and ~38MB on a Windows 11 laptop, machine dependent. No broad accuracy claim is intended.');
}

// 8 — status / future
{
  const s = base('today & next');
  heading(s, 'Shipping now', 'A working recorder, with a clear next step.', 'Version 0.4 is available for Windows, Linux and macOS; sensor depth differs by platform.');
  box(s, 0.68, 2.99, 5.74, 3.16, C.ink, 0.18);
  text(s, 'TODAY', 1.00, 3.29, 1.68, 0.25, 12, C.lime, { bold: true, charSpacing: 1.0 });
  box(s, 6.81, 2.99, 5.85, 3.16, C.white, 0.18);
  text(s, 'NEXT', 7.13, 3.29, 1.68, 0.25, 12, C.gray, { bold: true, charSpacing: 1.0 });
  const current = ['Install & auto-start', 'Local history & bb why', 'Self-test & feedback', 'Windows / Linux / macOS'];
  const next = ['Windows Event Log correlation', 'Network + per-program GPU', 'Shareable HTML report', 'Code signing + winget listing'];
  current.forEach((label, i) => {
    const y = 3.79 + i * 0.44;
    dot(s, 1.11, y + 0.15, 0.12, C.lime);
    text(s, label, 1.39, y, 4.56, 0.32, 18, C.white);
  });
  next.forEach((label, i) => {
    const y = 3.79 + i * 0.44;
    s.addShape(S.ellipse, { x: 7.07, y: y + 0.09, w: 0.12, h: 0.12,
      line: { color: C.gray, width: 1.4 }, fill: { color: C.white, transparency: 100 } });
    text(s, label, 7.41, y, 4.94, 0.32, 18, C.ink);
  });
  text(s, 'Windows has the broadest sensor coverage; on other platforms unsupported rules are skipped.', 0.70, 6.52, 11.9, 0.31, 13, C.gray);
  notes(s, 'README.md “Status”, “Install”, “Limits”, “Roadmap”. README labels v0.4 now; Cargo package version is 0.4.1. Future features are explicitly marked next.');
}

// 9 — close
{
  const s = base('get started', true);
  text(s, 'The next time it feels slow,\nknow why.', 0.67, 1.35, 11.82, 1.85, 46, C.white, { bold: true, valign: 'top' });
  text(s, '1  Install   →   2  Start recording   →   3  Ask bb why now', 0.70, 3.89, 11.8, 0.55, 20, C.lime, { bold: true });
  box(s, 0.68, 5.09, 11.98, 0.88, C.near, 0.14);
  text(s, 'github.com/AnakinSkywalker0/blackbox', 1.00, 5.37, 10.88, 0.32, 20, C.white, { bold: true, hyperlink: { url: 'https://github.com/AnakinSkywalker0/blackbox' } });
  text(s, 'Open source  /  MIT license', 0.70, 6.35, 7.5, 0.30, 13, C.muted);
  notes(s, 'README.md “Quick start”, “Install”, “License”. Visit repository for platform-specific install instructions and caveats.');
}

pptx.writeFile({ fileName: path.join(__dirname, 'blackbox-pitch-deck-illustrated.pptx') });
