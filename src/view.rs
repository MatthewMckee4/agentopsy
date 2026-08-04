use std::cmp::Reverse;
use std::fmt::{self, Write};
use std::path::Path;

use crate::trace::{Dashboard, Diagnostics, Operation, Session, Turn};

pub fn render(dashboard: &Dashboard) -> Result<String, fmt::Error> {
    let mut html = String::from(HEAD);
    let total_active = dashboard
        .sessions
        .iter()
        .map(|session| session.active_duration_ms)
        .sum();
    let total_tool = dashboard
        .sessions
        .iter()
        .map(|session| session.tool_duration_ms)
        .sum();
    let total_model = dashboard
        .sessions
        .iter()
        .map(|session| session.model_duration_ms)
        .sum();
    let open_sessions = dashboard
        .sessions
        .iter()
        .filter(|session| session.status.label() == "open")
        .count();
    let anomalies = dashboard
        .sessions
        .iter()
        .map(|session| anomaly_count(&session.diagnostics))
        .sum::<usize>()
        + dashboard.scan_errors.len();

    write!(
        html,
        r#"<header class="hero">
<div><p class="eyebrow">LOCAL TRACE LAB <span class="pulse"></span> LIVE</p><h1>Agent<span>opsy</span></h1><p class="tagline">See what your agent is doing.</p></div>
<div class="scan"><span>Scanned {}</span><span>{} ms · {} cached · {} parsed</span></div>
</header>
<main>
<section class="metrics" aria-label="Summary">
<div class="metric primary"><span>Active-turn time</span><strong>{}</strong><small>gaps excluded</small></div>
<div class="metric model"><span>Model time</span><strong>{}</strong><small>estimated</small></div>
<div class="metric tool"><span>Tool execution</span><strong>{}</strong><small>timestamp-derived</small></div>
<div class="metric"><span>Sessions</span><strong>{}</strong><small>{} open turns · {} anomalies</small></div>
</section>"#,
        escape(&dashboard.scanned_at.to_string()),
        dashboard.scan_duration_ms,
        dashboard.cache_hits,
        dashboard.cache_misses,
        duration(total_active),
        duration(total_model),
        duration(total_tool),
        dashboard.sessions.len(),
        open_sessions,
        anomalies,
    )?;

    render_split_bar(&mut html, total_model, total_tool)?;
    render_slowest(&mut html, dashboard)?;
    render_scan_errors(&mut html, &dashboard.scan_errors)?;

    write!(
        html,
        r#"<section class="sessions-head"><div><p class="eyebrow">RANKED BY ACTIVE DURATION</p><h2>Sessions</h2></div><div class="controls"><label class="search"><span>Filter</span><input id="filter" type="search" placeholder="cwd, prompt, model, tool…" autocomplete="off"></label><label class="live"><input id="live" type="checkbox" checked> Auto-refresh <span id="countdown">10s</span></label><button id="refresh" type="button">Refresh now</button></div></section>
<section id="sessions" class="session-list">"#,
    )?;

    for (index, session) in dashboard.sessions.iter().enumerate() {
        render_session(&mut html, session, index + 1, index == 0)?;
    }
    if dashboard.sessions.is_empty() {
        html.push_str(
            r#"<div class="empty"><strong>No Codex sessions found.</strong><span>Expected JSONL traces under ~/.codex/sessions.</span></div>"#,
        );
    }

    html.push_str(FOOT);
    Ok(html)
}

fn render_split_bar(html: &mut String, model_ms: u64, tool_ms: u64) -> fmt::Result {
    let total = model_ms.saturating_add(tool_ms);
    let model_width = percent(model_ms, total);
    let tool_width = 10_000_u64.saturating_sub(model_width);
    write!(
        html,
        r#"<section class="split-card"><div class="section-label"><span>Observed work split</span><span><i class="dot model-dot"></i>Model estimated <i class="dot tool-dot"></i>Tools</span></div><div class="split"><span class="model-fill" style="width:{}.{:02}%"></span><span class="tool-fill" style="width:{}.{:02}%"></span></div></section>"#,
        model_width / 100,
        model_width % 100,
        tool_width / 100,
        tool_width % 100,
    )
}

