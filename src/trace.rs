use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File, Metadata};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Instant, SystemTime};

use jiff::Timestamp;
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct Dashboard {
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub scan_duration_ms: u128,
    pub scanned_at: Timestamp,
    pub scan_errors: Vec<String>,
    pub sessions: Vec<Session>,
}

#[derive(Clone, Debug)]
pub struct Session {
    pub active_duration_ms: u64,
    pub cli_version: String,
    pub cwd: String,
    pub diagnostics: Diagnostics,
    pub effort: String,
    pub id: String,
    pub last_activity: Option<Timestamp>,
    pub model: String,
    pub model_duration_ms: u64,
    pub operations: Vec<Operation>,
    pub originator: String,
    pub prompt: String,
    pub source: String,
    pub started_at: Option<Timestamp>,
    pub status: SessionStatus,
    pub tool_duration_ms: u64,
    pub trace_path: String,
    pub turns: Vec<Turn>,
    pub wall_duration_ms: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Diagnostics {
    pub duplicate_call_ids: usize,
    pub duplicate_output_ids: usize,
    pub event_counts: BTreeMap<String, usize>,
    pub event_count: usize,
    pub inferred_turns: usize,
    pub invalid_timestamps: usize,
    pub matched_calls: usize,
    pub missing_call_ids: usize,
    pub missing_output_ids: usize,
    pub overlapping_tool_ms: u64,
    pub parse_errors: Vec<String>,
    pub trace_bytes: u64,
    pub unassigned_calls: usize,
    pub unmatched_calls: usize,
    pub unmatched_outputs: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionStatus {
    Open,
    Complete,
    Aborted,
    Inferred,
    Invalid,
}

impl SessionStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Complete => "complete",
            Self::Aborted => "aborted",
            Self::Inferred => "inferred",
            Self::Invalid => "invalid",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Turn {
    pub duration_ms: u64,
    pub ended_at: Timestamp,
    pub id: String,
    pub started_at: Timestamp,
    pub status: TurnStatus,
    pub tool_duration_ms: u64,
    pub tool_segments: Vec<ToolSegment>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnStatus {
    Complete,
    Aborted,
    Open,
    Inferred,
}

impl TurnStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Aborted => "aborted",
            Self::Open => "open",
            Self::Inferred => "estimated",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ToolSegment {
    pub duration_ms: u64,
    pub names: String,
    pub offset_ms: u64,
}

#[derive(Clone, Debug)]
pub struct Operation {
    pub call_id: String,
    pub duration_ms: Option<u64>,
    pub ended_at: Option<Timestamp>,
    pub name: String,
    pub preview: String,
    pub started_at: Option<Timestamp>,
    pub status: OperationStatus,
    pub turn_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationStatus {
    Returned,
    Failed,
    Pending,
}

impl OperationStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Returned => "returned",
            Self::Failed => "failed",
            Self::Pending => "pending",
        }
    }
}

#[derive(Default)]
pub struct TraceCache {
    entries: HashMap<PathBuf, CachedSession>,
}

#[derive(Clone)]
struct CachedSession {
    session: Session,
    stamp: FileStamp,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

impl FileStamp {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        }
    }
}

