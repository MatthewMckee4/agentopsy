use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File, Metadata};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

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

#[derive(Deserialize)]
struct OutputEnvelope<'a> {
    #[serde(borrow)]
    payload: OutputPayload<'a>,
}

#[derive(Deserialize)]
struct OutputPayload<'a> {
    call_id: Option<&'a str>,
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
    call_id: Option<String>,
    input: String,
    line_number: usize,
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
    ambiguous_call_ids: HashSet<String>,
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
            ambiguous_call_ids: HashSet::new(),
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
        let timestamp = self.parse_timestamp(header.timestamp, line_number);
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
                    self.end_turn(&payload, timestamp, status, line_number);
                }
            }
            ("response_item", "function_call" | "custom_tool_call" | "tool_search_call") => {
                if let Some(payload) = self.payload_value(line, line_number) {
                    self.record_call(&payload, payload_type, timestamp, line_number);
                }
            }
            (
                "response_item",
                "function_call_output" | "custom_tool_call_output" | "tool_search_output",
            ) => {
                self.record_output(line, timestamp, line_number);
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

    fn parse_timestamp(&mut self, value: Option<&str>, line_number: usize) -> Option<Timestamp> {
        let timestamp = value.and_then(|value| {
            if let Ok(timestamp) = value.parse() {
                Some(timestamp)
            } else {
                self.diagnostics.invalid_timestamps += 1;
                self.record_parse_error(line_number, "invalid envelope timestamp");
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
        let started_at = self
            .payload_timestamp(payload, "started_at", line_number)
            .or(timestamp);
        if let Some(started_at) = started_at {
            let id = string_field(payload, "turn_id")
                .map_or_else(|| format!("turn-{line_number}"), ToOwned::to_owned);
            let index = self.turns.len();
            self.turns.push(TurnBuilder {
                ended_at: None,
                id: id.clone(),
                started_at,
                status: TurnStatus::Open,
            });
            self.turn_indexes.insert(id.clone(), index);
            self.active_turn = Some(id);
        }
    }

    fn end_turn(
        &mut self,
        payload: &Value,
        envelope_timestamp: Option<Timestamp>,
        status: TurnStatus,
        line_number: usize,
    ) {
        let Some(id) = string_field(payload, "turn_id") else {
            return;
        };
        let completed_at = self.payload_timestamp(payload, "completed_at", line_number);
        let recorded_duration_ms = self.recorded_duration(payload, id, line_number);
        if let Some(index) = self.turn_indexes.get(id).copied() {
            let observed_start = self.turns[index].started_at;
            let (logical_start, logical_end) = match (recorded_duration_ms, completed_at) {
                (Some(duration_ms), Some(ended_at)) => {
                    match ended_at.checked_sub(Duration::from_millis(duration_ms)) {
                        Ok(started_at) => (started_at, Some(ended_at)),
                        Err(error) => {
                            self.record_parse_error(
                                line_number,
                                format!(
                                    "turn `{id}` duration_ms {duration_ms} exceeds the supported timestamp range: {error}"
                                ),
                            );
                            (observed_start, Some(ended_at))
                        }
                    }
                }
                (Some(duration_ms), None) => {
                    match observed_start.checked_add(Duration::from_millis(duration_ms)) {
                        Ok(ended_at) => (observed_start, Some(ended_at)),
                        Err(error) => {
                            self.record_parse_error(
                                line_number,
                                format!(
                                    "turn `{id}` duration_ms {duration_ms} exceeds the supported timestamp range: {error}"
                                ),
                            );
                            (observed_start, envelope_timestamp)
                        }
                    }
                }
                (None, completed_at) => (observed_start, completed_at.or(envelope_timestamp)),
            };
            if let Some(ended_at) = logical_end {
                if ended_at < logical_start {
                    self.record_parse_error(
                        line_number,
                        format!("turn `{id}` completed_at precedes started_at"),
                    );
                }
                self.turns[index].started_at = logical_start.min(ended_at);
                self.turns[index].ended_at = Some(ended_at.max(logical_start));
                self.turns[index].status = status;
            }
        }
        if self.active_turn.as_deref() == Some(id) {
            self.active_turn = None;
        }
    }

    fn payload_timestamp(
        &mut self,
        payload: &Value,
        key: &str,
        line_number: usize,
    ) -> Option<Timestamp> {
        let value = payload.get(key)?;
        let Some(seconds) = value.as_i64() else {
            self.diagnostics.invalid_timestamps += 1;
            self.record_parse_error(
                line_number,
                format!("{key} must be an integer Unix timestamp in seconds"),
            );
            return None;
        };
        match Timestamp::from_second(seconds) {
            Ok(timestamp) => Some(timestamp),
            Err(error) => {
                self.diagnostics.invalid_timestamps += 1;
                self.record_parse_error(line_number, format!("invalid {key} timestamp: {error}"));
                None
            }
        }
    }

    fn recorded_duration(
        &mut self,
        payload: &Value,
        turn_id: &str,
        line_number: usize,
    ) -> Option<u64> {
        let value = payload.get("duration_ms")?;
        if let Some(duration_ms) = value.as_u64() {
            Some(duration_ms)
        } else {
            self.record_parse_error(
                line_number,
                format!(
                    "turn `{turn_id}` duration_ms must be a non-negative integer, observed {}",
                    json_type(value)
                ),
            );
            None
        }
    }

    fn record_call(
        &mut self,
        payload: &Value,
        payload_type: &str,
        timestamp: Option<Timestamp>,
        line_number: usize,
    ) {
        let call_id = string_field(payload, "call_id").map(ToOwned::to_owned);
        if let Some(call_id) = &call_id {
            if !self.call_ids.insert(call_id.clone()) {
                self.diagnostics.duplicate_call_ids += 1;
                self.ambiguous_call_ids.insert(call_id.clone());
                self.record_parse_error(
                    line_number,
                    format!("duplicate call_id `{call_id}`; matching and timing are ambiguous"),
                );
            }
        } else {
            self.diagnostics.missing_call_ids += 1;
        }
        let name = string_field(payload, "name")
            .unwrap_or(if payload_type == "tool_search_call" {
                "tool_search"
            } else {
                "unknown"
            })
            .to_owned();
        let input = payload
            .get("arguments")
            .or_else(|| payload.get("input"))
            .map(|value| {
                value
                    .as_str()
                    .map_or_else(|| value.to_string(), ToOwned::to_owned)
            })
            .unwrap_or_default();
        self.calls.push(RawCall {
            call_id,
            input,
            line_number,
            name,
            started_at: timestamp,
            turn_id: self.active_turn.clone(),
        });
    }

    fn record_output(&mut self, line: &str, timestamp: Option<Timestamp>, line_number: usize) {
        let envelope = match serde_json::from_str::<OutputEnvelope<'_>>(line) {
            Ok(envelope) => envelope,
            Err(error) => {
                self.record_parse_error(line_number, format!("invalid tool output: {error}"));
                return;
            }
        };
        if let Some(call_id) = envelope.payload.call_id {
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
                self.ambiguous_call_ids.insert(call_id.to_owned());
                self.record_parse_error(
                    line_number,
                    format!(
                        "duplicate output call_id `{call_id}`; matching and timing are ambiguous"
                    ),
                );
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
        let mut operations = finalize_operations(
            self.calls,
            &mut self.outputs,
            &self.ambiguous_call_ids,
            &turns,
            &mut self.diagnostics,
        );
        self.diagnostics.unmatched_outputs = self
            .outputs
            .len()
            .saturating_add(self.diagnostics.missing_output_ids);
        operations.sort_by_key(|operation| operation.started_at);
        populate_tool_segments(&mut turns, &operations, &mut self.diagnostics);

        let active_duration_ms = saturating_sum(turns.iter().map(|turn| turn.duration_ms));
        let tool_duration_ms = saturating_sum(turns.iter().map(|turn| turn.tool_duration_ms));
        let model_duration_ms = active_duration_ms.saturating_sub(tool_duration_ms);
        // Wall span and last activity intentionally use persisted envelope timestamps. Turn
        // bounds use payload timing receipts and exclude persistence delay.
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

const fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number outside the supported integer range",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

pub fn saturating_sum(values: impl Iterator<Item = u64>) -> u64 {
    values.fold(0, u64::saturating_add)
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
                .unwrap_or(builder.started_at)
                .max(builder.started_at);
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
    ambiguous_call_ids: &HashSet<String>,
    turns: &[Turn],
    diagnostics: &mut Diagnostics,
) -> Vec<Operation> {
    calls
        .into_iter()
        .map(|call| {
            let output = call
                .call_id
                .as_ref()
                .filter(|call_id| !ambiguous_call_ids.contains(*call_id))
                .and_then(|call_id| outputs.remove(call_id));
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
            let turn_id = call
                .turn_id
                .filter(|turn_id| {
                    call.started_at.is_none_or(|started_at| {
                        turns.iter().any(|turn| {
                            turn.id == *turn_id
                                && started_at >= turn.started_at
                                && started_at <= turn.ended_at
                        })
                    })
                })
                .or_else(|| {
                    call.started_at.and_then(|started_at| {
                        turns
                            .iter()
                            .find(|turn| {
                                started_at >= turn.started_at && started_at <= turn.ended_at
                            })
                            .map(|turn| turn.id.clone())
                    })
                });
            if turn_id.is_none() {
                diagnostics.unassigned_calls += 1;
            }
            let (name, preview) = tool_summary(&call.name, &call.input);
            Operation {
                call_id: call
                    .call_id
                    .unwrap_or_else(|| format!("missing-call-line-{}", call.line_number)),
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
    let mut individual_tool_ms = 0_u64;

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
        individual_tool_ms = individual_tool_ms.saturating_add(saturating_sum(
            intervals
                .iter()
                .map(|(start, end, _)| elapsed_ms(*start, *end)),
        ));

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
        turn.tool_duration_ms =
            saturating_sum(turn.tool_segments.iter().map(|segment| segment.duration_ms));
    }

    let union_tool_ms = saturating_sum(turns.iter().map(|turn| turn.tool_duration_ms));
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
    use super::{OperationStatus, parse_reader, saturating_sum, tool_summary};
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
    fn uses_recorded_turn_duration_when_completion_is_persisted_late() {
        let trace = concat!(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"task_started","started_at":1767225600,"turn_id":"one"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"a","arguments":"{\"cmd\":\"cargo test\"}"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:03Z","type":"response_item","payload":{"type":"function_call_output","call_id":"a","output":"done"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:10:00Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"late","arguments":"{\"cmd\":\"cargo test\"}"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:11:00Z","type":"response_item","payload":{"type":"function_call_output","call_id":"late","output":"done"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T01:00:00Z","type":"event_msg","payload":{"type":"task_complete","completed_at":1767225605,"duration_ms":5000,"turn_id":"one"}}"#,
        );

        let session = parse_reader(
            Cursor::new(trace),
            "trace.jsonl".to_owned(),
            trace.len() as u64,
        );

        assert_eq!(session.active_duration_ms, 5_000);
        assert_eq!(session.tool_duration_ms, 2_000);
        assert_eq!(session.model_duration_ms, 3_000);
        assert_eq!(
            session.turns[0].started_at.to_string(),
            "2026-01-01T00:00:00Z"
        );
        assert_eq!(
            session.turns[0].ended_at.to_string(),
            "2026-01-01T00:00:05Z"
        );
        assert_eq!(session.diagnostics.unassigned_calls, 1);
        assert_eq!(session.diagnostics.overlapping_tool_ms, 0);
        assert_eq!(
            session
                .operations
                .iter()
                .find(|operation| operation.call_id == "late")
                .and_then(|operation| operation.turn_id.as_ref()),
            None
        );
    }

    #[test]
    fn derives_subsecond_start_from_completion_and_duration() {
        let trace = concat!(
            r#"{"timestamp":"2026-01-01T00:00:00.200Z","type":"event_msg","payload":{"type":"task_started","started_at":1767225600,"turn_id":"one"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:00.400Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"early","arguments":"{}"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:00.500Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"inside","arguments":"{}"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"function_call_output","call_id":"inside","output":"done"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T01:00:00Z","type":"event_msg","payload":{"type":"task_complete","completed_at":1767225606,"duration_ms":5500,"turn_id":"one"}}"#,
        );

        let session = parse_reader(
            Cursor::new(trace),
            "trace.jsonl".to_owned(),
            trace.len() as u64,
        );

        assert_eq!(
            session.turns[0].started_at.to_string(),
            "2026-01-01T00:00:00.5Z"
        );
        assert_eq!(
            session.turns[0].ended_at.to_string(),
            "2026-01-01T00:00:06Z"
        );
        assert_eq!(session.turns[0].duration_ms, 5_500);
        assert_eq!(session.tool_duration_ms, 500);
        assert_eq!(session.diagnostics.unassigned_calls, 1);
    }

    #[test]
    fn matches_tool_search_events_and_previews_object_arguments() {
        let trace = concat!(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"one"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"tool_search_call","arguments":{"query":"Rust tests"},"call_id":"search","status":"completed"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"tool_search_output","call_id":"search","status":"completed","tools":[]}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"one"}}"#,
        );

        let session = parse_reader(
            Cursor::new(trace),
            "trace.jsonl".to_owned(),
            trace.len() as u64,
        );

        assert_eq!(session.operations.len(), 1);
        assert_eq!(session.operations[0].name, "tool_search");
        assert_eq!(session.operations[0].preview, "Rust tests");
        assert_eq!(session.operations[0].status, OperationStatus::Returned);
        assert_eq!(session.diagnostics.matched_calls, 1);
    }

    #[test]
    fn missing_call_id_cannot_match_a_real_output_id() {
        let trace = concat!(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"one"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{}"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"function_call_output","call_id":"missing-call-line-2","output":"done"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"one"}}"#,
        );

        let session = parse_reader(
            Cursor::new(trace),
            "trace.jsonl".to_owned(),
            trace.len() as u64,
        );

        assert_eq!(session.operations[0].status, OperationStatus::Pending);
        assert_eq!(session.diagnostics.missing_call_ids, 1);
        assert_eq!(session.diagnostics.matched_calls, 0);
        assert_eq!(session.diagnostics.unmatched_calls, 1);
        assert_eq!(session.diagnostics.unmatched_outputs, 1);
    }

    #[test]
    fn rejects_malformed_recorded_duration_with_context() {
        let trace = concat!(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"task_started","started_at":1767225600,"turn_id":"one"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:05Z","type":"event_msg","payload":{"type":"task_complete","completed_at":1767225605,"duration_ms":"5000","turn_id":"one"}}"#,
        );

        let session = parse_reader(
            Cursor::new(trace),
            "trace.jsonl".to_owned(),
            trace.len() as u64,
        );

        assert_eq!(session.turns[0].duration_ms, 5_000);
        assert!(session.diagnostics.parse_errors.iter().any(|error| {
            error.contains("line 2: turn `one` duration_ms") && error.contains("observed string")
        }));
    }

    #[test]
    fn rejects_recorded_duration_outside_timestamp_range() {
        let trace = concat!(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"task_started","started_at":1767225600,"turn_id":"one"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:05Z","type":"event_msg","payload":{"type":"task_complete","completed_at":1767225605,"duration_ms":18446744073709551615,"turn_id":"one"}}"#,
        );

        let session = parse_reader(
            Cursor::new(trace),
            "trace.jsonl".to_owned(),
            trace.len() as u64,
        );

        assert_eq!(session.turns[0].duration_ms, 5_000);
        assert!(session.diagnostics.parse_errors.iter().any(|error| {
            error.contains("line 2: turn `one` duration_ms 18446744073709551615")
                && error.contains("supported timestamp range")
        }));
    }

    #[test]
    fn duplicate_call_ids_are_not_matched_or_timed() {
        let trace = concat!(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"one"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"duplicate","arguments":"{}"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"duplicate","arguments":"{}"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:03Z","type":"response_item","payload":{"type":"function_call_output","call_id":"duplicate","output":"done"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:04Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"one"}}"#,
        );

        let session = parse_reader(
            Cursor::new(trace),
            "trace.jsonl".to_owned(),
            trace.len() as u64,
        );

        assert_eq!(session.tool_duration_ms, 0);
        assert!(
            session
                .operations
                .iter()
                .all(|operation| operation.duration_ms.is_none())
        );
        assert_eq!(session.diagnostics.duplicate_call_ids, 1);
        assert!(session.diagnostics.parse_errors.iter().any(|error| {
            error.contains("line 3: duplicate call_id `duplicate`") && error.contains("ambiguous")
        }));
    }

    #[test]
    fn duplicate_output_ids_are_not_matched_or_timed() {
        let trace = concat!(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"one"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"duplicate","arguments":"{}"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"function_call_output","call_id":"duplicate","output":"first"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:03Z","type":"response_item","payload":{"type":"function_call_output","call_id":"duplicate","output":"second"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:04Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"one"}}"#,
        );

        let session = parse_reader(
            Cursor::new(trace),
            "trace.jsonl".to_owned(),
            trace.len() as u64,
        );

        assert_eq!(session.tool_duration_ms, 0);
        assert_eq!(session.operations[0].duration_ms, None);
        assert_eq!(session.diagnostics.duplicate_output_ids, 1);
        assert!(session.diagnostics.parse_errors.iter().any(|error| {
            error.contains("line 4: duplicate output call_id `duplicate`")
                && error.contains("ambiguous")
        }));
    }

    #[test]
    fn truncated_output_cannot_match_a_call() {
        let trace = concat!(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"one"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"a","arguments":"{}"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"function_call_output","call_id":"a","output":"truncated""#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"one"}}"#,
        );

        let session = parse_reader(
            Cursor::new(trace),
            "trace.jsonl".to_owned(),
            trace.len() as u64,
        );

        assert_eq!(session.operations[0].status, OperationStatus::Pending);
        assert_eq!(session.diagnostics.unmatched_calls, 1);
        assert!(session.diagnostics.parse_errors[0].starts_with("line 3: invalid tool output:"));
    }

    #[test]
    fn duration_totals_saturate() {
        assert_eq!(saturating_sum([u64::MAX, 1].into_iter()), u64::MAX);
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
