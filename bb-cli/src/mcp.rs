//! `bb mcp serve`: a Model Context Protocol server over stdio, so an AI agent can ask
//! blackbox why a build or command failed.
//!
//! It is a thin adapter. Every answer comes from the same code the CLI uses
//! (`diagnose_text`, `diagnose_failure`, `analyze`), and it is read-only: the database
//! is opened with SQLite's read-only flag, so it can't be changed through this server
//! even by a bug. It runs no fixes.
//!
//! Protocol: newline-delimited JSON-RPC 2.0. It speaks the current stateless revision
//! (2026-07-28: version and capabilities in each request's `_meta`, `server/discover`)
//! and, because most clients in use still do, the older `initialize` handshake.

use crate::failures::{diagnose_failure, findings_lines};
use bb_core::failure::{Failure, TextVerdict};
use bb_core::model::Finding;
use bb_core::rules::{analyze, Thresholds};
use bb_core::store::Store;
use chrono::{Local, TimeZone};
use serde_json::{json, Map, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

type Res = Result<(), String>;

pub const LATEST: &str = "2026-07-28";
/// Older revisions that use the `initialize` handshake, newest first.
pub const LEGACY: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// One request per line; anything longer is refused rather than buffered.
const MAX_LINE: usize = 1024 * 1024;

const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CAPS: &str = "io.modelcontextprotocol/clientCapabilities";
const META_SERVER: &str = "io.modelcontextprotocol/serverInfo";

const INSTRUCTIONS: &str = "blackbox records a computer's vitals and failed shell commands. Use `recent_failures` to see what failed lately, `why` to diagnose a failure or a moment in time, `explain` to diagnose error text you already have, and `selftest_stats` for how accurate its diagnoses are. Everything is read-only.";

pub struct Ctx {
    pub db: PathBuf,
}

struct RpcError {
    code: i64,
    message: String,
    data: Option<Value>,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> RpcError {
        RpcError { code, message: message.into(), data: None }
    }
}

fn server_info() -> Value {
    json!({ "name": "blackbox", "version": env!("CARGO_PKG_VERSION") })
}

// ---- tools ---------------------------------------------------------------------------------

fn tool_defs() -> Value {
    let read_only = json!({ "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false });
    json!([
        {
            "name": "recent_failures",
            "title": "Recent failed commands",
            "description": "List the most recent shell commands that failed on this machine (newest first), as recorded by the blackbox shell hook or `bb watch`. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": { "limit": { "type": "integer", "minimum": 1, "maximum": 50, "default": 10, "description": "How many to return" } },
                "additionalProperties": false
            },
            "annotations": read_only
        },
        {
            "name": "why",
            "title": "Why did it fail or slow down",
            "description": "Diagnose in plain English. With `failure_id` or `latest_failure`, explains that failed command (from its captured output when there is any, else its exit code) and says whether the machine was also struggling while it ran. Otherwise explains why the machine was slow around `when`, and lists failed commands in that window. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "failure_id": { "type": "integer", "description": "Id from recent_failures" },
                    "latest_failure": { "type": "boolean", "description": "Diagnose the most recent failed command" },
                    "when": { "type": "string", "default": "now", "description": "now, HH:MM, HH:MM:SS or '10m ago'. Ignored with failure_id or latest_failure" },
                    "span": { "type": "string", "default": "4m", "description": "Total window analysed, centred on `when` (30s, 90s, 10m...)" }
                },
                "additionalProperties": false
            },
            "annotations": read_only
        },
        {
            "name": "explain",
            "title": "Explain error text",
            "description": "Diagnose error text you already have (build output, a stack trace, a log tail) using the same rules as `why`. Says so when nothing in it looks like an error, or when it doesn't recognise the error. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "The error text. Only the last 256 KB is searched" },
                    "exit_code": { "type": "integer", "description": "The exit code the command returned, if known" }
                },
                "required": ["text"],
                "additionalProperties": false
            },
            "annotations": read_only
        },
        {
            "name": "selftest_stats",
            "title": "Diagnosis accuracy",
            "description": "How accurate blackbox's diagnoses were the last time `bb selftest` was run on this machine: detection accuracy, top-explanation accuracy and false-alarm rate. Read-only.",
            "inputSchema": { "type": "object", "additionalProperties": false },
            "annotations": read_only
        }
    ])
}

struct ToolOut {
    text: String,
    structured: Value,
}

fn open_ro(ctx: &Ctx) -> Result<Store, String> {
    Store::open_read_only(&ctx.db).map_err(|e| {
        format!("can't read the blackbox database at {}: {e}. blackbox creates it when `bb run` or `bb hook install` is first used.", ctx.db.display())
    })
}

fn finding_json(f: &Finding) -> Value {
    json!({ "title": f.title, "confidence": f.confidence.label(), "evidence": f.evidence, "hint": f.hint })
}