fn render_slowest(html: &mut String, dashboard: &Dashboard) -> fmt::Result {
    let mut operations = dashboard
        .sessions
        .iter()
        .flat_map(|session| {
            session
                .operations
                .iter()
                .filter_map(move |operation| operation.duration_ms.map(|_| (session, operation)))
        })
        .collect::<Vec<_>>();
    operations.sort_by_key(|(_, operation)| Reverse(operation.duration_ms));
    operations.truncate(5);

    html.push_str(
        r#"<section class="slowest panel"><div class="panel-head"><div><p class="eyebrow">GLOBAL</p><h2>Five slowest operations</h2></div><span class="hint">call/output timestamps</span></div>"#,
    );
    if operations.is_empty() {
        html.push_str(r#"<p class="empty-inline">No matched tool operations.</p>"#);
    } else {
        html.push_str(
            r#"<div class="table-wrap"><table><thead><tr><th>Tool</th><th>Session</th><th>Duration</th><th>Status</th><th>Command preview</th></tr></thead><tbody>"#,
        );
        for (session, operation) in operations {
            render_operation_row(html, operation, Some(session))?;
        }
        html.push_str("</tbody></table></div>");
    }
    html.push_str("</section>");
    Ok(())
}

fn render_scan_errors(html: &mut String, errors: &[String]) -> fmt::Result {
    if errors.is_empty() {
        return Ok(());
    }
    write!(
        html,
        r#"<details class="scan-errors panel"><summary>{} trace scan errors</summary><ul>"#,
        errors.len()
    )?;
    for error in errors {
        write!(html, "<li>{}</li>", escape(error))?;
    }
    html.push_str("</ul></details>");
    Ok(())
}

fn render_session(html: &mut String, session: &Session, rank: usize, open: bool) -> fmt::Result {
    let project = project_name(&session.cwd);
    let short_id = session.id.chars().take(8).collect::<String>();
    let search = format!(
        "{} {} {} {} {} {} {}",
        session.cwd,
        session.prompt,
        session.model,
        session.effort,
        session.source,
        session.trace_path,
        session
            .operations
            .iter()
            .map(|operation| operation.name.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    );
    let open_attribute = if open { " open" } else { "" };
    write!(
        html,
        r#"<article class="session" data-search="{}"><details data-session-key="{}"{}><summary><span class="rank">#{rank:02}</span><span class="session-title"><strong>{}</strong><code>{}</code><small>{}</small></span><span class="session-summary"><span class="status {}">{}</span><strong>{}</strong><small>{} turns · {} tools</small></span><span class="chevron">⌄</span></summary><div class="session-body">"#,
        escape_attribute(&search.to_lowercase()),
        escape_attribute(&session.id),
        open_attribute,
        escape(&project),
        escape(&short_id),
        escape(if session.prompt.is_empty() {
            "No user prompt recorded"
        } else {
            &session.prompt
        }),
        session.status.label(),
        session.status.label(),
        duration(session.active_duration_ms),
        session.turns.len(),
        session.operations.len(),
    )?;

    render_session_metrics(html, session)?;
    render_metadata(html, session)?;
    render_timeline(html, session)?;
    render_session_operations(html, session)?;
    render_diagnostics(html, session)?;
    html.push_str("</div></details></article>");
    Ok(())
}

fn render_session_metrics(html: &mut String, session: &Session) -> fmt::Result {
    write!(
        html,
        r#"<div class="session-metrics"><div><span>Model</span><strong>{}</strong><small>estimated</small></div><div><span>Tools</span><strong>{}</strong><small>timestamp-derived</small></div><div><span>Wall span</span><strong>{}</strong><small>includes gaps</small></div><div><span>Last activity</span><strong class="timestamp">{}</strong><small>UTC</small></div></div>"#,
        duration(session.model_duration_ms),
        duration(session.tool_duration_ms),
        duration(session.wall_duration_ms),
        timestamp(session.last_activity.as_ref()),
    )
}

fn render_metadata(html: &mut String, session: &Session) -> fmt::Result {
    html.push_str(r#"<div class="metadata">"#);
    metadata_item(html, "cwd", &session.cwd)?;
    metadata_item(html, "model", &joined(&session.model, &session.effort))?;
    metadata_item(
        html,
        "source",
        &joined(&session.source, &session.originator),
    )?;
    metadata_item(html, "Codex", &session.cli_version)?;
    metadata_item(html, "started", &timestamp(session.started_at.as_ref()))?;
    metadata_item(html, "session", &session.id)?;
    html.push_str("</div>");
    Ok(())
}

fn metadata_item(html: &mut String, label: &str, value: &str) -> fmt::Result {
    if value.is_empty() {
        return Ok(());
    }
    write!(
        html,
        r"<span><b>{}</b>{}</span>",
        escape(label),
        escape(value)
    )
}

fn render_timeline(html: &mut String, session: &Session) -> fmt::Result {
    html.push_str(
        r#"<section class="timeline-block"><div class="subhead"><div><h3>Activity timeline</h3><p>Each row is one active turn. Time between rows is excluded.</p></div><div class="legend"><span><i class="dot model-dot"></i>Model estimated</span><span><i class="dot tool-dot"></i>Tool timestamp-derived</span></div></div><div class="turns">"#,
    );
    for (index, turn) in session.turns.iter().enumerate() {
        render_turn(html, turn, index + 1)?;
    }
    if session.turns.is_empty() {
        html.push_str(r#"<p class="empty-inline">No turn timing markers.</p>"#);
    }
    html.push_str("</div></section>");
    Ok(())
}

fn render_turn(html: &mut String, turn: &Turn, number: usize) -> fmt::Result {
    write!(
        html,
        r#"<div class="turn"><div class="turn-label"><span>Turn {number}</span><small>{} · {}</small></div><div class="track" title="Model estimated: {}; tools: {}">"#,
        duration(turn.duration_ms),
        turn.status.label(),
        duration(turn.duration_ms.saturating_sub(turn.tool_duration_ms)),
        duration(turn.tool_duration_ms),
    )?;
    for segment in &turn.tool_segments {
        let left = percent(segment.offset_ms, turn.duration_ms);
        let width = percent(segment.duration_ms, turn.duration_ms).max(1);
        write!(
            html,
            r#"<span class="tool-segment" title="{} · {}" style="left:{}.{:02}%;width:{}.{:02}%"></span>"#,
            escape_attribute(&segment.names),
            duration(segment.duration_ms),
            left / 100,
            left % 100,
            width / 100,
            width % 100,
        )?;
    }
    html.push_str("</div></div>");
    Ok(())
}

fn render_session_operations(html: &mut String, session: &Session) -> fmt::Result {
    let mut operations = session.operations.iter().collect::<Vec<_>>();
    operations.sort_by_key(|operation| Reverse(operation.duration_ms));
    operations.truncate(5);
    html.push_str(
        r#"<section class="operations"><div class="subhead"><div><h3>Slowest operations</h3><p>Matched strictly by <code>call_id</code>.</p></div></div>"#,
    );
    if operations.is_empty() {
        html.push_str(r#"<p class="empty-inline">No tool calls recorded.</p>"#);
    } else {
        html.push_str(
            r#"<div class="table-wrap"><table><thead><tr><th>Tool</th><th>Duration</th><th>Status</th><th>Command preview</th><th>Call ID</th></tr></thead><tbody>"#,
        );
        for operation in operations {
            render_operation_row(html, operation, None)?;
        }
        html.push_str("</tbody></table></div>");
    }
    html.push_str("</section>");
    Ok(())
}

fn render_operation_row(
    html: &mut String,
    operation: &Operation,
    session: Option<&Session>,
) -> fmt::Result {
    write!(
        html,
        r#"<tr><td><code class="tool-name">{}</code></td>"#,
        escape(&operation.name)
    )?;
    if let Some(session) = session {
        write!(
            html,
            r#"<td><span class="project-name">{}</span><code>{}</code></td>"#,
            escape(&project_name(&session.cwd)),
            escape(&session.id.chars().take(8).collect::<String>()),
        )?;
    }
    write!(
        html,
        r#"<td class="duration">{}</td><td><span class="status {}">{}</span></td><td><code class="command" title="{}">{}</code></td>"#,
        operation
            .duration_ms
            .map_or_else(|| "—".to_owned(), duration),
        operation.status.label(),
        operation.status.label(),
        escape_attribute(if operation.preview.is_empty() {
            "No arguments recorded"
        } else {
            &operation.preview
        }),
        escape(if operation.preview.is_empty() {
            "No arguments recorded"
        } else {
            &operation.preview
        }),
    )?;
    if session.is_none() {
        write!(
            html,
            r#"<td><code title="{}">{}</code></td>"#,
            escape_attribute(&operation.call_id),
            escape(&operation.call_id.chars().take(12).collect::<String>()),
        )?;
    }
    html.push_str("</tr>");
    Ok(())
}

fn render_diagnostics(html: &mut String, session: &Session) -> fmt::Result {
    let diagnostics = &session.diagnostics;
    html.push_str(r#"<details class="diagnostics"><summary>Full trace diagnostics</summary><div class="diagnostic-grid">"#);
    diagnostic(html, "Events", diagnostics.event_count)?;
    diagnostic(html, "Trace bytes", diagnostics.trace_bytes)?;
    diagnostic(html, "Matched calls", diagnostics.matched_calls)?;
    diagnostic(html, "Unmatched calls", diagnostics.unmatched_calls)?;
    diagnostic(html, "Unmatched outputs", diagnostics.unmatched_outputs)?;
    diagnostic(html, "Unassigned calls", diagnostics.unassigned_calls)?;
    diagnostic(html, "Duplicate calls", diagnostics.duplicate_call_ids)?;
    diagnostic(html, "Duplicate outputs", diagnostics.duplicate_output_ids)?;
    diagnostic(html, "Missing call IDs", diagnostics.missing_call_ids)?;
    diagnostic(html, "Missing output IDs", diagnostics.missing_output_ids)?;
    diagnostic(html, "Invalid timestamps", diagnostics.invalid_timestamps)?;
    diagnostic(html, "Inferred turns", diagnostics.inferred_turns)?;
    write!(
        html,
        r"<div><span>Parallel overlap</span><strong>{}</strong></div>",
        duration(diagnostics.overlapping_tool_ms)
    )?;
    html.push_str("</div><h4>Event inventory</h4><div class=event-inventory>");
    for (event, count) in &diagnostics.event_counts {
        write!(
            html,
            r"<span><code>{}</code><b>{count}</b></span>",
            escape(event)
        )?;
    }
    html.push_str("</div>");
    if !diagnostics.parse_errors.is_empty() {
        html.push_str("<h4>Parse errors</h4><ul class=parse-errors>");
        for error in &diagnostics.parse_errors {
            write!(html, "<li>{}</li>", escape(error))?;
        }
        html.push_str("</ul>");
    }
    write!(
        html,
        r#"<p class="trace-path"><span>Trace</span><code>{}</code></p></details>"#,
        escape(&session.trace_path)
    )
}

fn diagnostic(html: &mut String, label: &str, value: impl fmt::Display) -> fmt::Result {
    write!(
        html,
        r"<div><span>{}</span><strong>{value}</strong></div>",
        escape(label)
    )
}

const fn anomaly_count(diagnostics: &Diagnostics) -> usize {
    diagnostics.parse_errors.len()
        + diagnostics.invalid_timestamps
        + diagnostics.unmatched_calls
        + diagnostics.unmatched_outputs
        + diagnostics.duplicate_call_ids
        + diagnostics.duplicate_output_ids
        + diagnostics.missing_call_ids
        + diagnostics.missing_output_ids
        + diagnostics.unassigned_calls
}

fn project_name(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("unknown workspace")
        .to_owned()
}

fn joined(left: &str, right: &str) -> String {
    match (left.is_empty(), right.is_empty()) {
        (false, false) => format!("{left} · {right}"),
        (false, true) => left.to_owned(),
        (true, false) => right.to_owned(),
        (true, true) => String::new(),
    }
}

fn duration(milliseconds: u64) -> String {
    if milliseconds < 1_000 {
        format!("{milliseconds} ms")
    } else if milliseconds < 60_000 {
        format!(
            "{}.{:01} s",
            milliseconds / 1_000,
            milliseconds % 1_000 / 100
        )
    } else if milliseconds < 3_600_000 {
        format!(
            "{}m {:02}s",
            milliseconds / 60_000,
            milliseconds % 60_000 / 1_000
        )
    } else {
        format!(
            "{}h {:02}m",
            milliseconds / 3_600_000,
            milliseconds % 3_600_000 / 60_000
        )
    }
}

fn timestamp(timestamp: Option<&jiff::Timestamp>) -> String {
    timestamp.map_or_else(|| "unknown".to_owned(), ToString::to_string)
}

fn percent(value: u64, total: u64) -> u64 {
    if total == 0 {
        0
    } else {
        value
            .saturating_mul(10_000)
            .saturating_div(total)
            .min(10_000)
    }
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn escape_attribute(value: &str) -> String {
    escape(value)
}

const HEAD: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="color-scheme" content="dark">
<title>Agentopsy — Codex trace diagnostics</title>
<style>
.status.open{color:var(--acid)}
:root{--bg:#090b0d;--panel:#101317;--panel-2:#15191e;--line:#252b32;--muted:#8b949e;--text:#edf1f5;--acid:#b8f35b;--blue:#77a8ff;--orange:#ff9f43;--red:#ff6b6b;--violet:#c6a0ff;--radius:16px}*{box-sizing:border-box}html{background:var(--bg)}body{margin:0;color:var(--text);background:radial-gradient(circle at 80% -20%,#1a2415 0,transparent 34rem),var(--bg);font:14px/1.5 Inter,ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}body:before{content:"";position:fixed;inset:0;pointer-events:none;opacity:.025;background-image:linear-gradient(#fff 1px,transparent 1px),linear-gradient(90deg,#fff 1px,transparent 1px);background-size:32px 32px}code,.duration,.rank,h1{font-family:"SFMono-Regular",Consolas,"Liberation Mono",monospace}header,main{width:min(1480px,calc(100% - 40px));margin:auto}.hero{display:flex;align-items:flex-end;justify-content:space-between;padding:64px 0 34px;border-bottom:1px solid var(--line)}.eyebrow{margin:0 0 7px;color:var(--muted);font:700 11px/1.2 "SFMono-Regular",monospace;letter-spacing:.16em}.pulse{display:inline-block;width:7px;height:7px;margin:0 3px;border-radius:50%;background:var(--acid);box-shadow:0 0 12px var(--acid)}h1{margin:0;font-size:clamp(42px,7vw,88px);line-height:.9;letter-spacing:-.08em}h1 span{color:var(--acid)}.tagline{margin:17px 0 0;color:#c5ccd3;font-size:17px}.scan{display:grid;gap:4px;text-align:right;color:var(--muted);font:12px/1.4 "SFMono-Regular",monospace}.scan span:first-child{color:var(--text)}main{padding:30px 0 80px}.metrics{display:grid;grid-template-columns:repeat(4,1fr);gap:12px}.metric{min-height:132px;padding:20px;border:1px solid var(--line);border-radius:var(--radius);background:linear-gradient(150deg,#15191e,#0e1114)}.metric span,.metric small{display:block;color:var(--muted)}.metric strong{display:block;margin:17px 0 3px;font:700 27px/1 "SFMono-Regular",monospace}.metric.primary strong{color:var(--acid)}.metric.model strong{color:var(--blue)}.metric.tool strong{color:var(--orange)}.split-card,.panel{margin-top:12px;padding:20px;border:1px solid var(--line);border-radius:var(--radius);background:rgba(16,19,23,.92)}.section-label,.panel-head,.subhead,.sessions-head{display:flex;align-items:flex-end;justify-content:space-between;gap:20px}.section-label{margin-bottom:12px;color:var(--muted);font-size:12px}.dot{display:inline-block;width:8px;height:8px;margin:0 6px 0 14px;border-radius:50%}.dot:first-child{margin-left:0}.model-dot{background:var(--blue)}.tool-dot{background:var(--orange)}.split{display:flex;height:12px;overflow:hidden;border-radius:99px;background:#20252b}.model-fill{background:linear-gradient(90deg,#5f8ee6,var(--blue))}.tool-fill{background:linear-gradient(90deg,var(--orange),#ffbc68)}.panel{margin-top:28px}.panel h2,.sessions-head h2{margin:0;font-size:24px;letter-spacing:-.03em}.hint{color:var(--muted);font:12px "SFMono-Regular",monospace}.table-wrap{margin-top:18px;overflow-x:auto}table{width:100%;border-collapse:collapse}th{padding:10px 12px;border-bottom:1px solid var(--line);color:var(--muted);font-size:11px;letter-spacing:.08em;text-align:left;text-transform:uppercase}td{padding:13px 12px;border-bottom:1px solid #1d2228;vertical-align:middle}tbody tr:last-child td{border:0}td code{font-size:12px}.tool-name{color:var(--violet)}.project-name{display:block;font-weight:650}.project-name+code{color:var(--muted)}.command{display:block;max-width:580px;overflow:hidden;color:#c9d1d9;text-overflow:ellipsis;white-space:nowrap}.status{display:inline-flex;padding:3px 8px;border:1px solid currentColor;border-radius:99px;color:var(--muted);font:700 10px "SFMono-Regular",monospace;letter-spacing:.06em;text-transform:uppercase}.status.active,.status.returned{color:var(--acid)}.status.complete{color:var(--blue)}.status.failed,.status.invalid{color:var(--red)}.status.pending,.status.inferred{color:var(--orange)}.sessions-head{margin:48px 0 16px}.controls{display:flex;align-items:flex-end;gap:10px}.search{display:grid;gap:5px;color:var(--muted);font-size:11px}.search input{width:300px}.search input,button{height:38px;border:1px solid var(--line);border-radius:9px;color:var(--text);background:#11151a;padding:0 12px;font:13px inherit}.search input:focus{border-color:var(--acid);outline:0;box-shadow:0 0 0 2px #b8f35b22}.live{display:flex;align-items:center;height:38px;color:var(--muted);font-size:12px}.live input{accent-color:var(--acid)}button{cursor:pointer}button:hover{border-color:#59616c}.session-list{display:grid;gap:10px}.session{border:1px solid var(--line);border-radius:var(--radius);background:rgba(16,19,23,.94);overflow:hidden}.session[hidden]{display:none}.session summary{display:grid;grid-template-columns:64px minmax(0,1fr) auto 20px;align-items:center;gap:18px;min-height:104px;padding:20px;cursor:pointer;list-style:none}.session summary::-webkit-details-marker{display:none}.session summary:hover{background:#14181d}.rank{color:#67717d;font-size:16px}.session-title{display:grid;grid-template-columns:auto 1fr;align-items:baseline;gap:10px;min-width:0}.session-title strong{overflow:hidden;font-size:18px;text-overflow:ellipsis;white-space:nowrap}.session-title code{color:var(--muted);font-size:11px}.session-title small{grid-column:1/-1;overflow:hidden;color:#9ba4ae;text-overflow:ellipsis;white-space:nowrap}.session-summary{display:grid;grid-template-columns:auto auto;align-items:center;justify-items:end;gap:5px 12px}.session-summary strong{font:700 19px "SFMono-Regular",monospace}.session-summary small{grid-column:1/-1;color:var(--muted)}.chevron{color:var(--muted);font-size:20px;transition:transform .15s}details[open]>summary .chevron{transform:rotate(180deg)}.session-body{padding:8px 20px 24px;border-top:1px solid var(--line);background:#0d1013}.session-metrics{display:grid;grid-template-columns:repeat(4,1fr);gap:1px;margin:12px 0 18px;overflow:hidden;border:1px solid var(--line);border-radius:12px;background:var(--line)}.session-metrics>div{padding:15px;background:#12161a}.session-metrics span,.session-metrics small{display:block;color:var(--muted);font-size:11px}.session-metrics strong{display:block;margin:6px 0 1px;font:650 17px "SFMono-Regular",monospace}.session-metrics .timestamp{font-size:12px}.metadata{display:flex;flex-wrap:wrap;gap:7px;margin-bottom:28px}.metadata span{max-width:100%;padding:5px 8px;border:1px solid #252b32;border-radius:7px;color:#adb6bf;background:#12161a;font:11px "SFMono-Regular",monospace;overflow-wrap:anywhere}.metadata b{margin-right:7px;color:#66717d;font-weight:500}.subhead h3{margin:0;font-size:16px}.subhead p{margin:3px 0 0;color:var(--muted);font-size:12px}.legend{display:flex;color:var(--muted);font-size:11px}.turns{display:grid;gap:9px;margin-top:16px}.turn{display:grid;grid-template-columns:175px 1fr;align-items:center;gap:15px}.turn-label{display:flex;justify-content:space-between;color:#c7ced5}.turn-label small{color:var(--muted)}.track{position:relative;height:19px;overflow:hidden;border:1px solid #2c3540;border-radius:6px;background:linear-gradient(90deg,#587fc4,#77a8ff)}.track:after{content:"";position:absolute;inset:0;background:linear-gradient(90deg,transparent 49.5%,#ffffff12 50%,transparent 50.5%);background-size:20% 100%}.tool-segment{position:absolute;z-index:1;top:0;bottom:0;min-width:2px;background:var(--orange);box-shadow:0 0 0 1px #2c1c0a}.operations{margin-top:32px}.diagnostics{margin-top:24px;border-top:1px solid var(--line);padding-top:18px}.diagnostics>summary,.scan-errors>summary{cursor:pointer;color:#b8c0c8;font-weight:650}.diagnostic-grid{display:grid;grid-template-columns:repeat(5,1fr);gap:8px;margin:16px 0}.diagnostic-grid div{padding:10px;border:1px solid var(--line);border-radius:8px;background:#11151a}.diagnostic-grid span,.diagnostic-grid strong{display:block}.diagnostic-grid span{color:var(--muted);font-size:10px}.diagnostic-grid strong{margin-top:4px;font:600 13px "SFMono-Regular",monospace}.diagnostics h4{margin:20px 0 9px;color:var(--muted);font-size:11px;letter-spacing:.08em;text-transform:uppercase}.event-inventory{display:flex;flex-wrap:wrap;gap:6px}.event-inventory span{display:flex;gap:8px;padding:4px 7px;border:1px solid var(--line);border-radius:6px;background:#11151a}.event-inventory code{color:#aeb6bf;font-size:10px}.event-inventory b{color:var(--acid);font:600 10px "SFMono-Regular",monospace}.trace-path{display:flex;gap:10px;margin:22px 0 0;color:var(--muted);font-size:11px}.trace-path code{overflow-wrap:anywhere}.parse-errors,.scan-errors ul{color:var(--red);font:11px "SFMono-Regular",monospace}.empty,.empty-inline{color:var(--muted)}.empty{display:grid;gap:4px;padding:50px;border:1px dashed var(--line);border-radius:var(--radius);text-align:center}.scan-errors{color:var(--red)}footer{display:flex;justify-content:space-between;margin-top:30px;padding-top:20px;border-top:1px solid var(--line);color:var(--muted);font-size:11px}footer code{color:#b7c0c9}
@media(max-width:900px){header,main{width:min(100% - 24px,1480px)}.hero{padding-top:40px}.metrics{grid-template-columns:repeat(2,1fr)}.sessions-head{align-items:flex-start;flex-direction:column}.controls{width:100%;flex-wrap:wrap}.search{flex:1}.search input{width:100%}.session summary{grid-template-columns:45px minmax(0,1fr) 20px}.session-summary{grid-column:2}.chevron{grid-column:3;grid-row:1/3}.session-metrics{grid-template-columns:repeat(2,1fr)}.turn{grid-template-columns:1fr}.diagnostic-grid{grid-template-columns:repeat(3,1fr)}}@media(max-width:560px){.hero{align-items:flex-start;flex-direction:column;gap:24px}.scan{text-align:left}.metrics{grid-template-columns:1fr}.metric{min-height:auto}.section-label,.panel-head,.subhead{align-items:flex-start;flex-direction:column}.session summary{padding:16px}.session-body{padding:6px 12px 20px}.session-metrics{grid-template-columns:1fr}.diagnostic-grid{grid-template-columns:repeat(2,1fr)}.legend{flex-direction:column}.controls button{width:100%}footer{flex-direction:column;gap:8px}}
</style>
</head>
<body>
"#;

const FOOT: &str = r"</section>
<footer><span>Local only · no uploads · no external requests</span><code>~/.codex/sessions/**/*.jsonl</code></footer>
</main>
<script>
const filter=document.querySelector('#filter');
const sessions=[...document.querySelectorAll('[data-search]')];
filter.addEventListener('input',()=>{const query=filter.value.trim().toLowerCase();for(const session of sessions)session.hidden=!session.dataset.search.includes(query)});
const details=[...document.querySelectorAll('[data-session-key]')];
const saved=JSON.parse(sessionStorage.getItem('agentopsy-open')||'null');
if(Array.isArray(saved)){for(const detail of details)detail.open=saved.includes(detail.dataset.sessionKey)}
for(const detail of details)detail.addEventListener('toggle',()=>sessionStorage.setItem('agentopsy-open',JSON.stringify(details.filter(item=>item.open).map(item=>item.dataset.sessionKey))));
document.querySelector('#refresh').addEventListener('click',()=>location.reload());
let remaining=10;setInterval(()=>{if(document.hidden||!document.querySelector('#live').checked)return;remaining-=1;if(remaining<=0)location.reload();document.querySelector('#countdown').textContent=remaining+'s'},1000);
</script>
</body>
</html>
";

#[cfg(test)]
mod tests {
    use super::{duration, escape};

    #[test]
    fn formats_duration_units() {
        assert_eq!(duration(925), "925 ms");
        assert_eq!(duration(1_250), "1.2 s");
        assert_eq!(duration(125_000), "2m 05s");
        assert_eq!(duration(7_500_000), "2h 05m");
    }

    #[test]
    fn escapes_html() {
        assert_eq!(
            escape("<tool a='b'>&\""),
            "&lt;tool a=&#39;b&#39;&gt;&amp;&quot;"
        );
    }
}