impl TraceCache {
    pub fn load(&mut self, root: &Path) -> Dashboard {
        let started = Instant::now();
        let mut paths = Vec::new();
        let mut scan_errors = Vec::new();
        collect_trace_paths(root, &mut paths, &mut scan_errors);
        paths.sort_unstable();

        let mut cache_hits = 0;
        let mut sessions = Vec::with_capacity(paths.len());
        let mut seen = HashSet::with_capacity(paths.len());
        let mut parse_tasks = Vec::new();

        for path in paths {
            seen.insert(path.clone());
            let metadata = match fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    scan_errors.push(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            let stamp = FileStamp::from_metadata(&metadata);

            if let Some(cached) = self.entries.get(&path)
                && cached.stamp == stamp
            {
                cache_hits += 1;
                sessions.push(cached.session.clone());
                continue;
            }

            let trace_path = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            parse_tasks.push(ParseTask {
                path,
                stamp,
                trace_path,
            });
        }
        let cache_misses = parse_tasks.len();
        for (task, result) in parse_files(&parse_tasks) {
            match result {
                Ok(session) => {
                    self.entries.insert(
                        task.path,
                        CachedSession {
                            session: session.clone(),
                            stamp: task.stamp,
                        },
                    );
                    sessions.push(session);
                }
                Err(error) => scan_errors.push(format!("{}: {error}", task.path.display())),
            }
        }

        self.entries.retain(|path, _| seen.contains(path));
        sessions.sort_by(|left, right| {
            right
                .active_duration_ms
                .cmp(&left.active_duration_ms)
                .then_with(|| right.last_activity.cmp(&left.last_activity))
        });

        Dashboard {
            cache_hits,
            cache_misses,
            scan_duration_ms: started.elapsed().as_millis(),
            scanned_at: Timestamp::now(),
            scan_errors,
            sessions,
        }
    }
}

#[derive(Clone)]
struct ParseTask {
    path: PathBuf,
    stamp: FileStamp,
    trace_path: String,
}

fn parse_files(tasks: &[ParseTask]) -> Vec<(ParseTask, io::Result<Session>)> {
    if tasks.is_empty() {
        return Vec::new();
    }
    let worker_count = thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let worker_count = worker_count.min(tasks.len());
    let next = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::channel();
    thread::scope(|scope| {
        for _ in 0..worker_count {
            let sender = sender.clone();
            let next = &next;
            scope.spawn(move || {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(task) = tasks.get(index).cloned() else {
                        break;
                    };
                    let result = parse_file(&task.path, task.trace_path.clone(), task.stamp.len);
                    if sender.send((task, result)).is_err() {
                        break;
                    }
                }
            });
        }
    });
    drop(sender);
    receiver.into_iter().collect()
}

fn collect_trace_paths(directory: &Path, paths: &mut Vec<PathBuf>, errors: &mut Vec<String>) {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            errors.push(format!("{}: {error}", directory.display()));
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                errors.push(format!("{}: {error}", directory.display()));
                continue;
            }
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                errors.push(format!("{}: {error}", entry.path().display()));
                continue;
            }
        };
        if file_type.is_dir() {
            collect_trace_paths(&entry.path(), paths, errors);
        } else if file_type.is_file() && entry.path().extension() == Some(OsStr::new("jsonl")) {
            paths.push(entry.path());
        }
    }
}

#[derive(Deserialize)]
struct PayloadEnvelope {
    payload: Value,
}

#[derive(Clone, Copy)]
struct EventHeader<'a> {
    event_type: &'a str,
    payload_type: &'a str,
    timestamp: Option<&'a str>,
}

impl<'a> EventHeader<'a> {
    fn from_line(line: &'a str) -> Option<Self> {
        let event_type = json_string_after(line, "\"type\":\"")?;
        let timestamp = json_string_after(line, "\"timestamp\":\"");
        let payload = line.split_once("\"payload\":")?.1;
        let payload_type = json_string_after(payload, "\"type\":\"").unwrap_or("-");
        Some(Self {
            event_type,
            payload_type,
            timestamp,
        })
    }
}

#[derive(Clone)]
struct TurnBuilder {
    ended_at: Option<Timestamp>,
    id: String,
    started_at: Timestamp,
    status: TurnStatus,
}

struct RawCall {
    call_id: String,
    input: String,
    name: String,
    started_at: Option<Timestamp>,
    turn_id: Option<String>,
}

struct RawOutput {
    failed: bool,
    timestamp: Option<Timestamp>,
}

#[derive(Default)]
struct MetadataFields {
    cli_version: String,
    cwd: String,
    effort: String,
    id: String,
    model: String,
    originator: String,
    prompt: String,
    source: String,
}

fn parse_file(path: &Path, trace_path: String, trace_bytes: u64) -> io::Result<Session> {
    let reader = BufReader::new(File::open(path)?);
    Ok(parse_reader(reader, trace_path, trace_bytes))
}

fn parse_reader(reader: impl BufRead, trace_path: String, trace_bytes: u64) -> Session {
    let mut parser = SessionParser::new(trace_bytes);
    for (line_index, line) in reader.lines().enumerate() {
        parser.parse_line(line_index + 1, line);
    }
    parser.finish(trace_path)
}