fn findings_json(fs: &[Finding]) -> Value {
    Value::Array(fs.iter().map(finding_json).collect())
}

fn time_of(ts: i64) -> Value {
    Local.timestamp_opt(ts, 0).single().map_or(Value::Null, |t| Value::String(t.to_rfc3339()))
}

fn failure_json(f: &Failure) -> Value {
    json!({
        "id": f.id, "ts": f.ts, "time": time_of(f.ts), "exit_code": f.exit_code, "command": f.command,
        "cwd": f.cwd, "duration_ms": f.duration_ms, "output_captured": f.output.is_some(),
    })
}

/// Rejects arguments the tool doesn't define, so a typo is an error, not silently ignored.
fn check_keys(args: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    match args.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(format!("unknown argument \"{k}\". This tool takes: {}", if allowed.is_empty() { "no arguments".to_string() } else { allowed.join(", ") })),
        None => Ok(()),
    }
}

fn arg_int(args: &Map<String, Value>, key: &str, min: i64, max: i64) -> Result<Option<i64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_i64() {
            Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
            _ => Err(format!("\"{key}\" must be an integer from {min} to {max}")),
        },
    }
}

fn arg_str<'a>(args: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(format!("\"{key}\" must be a string")),
    }
}

fn tool_recent_failures(args: &Map<String, Value>, ctx: &Ctx) -> Result<ToolOut, String> {
    check_keys(args, &["limit"])?;
    let limit = arg_int(args, "limit", 1, 50)?.unwrap_or(10) as u32;
    let store = open_ro(ctx)?;
    let rows = if store.has_failures().map_err(|e| e.to_string())? { store.recent_failures(limit).map_err(|e| e.to_string())? } else { Vec::new() };
    let text = if rows.is_empty() {
        "No failed commands have been recorded.".to_string()
    } else {
        rows.iter()
            .map(|f| {
                let when = Local.timestamp_opt(f.ts, 0).single().map_or_else(|| f.ts.to_string(), |t| t.format("%d %b %H:%M:%S").to_string());
                let kind = if f.output.is_some() { "" } else { " (exit code only)" };
                format!("#{} {} exit {}  {}{kind}", f.id, when, f.exit_code, f.command.lines().next().unwrap_or(""))
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(ToolOut { text, structured: json!({ "failures": rows.iter().map(failure_json).collect::<Vec<_>>() }) })
}

fn tool_why(args: &Map<String, Value>, ctx: &Ctx) -> Result<ToolOut, String> {
    check_keys(args, &["failure_id", "latest_failure", "when", "span"])?;
    let id = arg_int(args, "failure_id", 1, i64::MAX)?;
    let latest = match args.get("latest_failure") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err("\"latest_failure\" must be true or false".into()),
    };
    if id.is_some() && latest {
        return Err("give either failure_id or latest_failure, not both".into());
    }
    let store = open_ro(ctx)?;
    if id.is_some() || latest {
        return why_failure(&store, id);
    }
    why_window(&store, arg_str(args, "when")?.unwrap_or("now"), arg_str(args, "span")?.unwrap_or("4m"))
}

fn why_failure(store: &Store, id: Option<i64>) -> Result<ToolOut, String> {
    if !store.has_failures().map_err(|e| e.to_string())? {
        return Err("No failed commands have been recorded.".into());
    }
    let f = match id {
        Some(id) => store.failure(id).map_err(|e| e.to_string())?.ok_or_else(|| format!("No failed command has id {id}. Use recent_failures to list them."))?,
        None => store.recent_failures(1).map_err(|e| e.to_string())?.into_iter().next().ok_or("No failed commands have been recorded.")?,
    };
    let d = diagnose_failure(store, &f);
    let mut lines = vec![format!("#{} exit {}  {}", f.id, f.exit_code, f.command.lines().next().unwrap_or(""))];
    lines.extend(d.lines.iter().map(|l| format!("  {l}")));
    if !d.context.is_empty() {
        lines.push("  The machine was also struggling while it ran:".into());
        lines.extend(findings_lines(&d.context).into_iter().map(|l| format!("  {l}")));
    }
    Ok(ToolOut {
        text: lines.join("\n"),
        structured: json!({ "failure": failure_json(&f), "diagnosis": { "lines": d.lines, "findings": findings_json(&d.findings) }, "machine_context": findings_json(&d.context) }),
    })
}

fn why_window(store: &Store, when: &str, span: &str) -> Result<ToolOut, String> {
    let (from, to) = crate::why_window(when, span, Local::now()).map_err(|e| format!("can't read the time: {e}"))?;
    let samples = store.window(from, to).map_err(|e| e.to_string())?;
    let causes = if samples.is_empty() { Vec::new() } else { analyze(&samples, store.best_mhz().map_err(|e| e.to_string())?, &Thresholds::default()) };
    let failures = if store.has_failures().map_err(|e| e.to_string())? { store.failures_between(from, to).map_err(|e| e.to_string())? } else { Vec::new() };

    let mut lines = Vec::new();
    if samples.is_empty() {
        lines.push("Nothing was recorded then.".to_string());
    } else {
        lines.push(format!("Window: {} to {}  ({} samples)", crate::fmt_time(from), crate::fmt_time(to), samples.len()));
        if causes.is_empty() {
            lines.push("No clear cause. CPU, memory, disk, GPU, heat and power all looked normal in this window.".into());
        } else {
            lines.push("Most likely causes:".into());
            lines.extend(findings_lines(&causes));
        }
    }
    let mut failed = Vec::new();
    if !failures.is_empty() {
        lines.push("Failed commands in this window:".into());
    }
    for f in &failures {
        let d = diagnose_failure(store, f);
        lines.push(format!("  #{} {}  exit {}  {}", f.id, crate::fmt_time(f.ts), f.exit_code, f.command.lines().next().unwrap_or("")));
        lines.extend(d.lines.iter().map(|l| format!("    {l}")));
        failed.push(json!({ "failure": failure_json(f), "diagnosis": { "lines": d.lines, "findings": findings_json(&d.findings) }, "machine_context": findings_json(&d.context) }));
    }
    Ok(ToolOut {
        text: lines.join("\n"),
        structured: json!({ "window": { "from": from, "to": to, "samples": samples.len() }, "causes": findings_json(&causes), "failures": failed }),
    })
}

fn verdict_name(v: &TextVerdict) -> &'static str {
    match v {
        TextVerdict::Empty => "empty",
        TextVerdict::Binary => "binary",
        TextVerdict::NoError => "no_error",
        TextVerdict::Unrecognised { .. } => "unrecognised",
        TextVerdict::Findings(_) => "findings",
    }
}

fn tool_explain(args: &Map<String, Value>, _ctx: &Ctx) -> Result<ToolOut, String> {
    check_keys(args, &["text", "exit_code"])?;
    let text = arg_str(args, "text")?.ok_or("\"text\" is required")?;
    let exit = arg_int(args, "exit_code", i32::MIN as i64, i32::MAX as i64)?.map(|n| n as i32);
    let verdict = crate::explain::diagnose_bytes(text.as_bytes(), exit);
    let findings = match &verdict {
        TextVerdict::Findings(f) => f.clone(),
        _ => Vec::new(),
    };
    let lines = crate::explain::report(text.as_bytes(), exit);
    Ok(ToolOut { text: lines.join("\n"), structured: json!({ "verdict": verdict_name(&verdict), "findings": findings_json(&findings), "lines": lines }) })
}

fn tool_selftest_stats(args: &Map<String, Value>, ctx: &Ctx) -> Result<ToolOut, String> {
    check_keys(args, &[])?;
    let path = crate::selftest::stats_path(&ctx.db);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Ok(ToolOut {
            text: "`bb selftest` has not been run on this machine, so there are no accuracy numbers yet.".into(),
            structured: json!({ "available": false }),
        });
    };
    let mut v: Value = serde_json::from_str(&raw).map_err(|e| format!("the saved selftest results are unreadable ({e}). Run `bb selftest` again."))?;
    let pct = |k: &str| v[k].as_f64().map_or("not measured".to_string(), |p| format!("{p:.0}%"));
    let text = format!(
        "Last `bb selftest` (v{}): detection accuracy {} ({} of {}), top-explanation accuracy {}, false-alarm rate {} ({} of {} quiet runs).",
        v["version"].as_str().unwrap_or("?"),
        pct("detection_pct"),
        v["detected"],
        v["judged"],
        pct("top_explanation_pct"),
        pct("false_alarm_pct"),
        v["false_alarms"],
        v["controls_judged"],
    );
    if let Some(o) = v.as_object_mut() {
        o.insert("available".into(), json!(true));
    }
    Ok(ToolOut { text, structured: v })
}

