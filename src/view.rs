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
        r##"<a class="skip-link" href="#main">Skip to dashboard</a>
<header class="app-header">
<div class="brand"><span class="brand-mark" aria-hidden="true">A</span><span><strong>Agentopsy</strong><small>Codex trace diagnostics</small></span></div>
<div class="scan"><span class="privacy-badge"><i aria-hidden="true"></i> Local session data</span><span>Scanned {}</span><span>{} ms · {} cached · {} parsed</span></div>
</header>
<main id="main">
<section class="page-heading"><div><p class="eyebrow">Overview</p><h1>Session activity</h1><p>Active-turn time excludes gaps. Tool time comes from matched call and output timestamps; model time is estimated.</p></div></section>
<section class="metrics" aria-label="Summary">
<div class="metric primary"><span>Active-turn time</span><strong>{}</strong><small>gaps excluded</small></div>
<div class="metric model"><span>Model time</span><strong>{}</strong><small>estimated</small></div>
<div class="metric tool"><span>Tool execution</span><strong>{}</strong><small>timestamp-derived</small></div>
<div class="metric"><span>Sessions</span><strong>{}</strong><small>{} open turns · {} anomalies</small></div>
</section>"##,
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
        r#"<section class="sessions-head"><div><p class="eyebrow">Ranked by active duration</p><h2>Sessions</h2></div><div class="controls"><label class="search" for="filter"><span>Filter sessions</span><input id="filter" type="search" placeholder="cwd, prompt, model, tool…" autocomplete="off"></label><label class="live" for="live"><input id="live" type="checkbox" checked> <span>Auto-refresh <b id="countdown">10s</b></span></label><button id="refresh" type="button">Refresh</button></div></section>
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
        r#"<section class="split-card"><div class="section-label"><div><strong>Observed work split</strong><small>Active-turn time only</small></div><span class="legend"><span><i class="dot model-dot"></i>Model estimated</span><span><i class="dot tool-dot"></i>Tools</span></span></div><div class="split" role="img" aria-label="Observed work split between estimated model time and timestamp-derived tool time"><span class="model-fill" style="width:{}.{:02}%"></span><span class="tool-fill" style="width:{}.{:02}%"></span></div></section>"#,
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
        r#"<section class="slowest panel"><div class="panel-head"><div><p class="eyebrow">Across all sessions</p><h2>Five slowest operations</h2></div><span class="hint">Call/output timestamps</span></div>"#,
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
        r#"<article class="session" data-search="{}"><details data-session-key="{}"{}><summary><span class="rank">{rank:02}</span><span class="session-title"><strong>{}</strong><code>{}</code><small>{}</small></span><span class="session-summary"><span class="status {}">{}</span><strong>{}</strong><small>{} turns · {} tools</small></span><span class="chevron" aria-hidden="true">›</span></summary><div class="session-body">"#,
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
        r#"<div class="turn"><div class="turn-label"><span>Turn {number}</span><small>{} · {}</small></div><div class="track" role="img" aria-label="Turn {number}: model estimated {}; tools {}" title="Model estimated: {}; tools: {}">"#,
        duration(turn.duration_ms),
        turn.status.label(),
        duration(turn.duration_ms.saturating_sub(turn.tool_duration_ms)),
        duration(turn.tool_duration_ms),
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
        r#"<tr><td class="operation-tool"><code class="tool-name">{}</code></td>"#,
        escape(&operation.name)
    )?;
    if let Some(session) = session {
        write!(
            html,
            r#"<td class="operation-session"><span class="project-name">{}</span><code>{}</code></td>"#,
            escape(&project_name(&session.cwd)),
            escape(&session.id.chars().take(8).collect::<String>()),
        )?;
    }
    write!(
        html,
        r#"<td class="duration operation-duration">{}</td><td class="operation-status"><span class="status {}">{}</span></td><td class="operation-command"><code class="command" title="{}">{}</code></td>"#,
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
            r#"<td class="operation-call-id"><code title="{}">{}</code></td>"#,
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
<meta name="color-scheme" content="light">
<title>Agentopsy — Codex trace diagnostics</title>
<style>
:root{--background:#f8f8f9;--foreground:#18181b;--card:#fff;--card-foreground:#18181b;--muted:#f4f4f5;--muted-foreground:#6b6b74;--accent:#f4f4f5;--accent-foreground:#18181b;--primary:#18181b;--primary-foreground:#fafafa;--secondary:#f4f4f5;--secondary-foreground:#27272a;--destructive:#b42318;--destructive-muted:#fef3f2;--warning:#854d0e;--warning-muted:#fefce8;--border:#e4e4e7;--input:#d4d4d8;--ring:#18181b;--chart-model:#52525b;--chart-tool:#b45309;--radius:10px;--shadow:0 1px 2px rgb(0 0 0/.03)}
*{box-sizing:border-box}html{color-scheme:light;background:var(--background);scroll-behavior:smooth}body{margin:0;color:var(--foreground);background:var(--background);font:14px/1.5 ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;-webkit-font-smoothing:antialiased}button,input{font:inherit}code,.duration,.rank{font-family:"SFMono-Regular",Consolas,"Liberation Mono",monospace}code{font-size:.9em}.skip-link{position:fixed;top:8px;left:8px;transform:translateY(-150%);padding:8px 12px;border-radius:7px;color:var(--primary-foreground);background:var(--primary);z-index:2}.skip-link:focus{transform:none}.app-header,main{width:min(1280px,calc(100% - 40px));margin:auto}.app-header{display:flex;align-items:center;justify-content:space-between;min-height:64px;border-bottom:1px solid var(--border)}.brand{display:flex;align-items:center;gap:10px}.brand-mark{display:grid;width:32px;height:32px;place-items:center;border-radius:8px;color:var(--primary-foreground);background:var(--primary);font-weight:700}.brand>span:last-child{display:grid;line-height:1.25}.brand strong{font-size:14px}.brand small{color:var(--muted-foreground);font-size:11px}.scan{display:flex;align-items:center;gap:12px;color:var(--muted-foreground);font-size:11px}.privacy-badge{display:inline-flex;align-items:center;gap:6px;padding:4px 8px;border:1px solid var(--border);border-radius:999px;color:var(--secondary-foreground);background:var(--card);font-weight:500}.privacy-badge i{width:6px;height:6px;border-radius:50%;background:var(--primary)}main{padding:32px 0 64px}.page-heading{display:flex;align-items:flex-start;justify-content:space-between;gap:24px;margin-bottom:24px}.eyebrow{margin:0 0 4px;color:var(--muted-foreground);font-size:12px;font-weight:500}.page-heading h1{margin:0;font-size:28px;line-height:1.2;letter-spacing:-.025em}.page-heading p:last-child{max-width:700px;margin:8px 0 0;color:var(--muted-foreground)}.metrics{display:grid;grid-template-columns:repeat(4,1fr);gap:12px}.metric,.split-card,.panel,.session{border:1px solid var(--border);border-radius:var(--radius);background:var(--card);box-shadow:var(--shadow)}.metric{display:flex;min-height:126px;flex-direction:column;padding:18px}.metric span,.metric small{display:block;color:var(--muted-foreground)}.metric span{font-size:12px;font-weight:500}.metric strong{display:block;margin:auto 0 2px;font-size:25px;line-height:1.15;letter-spacing:-.025em}.metric small{font-size:11px}.split-card,.panel{margin-top:12px;padding:18px}.section-label,.panel-head,.subhead,.sessions-head{display:flex;align-items:flex-end;justify-content:space-between;gap:20px}.section-label{margin-bottom:12px}.section-label>div{display:grid;gap:1px}.section-label strong{font-size:13px}.section-label small,.legend,.hint{color:var(--muted-foreground);font-size:11px}.legend{display:flex;gap:14px}.legend span{display:flex;align-items:center;gap:6px}.dot{display:inline-block;width:7px;height:7px;border-radius:50%}.model-dot{background:var(--chart-model)}.tool-dot{background:var(--chart-tool)}.split{display:flex;height:8px;overflow:hidden;border-radius:999px;background:var(--muted)}.model-fill{background:var(--chart-model)}.tool-fill{background:var(--chart-tool)}.panel{margin-top:24px}.panel h2,.sessions-head h2{margin:0;font-size:20px;line-height:1.3;letter-spacing:-.02em}.table-wrap{margin:16px -18px -18px;overflow-x:auto;border-top:1px solid var(--border);border-radius:0 0 var(--radius) var(--radius)}table{width:100%;border-collapse:collapse;white-space:nowrap}th{padding:9px 14px;border-bottom:1px solid var(--border);color:var(--muted-foreground);background:var(--muted);font-size:10px;font-weight:600;letter-spacing:.06em;text-align:left;text-transform:uppercase}td{padding:11px 14px;border-bottom:1px solid var(--border);vertical-align:middle}tbody tr:last-child td{border-bottom:0}tbody tr:hover{background:var(--accent)}td code{font-size:11px}.tool-name{color:var(--card-foreground);font-weight:600}.project-name{display:block;font-weight:600}.project-name+code{color:var(--muted-foreground)}.command{display:block;max-width:500px;overflow:hidden;color:var(--muted-foreground);text-overflow:ellipsis;white-space:nowrap}.status{display:inline-flex;align-items:center;padding:2px 7px;border:1px solid var(--border);border-radius:999px;color:var(--secondary-foreground);background:var(--secondary);font-size:9px;font-weight:700;letter-spacing:.05em;text-transform:uppercase}.status.failed,.status.invalid{border-color:var(--destructive-muted);color:var(--destructive);background:var(--destructive-muted)}.status.pending,.status.inferred{border-color:var(--warning-muted);color:var(--warning);background:var(--warning-muted)}.sessions-head{margin:36px 0 12px}.controls{display:flex;align-items:flex-end;gap:8px}.search{display:grid;gap:5px;color:var(--muted-foreground);font-size:11px;font-weight:500}.search input{width:280px}.search input,button{height:36px;border:1px solid var(--input);border-radius:8px;padding:0 11px;color:var(--foreground);background:var(--card)}.search input::placeholder{color:var(--muted-foreground)}.search input:focus-visible,button:focus-visible,summary:focus-visible{outline:2px solid var(--ring);outline-offset:2px}.live{display:flex;align-items:center;gap:7px;height:36px;padding:0 4px;color:var(--muted-foreground);font-size:11px;white-space:nowrap}.live input{width:15px;height:15px;margin:0;accent-color:var(--primary)}.live b{color:var(--foreground);font-weight:500}button{border-color:var(--primary);color:var(--primary-foreground);background:var(--primary);font-size:12px;font-weight:500;cursor:pointer}button:hover{background:var(--secondary-foreground)}.session-list{display:grid;gap:8px}.session{overflow:hidden}.session[hidden]{display:none}.session summary{display:grid;grid-template-columns:36px minmax(0,1fr) auto 18px;align-items:center;gap:14px;min-height:86px;padding:14px 16px;cursor:pointer;list-style:none}.session summary::-webkit-details-marker{display:none}.session summary:hover{background:var(--accent)}.rank{display:grid;width:30px;height:30px;place-items:center;border-radius:7px;color:var(--muted-foreground);background:var(--muted);font-size:11px}.session-title{display:grid;grid-template-columns:auto 1fr;align-items:baseline;gap:8px;min-width:0}.session-title strong{overflow:hidden;font-size:15px;text-overflow:ellipsis;white-space:nowrap}.session-title code{color:var(--muted-foreground);font-size:10px}.session-title small{grid-column:1/-1;overflow:hidden;color:var(--muted-foreground);font-size:12px;text-overflow:ellipsis;white-space:nowrap}.session-summary{display:grid;grid-template-columns:auto auto;align-items:center;justify-items:end;gap:4px 10px}.session-summary strong{font-size:16px;font-weight:650}.session-summary small{grid-column:1/-1;color:var(--muted-foreground);font-size:11px}.chevron{color:var(--muted-foreground);font-size:22px;line-height:1;transition:transform .15s}details[open]>summary .chevron{transform:rotate(90deg)}.session-body{padding:18px;border-top:1px solid var(--border);background:var(--background)}.session-metrics{display:grid;grid-template-columns:repeat(4,1fr);gap:8px;margin:0 0 14px}.session-metrics>div{padding:12px;border:1px solid var(--border);border-radius:8px;background:var(--card)}.session-metrics span,.session-metrics small{display:block;color:var(--muted-foreground);font-size:10px}.session-metrics strong{display:block;margin:4px 0 1px;font-size:14px;font-weight:650}.session-metrics .timestamp{font-size:11px}.metadata{display:flex;flex-wrap:wrap;gap:6px;margin-bottom:24px}.metadata span{max-width:100%;padding:4px 7px;border:1px solid var(--border);border-radius:6px;color:var(--secondary-foreground);background:var(--card);font:10px "SFMono-Regular",Consolas,monospace;overflow-wrap:anywhere}.metadata b{margin-right:6px;color:var(--muted-foreground);font-weight:500}.subhead{align-items:flex-start}.subhead h3{margin:0;font-size:14px}.subhead p{margin:2px 0 0;color:var(--muted-foreground);font-size:11px}.turns{display:grid;gap:8px;margin-top:14px}.turn{display:grid;grid-template-columns:160px 1fr;align-items:center;gap:14px}.turn-label{display:flex;justify-content:space-between;font-size:12px}.turn-label small{color:var(--muted-foreground)}.track{position:relative;height:14px;overflow:hidden;border-radius:4px;background:var(--chart-model)}.tool-segment{position:absolute;top:0;bottom:0;min-width:2px;background:var(--chart-tool);box-shadow:0 0 0 1px var(--card)}.operations{margin-top:28px}.operations .table-wrap{margin:14px 0 0;border:1px solid var(--border);border-radius:8px;background:var(--card)}.diagnostics{margin-top:22px;padding-top:16px;border-top:1px solid var(--border)}.diagnostics>summary,.scan-errors>summary{width:max-content;max-width:100%;cursor:pointer;color:var(--secondary-foreground);font-size:12px;font-weight:600}.diagnostic-grid{display:grid;grid-template-columns:repeat(5,1fr);gap:7px;margin:14px 0}.diagnostic-grid div{padding:9px;border:1px solid var(--border);border-radius:7px;background:var(--card)}.diagnostic-grid span,.diagnostic-grid strong{display:block}.diagnostic-grid span{color:var(--muted-foreground);font-size:9px}.diagnostic-grid strong{margin-top:3px;font-size:12px}.diagnostics h4{margin:18px 0 8px;color:var(--muted-foreground);font-size:10px;font-weight:600;letter-spacing:.06em;text-transform:uppercase}.event-inventory{display:flex;flex-wrap:wrap;gap:5px}.event-inventory span{display:flex;gap:7px;padding:3px 6px;border:1px solid var(--border);border-radius:5px;background:var(--card)}.event-inventory code,.event-inventory b{font-size:9px}.event-inventory code{color:var(--muted-foreground)}.event-inventory b{font-weight:600}.trace-path{display:flex;gap:8px;margin:18px 0 0;color:var(--muted-foreground);font-size:10px}.trace-path code{overflow-wrap:anywhere}.parse-errors,.scan-errors ul{color:var(--destructive);font:10px "SFMono-Regular",Consolas,monospace}.scan-errors{color:var(--destructive)}.empty,.empty-inline{color:var(--muted-foreground)}.empty-inline{margin:18px 0 0}.empty{display:grid;gap:3px;padding:44px;border:1px dashed var(--input);border-radius:var(--radius);background:var(--card);text-align:center}footer{display:flex;justify-content:space-between;gap:16px;margin-top:28px;padding-top:18px;border-top:1px solid var(--border);color:var(--muted-foreground);font-size:10px}footer code{color:var(--secondary-foreground)}
@media(max-width:900px){.app-header,main{width:min(100% - 24px,1280px)}.metrics{grid-template-columns:repeat(2,1fr)}.sessions-head{align-items:flex-start;flex-direction:column}.controls{width:100%;flex-wrap:wrap}.search{flex:1}.search input{width:100%}.session summary{grid-template-columns:36px minmax(0,1fr) 18px}.session-summary{grid-column:2;justify-items:start}.session-summary small{grid-column:auto}.chevron{grid-column:3;grid-row:1/3}.session-metrics{grid-template-columns:repeat(2,1fr)}.turn{grid-template-columns:1fr}.diagnostic-grid{grid-template-columns:repeat(3,1fr)}}
@media(max-width:640px){.app-header{align-items:flex-start;flex-direction:column;gap:12px;padding:14px 0}.scan{display:grid;width:100%;gap:4px}.scan>span:not(.privacy-badge){overflow-wrap:anywhere}.scan .privacy-badge{width:max-content}.page-heading h1{font-size:24px}.metrics{grid-template-columns:1fr}.metric{min-height:108px}.section-label,.panel-head,.subhead{align-items:flex-start;flex-direction:column;gap:10px}.legend{flex-wrap:wrap}.controls{align-items:stretch}.search{flex-basis:100%}.live{flex:1}.session summary{padding:13px 12px}.session-body{padding:14px 12px}.diagnostic-grid{grid-template-columns:repeat(2,1fr)}.table-wrap{margin-right:-12px;margin-left:-12px}.slowest .table-wrap{margin-right:-18px;margin-left:-18px}footer{align-items:flex-start;flex-direction:column}.trace-path{flex-direction:column;gap:3px}}
@media(max-width:640px){.table-wrap{overflow-x:hidden}.table-wrap table,.table-wrap tbody{display:block}.table-wrap thead{position:absolute;width:1px;height:1px;overflow:hidden;clip-path:inset(50%);white-space:nowrap}.table-wrap tr{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:4px 12px;padding:11px 12px;border-bottom:1px solid var(--border)}.table-wrap tr:last-child{border-bottom:0}.table-wrap td{padding:0;border:0}.operation-tool{grid-column:1}.operation-session{grid-column:1}.operation-duration{grid-column:2;grid-row:1;text-align:right}.operation-status{grid-column:2;grid-row:2;text-align:right}.operation-command,.operation-call-id{display:grid;grid-column:1/-1;grid-template-columns:92px minmax(0,1fr);gap:8px;min-width:0;padding-top:5px!important}.operation-command:before{content:"Command preview"}.operation-call-id:before{content:"Call ID"}.operation-command:before,.operation-call-id:before{color:var(--muted-foreground);font-size:9px;font-weight:600;letter-spacing:.04em;text-transform:uppercase}.command{max-width:none;white-space:normal;overflow-wrap:anywhere}}
@media(max-width:640px){.operation-command,.operation-call-id{grid-template-columns:minmax(0,1fr);gap:3px}}
@media(max-width:640px){.operation-tool,.operation-session,.operation-tool code,.operation-session code{min-width:0;white-space:normal;overflow-wrap:anywhere}}
@media(max-width:420px){.session-metrics{grid-template-columns:1fr}.controls button{width:100%}.session-title{grid-template-columns:1fr}.session-title code{display:none}.session-summary{grid-template-columns:auto 1fr}.session-summary small{grid-column:1/-1}.diagnostic-grid{grid-template-columns:1fr}}
@media(prefers-reduced-motion:reduce){html{scroll-behavior:auto}.chevron{transition:none}}
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
let saved=null;
try{saved=JSON.parse(sessionStorage.getItem('agentopsy-open')||'null')}catch(error){console.warn('Agentopsy ignored invalid saved open state',error);sessionStorage.removeItem('agentopsy-open')}
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
    use std::collections::BTreeMap;
    use std::error::Error;

    use jiff::Timestamp;

    use super::{duration, escape, render};
    use crate::trace::{
        Dashboard, Diagnostics, Operation, OperationStatus, Session, SessionStatus, ToolSegment,
        Turn, TurnStatus,
    };

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

    #[test]
    fn renders_complete_dashboard_contract() -> Result<(), Box<dyn Error>> {
        let started = "2026-01-01T00:00:00Z".parse::<Timestamp>()?;
        let ended = "2026-01-01T00:00:03Z".parse::<Timestamp>()?;
        let diagnostics = Diagnostics {
            event_count: 2,
            event_counts: BTreeMap::from([("response_item".to_owned(), 2)]),
            matched_calls: 1,
            parse_errors: vec!["bad <line>&".to_owned()],
            trace_bytes: 42,
            ..Diagnostics::default()
        };
        let operation = Operation {
            call_id: "call-<1>&".to_owned(),
            duration_ms: Some(1_000),
            ended_at: Some(ended),
            name: "mcp__tool_name_that_wraps_at_mobile<&".to_owned(),
            preview: "cargo test <workspace>&".to_owned(),
            started_at: Some(started),
            status: OperationStatus::Returned,
            turn_id: Some("turn-1".to_owned()),
        };
        let turn = Turn {
            duration_ms: 3_000,
            ended_at: ended,
            id: "turn-1".to_owned(),
            started_at: started,
            status: TurnStatus::Complete,
            tool_duration_ms: 1_000,
            tool_segments: vec![ToolSegment {
                duration_ms: 1_000,
                names: "mcp__tool<&".to_owned(),
                offset_ms: 1_000,
            }],
        };
        let dashboard = Dashboard {
            cache_hits: 1,
            cache_misses: 1,
            scan_duration_ms: 7,
            scanned_at: ended,
            scan_errors: vec!["scan <error>&".to_owned()],
            sessions: vec![Session {
                active_duration_ms: 3_000,
                cli_version: "0.1<&".to_owned(),
                cwd: "/tmp/project<&".to_owned(),
                diagnostics,
                effort: "high".to_owned(),
                id: "session<&123".to_owned(),
                last_activity: Some(ended),
                model: "gpt<&".to_owned(),
                model_duration_ms: 2_000,
                operations: vec![operation],
                originator: "codex".to_owned(),
                prompt: "Fix <script>&".to_owned(),
                source: "cli".to_owned(),
                started_at: Some(started),
                status: SessionStatus::Open,
                tool_duration_ms: 1_000,
                trace_path: "/tmp/<trace>&.jsonl".to_owned(),
                turns: vec![turn],
                wall_duration_ms: 5_000,
            }],
        };

        let html = render(&dashboard)?;

        for expected in [
            r#"<header class="app-header">"#,
            "Active-turn time",
            r#"id="filter""#,
            r#"data-session-key="session&lt;&amp;123""#,
            "Activity timeline",
            r#"class="operation-tool""#,
            "Full trace diagnostics",
        ] {
            assert!(html.contains(expected), "missing {expected}");
        }
        assert!(html.contains("Fix &lt;script&gt;&amp;"));
        assert!(html.contains("mcp__tool_name_that_wraps_at_mobile&lt;&amp;"));
        assert!(html.contains("bad &lt;line&gt;&amp;"));

        Ok(())
    }
}