struct SessionParser {
    active_turn: Option<String>,
    call_ids: HashSet<String>,
    calls: Vec<RawCall>,
    diagnostics: Diagnostics,
    fields: MetadataFields,
    first_event: Option<Timestamp>,
    last_event: Option<Timestamp>,
    outputs: HashMap<String, RawOutput>,
    turn_indexes: HashMap<String, usize>,
    turns: Vec<TurnBuilder>,
}

impl SessionParser {
    fn new(trace_bytes: u64) -> Self {
        Self {
            active_turn: None,
            call_ids: HashSet::new(),
            calls: Vec::new(),
            diagnostics: Diagnostics {
                trace_bytes,
                ..Diagnostics::default()
            },
            fields: MetadataFields::default(),
            first_event: None,
            last_event: None,
            outputs: HashMap::new(),
            turn_indexes: HashMap::new(),
            turns: Vec::new(),
        }
    }

    fn parse_line(&mut self, line_number: usize, line: io::Result<String>) {
        self.diagnostics.event_count += 1;
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                self.record_parse_error(line_number, error);
                return;
            }
        };
        let Some(header) = EventHeader::from_line(&line) else {
            self.record_parse_error(line_number, "unrecognized JSONL envelope");
            return;
        };
        self.record_event(line_number, &line, header);
    }

    fn record_parse_error(&mut self, line_number: usize, error: impl std::fmt::Display) {
        self.diagnostics
            .parse_errors
            .push(format!("line {line_number}: {error}"));
    }

    fn record_event(&mut self, line_number: usize, line: &str, header: EventHeader<'_>) {
        let payload_type = header.payload_type;
        *self
            .diagnostics
            .event_counts
            .entry(format!("{}/{payload_type}", header.event_type))
            .or_default() += 1;
        let timestamp = self.parse_timestamp(header.timestamp);
        match (header.event_type, payload_type) {
            ("session_meta", _) => {
                if let Some(payload) = self.payload_value(line, line_number) {
                    read_session_metadata(&payload, &mut self.fields);
                }
            }
            ("turn_context", _) => {
                if let Some(payload) = self.payload_value(line, line_number) {
                    read_turn_context(&payload, &mut self.fields);
                }
            }
            ("event_msg", "user_message") => {
                if let Some(payload) = self.payload_value(line, line_number)
                    && let Some(message) = string_field(&payload, "message")
                {
                    self.fields.prompt = compact(message, 220);
                }
            }
            ("response_item", "message") => {
                if let Some(payload) = self.payload_value(line, line_number)
                    && string_field(&payload, "role") == Some("user")
                    && let Some(message) = message_content(&payload)
                {
                    self.fields.prompt = compact(message, 220);
                }
            }
            ("event_msg", "task_started") => {
                if let Some(payload) = self.payload_value(line, line_number) {
                    self.start_turn(&payload, timestamp, line_number);
                }
            }
            ("event_msg", "task_complete" | "turn_aborted") => {
                let status = if payload_type == "task_complete" {
                    TurnStatus::Complete
                } else {
                    TurnStatus::Aborted
                };
                if let Some(payload) = self.payload_value(line, line_number) {
                    self.end_turn(&payload, timestamp, status);
                }
            }
            ("response_item", "function_call" | "custom_tool_call") => {
                if let Some(payload) = self.payload_value(line, line_number) {
                    self.record_call(&payload, timestamp, line_number);
                }
            }
            ("response_item", "function_call_output" | "custom_tool_call_output") => {
                self.record_output(line, timestamp);
            }
            _ => {}
        }
    }

    fn payload_value(&mut self, line: &str, line_number: usize) -> Option<Value> {
        match serde_json::from_str::<PayloadEnvelope>(line) {
            Ok(envelope) => Some(envelope.payload),
            Err(error) => {
                self.record_parse_error(line_number, error);
                None
            }
        }
    }

    fn parse_timestamp(&mut self, value: Option<&str>) -> Option<Timestamp> {
        let timestamp = value.and_then(|value| {
            if let Ok(timestamp) = value.parse() {
                Some(timestamp)
            } else {
                self.diagnostics.invalid_timestamps += 1;
                None
            }
        });
        if let Some(timestamp) = timestamp {
            self.first_event = Some(
                self.first_event
                    .map_or(timestamp, |first| first.min(timestamp)),
            );
            self.last_event = Some(
                self.last_event
                    .map_or(timestamp, |last| last.max(timestamp)),
            );
        }
        timestamp
    }

    fn start_turn(&mut self, payload: &Value, timestamp: Option<Timestamp>, line_number: usize) {
        if let Some(timestamp) = timestamp {
            let id = string_field(payload, "turn_id")
                .map_or_else(|| format!("turn-{line_number}"), ToOwned::to_owned);
            let index = self.turns.len();
            self.turns.push(TurnBuilder {
                ended_at: None,
                id: id.clone(),
                started_at: timestamp,
                status: TurnStatus::Open,
            });
            self.turn_indexes.insert(id.clone(), index);
            self.active_turn = Some(id);
        }
    }

    fn end_turn(&mut self, payload: &Value, timestamp: Option<Timestamp>, status: TurnStatus) {
        if let (Some(timestamp), Some(id)) = (timestamp, string_field(payload, "turn_id")) {
            if let Some(index) = self.turn_indexes.get(id).copied() {
                self.turns[index].ended_at = Some(timestamp);
                self.turns[index].status = status;
            }
            if self.active_turn.as_deref() == Some(id) {
                self.active_turn = None;
            }
        }
    }

    fn record_call(&mut self, payload: &Value, timestamp: Option<Timestamp>, line_number: usize) {
        let call_id = string_field(payload, "call_id").map_or_else(
            || {
                self.diagnostics.missing_call_ids += 1;
                format!("missing-call-{line_number}")
            },
            ToOwned::to_owned,
        );
        if !self.call_ids.insert(call_id.clone()) {
            self.diagnostics.duplicate_call_ids += 1;
        }
        let name = string_field(payload, "name")
            .unwrap_or("unknown")
            .to_owned();
        let input = string_field(payload, "arguments")
            .or_else(|| string_field(payload, "input"))
            .unwrap_or_default()
            .to_owned();
        self.calls.push(RawCall {
            call_id,
            input,
            name,
            started_at: timestamp,
            turn_id: self.active_turn.clone(),
        });
    }

    fn record_output(&mut self, line: &str, timestamp: Option<Timestamp>) {
        if let Some(call_id) = json_string_after(line, "\"call_id\":\"") {
            if self
                .outputs
                .insert(
                    call_id.to_owned(),
                    RawOutput {
                        failed: raw_output_reports_failure(line),
                        timestamp,
                    },
                )
                .is_some()
            {
                self.diagnostics.duplicate_output_ids += 1;
            }
        } else {
            self.diagnostics.missing_output_ids += 1;
        }
    }

    fn finish(mut self, trace_path: String) -> Session {
        let mut turns = finalize_turns(
            self.turns,
            self.first_event,
            self.last_event,
            &mut self.diagnostics,
        );
        let mut operations =
            finalize_operations(self.calls, &mut self.outputs, &turns, &mut self.diagnostics);
        self.diagnostics.unmatched_outputs =
            self.outputs.len() + self.diagnostics.missing_output_ids;
        operations.sort_by_key(|operation| operation.started_at);
        populate_tool_segments(&mut turns, &operations, &mut self.diagnostics);

        let active_duration_ms: u64 = turns.iter().map(|turn| turn.duration_ms).sum();
        let tool_duration_ms: u64 = turns.iter().map(|turn| turn.tool_duration_ms).sum();
        let model_duration_ms = active_duration_ms.saturating_sub(tool_duration_ms);
        let wall_duration_ms = match (self.first_event, self.last_event) {
            (Some(start), Some(end)) => elapsed_ms(start, end),
            _ => 0,
        };
        let status = session_status(&turns, self.diagnostics.event_count);
        let id = if self.fields.id.is_empty() {
            Path::new(&trace_path)
                .file_stem()
                .and_then(OsStr::to_str)
                .unwrap_or("unknown")
                .to_owned()
        } else {
            self.fields.id
        };

        Session {
            active_duration_ms,
            cli_version: self.fields.cli_version,
            cwd: self.fields.cwd,
            diagnostics: self.diagnostics,
            effort: self.fields.effort,
            id,
            last_activity: self.last_event,
            model: self.fields.model,
            model_duration_ms,
            operations,
            originator: self.fields.originator,
            prompt: self.fields.prompt,
            source: self.fields.source,
            started_at: self.first_event,
            status,
            tool_duration_ms,
            trace_path,
            turns,
            wall_duration_ms,
        }
    }
}