fn call_tool(name: &str, args: &Map<String, Value>, ctx: &Ctx) -> Option<Result<ToolOut, String>> {
    Some(match name {
        "recent_failures" => tool_recent_failures(args, ctx),
        "why" => tool_why(args, ctx),
        "explain" => tool_explain(args, ctx),
        "selftest_stats" => tool_selftest_stats(args, ctx),
        _ => return None,
    })
}

// ---- protocol ------------------------------------------------------------------------------

fn unsupported(requested: &Value) -> RpcError {
    let mut supported = vec![LATEST];
    supported.extend(LEGACY);
    RpcError { code: -32022, message: "Unsupported protocol version".into(), data: Some(json!({ "supported": supported, "requested": requested })) }
}

/// Whether this request uses the stateless revision (`Ok(true)`), or is from an older
/// client with no per-request metadata (`Ok(false)`).
fn era_of(params: &Value) -> Result<bool, RpcError> {
    let Some(ver) = params.get("_meta").and_then(|m| m.get(META_VERSION)) else {
        return Ok(false);
    };
    let Some(v) = ver.as_str() else { return Err(RpcError::new(-32602, format!("{META_VERSION} must be a string"))) };
    if v != LATEST && !LEGACY.contains(&v) {
        return Err(unsupported(ver));
    }
    if v == LATEST && !params["_meta"][META_CAPS].is_object() {
        return Err(RpcError::new(-32602, format!("Invalid params: _meta must include {META_CAPS}")));
    }
    Ok(v == LATEST)
}