fn read_session_metadata(payload: &Value, fields: &mut MetadataFields) {
    copy_field(payload, "id", &mut fields.id);
    if fields.id.is_empty() {
        copy_field(payload, "session_id", &mut fields.id);
    }
    copy_field(payload, "cwd", &mut fields.cwd);
    copy_field(payload, "source", &mut fields.source);
    copy_field(payload, "originator", &mut fields.originator);
    copy_field(payload, "cli_version", &mut fields.cli_version);
}

fn read_turn_context(payload: &Value, fields: &mut MetadataFields) {
    copy_field(payload, "cwd", &mut fields.cwd);
    copy_field(payload, "model", &mut fields.model);
    copy_field(payload, "effort", &mut fields.effort);
}

fn copy_field(payload: &Value, key: &str, destination: &mut String) {
    if let Some(value) = string_field(payload, key) {
        destination.clear();
        destination.push_str(value);
    }
}

fn json_string_after<'a>(value: &'a str, marker: &str) -> Option<&'a str> {
    let remainder = value.split_once(marker)?.1;
    let end = remainder.find('"')?;
    Some(&remainder[..end])
}

fn string_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn message_content(payload: &Value) -> Option<&str> {
    payload
        .get("content")?
        .as_array()?
        .iter()
        .find_map(|item| string_field(item, "text"))
}

fn finalize_turns(
    mut builders: Vec<TurnBuilder>,
    first_event: Option<Timestamp>,
    last_event: Option<Timestamp>,
    diagnostics: &mut Diagnostics,
) -> Vec<Turn> {
    if builders.is_empty()
        && let (Some(started_at), Some(ended_at)) = (first_event, last_event)
    {
        diagnostics.inferred_turns = 1;
        builders.push(TurnBuilder {
            ended_at: Some(ended_at),
            id: "inferred-turn".to_owned(),
            started_at,
            status: TurnStatus::Inferred,
        });
    }

    let fallback_end = last_event;
    let mut turns = builders
        .into_iter()
        .map(|builder| {
            let ended_at = builder
                .ended_at
                .or(fallback_end)
                .unwrap_or(builder.started_at);
            Turn {
                duration_ms: elapsed_ms(builder.started_at, ended_at),
                ended_at,
                id: builder.id,
                started_at: builder.started_at,
                status: builder.status,
                tool_duration_ms: 0,
                tool_segments: Vec::new(),
            }
        })
        .collect::<Vec<_>>();
    turns.sort_by_key(|turn| turn.started_at);
    turns
}

fn finalize_operations(
    calls: Vec<RawCall>,
    outputs: &mut HashMap<String, RawOutput>,
    turns: &[Turn],
    diagnostics: &mut Diagnostics,
) -> Vec<Operation> {
    calls
        .into_iter()
        .map(|call| {
            let output = outputs.remove(&call.call_id);
            let duration_ms = match (
                call.started_at,
                output.as_ref().and_then(|item| item.timestamp),
            ) {
                (Some(start), Some(end)) => Some(elapsed_ms(start, end)),
                _ => None,
            };
            let status = match output.as_ref() {
                Some(output) if output.failed => OperationStatus::Failed,
                Some(_) => OperationStatus::Returned,
                None => OperationStatus::Pending,
            };
            if output.is_some() {
                diagnostics.matched_calls += 1;
            } else {
                diagnostics.unmatched_calls += 1;
            }
            let turn_id = call.turn_id.or_else(|| {
                call.started_at.and_then(|started_at| {
                    turns
                        .iter()
                        .find(|turn| started_at >= turn.started_at && started_at <= turn.ended_at)
                        .map(|turn| turn.id.clone())
                })
            });
            if turn_id.is_none() {
                diagnostics.unassigned_calls += 1;
            }
            let (name, preview) = tool_summary(&call.name, &call.input);
            Operation {
                call_id: call.call_id,
                duration_ms,
                ended_at: output.and_then(|item| item.timestamp),
                name,
                preview,
                started_at: call.started_at,
                status,
                turn_id,
            }
        })
        .collect()
}