/// Adds what the stateless revision requires of every result.
fn complete(mut result: Value, modern: bool) -> Value {
    if modern {
        if let Some(o) = result.as_object_mut() {
            o.insert("resultType".into(), json!("complete"));
            let meta = o.entry("_meta").or_insert_with(|| json!({}));
            if let Some(m) = meta.as_object_mut() {
                m.insert(META_SERVER.into(), server_info());
            }
        }
    }
    result
}

fn dispatch(method: &str, params: &Value, ctx: &Ctx) -> Result<Value, RpcError> {
    let modern = era_of(params)?;
    match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
            let version = if LEGACY.contains(&asked) { asked } else { LEGACY[0] };
            Ok(json!({ "protocolVersion": version, "capabilities": { "tools": {} }, "serverInfo": server_info(), "instructions": INSTRUCTIONS }))
        }
        "server/discover" => {
            let mut supported = vec![LATEST];
            supported.extend(LEGACY);
            Ok(complete(json!({ "supportedVersions": supported, "capabilities": { "tools": {} }, "instructions": INSTRUCTIONS }), true))
        }
        "ping" => Ok(complete(json!({}), modern)),
        "tools/list" => Ok(complete(json!({ "tools": tool_defs() }), modern)),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).ok_or_else(|| RpcError::new(-32602, "Invalid params: \"name\" (a string) is required"))?;
            let empty = Map::new();
            let args = match params.get("arguments") {
                None | Some(Value::Null) => &empty,
                Some(Value::Object(o)) => o,
                Some(_) => return Err(RpcError::new(-32602, "Invalid params: \"arguments\" must be an object")),
            };
            let out = call_tool(name, args, ctx).ok_or_else(|| RpcError::new(-32602, format!("Unknown tool: {name}")))?;
            let result = match out {
                Ok(t) => json!({ "content": [{ "type": "text", "text": t.text }], "structuredContent": t.structured, "isError": false }),
                Err(msg) => json!({ "content": [{ "type": "text", "text": msg }], "isError": true }),
            };
            Ok(complete(result, modern))
        }
        other => Err(RpcError::new(-32601, format!("Method not found: {other}"))),
    }
}

fn error_response(id: &Value, e: RpcError) -> String {
    let mut err = json!({ "code": e.code, "message": e.message });
    if let Some(d) = e.data {
        err["data"] = d;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": err }).to_string()
}

/// One incoming line to one outgoing line, or `None` for a notification (which gets no reply).
pub fn handle_line(line: &str, ctx: &Ctx) -> Option<String> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Some(error_response(&Value::Null, RpcError::new(-32700, "Parse error: not valid JSON")));
    };
    let Some(obj) = v.as_object() else {
        return Some(error_response(&Value::Null, RpcError::new(-32600, "Invalid Request: expected a single JSON object (batches are not supported)")));
    };
    let method = obj.get("method").and_then(Value::as_str);
    if method.is_none() && (obj.contains_key("result") || obj.contains_key("error")) {
        return None; // a reply to something we never asked
    }
    let id = match obj.get("id") {
        None => None,
        Some(i @ Value::String(_)) => Some(i.clone()),
        Some(i @ Value::Number(n)) if n.is_i64() || n.is_u64() => Some(i.clone()),
        Some(_) => return Some(error_response(&Value::Null, RpcError::new(-32600, "Invalid Request: id must be a string or an integer"))),
    };
    if obj.get("jsonrpc") != Some(&json!("2.0")) || method.is_none() {
        return Some(error_response(id.as_ref().unwrap_or(&Value::Null), RpcError::new(-32600, "Invalid Request: needs \"jsonrpc\": \"2.0\" and a string \"method\"")));
    }
    let Some(id) = id else { return None }; // a notification
    let params = obj.get("params").cloned().unwrap_or_else(|| json!({}));
    if !params.is_object() {
        return Some(error_response(&id, RpcError::new(-32602, "Invalid params: expected an object")));
    }
    Some(match dispatch(method.unwrap_or(""), &params, ctx) {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string(),
        Err(e) => error_response(&id, e),
    })
}