fn populate_tool_segments(
    turns: &mut [Turn],
    operations: &[Operation],
    diagnostics: &mut Diagnostics,
) {
    let individual_tool_ms: u64 = operations
        .iter()
        .filter_map(|operation| operation.duration_ms)
        .sum();

    for turn in &mut *turns {
        let mut intervals = operations
            .iter()
            .filter(|operation| operation.turn_id.as_deref() == Some(turn.id.as_str()))
            .filter_map(|operation| {
                let start = operation.started_at?;
                let end = operation.ended_at?;
                let start = start.max(turn.started_at);
                let end = end.min(turn.ended_at);
                (end >= start).then(|| (start, end, operation.name.clone()))
            })
            .collect::<Vec<_>>();
        intervals.sort_by_key(|(start, _, _)| *start);

        let mut merged = Vec::<MergedInterval>::new();
        for (start, end, name) in intervals {
            if let Some(last) = merged.last_mut()
                && start <= last.end
            {
                last.end = last.end.max(end);
                last.names.insert(name);
                continue;
            }
            merged.push(MergedInterval {
                end,
                names: BTreeSet::from([name]),
                start,
            });
        }

        turn.tool_segments = merged
            .into_iter()
            .map(|interval| ToolSegment {
                duration_ms: elapsed_ms(interval.start, interval.end),
                names: interval.names.into_iter().collect::<Vec<_>>().join(", "),
                offset_ms: elapsed_ms(turn.started_at, interval.start),
            })
            .collect();
        turn.tool_duration_ms = turn
            .tool_segments
            .iter()
            .map(|segment| segment.duration_ms)
            .sum();
    }

    let union_tool_ms: u64 = turns.iter().map(|turn| turn.tool_duration_ms).sum();
    diagnostics.overlapping_tool_ms = individual_tool_ms.saturating_sub(union_tool_ms);
}

struct MergedInterval {
    end: Timestamp,
    names: BTreeSet<String>,
    start: Timestamp,
}

fn session_status(turns: &[Turn], event_count: usize) -> SessionStatus {
    if turns.iter().any(|turn| turn.status == TurnStatus::Open) {
        SessionStatus::Open
    } else {
        match turns.last().map(|turn| turn.status) {
            Some(TurnStatus::Complete) => SessionStatus::Complete,
            Some(TurnStatus::Aborted) => SessionStatus::Aborted,
            Some(TurnStatus::Open) => SessionStatus::Open,
            None if event_count == 0 => SessionStatus::Invalid,
            Some(TurnStatus::Inferred) | None => SessionStatus::Inferred,
        }
    }
}

fn elapsed_ms(start: Timestamp, end: Timestamp) -> u64 {
    u64::try_from(end.duration_since(start).as_millis().max(0)).unwrap_or(0)
}

fn tool_summary(recorded_name: &str, input: &str) -> (String, String) {
    if recorded_name == "exec"
        && let Some((name, value)) = nested_tool(input)
    {
        let preview = value.map_or_else(
            || js_argument(input, "cmd:").unwrap_or_else(|| compact(input, 220)),
            |value| preview_value(&value),
        );
        return (name, preview);
    }

    let preview = if recorded_name == "apply_patch" {
        input
            .lines()
            .find(|line| {
                line.starts_with("*** Add File:")
                    || line.starts_with("*** Delete File:")
                    || line.starts_with("*** Update File:")
            })
            .unwrap_or(input)
            .to_owned()
    } else if let Ok(value) = serde_json::from_str::<Value>(input) {
        preview_value(&value)
    } else {
        input.to_owned()
    };
    (recorded_name.to_owned(), compact(&preview, 220))
}

fn nested_tool(input: &str) -> Option<(String, Option<Value>)> {
    let marker = "tools.";
    let marker_start = input.find(marker)? + marker.len();
    let remainder = &input[marker_start..];
    let name_end =
        remainder.find(|character: char| !character.is_ascii_alphanumeric() && character != '_')?;
    let name = remainder[..name_end].to_owned();
    let value_start = remainder[name_end..].find('{')? + name_end;
    let mut deserializer = serde_json::Deserializer::from_str(&remainder[value_start..]);
    Some((name, Value::deserialize(&mut deserializer).ok()))
}

fn js_argument(input: &str, marker: &str) -> Option<String> {
    let value = input.split_once(marker)?.1.trim_start();
    let quote = value.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let value = &value[quote.len_utf8()..];
    let mut escaped = false;
    let end = value.char_indices().find_map(|(index, character)| {
        if character == quote && !escaped {
            return Some(index);
        }
        escaped = character == '\\' && !escaped;
        if character != '\\' {
            escaped = false;
        }
        None
    })?;
    Some(compact(&value[..end].replace("\\n", " "), 220))
}