// ---- stdio ---------------------------------------------------------------------------------

enum Line {
    Eof,
    Text(String),
    TooLong,
}

/// Reads one line without ever holding more than `max` bytes of it.
fn read_line_bounded<R: BufRead>(r: &mut R, max: usize) -> std::io::Result<Line> {
    let mut buf: Vec<u8> = Vec::new();
    let mut too_long = false;
    let mut any = false;
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        any = true;
        let (take, done) = match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        if !too_long {
            if buf.len() + take > max + 1 {
                too_long = true;
                buf.clear();
            } else {
                buf.extend_from_slice(&chunk[..take]);
            }
        }
        r.consume(take);
        if done {
            break;
        }
    }
    if !any {
        return Ok(Line::Eof);
    }
    if too_long {
        return Ok(Line::TooLong);
    }
    Ok(Line::Text(String::from_utf8_lossy(&buf).trim_end_matches(['\n', '\r']).to_string()))
}

/// The server loop over any reader and writer. One reply line per request line, in order.
pub fn serve_io<R: BufRead, W: Write>(mut input: R, mut out: W, ctx: &Ctx, max_line: usize) -> std::io::Result<()> {
    loop {
        let reply = match read_line_bounded(&mut input, max_line)? {
            Line::Eof => return Ok(()),
            Line::TooLong => Some(error_response(&Value::Null, RpcError::new(-32600, "Invalid Request: the message is too large"))),
            Line::Text(t) if t.trim().is_empty() => None,
            Line::Text(t) => handle_line(&t, ctx),
        };
        if let Some(r) = reply {
            writeln!(out, "{r}")?;
            out.flush()?;
        }
    }
}

pub fn serve(db: &Path) -> Res {
    let ctx = Ctx { db: db.to_path_buf() };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve_io(stdin.lock(), stdout.lock(), &ctx, MAX_LINE).map_err(|e| format!("mcp: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bb_core::model::{ProcRow, Sample};

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bb-mcp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A database with a busy-CPU minute ending now and three failures: one with captured
    /// output, one with only an exit code, and one older one.
    fn fixture(tag: &str) -> (PathBuf, Ctx) {
        let dir = tmp(tag);
        let db = dir.join("bb.db");
        let mut s = Store::open(&db).unwrap();
        let now = Local::now().timestamp();
        let samples: Vec<Sample> = (0..120)
            .map(|i| Sample {
                ts: now - 120 + i,
                cpu_pct: 97.0,
                mem_used: 4 << 30,
                mem_total: 16 << 30,
                clock_mhz: 3000,
                procs: vec![ProcRow { name: "chrome.exe".into(), count: 10, cpu_pct: 80.0, disk_bps: 0, mem_bytes: 1 << 30 }],
                ..Default::default()
            })
            .collect();
        s.insert_many(&samples).unwrap();
        let mk = |ts: i64, cmd: &str, code: i32, out: Option<&str>| Failure { id: 0, ts, command: cmd.into(), exit_code: code, cwd: None, output: out.map(str::to_string), duration_ms: Some(2000) };
        s.insert_failure(&mk(now - 3600, "npm install", 1, Some("npm ERR! EACCES: permission denied"))).unwrap();
        s.insert_failure(&mk(now - 60, "cargo build", 101, Some("error: linker `link.exe` not found"))).unwrap();
        s.insert_failure(&mk(now - 30, "make", 127, None)).unwrap();
        drop(s);
        (dir, Ctx { db })
    }

    fn call(ctx: &Ctx, req: Value) -> Value {
        serde_json::from_str(&handle_line(&req.to_string(), ctx).expect("a reply")).unwrap()
    }

    fn modern_meta() -> Value {
        json!({ META_VERSION: LATEST, META_CAPS: {} })
    }

    fn tool(ctx: &Ctx, name: &str, args: Value) -> Value {
        call(ctx, json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": args } }))
    }

    fn text_of(r: &Value) -> String {
        r["result"]["content"][0]["text"].as_str().unwrap().to_string()
    }

    #[test]
    fn legacy_initialize_negotiates_a_version_and_declares_only_tools() {
        let (d, ctx) = fixture("init");
        let r = call(&ctx, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } } }));
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(r["result"]["serverInfo"]["name"], "blackbox");
        assert_eq!(r["result"]["capabilities"], json!({ "tools": {} }));
        assert!(r["result"].get("resultType").is_none(), "legacy replies stay legacy-shaped");
        let r = call(&ctx, json!({ "jsonrpc": "2.0", "id": "x", "method": "initialize", "params": { "protocolVersion": "1999-01-01" } }));
        assert_eq!(r["id"], "x");
        assert_eq!(r["result"]["protocolVersion"], LEGACY[0]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn discover_and_modern_requests_follow_the_stateless_revision() {
        let (d, ctx) = fixture("modern");
        let r = call(&ctx, json!({ "jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": { "_meta": modern_meta() } }));
        assert_eq!(r["result"]["resultType"], "complete");
        assert_eq!(r["result"]["supportedVersions"][0], LATEST);
        assert_eq!(r["result"]["_meta"][META_SERVER]["name"], "blackbox");
        let r = call(&ctx, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": { "_meta": modern_meta() } }));
        assert_eq!(r["result"]["resultType"], "complete");
        let r = call(&ctx, json!({ "jsonrpc": "2.0", "id": 3, "method": "ping", "params": { "_meta": modern_meta() } }));
        assert_eq!(r["result"]["resultType"], "complete");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unsupported_or_incomplete_modern_requests_get_the_defined_errors() {
        let (d, ctx) = fixture("errs");
        let r = call(&ctx, json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": { "_meta": { META_VERSION: "1900-01-01", META_CAPS: {} } } }));
        assert_eq!(r["error"]["code"], -32022);
        assert_eq!(r["error"]["data"]["requested"], "1900-01-01");
        assert!(r["error"]["data"]["supported"].as_array().unwrap().contains(&json!(LATEST)));
        let r = call(&ctx, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": { "_meta": { META_VERSION: LATEST } } }));
        assert_eq!(r["error"]["code"], -32602);
        let r = call(&ctx, json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": { "_meta": { META_VERSION: 5 } } }));
        assert_eq!(r["error"]["code"], -32602);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_tool_list_is_four_read_only_tools_with_valid_names_and_schemas() {
        let (d, ctx) = fixture("list");
        let r = call(&ctx, json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }));
        let tools = r["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["recent_failures", "why", "explain", "selftest_stats"]);
        for t in tools {
            let n = t["name"].as_str().unwrap();
            assert!(n.len() <= 128 && n.chars().all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c)));
            assert_eq!(t["inputSchema"]["type"], "object");
            assert_eq!(t["annotations"]["readOnlyHint"], true);
            assert_eq!(t["annotations"]["destructiveHint"], false);
            assert!(t["description"].as_str().unwrap().contains("Read-only"));
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn notifications_and_stray_replies_get_no_response() {
        let (d, ctx) = fixture("notif");
        assert!(handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, &ctx).is_none());
        assert!(handle_line(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#, &ctx).is_none());
        assert!(handle_line(r#"{"jsonrpc":"2.0","id":7,"result":{}}"#, &ctx).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn malformed_requests_get_clean_protocol_errors() {
        let (d, ctx) = fixture("bad");
        let code = |line: &str| -> i64 {
            let v: Value = serde_json::from_str(&handle_line(line, &ctx).unwrap()).unwrap();
            assert_eq!(v["jsonrpc"], "2.0");
            v["error"]["code"].as_i64().unwrap()
        };
        assert_eq!(code("{not json"), -32700);
        assert_eq!(code("[]"), -32600);
        assert_eq!(code("42"), -32600);
        assert_eq!(code(r#"{"jsonrpc":"2.0","id":1}"#), -32600);
        assert_eq!(code(r#"{"id":1,"method":"ping"}"#), -32600);
        assert_eq!(code(r#"{"jsonrpc":"2.0","id":true,"method":"ping"}"#), -32600);
        assert_eq!(code(r#"{"jsonrpc":"2.0","id":1.5,"method":"ping"}"#), -32600);
        assert_eq!(code(r#"{"jsonrpc":"2.0","id":1,"method":"nope/nothing"}"#), -32601);
        assert_eq!(code(r#"{"jsonrpc":"2.0","id":1,"method":"resources/list"}"#), -32601);
        assert_eq!(code(r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":[1]}"#), -32602);
        assert_eq!(code(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#), -32602);
        assert_eq!(code(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":5}}"#), -32602);
        assert_eq!(code(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"why","arguments":[]}}"#), -32602);
        assert_eq!(code(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"delete_everything"}}"#), -32602);
        // The error for a bad id is sent with a null id, and a bad method keeps its id.
        let v: Value = serde_json::from_str(&handle_line(r#"{"jsonrpc":"2.0","id":9,"method":"x"}"#, &ctx).unwrap()).unwrap();
        assert_eq!(v["id"], 9);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn recent_failures_lists_newest_first() {
        let (d, ctx) = fixture("recent");
        let r = tool(&ctx, "recent_failures", json!({}));
        assert_eq!(r["result"]["isError"], false);
        let fs = r["result"]["structuredContent"]["failures"].as_array().unwrap();
        assert_eq!(fs.iter().map(|f| f["command"].as_str().unwrap()).collect::<Vec<_>>(), ["make", "cargo build", "npm install"]);
        assert_eq!(fs[0]["output_captured"], false);
        assert_eq!(fs[1]["output_captured"], true);
        assert!(text_of(&r).contains("exit 101"));
        let r = tool(&ctx, "recent_failures", json!({ "limit": 1 }));
        assert_eq!(r["result"]["structuredContent"]["failures"].as_array().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn why_diagnoses_the_latest_failure_a_chosen_one_and_a_window() {
        let (d, ctx) = fixture("why");
        // Latest is `make`, exit 127, no output: only the exit code speaks, and the busy minute shows.
        let r = tool(&ctx, "why", json!({ "latest_failure": true }));
        assert_eq!(r["result"]["isError"], false);
        let t = text_of(&r);
        assert!(t.contains("make") && t.contains("A program or command was not found (exit 127)"), "{t}");
        assert!(t.contains("The machine was also struggling"), "{t}");
        // By id: the linker error is named from its captured output.
        let r = tool(&ctx, "why", json!({ "failure_id": 2 }));
        assert!(text_of(&r).contains("The C/C++ linker is missing"));
        assert_eq!(r["result"]["structuredContent"]["diagnosis"]["findings"][0]["confidence"], "high");
        // The recent window has the CPU hog and the two recent failures, not the old one.
        let r = tool(&ctx, "why", json!({ "when": "now", "span": "5m" }));
        let sc = &r["result"]["structuredContent"];
        assert!(sc["causes"][0]["title"].as_str().unwrap().contains("chrome"), "{sc}");
        assert_eq!(sc["failures"].as_array().unwrap().len(), 2);
        assert_eq!(sc["window"]["samples"], 120);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn explain_gives_the_same_answers_as_the_cli() {
        let (d, ctx) = fixture("explain");
        let r = tool(&ctx, "explain", json!({ "text": "bind: address already in use", "exit_code": 1 }));
        assert_eq!(r["result"]["structuredContent"]["verdict"], "findings");
        assert!(text_of(&r).contains("A network port is already in use"));
        assert_eq!(text_of(&r).lines().collect::<Vec<_>>(), crate::explain::report(b"bind: address already in use", Some(1)));
        for (text, verdict) in [("", "empty"), ("all good, 12 passed", "no_error"), ("abc\u{0}def", "binary")] {
            let r = tool(&ctx, "explain", json!({ "text": text }));
            assert_eq!(r["result"]["structuredContent"]["verdict"], verdict, "{text:?}");
        }
        let r = tool(&ctx, "explain", json!({ "text": "weird failure", "exit_code": 4 }));
        assert_eq!(r["result"]["structuredContent"]["verdict"], "unrecognised");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn selftest_stats_reports_saved_scores_or_says_there_are_none() {
        let (d, ctx) = fixture("stats");
        let r = tool(&ctx, "selftest_stats", json!({}));
        assert_eq!(r["result"]["structuredContent"]["available"], false);
        assert!(text_of(&r).contains("has not been run"));
        std::fs::write(
            crate::selftest::stats_path(&ctx.db),
            r#"{"version":"0.5.1","detected":3,"ranked_first":2,"judged":4,"controls_judged":1,"false_alarms":0,"detection_pct":75.0,"top_explanation_pct":50.0,"false_alarm_pct":0.0}"#,
        )
        .unwrap();
        let r = tool(&ctx, "selftest_stats", json!({}));
        assert_eq!(r["result"]["structuredContent"]["available"], true);
        assert!(text_of(&r).contains("detection accuracy 75% (3 of 4)") && text_of(&r).contains("false-alarm rate 0%"));
        std::fs::write(crate::selftest::stats_path(&ctx.db), "{broken").unwrap();
        assert_eq!(tool(&ctx, "selftest_stats", json!({}))["result"]["isError"], true);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bad_tool_arguments_are_tool_errors_the_model_can_read() {
        let (d, ctx) = fixture("args");
        for (name, args) in [
            ("recent_failures", json!({ "limit": 0 })),
            ("recent_failures", json!({ "limit": "ten" })),
            ("recent_failures", json!({ "lmit": 3 })),
            ("why", json!({ "failure_id": 1, "latest_failure": true })),
            ("why", json!({ "failure_id": 999 })),
            ("why", json!({ "when": "not a time" })),
            ("why", json!({ "latest_failure": "yes" })),
            ("explain", json!({})),
            ("explain", json!({ "text": 5 })),
            ("selftest_stats", json!({ "x": 1 })),
        ] {
            let r = tool(&ctx, name, args.clone());
            assert_eq!(r["result"]["isError"], true, "{name} {args}");
            assert!(!text_of(&r).is_empty());
            assert!(r.get("error").is_none(), "tool errors are results, not protocol errors");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_missing_database_is_a_readable_tool_error_and_is_not_created() {
        let dir = tmp("nodb");
        let ctx = Ctx { db: dir.join("nope").join("bb.db") };
        let r = tool(&ctx, "recent_failures", json!({}));
        assert_eq!(r["result"]["isError"], true);
        assert!(text_of(&r).contains("can't read the blackbox database"));
        assert!(!ctx.db.exists() && !ctx.db.parent().unwrap().exists());
        // explain and selftest_stats need no database.
        assert_eq!(tool(&ctx, "explain", json!({ "text": "ENOENT" }))["result"]["isError"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_tool_leaves_the_database_exactly_as_it_was() {
        let (d, ctx) = fixture("readonly");
        let before = std::fs::read(&ctx.db).unwrap();
        for (name, args) in [
            ("recent_failures", json!({})),
            ("why", json!({})),
            ("why", json!({ "latest_failure": true })),
            ("why", json!({ "failure_id": 1 })),
            ("explain", json!({ "text": "permission denied" })),
            ("selftest_stats", json!({})),
        ] {
            tool(&ctx, name, args);
        }
        assert_eq!(std::fs::read(&ctx.db).unwrap(), before, "the main database file must not change");
        let s = Store::open_read_only(&ctx.db).unwrap();
        assert_eq!(s.recent_failures(10).unwrap().len(), 3);
        assert_eq!(s.stats().unwrap().samples, 120);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn concurrent_calls_during_writes_never_corrupt_the_store() {
        let (d, ctx) = fixture("conc");
        let ctx = std::sync::Arc::new(ctx);
        let writer_db = ctx.db.clone();
        let writer = std::thread::spawn(move || {
            let s = Store::open(&writer_db).unwrap();
            for i in 0..60 {
                s.insert_failure(&Failure { id: 0, ts: Local::now().timestamp(), command: format!("job {i}"), exit_code: 1, cwd: None, output: Some("ENOSPC".into()), duration_ms: None }).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        });
        let readers: Vec<_> = (0..6)
            .map(|n| {
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    for i in 0..25 {
                        let name = ["recent_failures", "why", "explain", "selftest_stats"][(n + i) % 4];
                        let args = match name {
                            "explain" => json!({ "text": "permission denied" }),
                            "why" => json!({ "latest_failure": true }),
                            _ => json!({}),
                        };
                        let r: Value = serde_json::from_str(&handle_line(&json!({ "jsonrpc": "2.0", "id": i, "method": "tools/call", "params": { "name": name, "arguments": args } }).to_string(), &ctx).unwrap()).unwrap();
                        assert_eq!(r["id"], i);
                        assert!(r["error"].is_null(), "{r}");
                        assert_eq!(r["result"]["isError"], false, "{name}: {r}");
                    }
                })
            })
            .collect();
        for r in readers {
            r.join().unwrap();
        }
        writer.join().unwrap();
        // Every write landed, the file is sound, and the original data is intact.
        let s = Store::open(&ctx.db).unwrap();
        assert_eq!(s.recent_failures(1000).unwrap().len(), 63);
        assert_eq!(s.stats().unwrap().samples, 120);
        let ok: String = rusqlite_integrity(&ctx.db);
        assert_eq!(ok, "ok");
        let _ = std::fs::remove_dir_all(&d);
    }

    fn rusqlite_integrity(db: &Path) -> String {
        // bb-cli has no rusqlite dependency; ask SQLite through the store's own connection API
        // by reading every table back, which fails loudly on a damaged file.
        let s = Store::open_read_only(db).unwrap();
        s.integrity_check().unwrap()
    }

    #[test]
    fn the_stdio_loop_replies_once_per_request_in_order_and_survives_garbage() {
        let (d, ctx) = fixture("stdio");
        let input = [
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
            "",
            "garbage",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        ]
        .join("\r\n");
        let mut out = Vec::new();
        serve_io(std::io::Cursor::new(input.into_bytes()), &mut out, &ctx, MAX_LINE).unwrap();
        let lines: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["id"], 1);
        assert_eq!(lines[1]["error"]["code"], -32700);
        assert_eq!(lines[2]["id"], 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_oversized_line_is_refused_without_buffering_it_and_the_next_line_still_works() {
        let (d, ctx) = fixture("big");
        let mut input = "x".repeat(5000);
        input.push('\n');
        input.push_str(r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#);
        input.push('\n');
        let mut out = Vec::new();
        serve_io(std::io::Cursor::new(input.into_bytes()), &mut out, &ctx, 1000).unwrap();
        let lines: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["error"]["code"], -32600);
        assert_eq!(lines[1]["id"], 3);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_final_line_without_a_newline_is_still_answered() {
        let (d, ctx) = fixture("nonl");
        let mut out = Vec::new();
        serve_io(std::io::Cursor::new(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#.to_vec()), &mut out, &ctx, MAX_LINE).unwrap();
        assert_eq!(String::from_utf8(out).unwrap().lines().count(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }
}