fn preview_value(value: &Value) -> String {
    const PREFERRED_KEYS: [&str; 9] = [
        "cmd",
        "command",
        "prompt",
        "query",
        "q",
        "pattern",
        "objective",
        "path",
        "url",
    ];

    if let Some(object) = value.as_object() {
        for key in PREFERRED_KEYS {
            if let Some(value) = object.get(key) {
                let text = match value {
                    Value::String(text) => text.clone(),
                    Value::Array(items) => items
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                    _ => value.to_string(),
                };
                return compact(&text, 220);
            }
        }
    }
    compact(&value.to_string(), 220)
}

fn compact(value: &str, character_limit: usize) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= character_limit {
        return compact;
    }
    let mut shortened = compact.chars().take(character_limit).collect::<String>();
    shortened.push('…');
    shortened
}

fn raw_output_reports_failure(text: &str) -> bool {
    if text.contains("\"isError\":true")
        || text.contains("\"is_error\":true")
        || text.contains("\\\"isError\\\":true")
        || text.contains("\\\"is_error\\\":true")
        || text.contains("\"Err\":")
    {
        return true;
    }
    ["Process exited with code ", "\"exit_code\":"]
        .iter()
        .any(|marker| {
            text.match_indices(marker).any(|(index, _)| {
                text[index + marker.len()..]
                    .trim_start()
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse::<u32>()
                    .is_ok_and(|code| code != 0)
            })
        })
        || text.starts_with("\"Error:")
        || text.starts_with("\"ERROR:")
        || text.starts_with("\"tool_error")
}

#[cfg(test)]
mod tests {
    use super::{OperationStatus, parse_reader, tool_summary};
    use std::io::Cursor;

    #[test]
    fn excludes_turn_gaps_and_merges_overlapping_tools() {
        let trace = concat!(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"session","cwd":"/tmp/project"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"one"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"a","arguments":"{\"cmd\":\"cargo test\"}"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"function_call","name":"wait","call_id":"b","arguments":"{}"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:04Z","type":"response_item","payload":{"type":"function_call_output","call_id":"a","output":"Process exited with code 0"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:05Z","type":"response_item","payload":{"type":"function_call_output","call_id":"b","output":"done"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:06Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"one"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:01:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"two"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:01:02Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"two"}}"#,
        );

        let session = parse_reader(
            Cursor::new(trace),
            "trace.jsonl".to_owned(),
            trace.len() as u64,
        );

        assert_eq!(session.active_duration_ms, 8_000);
        assert_eq!(session.tool_duration_ms, 4_000);
        assert_eq!(session.model_duration_ms, 4_000);
        assert_eq!(session.diagnostics.overlapping_tool_ms, 2_000);
    }

    #[test]
    fn matches_outputs_by_call_id_and_reports_failed_exit() {
        let trace = concat!(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"one"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"a","arguments":"{\"cmd\":\"cargo test\"}"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:03Z","type":"response_item","payload":{"type":"function_call_output","call_id":"a","output":"Process exited with code 101"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:04Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"one"}}"#,
        );

        let session = parse_reader(
            Cursor::new(trace),
            "trace.jsonl".to_owned(),
            trace.len() as u64,
        );

        assert_eq!(session.operations[0].duration_ms, Some(2_000));
        assert_eq!(session.operations[0].status, OperationStatus::Failed);
        assert_eq!(session.operations[0].preview, "cargo test");
    }

    #[test]
    fn unwraps_exec_custom_tool_command() {
        let input = r#"const result = await tools.exec_command({"cmd":"cargo check","workdir":"/tmp"}); text(result.output);"#;

        let (name, preview) = tool_summary("exec", input);

        assert_eq!(name, "exec_command");
        assert_eq!(preview, "cargo check");
    }

    #[test]
    fn unwraps_javascript_style_exec_command() {
        let input =
            r#"const result = await tools.exec_command({ cmd: "cargo test -p agentopsy" });"#;

        let (name, preview) = tool_summary("exec", input);

        assert_eq!(name, "exec_command");
        assert_eq!(preview, "cargo test -p agentopsy");
    }
}
